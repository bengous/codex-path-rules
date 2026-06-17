# codex-path-rules

Path-scoped rule loading for Codex.

`codex-path-rules` is a Codex command hook that reads `.claude/rules/*.md` and injects the matching rule body as `additionalContext` only when a tool call touches a matching path.

It exists for repos that already keep Claude-style path rules and do not want to load every rule into every Codex session.

## Install

```sh
cargo install --git https://github.com/bengous/codex-path-rules
```

## Configure Codex

Add a hook to `.codex/config.toml` in your repo:

```toml
[features]
hooks = true

[[hooks.PreToolUse]]
matcher = "^(Bash|apply_patch|Edit|Write)$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Loading path rules"

[[hooks.SessionStart]]
matcher = "compact|clear"

[[hooks.SessionStart.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Resetting path rules"

[[hooks.PostCompact]]
matcher = "manual|auto"

[[hooks.PostCompact.hooks]]
type = "command"
command = "codex-path-rules"
timeout = 10
statusMessage = "Resetting path rules"
```

Codex requires project-local hooks to be trusted before they run. Use `/hooks` in the Codex CLI when prompted.

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

- Reads Markdown rules recursively under `.claude/rules/`.
- Supports `paths:` as a scalar, block list, or inline list.
- Supports glob `*`, `**`, `?`, and `{a,b}` brace alternation.
- Injects each rule once per session.
- Resets session cache on `SessionStart` and `PostCompact`.
- Fails open: hook errors are printed to stderr and never block the tool call.
- Ignores symlinked rule files.
- Caches state under `~/.cache/codex-path-rules/`, or `CODEX_PATH_RULES_CACHE` when set.

For `Bash`, path detection is intentionally lightweight. It recognizes common read commands such as `cat`, `nl`, `less`, `more`, `sed`, `head`, `tail`, `rg`, and `grep`. For edits, it reads path fields and patch headers from `apply_patch`, `Edit`, `Write`, and `MultiEdit` payloads.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo run --locked -- --self-test
```
