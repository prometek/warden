# Warden

[![CI](https://github.com/prometek/warden/actions/workflows/ci.yml/badge.svg)](https://github.com/prometek/warden/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Warden is a local orchestrator for AI coding agents. Give it a task and an existing Git
repository; it runs a coder, reviewer, and tester in isolated worktrees until their findings
are resolved.

Warden invokes installed agent CLIs instead of calling model APIs directly. It includes adapters
for Claude Code and OpenAI Codex CLI, plus an experimental Mistral adapter. Runs persist in SQLite,
survive crashes, and can optionally execute inside Docker.

> [!WARNING]
> Warden is experimental software. Version `0.1.x` may change configuration formats and CLI
> behavior. Run it on repositories backed by version control and review generated changes before
> merging them.

## Why Warden?

One agent can write code. Shipping reliable changes needs more than one opinion.

Warden provides:

- a default `coder → reviewer → tester` convergence loop;
- a separate Git worktree for every role;
- blocking findings that automatically return work to the coder;
- configurable cycle budgets and custom workflow steps;
- durable SQLite state, crash recovery, and quota-aware resume;
- batch execution for multiple tasks;
- a read-only terminal monitor;
- optional Docker isolation, resource limits, and controlled egress;
- optional evidence capture for test runs;
- an independent Git gate for pushing converged commits and watching CI.

## Requirements

- macOS or Linux;
- Git;
- one supported agent CLI installed and already authenticated:
  - `claude` for Claude Code;
  - `codex` for OpenAI Codex CLI;
  - `mistral` for experimental Mistral CLI support;
- Docker only when using `--isolation docker`;
- Rust 1.95 only when building from source.

Warden does not read API keys or authenticate agent CLIs for you.

## Install

### Prebuilt release

Download archive matching your platform from
[GitHub Releases](https://github.com/prometek/warden/releases), then verify its checksum:

```sh
grep "warden-<version>-<target>.tar.gz$" checksums.txt | shasum -a 256 -c -
tar xzf warden-<version>-<target>.tar.gz
```

Archive contains `warden`, `warden-tui`, and `warden-gated`. Move binaries to directory on
your `PATH`, for example `~/.local/bin`.

Available targets:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`

### Build from source

```sh
git clone https://github.com/prometek/warden.git
cd warden
cargo build --release --workspace
```

Binaries are written to `target/release/`.

## Quick start

Run Warden against an existing repository:

```sh
warden run \
  --repo /path/to/project \
  --intent "Add email validation to the signup form" \
  --tool claude
```

Replace `claude` with `codex` to use OpenAI Codex CLI. `mistral` adapter remains experimental.

Warden creates role worktrees under `~/.warden`, runs every workflow step, records findings,
and repeats until all blocking findings are resolved or a cycle budget is exhausted. Original
working checkout is not used as an agent workspace.

Run prints its ID and attach command at startup:

```sh
warden-tui attach --run-id <run-id> --warden-home ~/.warden
```

Use `--tui` to open monitor automatically:

```sh
warden run \
  --repo /path/to/project \
  --intent "Add email validation" \
  --tool claude \
  --tui
```

Use `warden run --help` for complete CLI reference.

## How convergence works

Default workflow has three sequential roles:

1. `coder` implements task and creates commit.
2. `reviewer` inspects change. Blocking findings return run to coder.
3. `tester` validates reviewed change. Blocking findings also return run to coder.

Reviewer sees complete change first, then focused corrections during later review passes. Each
role receives its own worktree. Warden stores run state, events, findings, token usage, quota
status, and cleanup work in `~/.warden/state.db`.

Default cycle limits are five review cycles and five test cycles. Change them with:

```sh
warden run \
  --repo /path/to/project \
  --intent "Refactor authentication" \
  --tool codex \
  --max-review-cycles 3 \
  --max-test-cycles 2
```

## Batch mode

Repeat `--intent` or provide one task per line:

```sh
warden run \
  --repo /path/to/project \
  --tool claude \
  --intents-file tasks.txt \
  --fail-fast
```

Blank lines and lines beginning with `#` are ignored. Every task runs independently.

## Docker isolation

Default `worktree` isolation separates agent workspaces but runs agent processes with your host
user permissions. Use Docker when filesystem isolation matters:

```sh
docker build -t warden-agent:0.1.0 crates/warden-sandbox/docker

warden run \
  --repo /path/to/project \
  --intent "Update dependencies" \
  --tool claude \
  --isolation docker
```

CPU and memory remain unrestricted unless configured:

```sh
warden run \
  --repo /path/to/project \
  --intent "Run migration" \
  --tool claude \
  --isolation docker \
  --docker-cpus 2 \
  --docker-memory 4g
```

Controlled egress requires both operator-provided internal Docker network and proxy:

```sh
warden run \
  --repo /path/to/project \
  --intent "Update API client" \
  --tool claude \
  --isolation docker \
  --docker-network warden-egress \
  --docker-egress-proxy http://warden-proxy:3128
```

Warden verifies network has Docker `internal` flag and refuses direct Internet access otherwise.
It does not create proxy or manage its allowlist. See
[Docker image documentation](crates/warden-sandbox/docker/README.md).

## Configure agents

No agent files are required. Each adapter provides default prompts and tool permissions.

Override coder using `<repo>/.warden/agents/coder.md`. Reviewer and tester definitions come from
`$XDG_CONFIG_HOME/warden/agents/` or `~/.config/warden/agents/` so coder cannot rewrite agents
judging its work.

Example:

```markdown
---
name: reviewer
description: Reviews correctness and maintainability.
tools: Read, Bash
model: sonnet
---

Review change described by JSON payload received on stdin.
Return blocking findings as NDJSON.
```

`--trust-repo-agents` allows repository-provided reviewer and tester definitions as fallback.
Use it only for repositories you trust.

## Customize workflow

Add `.warden/workflow.yaml` to change default pipeline or add agent and deterministic hook steps:

```yaml
name: reviewed-and-linted
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: scoped-re-review
    budget: review
  - role: lint
    type: hook
    run: cargo fmt --check
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
    budget: test
    evidence: true
```

Ready-to-copy configurations live under [`examples/workflows/`](examples/workflows/).

Repository can also define:

- `.warden/hooks.toml` for lifecycle commands;
- `.warden/policy.yaml` for denial and interactive approval rules.

Treat repository-defined hooks and workflows as executable code.

## Evidence

Warden can capture Playwright or asciinema evidence after successful test steps. Detection is
automatic; override it with `--evidence-tool`. Evidence is stored in
`.warden/evidence/<cycle>/` by default. Disable repository storage with:

```sh
--evidence-store-in-repo false
```

Missing evidence tool produces warning and does not turn converged run into failure.

## Git gate and CI

`warden-gated` is optional. Without it, Warden stops at `Converged` and never pushes to real
remote.

When configured, gate owns remote credentials, independently verifies persisted run state and
commit hash, pushes approved commit, opens or updates pull request, and watches CI. Warden itself
only pushes to local bare gate repository.

Start with:

```sh
warden-gated --help
warden-gated init-bare --help
warden-gated serve --help
```

Service templates are available under
[`crates/warden-gated/contrib/`](crates/warden-gated/contrib/). GitHub integration requires
authenticated `gh` CLI. Warden never merges pull requests automatically.

## Security

AI agents execute code and inspect repository contents. Choose isolation model deliberately.

- `worktree` isolates Git state, not host filesystem. Agent process keeps permissions granted by
  selected CLI and current OS user.
- `docker` limits mounted host paths, drops capabilities, disables privilege escalation, and caps
  process count. Host Docker daemon and kernel remain trusted.
- Docker network access is unrestricted by default. Internal network plus proxy enables enforced
  egress routing.
- Docker mounts host Claude configuration read-only for authentication. Agent can still read and
  exfiltrate those credentials unless egress is restricted.
- `.git` stays writable because linked worktrees require it.
- Reviewer and tester configuration is user-owned by default to reduce prompt tampering risk.
- Policy rules reduce accidental actions; they are not security sandbox.
- `warden-gated` provides independent authorization boundary only when it runs under separate OS
  identity from orchestrator. Same-user deployment remains logical separation, not OS isolation.

Review generated commits before merge. Do not run repository hooks or trusted agent definitions
from unknown projects.

## Project status

Warden currently targets local, single-operator workflows. Public APIs and configuration formats
may change before `1.0`. See [CHANGELOG.md](CHANGELOG.md) for release history and
[GitHub Issues](https://github.com/prometek/warden/issues) for active work.

## Development

Repository pins Rust toolchain in `rust-toolchain.toml`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
SQLX_OFFLINE=true cargo test --workspace --all-targets --all-features
cargo build --workspace --all-features --release
```

SQLite query metadata is committed, so build and tests do not require live database. Read
[`code-standards.md`](code-standards.md) before contributing. Bug reports and focused pull
requests are welcome.

## License

Warden is released under [MIT License](LICENSE).
