//! Lazy rule discovery across project scopes, front matter parsing, and path
//! matching relative to each rule's scope.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::HookResult;
use crate::glob::{glob_matches, split_top_level_commas};
use crate::pathutil::{clean_path, path_to_posix, path_to_string, resolve_path, strip_dot_slash};

/// Rule directory relative to each applicable project scope.
const RULES_DIR: &str = ".claude/rules";

/// A rule discovered under [`RULES_DIR`], ready to be matched and injected.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    /// Stable identity (the rule's canonical path) used to inject it at most
    /// once per session.
    pub(crate) key: String,
    /// Glob patterns from the `paths:` front matter; `None` means the rule
    /// applies to every touched path.
    pub(crate) paths: Option<Vec<String>>,
    /// Project scope against which touched paths and rule globs are matched.
    pub(crate) scope_root: PathBuf,
    /// Rule body with any front matter stripped.
    pub(crate) content: String,
}

/// Rules discovered during a scan, plus non-fatal rule diagnostics.
#[derive(Debug)]
pub(crate) struct RuleScan {
    pub(crate) rules: Vec<Rule>,
    pub(crate) diagnostics: Vec<RuleDiagnostic>,
}

/// One rule warning, identified by canonical path for per-session de-duplication.
#[derive(Debug)]
pub(crate) struct RuleDiagnostic {
    pub(crate) key: String,
    pub(crate) reason: String,
}

/// One rule directory with its source-specific trust and matching scope.
#[derive(Debug)]
enum RuleSource {
    Project {
        scope_root: PathBuf,
    },
    Shared {
        rules_dir: PathBuf,
        scope_root: PathBuf,
    },
}

impl RuleSource {
    fn project(scope_root: PathBuf) -> Self {
        Self::Project { scope_root }
    }

    fn shared(rules_dir: PathBuf, scope_root: PathBuf) -> Self {
        Self::Shared {
            rules_dir,
            scope_root,
        }
    }

    fn rules_dir(&self) -> PathBuf {
        match self {
            Self::Project { scope_root } => scope_root.join(RULES_DIR),
            Self::Shared { rules_dir, .. } => rules_dir.clone(),
        }
    }

    fn scope_root(&self) -> &Path {
        match self {
            Self::Project { scope_root } | Self::Shared { scope_root, .. } => scope_root,
        }
    }

    fn metadata(&self, rules_dir: &Path) -> io::Result<fs::Metadata> {
        match self {
            Self::Project { .. } => fs::symlink_metadata(rules_dir),
            Self::Shared { .. } => fs::metadata(rules_dir),
        }
    }
}

/// Outcome of parsing a rule file's optional front matter.
#[derive(Debug)]
struct ParsedRule {
    paths: Option<Vec<String>>,
    content: String,
}

/// Discover and parse rules under the working directory and the project scopes
/// traversed by touched paths. Rules with empty bodies are skipped, and absent
/// directories yield an empty list.
pub(crate) fn scan_rules(cwd: &Path, trigger_paths: &[String]) -> RuleScan {
    let extra_dirs = env::var_os("CODEX_PATH_RULES_EXTRA_DIRS");
    scan_rules_with_extra_dirs(cwd, trigger_paths, extra_dirs.as_deref())
}

/// Discover project-local rules plus any explicitly configured extra rule
/// directories. Project scopes are loaded from shallowest to deepest; extra
/// directories follow in caller order. Canonical rule paths are de-duplicated.
fn scan_rules_with_extra_dirs(
    cwd: &Path,
    trigger_paths: &[String],
    extra_dirs: Option<&OsStr>,
) -> RuleScan {
    let cwd = clean_path(cwd);
    let mut scan = RuleScan {
        rules: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut seen = HashSet::new();

    for scope_root in project_rule_scopes(&cwd, trigger_paths) {
        scan_rule_source(RuleSource::project(scope_root), &mut seen, &mut scan);
    }

    if let Some(extra_dirs) = extra_dirs {
        for dir in env::split_paths(extra_dirs) {
            if dir.as_os_str().is_empty() {
                continue;
            }

            let rules_dir = if dir.is_absolute() {
                clean_path(dir)
            } else {
                clean_path(cwd.join(dir))
            };
            let source = project_scope_for_rules_dir(&cwd, &rules_dir).map_or_else(
                || RuleSource::shared(rules_dir, cwd.clone()),
                RuleSource::project,
            );
            scan_rule_source(source, &mut seen, &mut scan);
        }
    }

    scan
}

/// Return project scopes from `cwd` through each touched path, shallowest first.
fn project_rule_scopes(cwd: &Path, trigger_paths: &[String]) -> Vec<PathBuf> {
    let mut scopes = vec![cwd.to_path_buf()];
    let mut checked = HashSet::from([cwd.to_path_buf()]);

    for trigger_path in trigger_paths {
        let absolute_path = resolve_path(cwd, trigger_path);
        if !absolute_path.starts_with(cwd) {
            continue;
        }
        let start = if absolute_path.is_dir() {
            absolute_path.as_path()
        } else {
            let Some(parent) = absolute_path.parent() else {
                continue;
            };
            parent
        };

        for scope_root in start.ancestors().take_while(|path| path.starts_with(cwd)) {
            if !checked.insert(scope_root.to_path_buf()) {
                continue;
            }
            scopes.push(scope_root.to_path_buf());
        }
    }

    scopes.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    scopes
}

/// Recover a project scope when an extra directory names nested project rules.
fn project_scope_for_rules_dir(cwd: &Path, rules_dir: &Path) -> Option<PathBuf> {
    project_scope_from_relative_rules_dir(cwd, rules_dir).or_else(|| {
        let canonical_cwd = fs::canonicalize(cwd).ok()?;
        let canonical_rules_dir = fs::canonicalize(rules_dir).ok()?;
        let relative_rules_dir = canonical_rules_dir.strip_prefix(canonical_cwd).ok()?;
        project_scope_from_relative_rules_dir(cwd, &cwd.join(relative_rules_dir))
    })
}

fn project_scope_from_relative_rules_dir(cwd: &Path, rules_dir: &Path) -> Option<PathBuf> {
    let relative_rules_dir = rules_dir.strip_prefix(cwd).ok()?;
    let claude_dir = relative_rules_dir.parent()?;
    if relative_rules_dir.file_name()? != "rules" || claude_dir.file_name()? != ".claude" {
        return None;
    }
    Some(clean_path(
        cwd.join(claude_dir.parent().unwrap_or(Path::new(""))),
    ))
}

fn scan_rule_source(source: RuleSource, seen: &mut HashSet<String>, scan: &mut RuleScan) {
    let rules_dir = source.rules_dir();
    let metadata = match source.metadata(&rules_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            push_scan_diagnostic(&rules_dir, error.to_string(), seen, scan);
            return;
        }
    };
    if !metadata.is_dir() {
        return;
    }

    if let Err(reason) = scan_rules_dir(&rules_dir, source.scope_root(), seen, scan) {
        push_scan_diagnostic(&rules_dir, reason, seen, scan);
    }
}

fn push_scan_diagnostic(
    path: &Path,
    reason: String,
    seen: &mut HashSet<String>,
    scan: &mut RuleScan,
) {
    let key = fs::canonicalize(path)
        .map(|canonical| path_to_string(&canonical))
        .unwrap_or_else(|_| path_to_string(path));
    if seen.insert(key.clone()) {
        scan.diagnostics.push(RuleDiagnostic { key, reason });
    }
}

/// Discover and parse every rule under one rule directory.
fn scan_rules_dir(
    rules_dir: &Path,
    scope_root: &Path,
    seen: &mut HashSet<String>,
    scan: &mut RuleScan,
) -> HookResult<()> {
    let mut files = find_markdown_files(rules_dir)?;
    files.sort();

    for absolute_path in files {
        let canonical_path = fs::canonicalize(&absolute_path).map_err(|error| {
            format!(
                "failed to canonicalize rule {}: {error}",
                path_to_string(&absolute_path)
            )
        })?;
        let key = path_to_string(&canonical_path);
        if !seen.insert(key.clone()) {
            continue;
        }
        let markdown = fs::read_to_string(&absolute_path).map_err(|error| {
            format!(
                "failed to read rule {}: {error}",
                path_to_string(&absolute_path)
            )
        })?;
        let parsed = match parse_rule_markdown(&markdown) {
            Ok(parsed) => parsed,
            Err(reason) => {
                scan.diagnostics.push(RuleDiagnostic {
                    key,
                    reason: reason.to_owned(),
                });
                continue;
            }
        };
        if parsed.content.is_empty() {
            continue;
        }

        scan.rules.push(Rule {
            key,
            paths: parsed.paths,
            scope_root: scope_root.to_path_buf(),
            content: parsed.content,
        });
    }

    Ok(())
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
fn parse_rule_markdown(markdown: &str) -> Result<ParsedRule, &'static str> {
    let text = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let Some((first_line, mut position)) = read_line(text, 0) else {
        return Ok(ParsedRule {
            paths: None,
            content: text.trim().to_owned(),
        });
    };

    if !is_frontmatter_delimiter(first_line) {
        return Ok(ParsedRule {
            paths: None,
            content: text.trim().to_owned(),
        });
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
            let paths = parse_paths(raw_frontmatter)?;
            return Ok(ParsedRule { paths, content });
        }

        if next_position == text.len() {
            break;
        }
        position = next_position;
    }

    Err("front matter is not closed")
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

/// Extract and validate the `paths` patterns from rule front matter.
///
/// Three YAML forms are understood, matching Claude Code's native rules: a
/// block list (`paths:` then `- value` items), an inline flow list
/// (`paths: [a, b]`), and a single scalar (`paths: value`). Values may be
/// single- or double-quoted and may carry a trailing ` # comment`. Duplicates
/// are dropped; for the block form, parsing stops at the first non-list line
/// after `paths:`.
fn parse_paths(frontmatter: &str) -> Result<Option<Vec<String>>, &'static str> {
    let mut lines = frontmatter.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("paths:") else {
            continue;
        };
        let rest = rest.trim();

        let paths = if rest.is_empty() {
            parse_block_list(lines)?
        } else if rest.starts_with('[') {
            parse_flow_list(rest)?
        } else {
            let value = unquote(rest)?;
            if value.is_empty() {
                Vec::new()
            } else {
                vec![value]
            }
        };

        if paths.is_empty() {
            return Err("`paths:` must contain at least one glob");
        }
        return Ok(Some(paths));
    }

    Ok(None)
}

/// Collect the `- value` items following a bare `paths:` line, stopping at the
/// first non-empty line that is not a list item.
fn parse_block_list<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Vec<String>, &'static str> {
    let mut paths = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(item) = trimmed.strip_prefix('-') else {
            break;
        };

        let value = unquote(item.trim())?;
        if !value.is_empty() && !paths.contains(&value) {
            paths.push(value);
        }
    }

    Ok(paths)
}

/// Parse an inline flow list such as `["a", "b"]`, splitting on top-level
/// commas so a brace group like `{ts,tsx}` inside an item stays intact.
fn parse_flow_list(rest: &str) -> Result<Vec<String>, &'static str> {
    let body = rest.strip_prefix('[').unwrap_or(rest);
    let Some((body, suffix)) = body.rsplit_once(']') else {
        return Err("`paths:` flow list is not closed");
    };
    let suffix = suffix.trim();
    if !suffix.is_empty() && !suffix.starts_with('#') {
        return Err("unexpected content after `paths:` flow list");
    }

    let mut paths = Vec::new();
    for item in split_top_level_commas(body) {
        let value = unquote(item.trim())?;
        if !value.is_empty() && !paths.contains(&value) {
            paths.push(value);
        }
    }

    Ok(paths)
}

/// Unwrap a single- or double-quoted scalar, or strip a trailing ` # comment`
/// from a bare scalar.
fn unquote(value: &str) -> Result<String, &'static str> {
    if let Some(quote @ ('"' | '\'')) = value.chars().next() {
        let offset = quote.len_utf8();
        let Some(end) = value[offset..].find(quote) else {
            return Err("quoted `paths:` glob is not closed");
        };
        let trailing = value[offset + end + offset..].trim();
        if !trailing.is_empty() && !trailing.starts_with('#') {
            return Err("unexpected content after quoted `paths:` glob");
        }
        return Ok(value[offset..offset + end].to_owned());
    }

    let uncommented = value.find(" #").map_or(value, |index| &value[..index]);
    Ok(uncommented.trim_end().to_owned())
}

/// Decide whether `trigger_path` activates `rule`.
///
/// A rule without `paths:` matches every path in its project scope. Otherwise,
/// the path relative to that scope must match one rule glob. Paths outside the
/// scope never match.
pub(crate) fn rule_matches(rule: &Rule, trigger_path: &str, cwd: &Path) -> bool {
    let Some(relative_path) = normalize_trigger_path(trigger_path, cwd, &rule.scope_root) else {
        return false;
    };

    rule.paths.as_ref().is_none_or(|paths| {
        paths
            .iter()
            .any(|pattern| glob_matches(pattern, &relative_path))
    })
}

/// Express `input_path` as a scope-relative POSIX path, or `None` if it resolves
/// outside that scope.
fn normalize_trigger_path(input_path: &str, cwd: &Path, scope_root: &Path) -> Option<String> {
    let absolute = resolve_path(cwd, input_path);
    let relative = absolute.strip_prefix(scope_root).ok()?;
    Some(strip_dot_slash(&path_to_posix(relative)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest::create_temp_dir;

    // parse_paths ------------------------------------------------------------

    fn parsed_paths(frontmatter: &str) -> Vec<String> {
        parse_paths(frontmatter)
            .expect("valid paths")
            .expect("paths key")
    }

    #[test]
    fn parse_paths_collects_list_items() {
        assert_eq!(
            parsed_paths("paths:\n  - src/**\n  - docs/**\n"),
            ["src/**", "docs/**"]
        );
    }

    #[test]
    fn parse_paths_strips_surrounding_quotes() {
        assert_eq!(
            parsed_paths("paths:\n  - \"src/**/*.css\"\n"),
            ["src/**/*.css"]
        );
    }

    #[test]
    fn parse_paths_strips_inline_comment() {
        assert_eq!(parsed_paths("paths:\n  - src/** # styles\n"), ["src/**"]);
    }

    #[test]
    fn parse_paths_drops_duplicates() {
        assert_eq!(parsed_paths("paths:\n  - a\n  - a\n"), ["a"]);
    }

    #[test]
    fn parse_paths_stops_at_first_non_list_line() {
        assert_eq!(parsed_paths("paths:\n  - a\nother: x\n  - b\n"), ["a"]);
    }

    #[test]
    fn parse_paths_skips_comments_between_block_list_items() {
        assert_eq!(
            parsed_paths("paths:\n  # first group\n  - a\n  # second group\n  - b\n"),
            ["a", "b"]
        );
    }

    #[test]
    fn parse_paths_returns_none_without_a_paths_key() {
        assert_eq!(
            parse_paths("name: rule\n").expect("valid front matter"),
            None
        );
    }

    #[test]
    fn parse_paths_reads_a_scalar_value() {
        assert_eq!(
            parsed_paths("paths: src/**/*.svelte\n"),
            ["src/**/*.svelte"]
        );
    }

    #[test]
    fn parse_paths_reads_a_quoted_scalar_value() {
        assert_eq!(
            parsed_paths("paths: \"**/agents/**/*.md\"\n"),
            ["**/agents/**/*.md"]
        );
    }

    #[test]
    fn parse_paths_strips_an_inline_comment_from_a_scalar() {
        assert_eq!(parsed_paths("paths: src/** # styles\n"), ["src/**"]);
    }

    #[test]
    fn parse_paths_reads_an_inline_flow_list() {
        assert_eq!(
            parsed_paths("paths: [\"src/**/*.ts\", \"lib/**\"]\n"),
            ["src/**/*.ts", "lib/**"]
        );
    }

    #[test]
    fn parse_paths_reads_an_unquoted_inline_flow_list() {
        assert_eq!(parsed_paths("paths: [a, b]\n"), ["a", "b"]);
    }

    #[test]
    fn parse_paths_keeps_a_brace_group_inside_a_flow_list_intact() {
        assert_eq!(
            parsed_paths("paths: [\"src/**/*.{ts,tsx}\"]\n"),
            ["src/**/*.{ts,tsx}"]
        );
    }

    #[test]
    fn parse_paths_rejects_an_unclosed_flow_list() {
        assert_eq!(
            parse_paths("paths: [src/**\n").unwrap_err(),
            "`paths:` flow list is not closed"
        );
    }

    #[test]
    fn parse_paths_rejects_an_unclosed_quoted_glob() {
        assert_eq!(
            parse_paths("paths: \"src/**\n").unwrap_err(),
            "quoted `paths:` glob is not closed"
        );
    }

    // parse_rule_markdown ----------------------------------------------------

    #[test]
    fn frontmatter_extracts_the_paths_list() {
        let parsed =
            parse_rule_markdown("---\npaths:\n  - src/**\n---\n\nBody.").expect("valid rule");
        assert_eq!(parsed.paths, Some(vec!["src/**".to_owned()]));
    }

    #[test]
    fn frontmatter_body_excludes_the_frontmatter() {
        let parsed =
            parse_rule_markdown("---\npaths:\n  - src/**\n---\n\nBody.").expect("valid rule");
        assert_eq!(parsed.content, "Body.");
    }

    #[test]
    fn markdown_without_frontmatter_has_no_paths() {
        assert_eq!(
            parse_rule_markdown("# Title\n\nNo frontmatter.")
                .expect("global rule")
                .paths,
            None
        );
    }

    #[test]
    fn frontmatter_extracts_a_scalar_paths_value() {
        let parsed = parse_rule_markdown("---\npaths: src/**\n---\n\nBody.").expect("valid rule");
        assert_eq!(parsed.paths, Some(vec!["src/**".to_owned()]));
    }

    #[test]
    fn frontmatter_ignores_a_leading_byte_order_mark() {
        assert_eq!(
            parse_rule_markdown("\u{feff}---\npaths:\n  - a\n---\nBody")
                .expect("valid rule")
                .content,
            "Body"
        );
    }

    #[test]
    fn frontmatter_rejects_an_unclosed_fence() {
        assert_eq!(
            parse_rule_markdown("---\npaths: src/**\nBody.").unwrap_err(),
            "front matter is not closed"
        );
    }

    #[test]
    fn frontmatter_rejects_an_empty_paths_value() {
        assert_eq!(
            parse_rule_markdown("---\npaths:\n---\nBody.").unwrap_err(),
            "`paths:` must contain at least one glob"
        );
    }

    #[test]
    fn frontmatter_rejects_an_empty_paths_flow_list() {
        assert_eq!(
            parse_rule_markdown("---\npaths: []\n---\nBody.").unwrap_err(),
            "`paths:` must contain at least one glob"
        );
    }

    // normalize_trigger_path -------------------------------------------------

    #[test]
    fn normalize_makes_a_relative_path_relative_to_cwd() {
        assert_eq!(
            normalize_trigger_path("src/app.ts", Path::new("/repo"), Path::new("/repo")).as_deref(),
            Some("src/app.ts")
        );
    }

    #[test]
    fn normalize_resolves_an_absolute_path_inside_cwd() {
        assert_eq!(
            normalize_trigger_path("/repo/src/app.ts", Path::new("/repo"), Path::new("/repo"))
                .as_deref(),
            Some("src/app.ts")
        );
    }

    #[test]
    fn normalize_rejects_a_path_outside_cwd() {
        assert_eq!(
            normalize_trigger_path("/elsewhere/app.ts", Path::new("/repo"), Path::new("/repo")),
            None
        );
    }

    #[test]
    fn normalize_makes_a_nested_path_relative_to_its_scope() {
        assert_eq!(
            normalize_trigger_path(
                "apps/journal/src/app.ts",
                Path::new("/repo"),
                Path::new("/repo/apps/journal")
            )
            .as_deref(),
            Some("src/app.ts")
        );
    }

    // rule_matches -----------------------------------------------------------

    fn rule_with(paths: Option<Vec<String>>) -> Rule {
        Rule {
            key: "k".to_owned(),
            paths,
            scope_root: PathBuf::from("/repo"),
            content: "c".to_owned(),
        }
    }

    fn nested_rule_with(paths: Option<Vec<String>>) -> Rule {
        Rule {
            key: "nested".to_owned(),
            paths,
            scope_root: PathBuf::from("/repo/apps/journal"),
            content: "nested".to_owned(),
        }
    }

    fn write_rule(dir: &Path, name: &str, marker: &str) {
        fs::create_dir_all(dir).expect("create rules dir");
        fs::write(
            dir.join(name),
            format!("---\npaths:\n  - \"src/**\"\n---\n\n{marker}"),
        )
        .expect("write rule");
    }

    fn join_path_components(components: &[&OsStr]) -> std::ffi::OsString {
        let mut joined = std::ffi::OsString::new();
        let separator = if cfg!(windows) { ";" } else { ":" };

        for (index, component) in components.iter().enumerate() {
            if index > 0 {
                joined.push(separator);
            }
            joined.push(component);
        }

        joined
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

    #[test]
    fn nested_rule_matches_an_app_relative_glob() {
        let rule = nested_rule_with(Some(vec!["src/**/*.svelte".to_owned()]));
        assert!(rule_matches(
            &rule,
            "apps/journal/src/components/App.svelte",
            Path::new("/repo")
        ));
    }

    #[test]
    fn nested_rule_does_not_match_the_same_relative_path_in_a_sibling_app() {
        let rule = nested_rule_with(Some(vec!["src/**/*.svelte".to_owned()]));
        assert!(!rule_matches(
            &rule,
            "apps/portfolio/src/components/App.svelte",
            Path::new("/repo")
        ));
    }

    #[test]
    fn nested_rule_without_paths_stays_inside_its_scope() {
        let rule = nested_rule_with(None);
        assert!(rule_matches(
            &rule,
            "apps/journal/README.md",
            Path::new("/repo")
        ));
        assert!(!rule_matches(
            &rule,
            "apps/portfolio/README.md",
            Path::new("/repo")
        ));
    }

    #[test]
    fn scan_rules_loads_root_then_nested_rules_for_a_touched_path() {
        let root = create_temp_dir("rules-nested").expect("temp dir");
        let repo = root.join("repo");
        let nested = repo.join("apps/journal/.claude/rules");
        write_rule(&repo.join(".claude/rules"), "root.md", "ROOT");
        write_rule(&nested, "journal.md", "JOURNAL");

        let scan = scan_rules_with_extra_dirs(
            &repo,
            &["apps/journal/src/components/App.svelte".to_owned()],
            None,
        );
        let markers = scan
            .rules
            .iter()
            .map(|rule| rule.content.as_str())
            .collect::<Vec<_>>();
        let scopes = scan
            .rules
            .iter()
            .map(|rule| rule.scope_root.clone())
            .collect::<Vec<_>>();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(markers, ["ROOT", "JOURNAL"]);
        assert_eq!(scopes, [repo.clone(), repo.join("apps/journal")]);
    }

    #[test]
    fn scan_rules_ignores_nested_rules_outside_the_touched_path() {
        let root = create_temp_dir("rules-nested-sibling").expect("temp dir");
        let repo = root.join("repo");
        write_rule(
            &repo.join("apps/journal/.claude/rules"),
            "journal.md",
            "JOURNAL",
        );

        let scan = scan_rules_with_extra_dirs(
            &repo,
            &["apps/portfolio/src/components/App.svelte".to_owned()],
            None,
        );
        let _ = fs::remove_dir_all(&root);

        assert!(scan.rules.is_empty());
    }

    #[test]
    fn scan_rules_finds_nested_rules_for_a_new_file_path() {
        let root = create_temp_dir("rules-nested-new-file").expect("temp dir");
        let repo = root.join("repo");
        write_rule(
            &repo.join("apps/journal/.claude/rules"),
            "journal.md",
            "JOURNAL",
        );

        let scan =
            scan_rules_with_extra_dirs(&repo, &["apps/journal/src/New.svelte".to_owned()], None);
        let _ = fs::remove_dir_all(&root);

        assert_eq!(scan.rules.len(), 1);
        assert_eq!(scan.rules[0].content, "JOURNAL");
    }

    #[test]
    fn nested_invalid_rules_are_reported_only_for_their_scope() {
        let root = create_temp_dir("rules-nested-invalid").expect("temp dir");
        let repo = root.join("repo");
        let rules_dir = repo.join("apps/journal/.claude/rules");
        fs::create_dir_all(&rules_dir).expect("create rules dir");
        fs::write(rules_dir.join("invalid.md"), "---\npaths: []\n---\nINVALID")
            .expect("write invalid rule");

        let unrelated =
            scan_rules_with_extra_dirs(&repo, &["apps/portfolio/src/App.svelte".to_owned()], None);
        let related =
            scan_rules_with_extra_dirs(&repo, &["apps/journal/src/App.svelte".to_owned()], None);
        let _ = fs::remove_dir_all(&root);

        assert!(unrelated.diagnostics.is_empty());
        assert_eq!(related.diagnostics.len(), 1);
    }

    #[test]
    fn project_rule_scope_is_stable_when_the_same_directory_is_configured_as_extra() {
        let root = create_temp_dir("rules-nested-extra").expect("temp dir");
        let repo = root.join("repo");
        let nested_rules = repo.join("apps/journal/.claude/rules");
        write_rule(&nested_rules, "journal.md", "JOURNAL");
        let joined = env::join_paths([&nested_rules]).expect("join paths");

        let outside = scan_rules_with_extra_dirs(
            &repo,
            &["src/App.svelte".to_owned()],
            Some(joined.as_os_str()),
        );
        let inside = scan_rules_with_extra_dirs(
            &repo,
            &["apps/journal/src/App.svelte".to_owned()],
            Some(joined.as_os_str()),
        );
        let scopes = [
            outside.rules[0].scope_root.clone(),
            inside.rules[0].scope_root.clone(),
        ];
        let expected_scope = repo.join("apps/journal");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(scopes, [expected_scope.clone(), expected_scope]);
    }

    #[test]
    fn a_regular_file_configured_as_an_extra_rules_directory_is_ignored() {
        let root = create_temp_dir("rules-extra-file").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        fs::create_dir_all(&repo).expect("create repo");
        fs::write(&extra, "not a directory").expect("write extra file");
        let joined = env::join_paths([&extra]).expect("join paths");

        let scan = scan_rules_with_extra_dirs(&repo, &[], Some(joined.as_os_str()));
        let _ = fs::remove_dir_all(&root);

        assert!(
            scan.rules.is_empty() && scan.diagnostics.is_empty(),
            "regular extra file should be ignored: {scan:?}"
        );
    }

    #[test]
    fn project_scopes_are_ordered_by_depth_across_multiple_touched_paths() {
        let root = create_temp_dir("rules-nested-order").expect("temp dir");
        let repo = root.join("repo");
        write_rule(
            &repo.join("apps/journal/packages/editor/.claude/rules"),
            "editor.md",
            "EDITOR",
        );
        write_rule(
            &repo.join("apps/portfolio/.claude/rules"),
            "portfolio.md",
            "PORTFOLIO",
        );

        let scan = scan_rules_with_extra_dirs(
            &repo,
            &[
                "apps/journal/packages/editor/src/App.ts".to_owned(),
                "apps/portfolio/src/App.ts".to_owned(),
            ],
            None,
        );
        let markers = scan
            .rules
            .iter()
            .map(|rule| rule.content.as_str())
            .collect::<Vec<_>>();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(markers, ["PORTFOLIO", "EDITOR"]);
    }

    // scan_rules_with_extra_dirs -------------------------------------------

    #[test]
    fn scan_rules_reads_extra_rule_dirs_after_project_rules() {
        let root = create_temp_dir("rules-extra").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        write_rule(&repo.join(".claude").join("rules"), "project.md", "PROJECT");
        write_rule(&extra, "shared.md", "SHARED");

        let joined = env::join_paths([&extra]).expect("join paths");
        let rules = scan_rules_with_extra_dirs(&repo, &[], Some(joined.as_os_str()));
        let markers = rules
            .rules
            .iter()
            .map(|rule| rule.content.as_str())
            .collect::<Vec<_>>();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(markers, ["PROJECT", "SHARED"]);
    }

    #[test]
    fn scan_rules_resolves_relative_extra_rule_dirs_against_cwd() {
        let root = create_temp_dir("rules-extra-relative").expect("temp dir");
        let repo = root.join("repo");
        let extra = repo.join("shared-rules");
        write_rule(&extra, "shared.md", "SHARED");

        let rules = scan_rules_with_extra_dirs(&repo, &[], Some(OsStr::new("shared-rules")));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].content, "SHARED");
    }

    #[test]
    fn scan_rules_skips_empty_extra_rule_dirs() {
        let root = create_temp_dir("rules-extra-empty").expect("temp dir");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        fs::write(repo.join("README.md"), "ROOT").expect("write readme");

        let rules = scan_rules_with_extra_dirs(&repo, &[], Some(OsStr::new("")));
        let _ = fs::remove_dir_all(&root);

        assert!(rules.rules.is_empty());
    }

    #[test]
    fn scan_rules_skips_empty_entries_between_extra_rule_dirs() {
        let root = create_temp_dir("rules-extra-empty-components").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        fs::create_dir_all(&repo).expect("create repo");
        fs::write(repo.join("README.md"), "ROOT").expect("write readme");
        write_rule(&extra, "shared.md", "SHARED");

        let joined = join_path_components(&[
            OsStr::new(""),
            extra.as_os_str(),
            OsStr::new(""),
            OsStr::new(""),
        ]);
        let rules = scan_rules_with_extra_dirs(&repo, &[], Some(joined.as_os_str()));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].content, "SHARED");
    }

    #[test]
    fn scan_rules_deduplicates_repeated_extra_rule_dirs() {
        let root = create_temp_dir("rules-extra-dedup").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        write_rule(&extra, "shared.md", "SHARED");

        let joined = env::join_paths([&extra, &extra]).expect("join paths");
        let rules = scan_rules_with_extra_dirs(&repo, &[], Some(joined.as_os_str()));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].content, "SHARED");
    }

    #[test]
    fn scan_rules_deduplicates_diagnostics_from_repeated_extra_rule_dirs() {
        let root = create_temp_dir("rules-extra-invalid-dedup").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        fs::create_dir_all(&extra).expect("create rules dir");
        fs::write(extra.join("invalid.md"), "---\npaths: []\n---\nINVALID")
            .expect("write invalid rule");

        let joined = env::join_paths([&extra, &extra]).expect("join paths");
        let scan = scan_rules_with_extra_dirs(&repo, &[], Some(joined.as_os_str()));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(scan.diagnostics.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn scan_rules_deduplicates_symlinked_extra_rule_dir_aliases() {
        use std::os::unix::fs::symlink;

        let root = create_temp_dir("rules-extra-symlink-dedup").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        let alias = root.join("shared-rules-alias");
        write_rule(&extra, "shared.md", "SHARED");
        symlink(&extra, &alias).expect("symlink rules dir");

        let joined = env::join_paths([&extra, &alias]).expect("join paths");
        let rules = scan_rules_with_extra_dirs(&repo, &[], Some(joined.as_os_str()));
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.rules[0].content, "SHARED");
    }

    // unquote ------------------------------------------------------------------

    #[test]
    fn unquote_removes_double_quotes() {
        assert_eq!(unquote("\"a/b\"").expect("quoted path"), "a/b");
    }

    #[test]
    fn unquote_removes_single_quotes() {
        assert_eq!(unquote("'a/b'").expect("quoted path"), "a/b");
    }

    #[test]
    fn unquote_strips_a_trailing_comment_from_a_bare_value() {
        assert_eq!(unquote("a/b # note").expect("bare path"), "a/b");
    }
}
