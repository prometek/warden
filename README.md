# Warden

Warden runs repository-defined AI workflows in isolated worktrees or Docker containers. Workflow roles, order, retries, and convergence rules live in your repository, not in Warden.

Warden has no default workflow and no built-in `coder`, `reviewer`, or `tester` roles.

## Features

- Explicit workflow graph with `agent` and `command` steps
- Configurable transitions for `clean`, `blocking`, and `error` outcomes
- Claude Code, Codex, and Mistral CLI adapters
- Per-step worktree isolation or Docker isolation
- Optional CPU, memory, and proxy-controlled Docker egress
- Durable state, crash cleanup, quota resumption, CI gating, and terminal UI
- Repository-defined agent prompts under `.warden/agents/`

## Requirements

- Rust toolchain from `rust-toolchain.toml`
- Git
- At least one supported agent CLI when workflow uses agent steps
- Docker when using `--isolation docker`

## Install

```bash
cargo install --path crates/warden
cargo install --path crates/warden-tui
cargo install --path crates/warden-gated
```

## Quick start

Create `.warden/workflow.yaml`:

```yaml
name: delivery
entry: implementation
steps:
  implementation:
    type: agent
    agent: implementer
    on_clean: checks
    on_blocking: implementation
    on_error: failed

  checks:
    type: command
    run: cargo test --workspace
    on_clean: review
    on_blocking: implementation
    on_error: failed

  review:
    type: agent
    agent: reviewer
    on_clean: converged
    on_blocking: implementation
    on_error: failed
    max_cycles: 3
```

Create each referenced agent definition, for example `.warden/agents/implementer.md`:

```markdown
---
tools: Read, Write, Edit, Bash
---

Implement the requested change. Read the JSON payload from stdin. Commit completed work before exiting. Return findings as NDJSON using this step's role as `source`.
```

Run it:

```bash
warden run \
  --repo . \
  --intent "Add email validation" \
  --tool claude \
  --max-cycles 5
```

`.warden/workflow.yaml` is mandatory. Missing agent definitions fail with their expected path; Warden never supplies hidden prompts or tool grants.

## Workflow model

Every step has an arbitrary identifier. `entry` selects the first step, independent of YAML ordering.

Step types:

- `agent`: references `.warden/agents/<agent>.md`. Any agent step may create a commit.
- `command`: runs `run` through the selected isolation backend. Exit `0` is `clean`; non-zero is `blocking`.

Every step declares all transitions:

- `on_clean`
- `on_blocking`
- `on_error`

Targets are another step identifier or terminals `converged` and `failed`. All steps must be reachable, and a path to `converged` must exist.

`--max-cycles` is global. Optional step `max_cycles` may only tighten it.

All agent steps receive the same versioned JSON payload:

```json
{
  "version": 4,
  "role": "implementation",
  "system_prompt": "...",
  "intent": "Add email validation",
  "current_commit": "abc123...",
  "diff": "diff --git ...",
  "findings": []
}
```

Agent output is NDJSON. A blocking finding follows `on_blocking`; malformed output or process failure follows `on_error`.

See [workflow examples](examples/workflows/).

## Isolation

Worktree mode runs agents as your host user:

```bash
warden run --repo . --intent "..." --tool codex --isolation worktree
```

Docker provides a filesystem boundary:

```bash
warden run \
  --repo . \
  --intent "..." \
  --tool claude \
  --isolation docker \
  --docker-cpus 2 \
  --docker-memory 4g
```

CPU and memory limits are optional. Network egress remains available by default. To force egress through a controlled proxy, provide both `--docker-network` and `--docker-egress-proxy`.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Contributions are welcome. Open an issue before large product changes so workflow semantics can be agreed first.

## License

[MIT](LICENSE)
