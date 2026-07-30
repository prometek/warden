# Example: `gate: scoped-re-review` and `max_cycles` (issue #81)

Issue #81 generalizes the reviewer's scoped re-review optimization (#37/
ADR-0014) to any step, and lets a step declare its own independent cycle
budget instead of sharing one of the three run-level buckets
(`review`/`test`/`extra`). This example adds a `secreview` role between the
reviewer and the tester to show both in one place:

```yaml
name: with-scoped-review
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
    budget: review
  - role: secreview
    agent: security-reviewer
    gate: scoped-re-review
    max_cycles: 3
  - role: tester
    agent: test-runner
    gate: loop-until-clean
    budget: test
    evidence: true
```

## `gate: scoped-re-review`

`secreview`'s first pass over a run is a full review of the whole cycle
diff. If a later step (here, `tester`) sends the cycle back to the coder,
`secreview`'s next invocation is scoped to just the coder's correctif (and
the findings that motivated it) instead of the whole diff again -- as long
as `secreview` itself was the previous step to have looked at the commit
this cycle's correctif is built against. If `secreview` was skipped for one
or more cycles (an earlier gated step, `reviewer` here, blocked before the
pipeline ever reached it), it gets a full pass again on its return: a
correctif-scoped payload only carries the current cycle's diff and would
silently miss whatever was committed during the cycles it missed.

This is exactly the behaviour `workflow.steps[1]` (here, `reviewer`) has
always had positionally; `scoped-re-review` makes it a property any step at
any position can opt into, instead of only the first gated step.

## `max_cycles: N`

`secreview` fixes its own cycle counter (`3` here) instead of sharing
`--max-cycles`/`--max-review-cycles`/`--max-test-cycles` with any other
step: a blocking finding from `secreview` reboucles the pipeline like any
other gated step, but is charged against this counter alone. `max_cycles`
is charged unconditionally on every invocation of the step (including a
clean one), exactly like `budget: test` -- never conditionally on the step
itself blocking, unlike `budget: review`.

`max_cycles` and `budget` are mutually exclusive on the same step, and
neither may be declared on the pipeline's first step (the producer). Unlike
`budget: review`/`budget: test`/`budget: extra`, a `max_cycles` counter is
tracked purely in memory for the run's duration -- it has no dedicated
column in the `runs` table, so it is not visible in `warden-tui`.

## Using this example

Same as `examples/workflows/with-techlead/`: copy `workflow.yaml` to
`.warden/workflow.yaml` in the repo you run `warden run --repo ...`
against, and provide `.claude/agents/security-reviewer.md` for the new
`secreview` role (resolved the same way as any custom role, ADR-0013) --
`reviewer`/`tester` still resolve through Warden's built-in, role-asymmetric
path regardless of where they sit in the pipeline.
