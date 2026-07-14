//! Event dispatch and orchestration: from a parsed hook payload to the JSON
//! output Codex expects.

use std::path::Path;

use serde_json::{Value, json};

use crate::HookResult;
use crate::pathutil::{clean_path, resolve_path};
use crate::payload::read_field_string;
use crate::render::render_rules;
use crate::rules::{rule_matches, scan_rules};
use crate::session::{
    STALE_STATE_AGE, acquire_state_lock, read_session_id, read_state, reset_state,
    sweep_stale_sessions, write_state,
};
use crate::touched::extract_touched_paths;

/// Run the hook against an already-parsed payload, using the default on-disk
/// cache location.
///
/// # Errors
///
/// See [`run_hook_with_cache`].
pub(crate) fn run_hook(input: &Value, fallback_cwd: &Path) -> HookResult<Option<Value>> {
    run_hook_with_cache(input, fallback_cwd, None)
}

/// Core hook logic, shared by production, the self-test, and the unit tests.
///
/// On `SessionStart`/`SessionEnd`/`PostCompact` it resets the session state,
/// sweeps stale cache entries, and returns `None`. On `PreToolUse` it matches
/// the touched paths against the discovered rules and returns `Some(output)`
/// carrying the `additionalContext` for the matching rules not yet injected
/// this session, plus a top-level `systemMessage` for invalid rule content.
/// Only the rules actually emitted are marked as injected: a rule deferred by
/// the batch budget stays eligible for the next matching tool call. All other
/// events, and calls with neither context nor diagnostics, return `None`
/// without touching the state file.
///
/// `cache_root` overrides the state location (used by tests); production passes
/// `None`, deriving the location from the environment.
///
/// # Errors
///
/// Propagates failures scanning rules or reading/writing the session state.
pub(crate) fn run_hook_with_cache(
    input: &Value,
    fallback_cwd: &Path,
    cache_root: Option<&Path>,
) -> HookResult<Option<Value>> {
    let cwd = read_field_string(input, &["cwd"]).map_or_else(
        || clean_path(fallback_cwd),
        |value| resolve_path(fallback_cwd, value),
    );
    let event_name = read_field_string(input, &["hook_event_name", "hookEventName"]);
    let session_id = read_session_id(input, &cwd);

    if matches!(
        event_name,
        Some("SessionStart" | "SessionEnd" | "PostCompact")
    ) {
        let _lock = acquire_state_lock(&cwd, &session_id, cache_root)?;
        reset_state(&cwd, &session_id, cache_root)?;
        sweep_stale_sessions(cache_root, STALE_STATE_AGE);
        return Ok(None);
    }

    if event_name != Some("PreToolUse") {
        return Ok(None);
    }

    let touched_paths = extract_touched_paths(input, &cwd);
    if touched_paths.is_empty() {
        return Ok(None);
    }

    let scan = scan_rules(&cwd)?;
    let matched = scan
        .rules
        .into_iter()
        .filter(|rule| {
            touched_paths
                .iter()
                .any(|trigger_path| rule_matches(rule, trigger_path, &cwd))
        })
        .collect::<Vec<_>>();
    let system_message = (!scan.diagnostics.is_empty())
        .then(|| format!("Invalid path rule(s):\n- {}", scan.diagnostics.join("\n- ")));
    let context = if matched.is_empty() {
        None
    } else {
        let _lock = acquire_state_lock(&cwd, &session_id, cache_root)?;
        let mut state = read_state(&cwd, &session_id, cache_root)?;
        let candidates = matched
            .into_iter()
            .filter(|rule| !state.injected_rules.contains(&rule.key))
            .collect::<Vec<_>>();
        let batch = render_rules(&candidates);
        if batch.emitted_keys.is_empty() {
            None
        } else {
            state.injected_rules.extend(batch.emitted_keys);
            write_state(&cwd, &session_id, cache_root, &state)?;
            Some(batch.context)
        }
    };

    if system_message.is_none() && context.is_none() {
        return Ok(None);
    }

    let mut output = json!({});
    if let Some(system_message) = system_message {
        output["systemMessage"] = Value::String(system_message);
    }
    if let Some(context) = context {
        output["hookSpecificOutput"] = json!({
            "hookEventName": "PreToolUse",
            "additionalContext": context,
        });
    }
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::pathutil::path_to_string;
    use crate::selftest::{additional_context, create_temp_dir};

    /// Write a rule big enough that two of them cannot share one batch.
    fn write_oversized_rule(repo: &Path, name: &str, marker: &str) {
        let body = format!(
            "---\npaths:\n  - \"src/**\"\n---\n\n{marker}\n{}",
            "x".repeat(6000)
        );
        fs::write(repo.join(".claude").join("rules").join(name), body).expect("write rule");
    }

    fn edit_src_app(repo: &Path, cache: &Path) -> Option<Value> {
        run_hook_with_cache(
            &json!({
                "hook_event_name": "PreToolUse",
                "session_id": "test-session",
                "cwd": path_to_string(repo),
                "tool_name": "Edit",
                "tool_input": { "file_path": "src/app.ts" },
            }),
            repo,
            Some(cache),
        )
        .expect("hook run should succeed")
    }

    fn write_rule(repo: &Path, name: &str, markdown: &str) {
        fs::write(repo.join(".claude").join("rules").join(name), markdown).expect("write rule");
    }

    fn temp_repo() -> (PathBuf, PathBuf, PathBuf) {
        let root = create_temp_dir("hook-test").expect("temp dir");
        let repo = root.join("repo");
        let cache = root.join("cache");
        fs::create_dir_all(repo.join(".claude").join("rules")).expect("rules dir");
        (root, repo, cache)
    }

    #[test]
    fn a_rule_deferred_by_the_batch_budget_is_injected_on_the_next_call() {
        let (root, repo, cache) = temp_repo();
        write_oversized_rule(&repo, "a.md", "RULE-ALPHA");
        write_oversized_rule(&repo, "b.md", "RULE-BRAVO");

        let first = additional_context(edit_src_app(&repo, &cache)).expect("first context");
        let second = additional_context(edit_src_app(&repo, &cache)).expect("second context");
        let third = edit_src_app(&repo, &cache);
        let _ = fs::remove_dir_all(&root);

        assert!(
            first.contains("RULE-ALPHA") && !first.contains("RULE-BRAVO"),
            "the first batch should hold only the first rule"
        );
        assert!(
            second.contains("RULE-BRAVO"),
            "the deferred rule should be injected by the next matching call"
        );
        assert!(third.is_none(), "both rules injected; nothing further");
    }

    #[test]
    fn a_non_matching_call_leaves_no_state_behind() {
        let (root, repo, cache) = temp_repo();
        write_oversized_rule(&repo, "a.md", "RULE-ALPHA");

        let output = run_hook_with_cache(
            &json!({
                "hook_event_name": "PreToolUse",
                "session_id": "test-session",
                "cwd": path_to_string(&repo),
                "tool_name": "Edit",
                "tool_input": { "file_path": "docs/readme.md" },
            }),
            &repo,
            Some(&cache),
        )
        .expect("hook run should succeed");

        let cache_created = cache.exists();
        let _ = fs::remove_dir_all(&root);
        assert!(output.is_none());
        assert!(
            !cache_created,
            "no state should be written when nothing matches"
        );
    }

    #[test]
    fn a_valid_sibling_is_injected_while_invalid_rules_emit_a_system_message() {
        let (root, repo, cache) = temp_repo();
        write_rule(&repo, "invalid.md", "---\npaths: []\n---\nINVALID");
        write_rule(&repo, "valid.md", "---\npaths: src/**\n---\nVALID");

        let output = edit_src_app(&repo, &cache).expect("hook output");
        let context = additional_context(Some(output.clone())).expect("valid context");
        let system_message = output["systemMessage"].as_str().expect("system message");
        let _ = fs::remove_dir_all(&root);

        assert!(
            context.contains("VALID"),
            "valid sibling should be injected"
        );
        assert!(
            !context.contains("Invalid path rule") && !context.contains("invalid.md"),
            "diagnostic must not reach additionalContext"
        );
        assert!(system_message.contains("invalid.md"));
        assert!(system_message.contains("`paths:` must contain at least one glob"));
    }

    #[test]
    fn invalid_rules_emit_a_system_message_without_additional_context() {
        let (root, repo, cache) = temp_repo();
        write_rule(&repo, "invalid.md", "---\npaths:\n---\nINVALID");

        let output = edit_src_app(&repo, &cache).expect("diagnostic output");
        let _ = fs::remove_dir_all(&root);

        assert!(output["systemMessage"].as_str().is_some());
        assert!(
            output.get("hookSpecificOutput").is_none(),
            "diagnostic-only output must not add agent context"
        );
    }
}
