# Example: custom workflow with a `techlead` role

Issue #73 lets a repo define its own pipeline in `.warden/workflow.yaml`
instead of being stuck with the hardcoded coder -> gate review -> gate test
sequence. This example appends a fourth role -- `techlead` -- after the
built-in reviewer/tester pair: it runs once both have come back clean for a
cycle, and can still send the whole cycle back to the coder with a blocking
finding (`gate: loop-until-clean`), exactly like the reviewer/tester already
do.

## Using this example

Copy both files into the repo you run `warden run --repo ...` against.
`workflow.yaml` here lives at this example's own top level (not under
`.warden/`) purely so it isn't swallowed by this repo's own `.gitignore`
(`.warden/` is Warden's runtime state directory, never committed) --  in
your actual repo it belongs under `.warden/workflow.yaml`:

```
your-repo/
├── .warden/workflow.yaml       <- this example's workflow.yaml
└── .claude/agents/techlead.md  <- from this example, same relative path
```

Then run `warden` exactly as usual (`warden run --repo your-repo --intent
"..." --tool claude`) -- no new flag is required to pick up
`.warden/workflow.yaml`; its mere presence is what activates it. Without it
(no `.warden/workflow.yaml` at all), a run uses the built-in default
pipeline unchanged.

## Ordering is not restricted

Every workflow step -- built-in or custom -- runs through the exact same
execution path (worktree, subprocess spawn, findings validation, crash
recovery). A step literally named `coder`/`reviewer`/`tester` still
resolves through the existing, hardened, role-asymmetric path
(`warden::agent_def::resolve_agent_definition`) -- that trust model is
inherent to what those three names *mean*, not to their position -- but
nothing stops you from inserting `techlead` *between* the reviewer and the
tester instead of after both, or reordering the pipeline further. The only
structural rule `warden` enforces is that the first step is the pipeline's
producer (it creates the commit/diff every later step reviews) and must not
declare a `gate`.

Any role other than `coder`/`reviewer`/`tester` (like `techlead` here) is
resolved from `.claude/agents/<agent>.md` -- Claude Code's own subagent file
convention (ADR-0013) -- with no adapter default to fall back to: a missing
file is a hard, actionable error naming the role and the exact path
expected, not a silently skipped step.

## Cycle budget

`workflow.yaml`'s first two gated steps (positions 1 and 2 -- the reviewer
and tester in this example) are bound by `--max-review-cycles`/
`--max-test-cycles` respectively; any step beyond those two shares a single
budget, controlled by `--max-cycles` (default 5).
