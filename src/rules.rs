//! Rule discovery under `.claude/rules`, front matter parsing, and matching of
//! touched paths against a rule's globs.

use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::HookResult;
use crate::glob::{glob_matches, split_top_level_commas};
use crate::pathutil::{clean_path, path_to_posix, path_to_string, resolve_path, strip_dot_slash};

/// Directory, relative to the working directory, scanned for `*.md` rule files.
const RULES_DIR: &str = ".claude/rules";

/// A rule discovered under [`RULES_DIR`], ready to be matched and injected.
#[derive(Debug, Clone)]
pub(crate) struct Rule {
    /// Stable identity (the rule's absolute path) used to inject it at most
    /// once per session.
    pub(crate) key: String,
    /// Glob patterns from the `paths:` front matter; `None` means the rule
    /// applies to every touched path.
    pub(crate) paths: Option<Vec<String>>,
    /// Rule body with any front matter stripped.
    pub(crate) content: String,
}

/// Outcome of parsing a rule file's optional front matter.
#[derive(Debug)]
struct ParsedRule {
    paths: Option<Vec<String>>,
    content: String,
}

/// Discover and parse every rule under [`RULES_DIR`], sorted by path for stable
/// ordering. Rules with empty bodies are skipped, and an absent directory
/// yields an empty list.
///
/// # Errors
///
/// Returns an error if the directory tree cannot be traversed or a rule file
/// cannot be read.
pub(crate) fn scan_rules(cwd: &Path) -> HookResult<Vec<Rule>> {
    let extra_dirs = env::var_os("CODEX_PATH_RULES_EXTRA_DIRS");
    scan_rules_with_extra_dirs(cwd, extra_dirs.as_deref())
}

/// Discover project-local rules plus any explicitly configured extra rule
/// directories. Project-local rules are always loaded first; extra directories
/// are loaded in the order provided by the caller. Repeated directories are
/// de-duplicated by absolute rule path so a rule is never injected twice.
fn scan_rules_with_extra_dirs(cwd: &Path, extra_dirs: Option<&OsStr>) -> HookResult<Vec<Rule>> {
    let rules_dir = resolve_path(cwd, RULES_DIR);
    let mut rules = Vec::new();
    let mut seen = HashSet::new();

    append_new_rules(&mut rules, &mut seen, scan_rules_dir(&rules_dir)?);

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
            append_new_rules(&mut rules, &mut seen, scan_rules_dir(&rules_dir)?);
        }
    }

    Ok(rules)
}

/// Append rules whose keys have not already been seen.
fn append_new_rules(rules: &mut Vec<Rule>, seen: &mut HashSet<String>, new_rules: Vec<Rule>) {
    for rule in new_rules {
        if seen.insert(rule.key.clone()) {
            rules.push(rule);
        }
    }
}

/// Discover and parse every rule under one rule directory.
fn scan_rules_dir(rules_dir: &Path) -> HookResult<Vec<Rule>> {
    if !rules_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = find_markdown_files(rules_dir)?;
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
pub(crate) fn rule_matches(rule: &Rule, trigger_path: &str, cwd: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest::create_temp_dir;

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

    // scan_rules_with_extra_dirs -------------------------------------------

    #[test]
    fn scan_rules_reads_extra_rule_dirs_after_project_rules() {
        let root = create_temp_dir("rules-extra").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        write_rule(&repo.join(".claude").join("rules"), "project.md", "PROJECT");
        write_rule(&extra, "shared.md", "SHARED");

        let joined = env::join_paths([&extra]).expect("join paths");
        let rules =
            scan_rules_with_extra_dirs(&repo, Some(joined.as_os_str())).expect("scan rules");
        let markers = rules
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

        let rules = scan_rules_with_extra_dirs(&repo, Some(OsStr::new("shared-rules")))
            .expect("scan rules");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].content, "SHARED");
    }

    #[test]
    fn scan_rules_skips_empty_extra_rule_dirs() {
        let root = create_temp_dir("rules-extra-empty").expect("temp dir");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        fs::write(repo.join("README.md"), "ROOT").expect("write readme");

        let rules = scan_rules_with_extra_dirs(&repo, Some(OsStr::new(""))).expect("scan rules");
        let _ = fs::remove_dir_all(&root);

        assert!(rules.is_empty());
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
        let rules =
            scan_rules_with_extra_dirs(&repo, Some(joined.as_os_str())).expect("scan rules");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].content, "SHARED");
    }

    #[test]
    fn scan_rules_deduplicates_repeated_extra_rule_dirs() {
        let root = create_temp_dir("rules-extra-dedup").expect("temp dir");
        let repo = root.join("repo");
        let extra = root.join("shared-rules");
        write_rule(&extra, "shared.md", "SHARED");

        let joined = env::join_paths([&extra, &extra]).expect("join paths");
        let rules =
            scan_rules_with_extra_dirs(&repo, Some(joined.as_os_str())).expect("scan rules");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].content, "SHARED");
    }

    // unquote ------------------------------------------------------------------

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
