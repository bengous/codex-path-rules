# codex-path-rules

Path-scoped rule loading for Codex.

`codex-path-rules` is a Codex command hook that reads path-scoped Markdown rules from `.claude/rules/` at the Codex working directory, from nested `.claude/rules/` directories along each touched path, and from any configured shared rule directories. It injects matching rule bodies as `additionalContext` only when a tool call touches a matching path.

It exists for repos that already keep Claude-style path rules and do not want to load every rule into every Codex session.

## Requirements

- Codex CLI 0.129.0 or newer. Earlier releases reject `hookSpecificOutput.additionalContext` on `PreToolUse` as unsupported and inject nothing ([openai/codex#20692](https://github.com/openai/codex/pull/20692)).
- Hooks enabled. Recent Codex releases enable the `hooks` feature by default; older ones need `[features] hooks = true`.

## Install

### Download a verified binary

Download the archive for your platform and `SHA256SUMS` from the
[latest release](https://github.com/bengous/codex-path-rules/releases/latest):

| Platform | Archive |
| --- | --- |
| Linux x64 (glibc 2.35+) | `codex-path-rules-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `codex-path-rules-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `codex-path-rules-aarch64-apple-darwin.tar.gz` |
| Windows x64 | `codex-path-rules-x86_64-pc-windows-msvc.zip` |

Verify the downloaded archive before extracting it:

```sh
sha256sum --ignore-missing --check SHA256SUMS
```

On macOS, use `shasum -a 256 <archive>` and compare its output with the
matching line in `SHA256SUMS`. On Windows, use
`Get-FileHash <archive> -Algorithm SHA256` and compare it likewise. Extract
`codex-path-rules` (`codex-path-rules.exe` on Windows) into an existing user
directory in `PATH`, then verify it:

```sh
codex-path-rules --version
codex-path-rules --self-test
```

Release archives are checksummed but not signed or notarized. Compile the tag
from source when you need the strongest available provenance. Each archive
also includes the project and third-party license notices.

### Compile the release tag

Install Rust 1.97.1, then compile the exact release tag with Cargo.
Replace `<tag>` with the tag named on the
[latest release](https://github.com/bengous/codex-path-rules/releases/latest)
page.

```sh
cargo +1.97.1 install --locked \
  --git https://github.com/bengous/codex-path-rules --tag <tag>
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

Nested project rules are discovered automatically and do not need extra configuration. To load shared rule directories outside the current project, set `CODEX_PATH_RULES_EXTRA_DIRS` in the environment that launches Codex:

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

In a monorepo, a rule at `apps/journal/.claude/rules/svelte.md` uses paths relative to `apps/journal`. For example, `src/**/*.svelte` matches `apps/journal/src/App.svelte` but not `apps/portfolio/src/App.svelte`.

When a matching path is touched, Codex receives:

```xml
<rule>
# Frontend rules

Keep component styles in the matching stylesheet.
</rule>
```

Codex also shows this human-facing message only when the rule is actually injected:

```text
Path rules loaded:
- .claude/rules/frontend.md
```

## Behavior

- Reads Markdown rules recursively under `cwd/.claude/rules/`, then under each nested `.claude/rules/` directory along a touched path. It does not scan unrelated project subtrees. Project rule directory symlinks and symlinked rule files are ignored; explicitly configured shared directory symlinks remain allowed.
- Matches each project rule relative to the directory that contains its `.claude/` directory. A nested rule without `paths:` applies throughout that nested scope, not the whole project.
- Also reads Markdown rules from each directory in `CODEX_PATH_RULES_EXTRA_DIRS`, after project-local rules. Relative extra directories resolve against the hook `cwd`. A shared directory outside the project keeps its globs relative to `cwd`; an extra directory that names a project's own `.claude/rules` keeps that directory's project scope instead. Repeated rule paths and aliases are de-duplicated.
- Supports `paths:` as a scalar, block list, or inline list; globs support `*`, `**`, `?`, and `{a,b}` brace alternation.
- Rules without front matter apply throughout their rule scope. A leading `---` opens front matter and must have a closing fence.
- Reports every successfully injected rule through `systemMessage`. Project rule paths are relative to `cwd`; shared external rule paths stay absolute. Already-injected rules stay silent, and deferred rules are reported only when a later batch injects them.
- Skips malformed rules and empty `paths:` values without blocking the tool call. An unreadable rule directory also leaves valid rules available. Codex shows each rule warning once per session through `systemMessage`; warnings are never added to agent context.
- Injects each rule once per session; resets on `SessionStart` (startup/clear), `SessionEnd`, and `PostCompact`.
- Budgets injection at 6000 characters per rule and 12000 per batch. A rule that does not fit the current batch is deferred: it stays eligible and is injected by the next matching tool call, never silently lost.
- Rule bodies reach the model verbatim, except literal `</rule>` sequences, which are neutralized so a rule cannot break out of its wrapper block.
- Fails open: hook errors are printed to stderr and never block the tool call.
- Caches state under `~/.cache/codex-path-rules/` (respects `XDG_CACHE_HOME`; override with `CODEX_PATH_RULES_CACHE`). Session state idle for 7 days is swept on reset events, and lock directories leaked by a killed hook are broken after 60 seconds.

For `Bash`, path detection is intentionally lightweight. It recognizes common read commands such as `cat`, `nl`, `less`, `more`, `sed`, `head`, `tail`, `rg`, and `grep`. A direct `cd <dir>` segment before `&&` or `;` updates the base for later paths. The hook also reads pathspecs after a literal `--` for direct `git diff`, `git show`, `git log`, and `git blame` commands, plus explicit contiguous roots before the first predicate or operator in `find`. For edits, it reads path fields and patch headers from `apply_patch`, `Edit`, `Write`, and `MultiEdit` payloads.

## Known limitations

- Project rule discovery starts at the hook `cwd`; it does not search parent directories or detect a Git root. Start Codex from the intended project root when parent rules must apply.
- The reference matcher excludes MCP tools because their names and input schemas vary by server. To cover one, add its tool name to the matcher; path extraction can read only direct `path`, `file_path`, or `filePath` fields from an otherwise unrecognized tool input.
- Bash path extraction is a best-effort lexer, not a full shell parser. Only the commands listed above and direct `cd` segments have dedicated handling; redirections, grouped commands, and complex conditional chains may be missed or misclassified.
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
