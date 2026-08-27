# codex-path-rules

A Codex CLI hook (Rust binary) that injects `.claude/rules/*.md` bodies as `additionalContext` on `PreToolUse`, only when the tool call touches a path matching a rule's `paths:` globs, at most once per session. Project rules are discovered at `cwd` and in nested `.claude/rules` directories along touched paths; shared external rule directories remain explicit. Invalid rules are skipped and reported once per session through the human-facing `systemMessage`. It targets Codex's Claude-style hook schema (snake_case payload on stdin, `hookSpecificOutput` JSON on stdout).

## Commands

Toolchain is pinned to 1.97.1 (`rust-toolchain.toml`, `.mise.toml`); CI runs exactly these four gates:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo run --locked -- --self-test
```

Single test: `cargo test render_defers` (name substring). Unit tests are colocated in each module's `#[cfg(test)] mod tests`; `tests/cli.rs` covers process-level CLI behavior. The self-test (`src/selftest.rs`) is an end-to-end scenario compiled into the production binary and also run as a unit test.

## Architecture

Library (`src/lib.rs`, one module per concern) + thin CLI (`src/main.rs`). Public API is only `run` and `run_self_test`; everything else is `pub(crate)`.

`PreToolUse` flow through `hook::run_hook_with_cache` (the orchestrator, and the entry point tests use to simulate whole sessions against a temp `cache_root`):

1. `touched` extracts paths from the tool input (path fields, `apply_patch` bodies, and a best-effort shell lexer for `Bash` — deliberately not a full parser). Direct `cd` segments before `&&` or `;` update the base for later paths.
2. `rules` models project and shared sources separately, scans `cwd/.claude/rules` plus nested rule directories along touched paths, and matches each path relative to its rule scope. Project directory symlinks are skipped; explicit shared directory symlinks are allowed. Scan errors become diagnostics so valid rules remain available. No lock or state IO happens unless at least one rule matches or a diagnostic exists.
3. `session` guards per-session state (`injectedRules` and `warnedRules` in a JSON file under the cache root, namespaced by cwd hash) with a `create_dir`-based lock.
4. `render` produces the batch and returns which rules were actually emitted.

## Invariants (violating these reintroduces fixed bugs)

- **Fail open**: this is a context-injection hook, never a gate. Hook runtime errors go to stderr and the process exits 0; self-test failures and invalid CLI arguments may exit nonzero. Errors are contextualized strings (`HookResult<T> = Result<T, String>`), never silently swallowed.
- **Only emitted rules are marked injected.** `render_rules` may *defer* a rule that overflows the 12000-char batch budget (6000 per rule, truncation notice counted inside the limit); a deferred rule must stay out of `injectedRules` so a later matching call injects it. Marking before rendering loses rules for the whole session.
- **Only displayed diagnostics are marked warned.** Failed rule sources and invalid rules remain out of agent context; their canonical keys enter `warnedRules` only when the hook emits the human-facing `systemMessage`.
- **Nested rules stay scoped.** A rule under `<scope>/.claude/rules` matches paths relative to `<scope>`, including rules without `paths:`. Unrelated subtrees are never scanned.
- **Rule source decides trust.** Project rule directory symlinks are skipped. `CODEX_PATH_RULES_EXTRA_DIRS` may explicitly name a shared symlink.
- **Rule bodies reach the model verbatim** except literal `</rule>`, which is neutralized. Do not HTML-escape: it corrupts code samples and inflates lengths past the budget math.
- **Session resets** happen on `SessionStart`/`SessionEnd`/`PostCompact` only — never on `resume` (a resumed session keeps its context, so re-injection would duplicate). Cache hygiene (7-day sweep, 60s stale-lock breaking) rides on reset events and is best-effort by design.
- Project rule directory symlinks and symlinked rule files are skipped so local rules cannot pull out-of-tree content into model context.
- `unsafe_code = "forbid"`, `clippy::all = "deny"`, no new dependencies beyond `serde_json`/`sha2` without cause — the crate is meant to be upstreamable into codex-rs or packaged as a plugin.

## Codex compatibility notes

`additionalContext` on `PreToolUse` requires Codex ≥ openai/codex#20692 (May 2026); some releases don't fire `PreToolUse` for `apply_patch`/MCP (openai/codex#16732). The README's `.codex/config.toml` example is the reference wiring; keep its matchers in sync with the tool names handled in `touched::extract_touched_paths`.
