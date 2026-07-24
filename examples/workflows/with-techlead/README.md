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

## Current engine limitation

The convergence loop's built-in `coder`/`reviewer`/`tester` steps still run
through their existing, hardened resolution path
(`warden::agent_def::resolve_agent_definition`) -- a custom
`workflow.yaml` may only **append** steps after them, never reorder,
replace, or omit them. The first three steps of `workflow.yaml` must always
be exactly `coder`, `reviewer`, `tester` in that order; `warden` rejects
anything else with a clear error at startup.

Every step beyond those three (like `techlead` here) is resolved from
`.claude/agents/<agent>.md` -- Claude Code's own subagent file convention
(ADR-0013) -- with no adapter default to fall back to: a missing file is a
hard, actionable error naming the role and the exact path expected, not a
silently skipped step.

## Cycle budget

Any step beyond the built-in pair shares a single cycle budget, controlled
by `--max-cycles` (default 5) -- distinct from `--max-review-cycles`/
`--max-test-cycles`, which still bound the built-in reviewer/tester pair
only.
