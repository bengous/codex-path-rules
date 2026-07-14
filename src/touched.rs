//! Extraction of the filesystem paths a tool call is about to touch, from the
//! tool's JSON input: explicit path fields, patch bodies, and a best-effort
//! shell lexer for `Bash` commands.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::pathutil::{basename, path_to_posix, resolve_path, unique_paths};
use crate::payload::{read_field_string, read_object, read_string};

/// Collect the deduplicated, normalized paths a tool call is about to touch.
///
/// The `path`/`file_path`/`filePath` fields are always read. For `Bash` the
/// command is additionally parsed for read-like file arguments; for
/// `apply_patch`/`Edit`/`Write`/`MultiEdit` the patch body is parsed for the
/// affected files.
pub(crate) fn extract_touched_paths(input: &Value, cwd: &Path) -> Vec<String> {
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
fn extract_path_fields(value: &serde_json::Map<String, Value>) -> Vec<String> {
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
/// `rg`/`grep`, direct `git diff`/`show`/`log`/`blame`, and `find`).
/// Unrecognized commands contribute no paths.
fn extract_segment_paths(segment: &[String], cwd: &Path) -> Vec<String> {
    let tokens = segment
        .iter()
        .skip_while(|token| is_environment_assignment(token))
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
            &[
                "-g",
                "--glob",
                "--type",
                "-t",
                "--type-not",
                "-T",
                "-e",
                "--regexp",
                "-f",
                "--file",
            ],
        );
    }
    if command == "grep" {
        return extract_search_paths(
            args,
            cwd,
            &["-e", "--regexp", "-f", "--file", "-m", "-A", "-B", "-C"],
        );
    }
    if command == "git"
        && matches!(
            args.first().map(String::as_str),
            Some("diff" | "show" | "log" | "blame")
        )
    {
        let Some(separator) = args.iter().position(|arg| arg == "--") else {
            return Vec::new();
        };
        return args[separator + 1..]
            .iter()
            .flat_map(|arg| expand_path_arg(arg, cwd))
            .collect();
    }
    if command == "find" {
        return args
            .iter()
            .take_while(|arg| !arg.starts_with(['-', '(', '!']))
            .flat_map(|arg| expand_path_arg(arg, cwd))
            .collect();
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
/// Positional arguments are files, except that the first one is the inline
/// search pattern — unless `--files` mode is active or the pattern was already
/// supplied via `-e`/`-f`/`--regexp`/`--file`, in which case every positional
/// argument is a file. `options_with_values` lists the flags whose following
/// argument must be skipped.
fn extract_search_paths(args: &[String], cwd: &Path, options_with_values: &[&str]) -> Vec<String> {
    let files_mode = args.iter().any(|arg| arg == "--files");
    let has_pattern_flag = args.iter().any(|arg| {
        matches!(arg.as_str(), "-e" | "-f" | "--regexp" | "--file")
            || arg.starts_with("--regexp=")
            || arg.starts_with("--file=")
    });
    let positional = positional_args(args, options_with_values);
    let files = if files_mode || has_pattern_flag {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn bash_paths_keep_the_grep_file_when_the_pattern_uses_dash_e() {
        assert_eq!(
            extract_bash_paths("grep -e foo a.ts", Path::new("/repo")),
            ["a.ts"]
        );
    }

    #[test]
    fn bash_paths_keep_the_rg_file_when_the_pattern_uses_dash_e() {
        assert_eq!(
            extract_bash_paths("rg -e foo src/file.ts", Path::new("/repo")),
            ["src/file.ts"]
        );
    }

    #[test]
    fn bash_paths_are_empty_for_an_unhandled_command() {
        assert!(extract_bash_paths("echo hello", Path::new("/repo")).is_empty());
    }

    #[test]
    fn bash_paths_read_git_diff_pathspecs_after_the_separator() {
        assert_eq!(
            extract_bash_paths("git diff main -- src/lib.rs README.md", Path::new("/repo")),
            ["src/lib.rs", "README.md"]
        );
    }

    #[test]
    fn bash_paths_keep_a_git_pathspec_that_looks_like_an_assignment() {
        assert_eq!(
            extract_bash_paths("git diff -- RULE=name", Path::new("/repo")),
            ["RULE=name"]
        );
    }

    #[test]
    fn bash_paths_read_git_show_pathspecs_after_the_separator() {
        assert_eq!(
            extract_bash_paths("git show HEAD -- src/touched.rs", Path::new("/repo")),
            ["src/touched.rs"]
        );
    }

    #[test]
    fn bash_paths_read_git_log_pathspecs_after_the_separator() {
        assert_eq!(
            extract_bash_paths("git log -- README.md", Path::new("/repo")),
            ["README.md"]
        );
    }

    #[test]
    fn bash_paths_read_git_blame_pathspecs_after_the_separator() {
        assert_eq!(
            extract_bash_paths("git blame HEAD -- src/lib.rs", Path::new("/repo")),
            ["src/lib.rs"]
        );
    }

    #[test]
    fn bash_paths_ignore_git_arguments_without_a_separator() {
        assert!(extract_bash_paths("git diff main src/lib.rs", Path::new("/repo")).is_empty());
    }

    #[test]
    fn bash_paths_ignore_git_revision_path_syntax() {
        assert!(extract_bash_paths("git show HEAD:README.md", Path::new("/repo")).is_empty());
    }

    #[test]
    fn bash_paths_ignore_unsupported_git_subcommands() {
        assert!(extract_bash_paths("git status -- src/lib.rs", Path::new("/repo")).is_empty());
    }

    #[test]
    fn bash_paths_ignore_git_with_global_options() {
        assert!(
            extract_bash_paths("git -C other diff -- src/lib.rs", Path::new("/repo")).is_empty()
        );
    }

    #[test]
    fn bash_paths_read_contiguous_find_roots() {
        assert_eq!(
            extract_bash_paths("find src tests -type f", Path::new("/repo")),
            ["src", "tests"]
        );
    }

    #[test]
    fn bash_paths_keep_a_find_root_that_looks_like_an_assignment() {
        assert_eq!(
            extract_bash_paths("find RULE=name -type f", Path::new("/repo")),
            ["RULE=name"]
        );
    }

    #[test]
    fn bash_paths_stop_find_roots_at_an_expression_group() {
        assert_eq!(
            extract_bash_paths("find src ( -name '*.rs' )", Path::new("/repo")),
            ["src"]
        );
    }

    #[test]
    fn bash_paths_stop_find_roots_at_negation() {
        assert_eq!(
            extract_bash_paths("find src ! -path target", Path::new("/repo")),
            ["src"]
        );
    }

    #[test]
    fn bash_paths_do_not_infer_a_find_root() {
        assert!(extract_bash_paths("find -type f", Path::new("/repo")).is_empty());
    }

    #[test]
    fn bash_paths_ignore_find_with_a_global_option() {
        assert!(extract_bash_paths("find -H src -type f", Path::new("/repo")).is_empty());
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
}
