//! Path-scoped rule injection hook for the Codex CLI.
//!
//! Codex invokes the `codex-path-rules` binary for `PreToolUse`,
//! `SessionStart`, `SessionEnd`, and `PostCompact` events (wired in
//! `.codex/config.toml`). The hook payload arrives as JSON on stdin. For
//! `PreToolUse` the hook inspects the paths a tool is about to touch; whenever
//! a touched path matches the `paths:` globs of a rule file under
//! `.claude/rules`, that rule's body is emitted once per session as
//! `additionalContext`, giving the agent the relevant guidance just in time.
//! Which rules were already injected is cached per session; the cache is
//! cleared on `SessionStart`/`SessionEnd`/`PostCompact`.
//!
//! # Failure model
//!
//! This is a context-injection hook, never a gate: a failure must not block
//! the developer's tool call. The binary therefore reports any error on stderr
//! and still exits `0` ("fail open"). Errors stay explicit and contextualized
//! so they remain actionable — nothing is swallowed silently.
//!
//! # Module map
//!
//! - `hook` — event dispatch and orchestration.
//! - `payload` — typed access to the JSON hook payload.
//! - `rules` — rule discovery, front matter parsing, and path matching.
//! - `glob` — the glob matcher (`*`, `**`, `?`, `{a,b}`).
//! - `touched` — extraction of the paths a tool call is about to touch.
//! - `render` — rendering matched rules within the injection budget.
//! - `session` — session identity, injected-rule state, locking, and cache
//!   hygiene.
//! - `pathutil` — small path string helpers.
//! - `selftest` — the end-to-end `--self-test` scenario.

mod glob;
mod hook;
mod pathutil;
mod payload;
mod render;
mod rules;
mod selftest;
mod session;
mod touched;

use std::env;

pub use selftest::run_self_test;

/// Result of a hook operation. The error is a human-readable, contextualized
/// message meant for the hook user's stderr (see the crate-level failure
/// model); a structured error type would add nothing because every error
/// funnels into a single `eprintln!`.
pub type HookResult<T> = Result<T, String>;

/// Run the hook against the raw stdin payload, returning the JSON line to
/// print on stdout, if any.
///
/// # Errors
///
/// Propagates failures parsing the payload, resolving the working directory,
/// running the hook, or serializing its output.
pub fn run(input_text: &str) -> HookResult<Option<String>> {
    let input = payload::parse_json_object(input_text)?;
    let fallback_cwd =
        env::current_dir().map_err(|error| format!("failed to read current directory: {error}"))?;

    hook::run_hook(&input, &fallback_cwd)?
        .map(|output| {
            serde_json::to_string(&output)
                .map_err(|error| format!("failed to render hook output: {error}"))
        })
        .transpose()
}
