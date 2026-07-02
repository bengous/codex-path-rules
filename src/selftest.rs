//! End-to-end smoke test of the hook, also runnable in production via
//! `--self-test`.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::HookResult;
use crate::hook::run_hook_with_cache;
use crate::pathutil::path_to_string;

/// Build a throwaway repository with one CSS rule, then exercise injection,
/// once-per-session de-duplication, reset on `PostCompact` and `SessionEnd`,
/// and `rg --files` discovery end to end.
///
/// # Errors
///
/// Returns an error describing the first failed expectation or IO step.
pub fn run_self_test() -> HookResult<()> {
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
            "Prefer `Vec<String>`-style generics in examples.",
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
        first_context.contains("Vec<String>"),
        "code characters should reach the context verbatim",
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
            "hook_event_name": "SessionEnd",
            "session_id": "test-session",
            "cwd": path_to_string(&repo),
        }),
        &repo,
        Some(&cache),
    )?;
    let after_session_end = run_hook_with_cache(
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
        additional_context(after_session_end)?.contains("CSS rule"),
        "session end reset did not allow reinjection",
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
pub(crate) fn additional_context(output: Option<Value>) -> HookResult<String> {
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
pub(crate) fn create_temp_dir(prefix: &str) -> HookResult<PathBuf> {
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
}
