//! Path-scoped rule injection hook for the Codex CLI.
//!
//! Codex invokes this binary for `PreToolUse`, `SessionStart`, and
//! `PostCompact` events (wired in `.codex/config.toml`). The hook payload
//! arrives as JSON on stdin. For `PreToolUse` the hook inspects the paths a
//! tool is about to touch; whenever a touched path matches the `paths:` globs
//! of a rule file under [`RULES_DIR`], that rule's body is emitted once per
//! session as `additionalContext`, giving the agent the relevant guidance just
//! in time. Which rules were already injected is cached per session and cleared
//! on `SessionStart`/`PostCompact`.
//!
//! # Failure model
//!
//! This is a context-injection hook, never a gate: a failure must not block the
//! developer's tool call. [`main`] therefore reports any error on stderr and
//! still exits `0` ("fail open"). Errors stay explicit and contextualized so
//! they remain actionable — nothing is swallowed silently.
//!
//! # Structure
//!
//! The logic is decomposed into small, side-effect-free functions (glob
//! matching, front matter parsing, shell path extraction) separated from the
//! IO/CLI boundary, so it can be lifted into a library later. The unit tests at
//! the end of the file pin that behavior.

use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Directory, relative to the working directory, scanned for `*.md` rule files.
const RULES_DIR: &str = ".claude/rules";
/// Maximum number of characters emitted for a single rule body.
const MAX_RULE_CHARS: usize = 6000;
/// Maximum number of characters emitted across all rules in one injection.
const MAX_BATCH_CHARS: usize = 12000;
/// Time spent waiting for a contended session cache lock before failing open.
const STATE_LOCK_RETRIES: usize = 200;
const STATE_LOCK_SLEEP: Duration = Duration::from_millis(5);

/// Result of a hook operation. The error is a human-readable, contextualized
/// message meant for the hook user's stderr (see the crate-level failure model).
type HookResult<T> = Result<T, String>;

/// A rule discovered under [`RULES_DIR`], ready to be matched and injected.
#[derive(Debug, Clone)]
struct Rule {
    /// Stable identity (the rule's absolute path) used to inject it at most
    /// once per session.
    key: String,
    /// Glob patterns from the `paths:` front matter; `None` means the rule
    /// applies to every touched path.
    paths: Option<Vec<String>>,
    /// Rule body with any front matter stripped.
    content: String,
}

/// Per-session record of which rules have already been injected.
#[derive(Debug, Default)]
struct HookState {
    injected_rules: Vec<String>,
}

/// Directory-backed lock guarding one session cache file.
#[derive(Debug)]
struct StateLock {
    path: PathBuf,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "[path-rules] failed to remove cache state lock {}: {error}",
                path_to_string(&self.path)
            );
        }
    }
}

/// Outcome of parsing a rule file's optional front matter.
#[derive(Debug)]
struct ParsedRule {
    paths: Option<Vec<String>>,
    content: String,
}

fn main() {
    if env::args().any(|arg| arg == "--self-test") {
        match run_self_test() {
            Ok(()) => {
                println!("path-rules self-test passed");
            }
            Err(error) => {
                eprintln!("[path-rules] self-test failed: {error}");
                process::exit(1);
            }
        }
        return;
    }

    // Fail open: a context-injection hook must never block the developer's tool
    // call, so a runtime error is reported on stderr and the process still exits
    // 0. The `--self-test` branch above is the only path allowed to exit nonzero.
    if let Err(error) = run_cli() {
        eprintln!("[path-rules] {error}");
    }
}

/// Read the hook payload from stdin, run the hook, and print any output JSON.
///
/// # Errors
///
/// Propagates failures reading stdin, parsing the payload, resolving the
/// working directory, running the hook, or serializing its output.
fn run_cli() -> HookResult<()> {
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("failed to read stdin: {error}"))?;

    let input = parse_json_object(&text)?;
    let fallback_cwd =
        env::current_dir().map_err(|error| format!("failed to read current directory: {error}"))?;

    if let Some(output) = run_hook(&input, &fallback_cwd)? {
        let rendered = serde_json::to_string(&output)
            .map_err(|error| format!("failed to render hook output: {error}"))?;
        println!("{rendered}");
    }

    Ok(())
}

/// Run the hook against an already-parsed payload, using the default on-disk
/// cache location.
///
/// # Errors
///
/// See [`run_hook_with_cache`].
fn run_hook(input: &Value, fallback_cwd: &Path) -> HookResult<Option<Value>> {
    run_hook_with_cache(input, fallback_cwd, None)
}

/// Core hook logic, shared by production and the self-test.
///
/// On `SessionStart`/`PostCompact` it resets the session state and returns
/// `None`. On `PreToolUse` it matches the touched paths against the discovered
/// rules and returns `Some(output)` carrying the `additionalContext` for any
/// rules not yet injected this session, or `None` when there is nothing to
/// inject. All other events return `None`.
///
/// `cache_root` overrides the state location (used by tests); production passes
/// `None`, deriving the location from the environment via [`cache_file`].
///
/// # Errors
///
/// Propagates failures scanning rules or reading/writing the session state.
fn run_hook_with_cache(
    input: &Value,
    fallback_cwd: &Path,
    cache_root: Option<&Path>,
) -> HookResult<Option<Value>> {
    let cwd = read_field_string(input, &["cwd"])
        .map_or_else(|| clean_path(fallback_cwd), resolve_from_process);
    let event_name = read_field_string(input, &["hook_event_name", "hookEventName"]);
    let session_id = read_session_id(input, &cwd);

    if matches!(event_name, Some("SessionStart" | "PostCompact")) {
        let _lock = acquire_state_lock(&cwd, &session_id, cache_root)?;
        reset_state(&cwd, &session_id, cache_root)?;
        return Ok(None);
    }

    if event_name != Some("PreToolUse") {
        return Ok(None);
    }

    let touched_paths = extract_touched_paths(input, &cwd);
    if touched_paths.is_empty() {
        return Ok(None);
    }

    let rules = scan_rules(&cwd)?;
    let _lock = acquire_state_lock(&cwd, &session_id, cache_root)?;
    let mut state = read_state(&cwd, &session_id, cache_root)?;
    let mut selected = Vec::new();

    for rule in rules {
        let matched = touched_paths
            .iter()
            .any(|trigger_path| rule_matches(&rule, trigger_path, &cwd));
        if !matched || state.injected_rules.contains(&rule.key) {
            continue;
        }

        state.injected_rules.push(rule.key.clone());
        selected.push(rule);
    }

    write_state(&cwd, &session_id, cache_root, &state)?;

    if selected.is_empty() {
        return Ok(None);
    }

    Ok(Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": render_rules(&selected),
        }
    })))
}

/// Parse the hook payload, which Codex always sends as a JSON object.
///
/// Empty or whitespace-only input maps to an empty object, because some events
/// carry no payload.
///
/// # Errors
///
/// Returns an error if the input is not valid JSON, or if it is valid JSON of
/// any kind other than an object. A non-object payload is rejected rather than
/// silently treated as empty, so malformed input fails explicitly.
fn parse_json_object(text: &str) -> HookResult<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }

    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| format!("failed to parse hook JSON: {error}"))?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(format!(
            "hook payload must be a JSON object, got {}",
            json_kind(&parsed)
        ))
    }
}

/// Human-readable name of a JSON value's kind, for error messages.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Resolve a stable, filesystem-safe session identifier.
///
/// Prefers the payload's session id, then the `CODEX_SESSION_ID` environment
/// variable, and finally derives a per-process, per-directory fallback so
/// independent sessions in the same directory do not share state.
fn read_session_id(input: &Value, cwd: &Path) -> String {
    let id = read_field_string(input, &["session_id", "sessionId", "thread_id", "threadId"])
        .map(ToOwned::to_owned)
        .or_else(|| {
            env::var("CODEX_SESSION_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| {
            format!(
                "unknown-{}-{}",
                parent_process_id(),
                &hash_text(&path_to_string(cwd))[..8]
            )
        });

    sanitize_file_name(&id)
}

/// Resolve the path of the per-session state file.
///
/// The cache root is taken from `cache_root`, else `CODEX_PATH_RULES_CACHE`,
/// else `XDG_CACHE_HOME`, else `~/.cache`. Entries are namespaced by a hash of
/// the working directory so different repositories never collide.
fn cache_file(cwd: &Path, session_id: &str, cache_root: Option<&Path>) -> PathBuf {
    let root = cache_root
        .map(PathBuf::from)
        .or_else(|| env::var_os("CODEX_PATH_RULES_CACHE").map(PathBuf::from))
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|value| PathBuf::from(value).join("codex-path-rules"))
        })
        .unwrap_or_else(|| home_dir().join(".cache").join("codex-path-rules"));

    root.join(&hash_text(&path_to_string(cwd))[..16])
        .join(format!("{session_id}.json"))
}

/// Resolve the directory used as the lock for a session state file.
fn state_lock_dir(cwd: &Path, session_id: &str, cache_root: Option<&Path>) -> PathBuf {
    cache_file(cwd, session_id, cache_root).with_extension("json.lock")
}

/// Acquire the per-session cache lock.
///
/// Uses `create_dir` as the atomic operation, keeping the implementation in the
/// standard library.
///
/// # Errors
///
/// Returns an error if the lock parent cannot be created, the lock cannot be
/// created, or another process keeps the lock too long.
fn acquire_state_lock(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
) -> HookResult<StateLock> {
    let path = state_lock_dir(cwd, session_id, cache_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create cache lock directory: {error}"))?;
    }

    for _ in 0..STATE_LOCK_RETRIES {
        match fs::create_dir(&path) {
            Ok(()) => return Ok(StateLock { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // ponytail: stale lock after SIGKILL fails open; add pid/mtime cleanup if it gets noisy.
                thread::sleep(STATE_LOCK_SLEEP);
            }
            Err(error) => {
                return Err(format!(
                    "failed to create cache state lock {}: {error}",
                    path_to_string(&path)
                ));
            }
        }
    }

    Err(format!(
        "timed out acquiring cache state lock {}",
        path_to_string(&path)
    ))
}

/// Load the per-session state, returning a fresh default when no cache file
/// exists yet.
///
/// # Errors
///
/// A missing cache file is the expected first-run case and yields a default
/// state. Any other IO error, or a corrupt (non-JSON) cache file, is surfaced
/// as an error rather than silently reset, in line with the repository's
/// fail-fast policy.
fn read_state(cwd: &Path, session_id: &str, cache_root: Option<&Path>) -> HookResult<HookState> {
    let file = cache_file(cwd, session_id, cache_root);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HookState::default()),
        Err(error) => {
            return Err(format!(
                "failed to read cache state {}: {error}",
                path_to_string(&file)
            ));
        }
    };

    let parsed: Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "failed to parse cache state {}: {error}",
            path_to_string(&file)
        )
    })?;

    let injected_rules = parsed
        .get("injectedRules")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    Ok(HookState { injected_rules })
}

/// Persist the per-session state, creating the cache directory if needed.
///
/// # Errors
///
/// Returns an error if the cache directory cannot be created or the file
/// cannot be written.
fn write_state(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
    state: &HookState,
) -> HookResult<()> {
    let file = cache_file(cwd, session_id, cache_root);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create cache directory: {error}"))?;
    }

    let output = json!({ "injectedRules": state.injected_rules });
    let temporary = file.with_extension(format!("json.tmp-{}", process::id()));
    fs::write(&temporary, format!("{output}\n")).map_err(|error| {
        format!(
            "failed to write temporary cache state {}: {error}",
            path_to_string(&temporary)
        )
    })?;
    fs::rename(&temporary, &file).map_err(|error| {
        format!(
            "failed to replace cache state {}: {error}",
            path_to_string(&file)
        )
    })
}

/// Delete the per-session state file so rules are re-injected from scratch.
///
/// A missing file is treated as success.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be removed.
fn reset_state(cwd: &Path, session_id: &str, cache_root: Option<&Path>) -> HookResult<()> {
    match fs::remove_file(cache_file(cwd, session_id, cache_root)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to reset cache state: {error}")),
    }
}

/// Discover and parse every rule under [`RULES_DIR`], sorted by path for stable
/// ordering. Rules with empty bodies are skipped, and an absent directory
/// yields an empty list.
///
/// # Errors
///
/// Returns an error if the directory tree cannot be traversed or a rule file
/// cannot be read.
fn scan_rules(cwd: &Path) -> HookResult<Vec<Rule>> {
    let rules_dir = resolve_path(cwd, RULES_DIR);
    if !rules_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = find_markdown_files(&rules_dir)?;
    files.sort();

    let mut rules = Vec::new();
    for absolute_path in files {
        let markdown = fs::read_to_string(&absolute_path).map_err(|error| {
            format!(
                "failed to read rule {}: {error}",
                path_to_string(&absolute_path)
            )
        })?;
        let parsed = parse_rule_markdown(&markdown);
        if parsed.content.is_empty() {
            continue;
        }

        rules.push(Rule {
            key: path_to_string(&absolute_path),
            paths: parsed.paths,
            content: parsed.content,
        });
    }

    Ok(rules)
}

/// Recursively collect regular `*.md` files under `dir`.
///
/// Symlinks are ignored so a repo-local rule cannot inject an arbitrary
/// out-of-tree file into model context via `fs::read_to_string`.
///
/// # Errors
///
/// Returns an error if a directory or one of its entries cannot be read.
fn find_markdown_files(dir: &Path) -> HookResult<Vec<PathBuf>> {
    let mut found = Vec::new();
    let entries = fs::read_dir(dir)
        .map_err(|error| format!("failed to read directory {}: {error}", path_to_string(dir)))?;

    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read directory entry: {error}"))?;
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "failed to read file type for {}: {error}",
                path_to_string(&entry.path())
            )
        })?;
        let path = entry.path();

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            found.extend(find_markdown_files(&path)?);
        } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "md")
        {
            found.push(path);
        }
    }

    Ok(found)
}

/// Split a rule file into its optional front matter and its body.
///
/// A leading UTF-8 BOM is ignored. Front matter is the block delimited by `---`
/// lines at the very start of the file; its `paths:` entries become the rule's
/// globs. With no front matter, the whole trimmed text is the body.
fn parse_rule_markdown(markdown: &str) -> ParsedRule {
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let Some((first_line, mut position)) = read_line(text, 0) else {
        return ParsedRule {
            paths: None,
            content: text.trim().to_owned(),
        };
    };

    if !is_frontmatter_delimiter(first_line) {
        return ParsedRule {
            paths: None,
            content: text.trim().to_owned(),
        };
    }

    let frontmatter_start = position;
    while position <= text.len() {
        let line_start = position;
        let Some((line, next_position)) = read_line(text, position) else {
            break;
        };

        if is_frontmatter_delimiter(line) {
            let raw_frontmatter = &text[frontmatter_start..line_start];
            let content = text[next_position..].trim().to_owned();
            let paths = parse_paths(raw_frontmatter);
            return ParsedRule {
                paths: (!paths.is_empty()).then_some(paths),
                content,
            };
        }

        if next_position == text.len() {
            break;
        }
        position = next_position;
    }

    ParsedRule {
        paths: None,
        content: text.trim().to_owned(),
    }
}

/// Read one line starting at byte offset `start`, returning the line (without
/// its trailing `\n`) and the offset of the next line, or `None` at end of input.
fn read_line(text: &str, start: usize) -> Option<(&str, usize)> {
    if start >= text.len() {
        return None;
    }

    if let Some(relative_end) = text[start..].find('\n') {
        let end = start + relative_end;
        Some((&text[start..end], end + 1))
    } else {
        Some((&text[start..], text.len()))
    }
}

/// True when a line is a `---` front matter fence, ignoring a trailing `\r` and
/// trailing spaces or tabs.
fn is_frontmatter_delimiter(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    line.trim_end_matches([' ', '\t']) == "---"
}

/// Extract the `paths` patterns from rule front matter.
///
/// Three YAML forms are understood, matching Claude Code's native rules: a
/// block list (`paths:` then `- value` items), an inline flow list
/// (`paths: [a, b]`), and a single scalar (`paths: value`). Values may be
/// single- or double-quoted and may carry a trailing ` # comment`. Duplicates
/// are dropped; for the block form, parsing stops at the first non-list line
/// after `paths:`.
fn parse_paths(frontmatter: &str) -> Vec<String> {
    let mut lines = frontmatter.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("paths:") else {
            continue;
        };

        let rest = rest.trim();
        if rest.is_empty() {
            return parse_block_list(lines);
        }
        if rest.starts_with('[') {
            return parse_flow_list(rest);
        }

        let value = unquote(rest);
        return if value.is_empty() {
            Vec::new()
        } else {
            vec![value]
        };
    }

    Vec::new()
}

/// Collect the `- value` items following a bare `paths:` line, stopping at the
/// first non-empty line that is not a list item.
fn parse_block_list<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut paths = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(item) = trimmed.strip_prefix('-') else {
            break;
        };

        let value = unquote(item.trim());
        if !value.is_empty() && !paths.contains(&value) {
            paths.push(value);
        }
    }

    paths
}

/// Parse an inline flow list such as `["a", "b"]`, splitting on top-level
/// commas so a brace group like `{ts,tsx}` inside an item stays intact.
fn parse_flow_list(rest: &str) -> Vec<String> {
    let body = rest.strip_prefix('[').unwrap_or(rest);
    let body = body.rsplit_once(']').map_or(body, |(before, _)| before);

    let mut paths = Vec::new();
    for item in split_top_level_commas(body) {
        let value = unquote(item.trim());
        if !value.is_empty() && !paths.contains(&value) {
            paths.push(value);
        }
    }

    paths
}

/// Split `text` on commas that sit outside any quotes or `{}`/`[]` nesting, so
/// a list separator is never confused with a comma inside a quoted value or a
/// `{a,b}` brace group.
fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0;

    for (offset, character) in text.char_indices() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }

    parts.push(&text[start..]);
    parts
}

/// Unwrap a single- or double-quoted scalar, or strip a trailing ` # comment`
/// from a bare scalar.
fn unquote(value: &str) -> String {
    if let Some(quote @ ('"' | '\'')) = value.chars().next() {
        let offset = quote.len_utf8();
        if let Some(end) = value[offset..].find(quote) {
            return value[offset..offset + end].to_owned();
        }
        return value.to_owned();
    }

    let uncommented = value.find(" #").map_or(value, |index| &value[..index]);
    uncommented.trim_end().to_owned()
}

/// Decide whether `trigger_path` activates `rule`.
///
/// A rule without `paths:` matches every path; otherwise the path, normalized
/// relative to `cwd`, must match at least one of the rule's globs. Paths that
/// resolve outside `cwd` never match.
fn rule_matches(rule: &Rule, trigger_path: &str, cwd: &Path) -> bool {
    let Some(relative_path) = normalize_trigger_path(trigger_path, cwd) else {
        return false;
    };

    rule.paths.as_ref().is_none_or(|paths| {
        paths
            .iter()
            .any(|pattern| glob_matches(pattern, &relative_path))
    })
}

/// Express `input_path` as a `cwd`-relative POSIX path, or `None` if it resolves
/// outside `cwd`.
fn normalize_trigger_path(input_path: &str, cwd: &Path) -> Option<String> {
    let absolute = resolve_path(cwd, input_path);
    let relative = absolute.strip_prefix(cwd).ok()?;
    Some(strip_dot_slash(&path_to_posix(relative)))
}

/// Match a POSIX-style path glob against a candidate path.
///
/// Supports `?` (any single character except `/`), `*` (any run within one path
/// segment), `**` (any run across segments, including `/`), and `{a,b}` brace
/// alternation, expanded to its alternatives before matching so the candidate
/// matches when any expansion does. Both inputs are normalized to forward
/// slashes with a leading `./` stripped.
fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let candidate = to_posix(candidate);
    expand_braces(&to_posix(pattern)).iter().any(|expanded| {
        let expanded = strip_dot_slash(expanded);
        let mut failed = HashSet::new();
        glob_matches_from(expanded.as_bytes(), 0, candidate.as_bytes(), 0, &mut failed)
    })
}

/// Expand brace alternations such as `a.{x,y}` into the concrete patterns
/// `["a.x", "a.y"]`, taking the cartesian product across multiple groups.
///
/// Follows shell/native semantics: a `{...}` group without a top-level comma
/// (e.g. `{x}`) is left literal, and an unbalanced brace disables expansion.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = matching_brace(pattern, open) else {
        return vec![pattern.to_owned()];
    };

    let alternatives = split_top_level_commas(&pattern[open + 1..close]);
    if alternatives.len() < 2 {
        return vec![pattern.to_owned()];
    }

    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let mut expanded = Vec::new();
    for alternative in alternatives {
        let branch = format!("{prefix}{}{suffix}", alternative.trim());
        for expansion in expand_braces(&branch) {
            if !expanded.contains(&expansion) {
                expanded.push(expansion);
            }
        }
    }

    expanded
}

/// Byte offset of the `}` closing the `{` at `open`, accounting for nested
/// braces, or `None` when the brace is unbalanced.
fn matching_brace(pattern: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in pattern[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }

    None
}

/// Recursive backtracking matcher backing [`glob_matches`].
///
/// `failed` memoizes `(pattern_index, candidate_index)` pairs already known not
/// to match, which bounds the otherwise exponential backtracking of overlapping
/// `*`/`**` wildcards to polynomial time.
fn glob_matches_from(
    pattern: &[u8],
    pattern_index: usize,
    candidate: &[u8],
    candidate_index: usize,
    failed: &mut HashSet<(usize, usize)>,
) -> bool {
    if failed.contains(&(pattern_index, candidate_index)) {
        return false;
    }

    let matched = if pattern_index == pattern.len() {
        candidate_index == candidate.len()
    } else if pattern[pattern_index..].starts_with(b"**/") {
        glob_matches_from(
            pattern,
            pattern_index + 3,
            candidate,
            candidate_index,
            failed,
        ) || candidate[candidate_index..]
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .map(|(offset, _)| candidate_index + offset + 1)
            .any(|next_index| {
                glob_matches_from(pattern, pattern_index + 3, candidate, next_index, failed)
            })
    } else if pattern[pattern_index..].starts_with(b"**") {
        (candidate_index..=candidate.len()).any(|next_index| {
            glob_matches_from(pattern, pattern_index + 2, candidate, next_index, failed)
        })
    } else if pattern[pattern_index] == b'*' {
        let segment_end = candidate[candidate_index..]
            .iter()
            .position(|byte| *byte == b'/')
            .map_or(candidate.len(), |offset| candidate_index + offset);
        (candidate_index..=segment_end).any(|next_index| {
            glob_matches_from(pattern, pattern_index + 1, candidate, next_index, failed)
        })
    } else if pattern[pattern_index] == b'?' {
        candidate
            .get(candidate_index)
            .is_some_and(|candidate_char| *candidate_char != b'/')
            && glob_matches_from(
                pattern,
                pattern_index + 1,
                candidate,
                candidate_index + 1,
                failed,
            )
    } else {
        candidate
            .get(candidate_index)
            .is_some_and(|candidate_char| *candidate_char == pattern[pattern_index])
            && glob_matches_from(
                pattern,
                pattern_index + 1,
                candidate,
                candidate_index + 1,
                failed,
            )
    };

    if !matched {
        failed.insert((pattern_index, candidate_index));
    }
    matched
}

/// Collect the deduplicated, normalized paths a tool call is about to touch.
///
/// The `path`/`file_path`/`filePath` fields are always read. For `Bash` the
/// command is additionally parsed for read-like file arguments; for
/// `apply_patch`/`Edit`/`Write`/`MultiEdit` the patch body is parsed for the
/// affected files.
fn extract_touched_paths(input: &Value, cwd: &Path) -> Vec<String> {
    let tool_name = read_field_string(input, &["tool_name", "toolName"]).unwrap_or_default();
    let tool_input =
        read_object(input.get("tool_input")).or_else(|| read_object(input.get("toolInput")));
    let command = tool_input
        .and_then(|value| read_string(value.get("command")))
        .unwrap_or_default();
    let field_paths = tool_input.map(extract_path_fields).unwrap_or_default();

    if tool_name == "Bash" {
        return unique_paths(
            field_paths
                .into_iter()
                .chain(extract_bash_paths(command, cwd)),
        );
    }
    if matches!(tool_name, "apply_patch" | "Edit" | "Write" | "MultiEdit") {
        return unique_paths(field_paths.into_iter().chain(extract_patch_paths(command)));
    }

    unique_paths(field_paths)
}

/// Read the `path`, `file_path`, and `filePath` string fields of a tool input.
fn extract_path_fields(value: &Map<String, Value>) -> Vec<String> {
    ["path", "file_path", "filePath"]
        .into_iter()
        .filter_map(|key| read_string(value.get(key)).map(ToOwned::to_owned))
        .collect()
}

/// Extract affected file paths from an `apply_patch` body or a unified diff.
///
/// Recognizes `*** Add/Update/Delete File:` headers and `--- a/` / `+++ b/`
/// markers, skipping `/dev/null`.
fn extract_patch_paths(command: &str) -> Vec<String> {
    command
        .lines()
        .filter_map(|line| {
            let path = ["*** Add File:", "*** Update File:", "*** Delete File:"]
                .into_iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .or_else(|| line.strip_prefix("--- a/"))
                .or_else(|| line.strip_prefix("+++ b/"))?
                .trim();

            (path != "/dev/null").then(|| path.to_owned())
        })
        .collect()
}

/// Extract the file paths a Bash command reads.
///
/// The command is tokenized and split on `|`, `;`, `&&`, and `||`; a
/// `bash -c` / `sh -c` wrapper is unwrapped and its inner script re-parsed.
/// Each segment is then inspected by [`extract_segment_paths`].
fn extract_bash_paths(command: &str, cwd: &Path) -> Vec<String> {
    let tokens = tokenize_shell(command);
    if tokens.is_empty() {
        return Vec::new();
    }

    if let Some(unwrapped) = unwrap_shell(&tokens) {
        return extract_bash_paths(&unwrapped, cwd);
    }

    let mut paths = Vec::new();
    for segment in split_command_segments(&tokens) {
        paths.extend(extract_segment_paths(&segment, cwd));
    }

    unique_paths(paths)
}

/// If `tokens` is a `bash`/`sh`/`zsh` invocation with `-c`/`-lc`, return the
/// inner script string so it can be re-parsed.
fn unwrap_shell(tokens: &[String]) -> Option<String> {
    let command = basename(tokens.first()?);
    if !matches!(command.as_str(), "bash" | "sh" | "zsh") {
        return None;
    }

    tokens
        .iter()
        .position(|token| token == "-c" || token == "-lc")
        .and_then(|index| tokens.get(index + 1))
        .cloned()
}

/// Split tokens into command segments at the `|`, `;`, `&&`, and `||`
/// operators, dropping the operators themselves.
fn split_command_segments(tokens: &[String]) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();

    for token in tokens {
        if matches!(token.as_str(), "|" | ";" | "&&" | "||") {
            if !current.is_empty() {
                segments.push(current);
                current = Vec::new();
            }
            continue;
        }

        current.push(token.clone());
    }

    if !current.is_empty() {
        segments.push(current);
    }

    segments
}

/// Extract file paths from a single command segment.
///
/// Leading `NAME=value` environment assignments are ignored, then the command
/// name selects how its arguments are interpreted (`cat`/`sed`/`head`/`tail`/
/// `rg`/`grep`). Unrecognized commands contribute no paths.
fn extract_segment_paths(segment: &[String], cwd: &Path) -> Vec<String> {
    let tokens = segment
        .iter()
        .filter(|token| !is_environment_assignment(token))
        .cloned()
        .collect::<Vec<_>>();
    let command = tokens
        .first()
        .map_or_else(String::new, |token| basename(token));
    let args = &tokens[1..];

    if matches!(command.as_str(), "cat" | "nl" | "less" | "more") {
        return path_args(&positional_args(args, &[]), cwd);
    }
    if command == "sed" {
        return extract_sed_paths(args, cwd);
    }
    if matches!(command.as_str(), "head" | "tail") {
        return path_args(&positional_args(args, &["-n", "-c"]), cwd);
    }
    if command == "rg" {
        return extract_search_paths(
            args,
            cwd,
            &["-g", "--glob", "--type", "-t", "--type-not", "-T"],
        );
    }
    if command == "grep" {
        return extract_search_paths(args, cwd, &["-e", "-f", "-m", "-A", "-B", "-C"]);
    }

    Vec::new()
}

/// Extract the file operands of a `sed` invocation.
///
/// When no `-e`/`-f` script flag is present the first positional argument is the
/// inline script and is dropped; the remainder are treated as files.
fn extract_sed_paths(args: &[String], cwd: &Path) -> Vec<String> {
    let positional = positional_args(args, &["-e", "-f"]);
    let has_script_flag = args
        .iter()
        .any(|arg| arg == "-e" || arg == "-f" || arg.starts_with("-e"));
    let file_args = if has_script_flag {
        positional
    } else {
        positional.into_iter().skip(1).collect()
    };

    path_args(&file_args, cwd)
}

/// Extract the file operands of an `rg`/`grep` invocation.
///
/// Without `--files` the first positional argument is the search pattern and is
/// dropped; the remainder are treated as files. `options_with_values` lists the
/// flags whose following argument must be skipped.
fn extract_search_paths(args: &[String], cwd: &Path, options_with_values: &[&str]) -> Vec<String> {
    let files_mode = args.iter().any(|arg| arg == "--files");
    let positional = positional_args(args, options_with_values);
    let files = if files_mode {
        positional
    } else {
        positional.into_iter().skip(1).collect()
    };

    path_args(&files, cwd)
}

/// Return the positional (non-flag) arguments from `args`.
///
/// Flags listed in `options_with_values` consume the following argument as their
/// value (unless written as `--flag=value`); a `--` terminator makes every
/// remaining argument positional.
fn positional_args(args: &[String], options_with_values: &[&str]) -> Vec<String> {
    let mut positional = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];

        if arg == "--" {
            positional.extend(args[index + 1..].iter().cloned());
            break;
        }

        if arg.starts_with("--") {
            let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
            if !arg.contains('=') && options_with_values.contains(&name) {
                index += 1;
            }
            index += 1;
            continue;
        }

        if arg.starts_with('-') && arg != "-" {
            if options_with_values.contains(&arg.as_str()) {
                index += 1;
            }
            index += 1;
            continue;
        }

        positional.push(arg.clone());
        index += 1;
    }

    positional
}

/// Keep the path-like arguments (see [`looks_like_path`]), expanding any that
/// name a directory (see [`expand_path_arg`]).
fn path_args(args: &[String], cwd: &Path) -> Vec<String> {
    args.iter()
        .filter(|arg| looks_like_path(arg, cwd))
        .flat_map(|arg| expand_path_arg(arg, cwd))
        .collect()
}

/// Expand a path argument: a directory yields itself plus the files beneath it
/// (bounded by [`find_files_limited`]); anything else yields just the argument.
fn expand_path_arg(arg: &str, cwd: &Path) -> Vec<String> {
    let absolute = resolve_path(cwd, arg);
    match fs::metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() => {
            let mut paths = vec![arg.to_owned()];
            paths.extend(
                find_files_limited(&absolute, 200)
                    .into_iter()
                    .map(|file| path_to_posix(file.strip_prefix(cwd).unwrap_or(&file))),
            );
            paths
        }
        _ => vec![arg.to_owned()],
    }
}

/// Collect up to `limit` files beneath `dir`, skipping `.git`, `node_modules`,
/// and `dist`.
///
/// Best-effort by design: unreadable directories are skipped rather than raising
/// an error, because this only enriches path discovery and must not fail the
/// hook.
fn find_files_limited(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    visit_files_limited(dir, limit, &mut found);
    found
}

/// Depth-first worker for [`find_files_limited`], stopping once `found` reaches
/// `limit`.
fn visit_files_limited(dir: &Path, limit: usize, found: &mut Vec<PathBuf>) {
    if found.len() >= limit {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        if found.len() >= limit {
            return;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | "dist") {
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            visit_files_limited(&path, limit, found);
        } else if file_type.is_file() || file_type.is_symlink() {
            found.push(path);
        }
    }
}

/// Heuristic deciding whether a shell argument denotes a filesystem path.
///
/// An existing file always qualifies. Otherwise the value must look path-like:
/// starting with `.`, `/`, or `~`, containing `/`, or ending in a plausible
/// extension. Empty values, `-`, `$`-prefixed values, and multi-line values are
/// rejected.
fn looks_like_path(value: &str, cwd: &Path) -> bool {
    if value.is_empty() || value == "-" || value.starts_with('$') || value.contains('\n') {
        return false;
    }
    if resolve_path(cwd, value).exists() {
        return true;
    }

    value.starts_with('.')
        || value.starts_with('/')
        || value.starts_with('~')
        || value.contains('/')
        || has_path_like_extension(value)
}

/// True when `value` ends in a plausible file extension (alphanumeric plus `_`
/// or `-`), used as a weak path-likeness signal.
fn has_path_like_extension(value: &str) -> bool {
    let Some((_, extension)) = value.rsplit_once('.') else {
        return false;
    };

    !extension.is_empty()
        && extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

/// Tokenize a shell command into words and operators.
///
/// Handles single and double quotes, backslash escapes, and the `|`, `;`, `&&`,
/// and `||` control operators as standalone tokens. This is a best-effort lexer
/// for path discovery, not a full POSIX shell parser.
fn tokenize_shell(command: &str) -> Vec<String> {
    let chars = command.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut index = 0;

    while index < chars.len() {
        let character = chars[index];
        let next = chars.get(index + 1).copied();

        if let Some(current_quote) = quote {
            if character == current_quote {
                quote = None;
            } else if character == '\\' && current_quote == '"' {
                if let Some(next_character) = next {
                    token.push(next_character);
                    index += 1;
                }
            } else {
                token.push(character);
            }
            index += 1;
            continue;
        }

        if matches!(character, '\'' | '"') {
            quote = Some(character);
            index += 1;
            continue;
        }

        if character == '\\' {
            if let Some(next_character) = next {
                token.push(next_character);
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        if character.is_whitespace() {
            if !token.is_empty() {
                tokens.push(token);
                token = String::new();
            }
            index += 1;
            continue;
        }

        if matches!((character, next), ('&', Some('&')) | ('|', Some('|'))) {
            if !token.is_empty() {
                tokens.push(token);
                token = String::new();
            }
            tokens.push(format!("{}{}", character, next.unwrap_or_default()));
            index += 2;
            continue;
        }

        if matches!(character, '|' | ';') {
            if !token.is_empty() {
                tokens.push(token);
                token = String::new();
            }
            tokens.push(character.to_string());
            index += 1;
            continue;
        }

        token.push(character);
        index += 1;
    }

    if !token.is_empty() {
        tokens.push(token);
    }

    tokens
}

/// Render the selected rules as `<rule>…</rule>` blocks for injection.
///
/// Each body is HTML-escaped and truncated to [`MAX_RULE_CHARS`]; rules are
/// emitted until the cumulative [`MAX_BATCH_CHARS`] budget would be exceeded.
fn render_rules(selected: &[Rule]) -> String {
    let mut rendered_rules = Vec::new();
    let mut remaining = MAX_BATCH_CHARS;

    for rule in selected {
        let limit = MAX_RULE_CHARS.min(remaining);
        let content = truncate(&rule.content, limit);
        let rendered = format!("<rule>\n{}\n</rule>", escape_text(&content));
        let rendered_len = rendered.chars().count();

        if rendered_len > remaining {
            break;
        }

        rendered_rules.push(rendered);
        remaining -= rendered_len;
    }

    rendered_rules.join("\n\n")
}

/// Truncate `text` to at most `max_chars` characters, appending a notice when
/// content was dropped.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let prefix = text.chars().take(max_chars).collect::<String>();
    format!("{prefix}\n[Rule truncated. Read the rule file for full details if needed.]")
}

/// Normalize each path to POSIX form without a leading `./`, returning the
/// distinct, non-empty results in first-seen order.
fn unique_paths(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();

    for path in paths {
        let normalized = strip_dot_slash(&to_posix(&path));
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
            unique.push(normalized);
        }
    }

    unique
}

/// Resolve `path` against the process's current directory; used only when the
/// payload omits `cwd`.
fn resolve_from_process(path: &str) -> PathBuf {
    let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_path(&base, path)
}

/// Resolve `path` against `cwd` (absolute paths are kept as-is) and lexically
/// clean the result via [`clean_path`].
fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        clean_path(path)
    } else {
        clean_path(cwd.join(path))
    }
}

/// Lexically normalize a path: drop `.` components and resolve `..` without
/// touching the filesystem (so symlinks are not followed). An empty result
/// becomes `.`.
fn clean_path(path: impl AsRef<Path>) -> PathBuf {
    let mut cleaned = PathBuf::new();

    for component in path.as_ref().components() {
        match component {
            Component::Prefix(prefix) => cleaned.push(prefix.as_os_str()),
            Component::RootDir => cleaned.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !cleaned.pop() && !cleaned.has_root() {
                    cleaned.push("..");
                }
            }
            Component::Normal(value) => cleaned.push(value),
        }
    }

    if cleaned.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

/// Final path component of `path`, or the whole string when it has none.
fn basename(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// True when a token is a `NAME=value` shell environment assignment, so it can
/// be skipped when locating the command name.
fn is_environment_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Return the first present, non-empty string among `keys`.
fn read_field_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| read_string(value.get(*key)))
}

/// Borrow `value` as a JSON object, if it is one.
fn read_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// Read a string value, treating whitespace-only strings as absent.
fn read_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Drop a single leading `./` from a path string.
fn strip_dot_slash(value: &str) -> String {
    value.strip_prefix("./").unwrap_or(value).to_owned()
}

/// Convert Windows-style backslash separators to forward slashes.
fn to_posix(value: &str) -> String {
    value.replace('\\', "/")
}

/// A path rendered as a POSIX-style (forward-slash) string.
fn path_to_posix(path: &Path) -> String {
    to_posix(&path_to_string(path))
}

/// A path rendered as a string, lossily decoding any non-UTF-8 components.
fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// HTML-escape `&`, `<`, and `>` so a rule body cannot break out of its
/// `<rule>` wrapper.
fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Lowercase hex SHA-256 of `value`, used to derive collision-resistant cache
/// path components.
fn hash_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

/// Replace any character other than alphanumerics, `.`, `_`, or `-` with `_`, so
/// a session identifier is safe to embed in a file name.
fn sanitize_file_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The user's home directory from `HOME`, falling back to `.` when unset.
fn home_dir() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Best-effort parent process id, used only to disambiguate the fallback
/// session identifier.
///
/// Reads `/proc/self/stat`; the executable name there is parenthesized and may
/// itself contain spaces or `)`, so the fields after the last `") "` are parsed.
/// Falls back to this process's id off Linux or on any parse failure.
fn parent_process_id() -> u32 {
    let Ok(stat) = fs::read_to_string("/proc/self/stat") else {
        return process::id();
    };
    let Some(after_command) = stat.rsplit_once(") ") else {
        return process::id();
    };
    after_command
        .1
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or_else(process::id)
}

/// End-to-end smoke test of the hook, also runnable in production via
/// `--self-test`.
///
/// Builds a throwaway repository with one CSS rule, then exercises injection,
/// once-per-session de-duplication, reset on `PostCompact`, and `rg --files`
/// discovery end to end.
///
/// # Errors
///
/// Returns an error describing the first failed expectation or IO step.
fn run_self_test() -> HookResult<()> {
    let root = create_temp_dir("codex-path-rules")?;
    let cache = root.join("cache");
    let repo = root.join("repo");

    fs::create_dir_all(repo.join(".claude").join("rules"))
        .map_err(|error| format!("failed to create self-test rules directory: {error}"))?;
    fs::create_dir_all(repo.join("src").join("styles"))
        .map_err(|error| format!("failed to create self-test styles directory: {error}"))?;
    fs::write(
        repo.join("src").join("styles").join("stage.css"),
        ".stage {}\n",
    )
    .map_err(|error| format!("failed to write self-test CSS file: {error}"))?;
    fs::write(
        repo.join(".claude").join("rules").join("css.md"),
        [
            "---",
            "paths:",
            "  - \"src/**/*.css\"",
            "---",
            "",
            "# CSS rule",
            "",
            "Keep CSS in feature files.",
        ]
        .join("\n"),
    )
    .map_err(|error| format!("failed to write self-test rule: {error}"))?;

    let first = run_hook_with_cache(
        &json!({
            "hook_event_name": "PreToolUse",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
            "tool_name": "Bash",
            "tool_input": { "command": "sed -n '1,20p' src/styles/stage.css" },
        }),
        &repo,
        Some(&cache),
    )?;
    let first_context = additional_context(first)?;
    require(
        first_context.contains("Keep CSS in feature files."),
        "expected CSS rule context",
    )?;
    require(
        !first_context.lines().any(|line| line.starts_with("---")),
        "frontmatter leaked into context",
    )?;
    require(
        !first_context.contains("project-rules")
            && !first_context.contains("trigger=")
            && !first_context.contains("path="),
        "internal rule metadata leaked into context",
    )?;
    require(
        first_context.starts_with("<rule>\n"),
        "context did not start with a rule block",
    )?;

    let second = run_hook_with_cache(
        &json!({
            "hook_event_name": "PreToolUse",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
            "tool_name": "Bash",
            "tool_input": { "command": "cat src/styles/stage.css" },
        }),
        &repo,
        Some(&cache),
    )?;
    require(second.is_none(), "rule was reinjected in the same session")?;

    run_hook_with_cache(
        &json!({
            "hook_event_name": "PostCompact",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
        }),
        &repo,
        Some(&cache),
    )?;
    let after_compact = run_hook_with_cache(
        &json!({
            "hook_event_name": "PreToolUse",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
            "tool_name": "apply_patch",
            "tool_input": {
                "command": "*** Begin Patch\n*** Update File: src/styles/stage.css\n*** End Patch\n"
            },
        }),
        &repo,
        Some(&cache),
    )?;
    require(
        additional_context(after_compact)?.contains("CSS rule"),
        "compact reset did not allow reinjection",
    )?;

    run_hook_with_cache(
        &json!({
            "hook_event_name": "PostCompact",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
        }),
        &repo,
        Some(&cache),
    )?;
    let rg_files = run_hook_with_cache(
        &json!({
            "hook_event_name": "PreToolUse",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
            "tool_name": "Bash",
            "tool_input": { "command": "rg --files src/styles" },
        }),
        &repo,
        Some(&cache),
    )?;
    require(
        additional_context(rg_files)?.contains("CSS rule"),
        "rg --files did not discover CSS rule",
    )?;

    let cache_metadata =
        fs::metadata(&cache).map_err(|error| format!("cache directory missing: {error}"))?;
    require(cache_metadata.is_dir(), "cache path is not a directory")?;

    fs::remove_dir_all(root)
        .map_err(|error| format!("failed to clean self-test directory: {error}"))?;
    Ok(())
}

/// Extract `hookSpecificOutput.additionalContext` from a hook output value.
///
/// # Errors
///
/// Returns an error when the field is absent (the hook injected nothing).
fn additional_context(output: Option<Value>) -> HookResult<String> {
    output
        .and_then(|value| {
            value
                .get("hookSpecificOutput")
                .and_then(|value| value.get("additionalContext"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| "expected hook additionalContext output".to_owned())
}

/// Turn a failed self-test condition into a descriptive error.
///
/// # Errors
///
/// Returns `message` as the error when `condition` is false.
fn require(condition: bool, message: &str) -> HookResult<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

/// Create a uniquely named directory under the system temp directory.
///
/// # Errors
///
/// Returns an error if no unique directory can be created after many attempts.
fn create_temp_dir(prefix: &str) -> HookResult<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());

    for attempt in 0..100 {
        let path = env::temp_dir().join(format!("{prefix}-{}-{nanos}-{attempt}", process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("failed to create temporary directory: {error}")),
        }
    }

    Err("failed to create unique temporary directory".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_test_should_pass() {
        run_self_test().expect("self-test should pass");
    }

    // glob_matches -----------------------------------------------------------

    #[test]
    fn glob_literal_matches_identical_path() {
        assert!(glob_matches("src/app.ts", "src/app.ts"));
    }

    #[test]
    fn glob_literal_rejects_a_different_path() {
        assert!(!glob_matches("src/app.ts", "src/app.js"));
    }

    #[test]
    fn glob_single_star_stays_within_one_segment() {
        assert!(glob_matches("src/*.css", "src/stage.css"));
    }

    #[test]
    fn glob_single_star_rejects_crossing_a_slash() {
        assert!(!glob_matches("src/*.css", "src/styles/stage.css"));
    }

    #[test]
    fn glob_double_star_crosses_segments() {
        assert!(glob_matches("src/**/*.css", "src/a/b/stage.css"));
    }

    #[test]
    fn glob_double_star_matches_zero_intermediate_segments() {
        assert!(glob_matches("src/**/*.css", "src/stage.css"));
    }

    #[test]
    fn glob_leading_double_star_matches_any_prefix() {
        assert!(glob_matches("**/tokens.css", "src/styles/tokens.css"));
    }

    #[test]
    fn glob_question_mark_matches_one_non_slash_character() {
        assert!(glob_matches("a?c.ts", "abc.ts"));
    }

    #[test]
    fn glob_question_mark_rejects_a_slash() {
        assert!(!glob_matches("a?c", "a/c"));
    }

    #[test]
    fn glob_brace_matches_a_listed_extension() {
        assert!(glob_matches("src/**/*.{css,astro,svelte}", "src/a/b.css"));
    }

    #[test]
    fn glob_brace_matches_a_middle_alternative() {
        assert!(glob_matches("src/**/*.{css,astro,svelte}", "src/a/b.astro"));
    }

    #[test]
    fn glob_brace_matches_a_trailing_alternative() {
        assert!(glob_matches(
            "src/**/*.{css,astro,svelte}",
            "src/a/b.svelte"
        ));
    }

    #[test]
    fn glob_brace_rejects_an_unlisted_extension() {
        assert!(!glob_matches("src/**/*.{css,astro}", "src/a/b.ts"));
    }

    #[test]
    fn glob_brace_expands_two_groups_independently() {
        assert!(glob_matches("{src,lib}/**/*.{ts,tsx}", "lib/a/b.tsx"));
    }

    // tokenize_shell ---------------------------------------------------------

    #[test]
    fn tokenize_splits_on_whitespace() {
        assert_eq!(tokenize_shell("cat a b"), ["cat", "a", "b"]);
    }

    #[test]
    fn tokenize_keeps_single_quoted_text_together() {
        assert_eq!(
            tokenize_shell("sed -n '1,20p' f"),
            ["sed", "-n", "1,20p", "f"]
        );
    }

    #[test]
    fn tokenize_unescapes_inside_double_quotes() {
        assert_eq!(tokenize_shell(r#"echo "a\"b""#), ["echo", "a\"b"]);
    }

    #[test]
    fn tokenize_emits_logical_and_as_a_token() {
        assert_eq!(tokenize_shell("a && b"), ["a", "&&", "b"]);
    }

    #[test]
    fn tokenize_emits_pipe_and_semicolon_as_tokens() {
        assert_eq!(tokenize_shell("a | b ; c"), ["a", "|", "b", ";", "c"]);
    }

    // parse_paths ------------------------------------------------------------

    #[test]
    fn parse_paths_collects_list_items() {
        assert_eq!(
            parse_paths("paths:\n  - src/**\n  - docs/**\n"),
            ["src/**", "docs/**"]
        );
    }

    #[test]
    fn parse_paths_strips_surrounding_quotes() {
        assert_eq!(
            parse_paths("paths:\n  - \"src/**/*.css\"\n"),
            ["src/**/*.css"]
        );
    }

    #[test]
    fn parse_paths_strips_inline_comment() {
        assert_eq!(parse_paths("paths:\n  - src/** # styles\n"), ["src/**"]);
    }

    #[test]
    fn parse_paths_drops_duplicates() {
        assert_eq!(parse_paths("paths:\n  - a\n  - a\n"), ["a"]);
    }

    #[test]
    fn parse_paths_stops_at_first_non_list_line() {
        assert_eq!(parse_paths("paths:\n  - a\nother: x\n  - b\n"), ["a"]);
    }

    #[test]
    fn parse_paths_returns_empty_without_a_paths_key() {
        assert!(parse_paths("name: rule\n").is_empty());
    }

    #[test]
    fn parse_paths_reads_a_scalar_value() {
        assert_eq!(parse_paths("paths: src/**/*.svelte\n"), ["src/**/*.svelte"]);
    }

    #[test]
    fn parse_paths_reads_a_quoted_scalar_value() {
        assert_eq!(
            parse_paths("paths: \"**/agents/**/*.md\"\n"),
            ["**/agents/**/*.md"]
        );
    }

    #[test]
    fn parse_paths_strips_an_inline_comment_from_a_scalar() {
        assert_eq!(parse_paths("paths: src/** # styles\n"), ["src/**"]);
    }

    #[test]
    fn parse_paths_reads_an_inline_flow_list() {
        assert_eq!(
            parse_paths("paths: [\"src/**/*.ts\", \"lib/**\"]\n"),
            ["src/**/*.ts", "lib/**"]
        );
    }

    #[test]
    fn parse_paths_reads_an_unquoted_inline_flow_list() {
        assert_eq!(parse_paths("paths: [a, b]\n"), ["a", "b"]);
    }

    #[test]
    fn parse_paths_keeps_a_brace_group_inside_a_flow_list_intact() {
        assert_eq!(
            parse_paths("paths: [\"src/**/*.{ts,tsx}\"]\n"),
            ["src/**/*.{ts,tsx}"]
        );
    }

    // parse_rule_markdown ----------------------------------------------------

    #[test]
    fn frontmatter_extracts_the_paths_list() {
        let parsed = parse_rule_markdown("---\npaths:\n  - src/**\n---\n\nBody.");
        assert_eq!(parsed.paths, Some(vec!["src/**".to_owned()]));
    }

    #[test]
    fn frontmatter_body_excludes_the_frontmatter() {
        let parsed = parse_rule_markdown("---\npaths:\n  - src/**\n---\n\nBody.");
        assert_eq!(parsed.content, "Body.");
    }

    #[test]
    fn markdown_without_frontmatter_has_no_paths() {
        assert_eq!(
            parse_rule_markdown("# Title\n\nNo frontmatter.").paths,
            None
        );
    }

    #[test]
    fn frontmatter_extracts_a_scalar_paths_value() {
        let parsed = parse_rule_markdown("---\npaths: src/**\n---\n\nBody.");
        assert_eq!(parsed.paths, Some(vec!["src/**".to_owned()]));
    }

    #[test]
    fn frontmatter_ignores_a_leading_byte_order_mark() {
        assert_eq!(
            parse_rule_markdown("\u{feff}---\npaths:\n  - a\n---\nBody").content,
            "Body"
        );
    }

    // extract_bash_paths -----------------------------------------------------

    #[test]
    fn bash_paths_reads_the_cat_argument() {
        assert_eq!(
            extract_bash_paths("cat src/app.ts", Path::new("/repo")),
            ["src/app.ts"]
        );
    }

    #[test]
    fn bash_paths_skips_the_sed_script_and_keeps_the_file() {
        assert_eq!(
            extract_bash_paths("sed -n '1,20p' src/styles/stage.css", Path::new("/repo")),
            ["src/styles/stage.css"]
        );
    }

    #[test]
    fn bash_paths_ignores_a_leading_environment_assignment() {
        assert_eq!(
            extract_bash_paths("FOO=bar cat src/app.ts", Path::new("/repo")),
            ["src/app.ts"]
        );
    }

    #[test]
    fn bash_paths_unwraps_a_bash_dash_c_wrapper() {
        assert_eq!(
            extract_bash_paths("bash -c 'cat src/app.ts'", Path::new("/repo")),
            ["src/app.ts"]
        );
    }

    #[test]
    fn bash_paths_collects_across_a_pipeline() {
        assert_eq!(
            extract_bash_paths("cat a.ts | grep x b.ts", Path::new("/repo")),
            ["a.ts", "b.ts"]
        );
    }

    #[test]
    fn bash_paths_rg_files_keeps_the_directory_operand() {
        assert_eq!(
            extract_bash_paths("rg --files src/styles", Path::new("/repo")),
            ["src/styles"]
        );
    }

    #[test]
    fn bash_paths_are_empty_for_an_unhandled_command() {
        assert!(extract_bash_paths("echo hello", Path::new("/repo")).is_empty());
    }

    // extract_patch_paths ----------------------------------------------------

    #[test]
    fn patch_paths_read_an_apply_patch_header() {
        let body = "*** Begin Patch\n*** Update File: src/styles/stage.css\n*** End Patch\n";
        assert_eq!(extract_patch_paths(body), ["src/styles/stage.css"]);
    }

    #[test]
    fn patch_paths_read_unified_diff_headers() {
        assert_eq!(
            extract_patch_paths("--- a/src/old.ts\n+++ b/src/new.ts\n"),
            ["src/old.ts", "src/new.ts"]
        );
    }

    #[test]
    fn patch_paths_skip_a_dev_null_target() {
        assert!(extract_patch_paths("*** Add File: /dev/null\n").is_empty());
    }

    // clean_path -------------------------------------------------------------

    #[test]
    fn clean_path_drops_current_dir_components() {
        assert_eq!(clean_path("a/./b"), Path::new("a/b"));
    }

    #[test]
    fn clean_path_resolves_parent_dir_components() {
        assert_eq!(clean_path("a/b/../c"), Path::new("a/c"));
    }

    #[test]
    fn clean_path_keeps_an_unrooted_leading_parent_dir() {
        assert_eq!(clean_path("../a"), Path::new("../a"));
    }

    #[test]
    fn clean_path_maps_an_empty_path_to_dot() {
        assert_eq!(clean_path(""), Path::new("."));
    }

    // normalize_trigger_path -------------------------------------------------

    #[test]
    fn normalize_makes_a_relative_path_relative_to_cwd() {
        assert_eq!(
            normalize_trigger_path("src/app.ts", Path::new("/repo")).as_deref(),
            Some("src/app.ts")
        );
    }

    #[test]
    fn normalize_resolves_an_absolute_path_inside_cwd() {
        assert_eq!(
            normalize_trigger_path("/repo/src/app.ts", Path::new("/repo")).as_deref(),
            Some("src/app.ts")
        );
    }

    #[test]
    fn normalize_rejects_a_path_outside_cwd() {
        assert_eq!(
            normalize_trigger_path("/elsewhere/app.ts", Path::new("/repo")),
            None
        );
    }

    // rule_matches -----------------------------------------------------------

    fn rule_with(paths: Option<Vec<String>>) -> Rule {
        Rule {
            key: "k".to_owned(),
            paths,
            content: "c".to_owned(),
        }
    }

    #[test]
    fn rule_matches_when_a_glob_matches() {
        let rule = rule_with(Some(vec!["src/**/*.css".to_owned()]));
        assert!(rule_matches(
            &rule,
            "src/styles/stage.css",
            Path::new("/repo")
        ));
    }

    #[test]
    fn rule_does_not_match_when_no_glob_matches() {
        let rule = rule_with(Some(vec!["docs/**".to_owned()]));
        assert!(!rule_matches(&rule, "src/app.ts", Path::new("/repo")));
    }

    #[test]
    fn rule_without_paths_matches_any_path() {
        let rule = rule_with(None);
        assert!(rule_matches(&rule, "anything/here.txt", Path::new("/repo")));
    }

    #[test]
    fn rule_never_matches_a_path_outside_cwd() {
        let rule = rule_with(None);
        assert!(!rule_matches(&rule, "/outside/x", Path::new("/repo")));
    }

    // unique_paths -----------------------------------------------------------

    #[test]
    fn unique_paths_dedup_preserving_first_seen_order() {
        let input = vec!["b".to_owned(), "a".to_owned(), "b".to_owned()];
        assert_eq!(unique_paths(input), ["b", "a"]);
    }

    #[test]
    fn unique_paths_normalize_separators_and_strip_dot_slash() {
        assert_eq!(
            unique_paths(vec!["./src\\app.ts".to_owned()]),
            ["src/app.ts"]
        );
    }

    #[test]
    fn unique_paths_drop_empty_entries() {
        assert_eq!(unique_paths(vec![String::new(), "a".to_owned()]), ["a"]);
    }

    // parse_json_object ------------------------------------------------------

    #[test]
    fn json_object_accepts_an_object() {
        let value = parse_json_object(r#"{"a":1}"#).expect("object should parse");
        assert_eq!(value.get("a").and_then(Value::as_i64), Some(1));
    }

    #[test]
    fn json_object_treats_blank_input_as_an_empty_object() {
        let value = parse_json_object("   ").expect("blank input should parse");
        assert!(
            value.as_object().is_some_and(Map::is_empty),
            "expected empty object, got {value}"
        );
    }

    #[test]
    fn json_object_rejects_a_non_object_payload() {
        let error = parse_json_object("[1,2]").unwrap_err();
        assert!(
            error.contains("must be a JSON object"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn json_object_rejects_invalid_json() {
        let error = parse_json_object("{not json").unwrap_err();
        assert!(
            error.contains("failed to parse hook JSON"),
            "unexpected error: {error}"
        );
    }

    // read_state / write_state -----------------------------------------------

    #[test]
    fn read_state_returns_default_when_the_cache_is_missing() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let state =
            read_state(Path::new("/repo"), "sess", Some(&root)).expect("missing cache is ok");
        let _ = fs::remove_dir_all(&root);
        assert!(state.injected_rules.is_empty());
    }

    #[test]
    fn read_state_errors_when_the_cache_is_corrupt() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let file = cache_file(Path::new("/repo"), "sess", Some(&root));
        fs::create_dir_all(file.parent().expect("cache parent")).expect("create cache dir");
        fs::write(&file, "not json").expect("write corrupt cache");
        let error = read_state(Path::new("/repo"), "sess", Some(&root)).unwrap_err();
        let _ = fs::remove_dir_all(&root);
        assert!(
            error.contains("failed to parse cache state"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn state_round_trips_through_write_then_read() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let written = HookState {
            injected_rules: vec!["a".to_owned(), "b".to_owned()],
        };
        write_state(Path::new("/repo"), "sess", Some(&root), &written).expect("write state");
        let loaded = read_state(Path::new("/repo"), "sess", Some(&root)).expect("read state");
        let _ = fs::remove_dir_all(&root);
        assert_eq!(loaded.injected_rules, ["a", "b"]);
    }

    #[test]
    fn state_lock_removes_the_lock_dir_when_dropped() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let lock_dir = state_lock_dir(Path::new("/repo"), "sess", Some(&root));

        {
            let _lock =
                acquire_state_lock(Path::new("/repo"), "sess", Some(&root)).expect("lock state");
            assert!(lock_dir.is_dir());
        }

        let exists = lock_dir.exists();
        let _ = fs::remove_dir_all(&root);
        assert!(!exists);
    }

    #[cfg(unix)]
    #[test]
    fn find_markdown_files_ignores_symlinked_rules() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let rules = root.join("rules");
        fs::create_dir_all(&rules).expect("create rules dir");
        fs::write(root.join("secret"), "secret").expect("write target file");
        std::os::unix::fs::symlink(root.join("secret"), rules.join("secret.md"))
            .expect("create symlinked rule");

        let files = find_markdown_files(&rules).expect("read rules");
        let _ = fs::remove_dir_all(&root);

        assert!(files.is_empty());
    }

    // small helpers ----------------------------------------------------------

    #[test]
    fn env_assignment_recognizes_a_name_value_pair() {
        assert!(is_environment_assignment("FOO=bar"));
    }

    #[test]
    fn env_assignment_rejects_a_bare_word() {
        assert!(!is_environment_assignment("cat"));
    }

    #[test]
    fn env_assignment_rejects_a_leading_digit_name() {
        assert!(!is_environment_assignment("1A=b"));
    }

    #[test]
    fn looks_like_path_accepts_a_value_with_a_slash() {
        assert!(looks_like_path("src/app.ts", Path::new("/repo")));
    }

    #[test]
    fn looks_like_path_accepts_a_value_with_an_extension() {
        assert!(looks_like_path("README.md", Path::new("/repo")));
    }

    #[test]
    fn looks_like_path_rejects_a_lone_dash() {
        assert!(!looks_like_path("-", Path::new("/repo")));
    }

    #[test]
    fn looks_like_path_rejects_a_variable_reference() {
        assert!(!looks_like_path("$HOME", Path::new("/repo")));
    }

    #[test]
    fn looks_like_path_rejects_a_bare_word() {
        assert!(!looks_like_path("hello", Path::new("/repo")));
    }

    #[test]
    fn unquote_removes_double_quotes() {
        assert_eq!(unquote("\"a/b\""), "a/b");
    }

    #[test]
    fn unquote_removes_single_quotes() {
        assert_eq!(unquote("'a/b'"), "a/b");
    }

    #[test]
    fn unquote_strips_a_trailing_comment_from_a_bare_value() {
        assert_eq!(unquote("a/b # note"), "a/b");
    }
}
