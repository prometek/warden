# Example: a non-agent `type: hook` step

Issue #79 opens `.warden/workflow.yaml` to steps that aren't an agent at all.
Before this, every step spawned an LLM subprocess -- expensive and
non-deterministic even for a check like `cargo fmt --check`, which has only
one correct answer and needs no judgement call. This example inserts a
`lint` step between the reviewer and the tester that runs a deterministic
shell command instead: no LLM round-trip, no extra tool grant, fully
reproducible.

## Using this example

Copy `workflow.yaml` into the repo you run `warden run --repo ...` against,
at `.warden/workflow.yaml` (it lives at this example's own top level here
purely so it isn't swallowed by this repo's own `.gitignore`, which excludes
`.warden/` as Warden's runtime state directory):

```
your-repo/
└── .warden/workflow.yaml   <- this example's workflow.yaml
```

Then run `warden` exactly as usual -- no new flag is required to pick up
`.warden/workflow.yaml`; its mere presence is what activates it.

## What `type: hook` changes

A step's `type` is `agent` by default (the only kind before issue #79, and
what every step in every `workflow.yaml` written before this issue still
means). Declaring `type: hook` instead:

- Replaces `agent: <name>` with `run: "<shell command>"` -- `agent` and
  `run` are mutually exclusive, and the wrong one for a step's declared
  `type` is a parse error naming the problem.
- Runs that command via `sh -c` inside the run's sandbox, checked out at
  this cycle's commit, in its own isolated worktree -- the same worktree
  isolation, `agent_processes` bookkeeping (pid, exit code), and
  `AgentStarted`/`AgentFinished` event pair any other step gets, so it is
  just as visible to crash recovery and `warden-tui`. Unlike a `type: agent`
  step, it reports no *token* usage (`usage: None` on its `AgentFinished`
  event) -- a deterministic command consumes no LLM.
- Is evaluated against `.warden/policy.yaml` first (issue #51, ADR-0016) --
  a `deny`-matched command is blocked before it ever reaches the sandbox,
  exactly like a `.warden/hooks.toml` lifecycle hook's own command already
  is.
- Never the pipeline's producer (`steps[0]`): a hook authors no commit, so
  it can only gate work an earlier `type: agent` step has already written.
  Cannot capture evidence either (`evidence: true` on a `type: hook` step is
  a parse error) -- ADR-0009 evidence records an *agent's* command session,
  which a hook step has none of.

**Trust model**: `run` is declared in `.warden/workflow.yaml`, a file that
lives in (and can be committed by) the repo under review -- the same trust
class as `.warden/hooks.toml`. It runs with a deliberately narrow, explicit
environment allowlist (`HOME`/`LANG`/`TERM`/`CARGO_HOME`/`RUSTUP_HOME`),
never the operator's full environment -- see
`warden::orchestrator::agents::WORKFLOW_STEP_ENV_ALLOWLIST`'s own doc
comment for the full rationale and the `--isolation docker` caveats.

## The verdict

A zero exit is a clean pass -- no finding at all. A non-zero exit (or a
policy denial) is exactly **one** blocking finding, sourced as this step's
own role (`FindingSource::role("lint")` here), aggregated into the
convergence loop the same way a reviewer/tester's own findings are: it
reboucles the pipeline back to the coder, within this step's own cycle
budget (`gate`/`budget` work identically to any other step -- see the
`with-techlead` example for the full budget-declaration story).

## Ordering is not restricted

Every workflow step -- built-in, custom agent, or hook -- goes through the
same convergence loop and the same findings aggregation. Nothing stops you
from moving `lint` earlier, later, or declaring several `type: hook` steps
for different deterministic checks (`cargo fmt`, `cargo clippy`, a
conventional-commit check, ...).
