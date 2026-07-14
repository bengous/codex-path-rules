# codex-path-rules

Path-scoped rule loading for Codex.

`codex-path-rules` is a Codex command hook that reads path-scoped Markdown rules from the repo's `.claude/rules/` directory and any configured shared rule directories, then injects matching rule bodies as `additionalContext` only when a tool call touches a matching path.

It exists for repos that already keep Claude-style path rules and do not want to load every rule into every Codex session.

## Requirements

- A Codex CLI recent enough to honor `hookSpecificOutput.additionalContext` on `PreToolUse` ([openai/codex#20692](https://github.com/openai/codex/pull/20692), May 2026). Older releases reject the hook output as unsupported and inject nothing.
- Hooks enabled. Recent Codex releases enable the `hooks` feature by default; older ones need `[features] hooks = true`.

## Install

```sh
cargo install --locked --git https://github.com/bengous/codex-path-rules
```

## Configure Codex

Add hooks to `.codex/config.toml` in your repo:

```toml
[features]
hooks = true

[[hooks.PreToolUse]]
matcher = "^(Bash|apply_patch|Edit|Write|MultiEdit)$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Loading path rules"

# `resume` is deliberately excluded: a resumed session keeps its context, so
# rules already injected must stay de-duplicated.
[[hooks.SessionStart]]
matcher = "startup|clear"

[[hooks.SessionStart.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Resetting path rules"

[[hooks.SessionEnd]]

[[hooks.SessionEnd.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Cleaning path rules state"

# Compaction rewrites the context, so previously injected rules may be gone.
[[hooks.PostCompact]]
matcher = "manual|auto"

[[hooks.PostCompact.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Resetting path rules"
```

Codex requires project-local hooks to be trusted before they run. Use `/hooks` in the Codex CLI when prompted.

To load shared rule directories in addition to the current repo's `.claude/rules`, set `CODEX_PATH_RULES_EXTRA_DIRS` in the environment that launches Codex:

```sh
CODEX_PATH_RULES_EXTRA_DIRS="$HOME/.claude/rules" codex
```

`$HOME/.claude/rules` is Claude Code's user-level rules directory. You can use any existing custom directory instead.

Multiple directories use the platform path separator (`:` on macOS/Linux, `;` on Windows). For example, on macOS/Linux:

```sh
CODEX_PATH_RULES_EXTRA_DIRS="$HOME/work/agent-rules:$HOME/.claude/rules" codex
```

On macOS/Linux, you can also pin the variable on the hook command itself:

```toml
[[hooks.PreToolUse.hooks]]
type = "command"
command = "CODEX_PATH_RULES_EXTRA_DIRS=$HOME/work/agent-rules codex-path-rules"
timeout = 10
statusMessage = "Loading path rules"
```

## Rule Files

Create Markdown files under `.claude/rules/`:

```md
---
paths:
  - "src/**/*.css"
  - "src/**/*.svelte"
---

# Frontend rules

Keep component styles in the matching stylesheet.
```

When a matching path is touched, Codex receives:

```xml
<rule>
# Frontend rules

Keep component styles in the matching stylesheet.
</rule>
```

## Behavior

- Reads Markdown rules recursively under `.claude/rules/`; symlinked rule files are ignored.
- Also reads Markdown rules from each directory in `CODEX_PATH_RULES_EXTRA_DIRS`, after project-local rules. Relative extra directories resolve against the hook `cwd`; repeated rule paths and aliases are de-duplicated.
- Supports `paths:` as a scalar, block list, or inline list; globs support `*`, `**`, `?`, and `{a,b}` brace alternation.
- Injects each rule once per session; resets on `SessionStart` (startup/clear), `SessionEnd`, and `PostCompact`.
- Budgets injection at 6000 characters per rule and 12000 per batch. A rule that does not fit the current batch is deferred: it stays eligible and is injected by the next matching tool call, never silently lost.
- Rule bodies reach the model verbatim, except literal `</rule>` sequences, which are neutralized so a rule cannot break out of its wrapper block.
- Fails open: hook errors are printed to stderr and never block the tool call.
- Caches state under `~/.cache/codex-path-rules/` (respects `XDG_CACHE_HOME`; override with `CODEX_PATH_RULES_CACHE`). Session state idle for 7 days is swept on reset events, and lock directories leaked by a killed hook are broken after 60 seconds.

For `Bash`, path detection is intentionally lightweight. It recognizes common read commands such as `cat`, `nl`, `less`, `more`, `sed`, `head`, `tail`, `rg`, and `grep`. It also reads pathspecs after a literal `--` for direct `git diff`, `git show`, `git log`, and `git blame` commands, plus explicit contiguous roots before the first predicate or operator in `find`. For edits, it reads path fields and patch headers from `apply_patch`, `Edit`, `Write`, and `MultiEdit` payloads.

## Known limitations

- The reference matcher excludes MCP tools because their names and input schemas vary by server. To cover one, add its tool name to the matcher; path extraction can read only direct `path`, `file_path`, or `filePath` fields from an otherwise unrecognized tool input.
- Bash path extraction is a best-effort lexer, not a full shell parser. Only the commands listed above have dedicated handling; redirections and subshells may be missed or misclassified.
- Git detection handles only direct `git diff`, `git show`, `git log`, and `git blame` commands with a literal `--`. Global Git options, other subcommands, paths before `--`, and `REV:path` are ignored.
- Find detection reads explicit roots until the first predicate or operator. It does not infer the default `.` root or handle global options.
- Each directory operand expands to the directory itself and at most 200 files beneath it. `.git`, `node_modules`, `dist`, and unreadable directories are skipped.

## Development

The crate is a small library (`src/lib.rs`, one module per concern — see the crate docs for the module map) plus a thin CLI (`src/main.rs`).

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo run --locked -- --self-test
```
