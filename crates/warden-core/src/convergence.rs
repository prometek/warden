//! Convergence rules: interpreting findings from a workflow step's agent and
//! deciding the next [`RunState`]. Pure logic — no I/O, no clock, no
//! subprocess. Parsing of agent stdout also lives here since it's the
//! boundary where untrusted external input is validated before it can ever
//! reach the state machine (code-standards.md, "Validation à la frontière").

use serde::Deserialize;

use crate::error::{CoreError, Result};
use crate::state::RunState;
use crate::workflow::{Role, Workflow};

/// Which agent (or, for CI/Warden itself, which non-agent process) raised a
/// finding (`FINDINGS.source`).
///
/// **Issue #73**: the closed `Reviewer`/`Tester` pair is now
/// [`FindingSource::Role`], carrying the open, workflow-defined
/// [`crate::workflow::Role`] name that actually produced it -- `"reviewer"`/
/// `"tester"` are no longer special-cased here at all, just the names the
/// *built-in* default workflow happens to use for its two gated steps. This
/// is what lets a custom role's findings (e.g. a `techlead` step) aggregate
/// in [`decide_next_state_for_step`] exactly the way a reviewer's or
/// tester's already did -- there's no separate code path for "the two
/// hardcoded roles" versus "everything else".
///
/// `Ci` (issue #5) covers a failing check surfaced by `warden-gated`'s CI
/// watcher; `Warden` (issue #24 review, M4; resolve-and-compare rework in
/// issue #30) covers a finding the orchestrator raises directly from a
/// structural check that re-resolves the three literal paths
/// `.warden/agents/{coder,reviewer,tester}.md` through the OS from a cycle's
/// resulting commit and compares them against the run-start snapshot
/// (`warden::orchestrator::agent_definition_tampering_finding`) -- both are
/// reserved words no workflow role name may claim (see [`FindingSource::parse`]),
/// since neither ever comes from an agent subprocess's own judgement at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingSource {
    /// The workflow role name that raised this finding (e.g. `"reviewer"`,
    /// `"tester"`, or any custom role a `.warden/workflow.yaml` declares).
    Role(String),
    Ci,
    Warden,
}

impl FindingSource {
    /// Words no workflow role may claim (`Workflow::parse_yaml`'s review
    /// finding F1: a step named `role: warden` or `role: ci` would parse a
    /// finding it raises as [`FindingSource::Warden`]/[`FindingSource::Ci`]
    /// instead of [`FindingSource::Role`], breaking
    /// `validate_finding_sources_for_role`'s round-trip for that step every
    /// cycle). Kept as the single source of truth here, next to [`Self::parse`]
    /// which is the other place that must agree with it, so
    /// `crate::workflow::Workflow::parse_yaml` never re-lists these words
    /// itself.
    pub const RESERVED_ROLE_NAMES: &'static [&'static str] = &["ci", "warden"];

    /// Convenience constructor for a role-sourced finding -- `FindingSource::role("techlead")`
    /// reads more directly at call sites than `FindingSource::Role("techlead".to_string())`.
    pub fn role(name: impl Into<String>) -> Self {
        FindingSource::Role(name.into())
    }

    pub fn as_str(&self) -> &str {
        match self {
            FindingSource::Role(name) => name,
            FindingSource::Ci => "ci",
            FindingSource::Warden => "warden",
        }
    }

    /// **Issue #73**: roles are open (workflow-defined), so this no longer
    /// rejects an unrecognized name -- there is no fixed set of role names
    /// for this crate to validate against in the first place. Only a blank
    /// name, or one of the two reserved, non-role words (`"ci"`/`"warden"`,
    /// which never legitimately arrive from an agent's own stdout) are
    /// rejected here. The security property this used to provide --
    /// rejecting a role a *particular* invocation isn't entitled to claim --
    /// still holds, just one layer up: [`validate_finding_sources_for_role`]
    /// checks the parsed source against the specific role the caller
    /// expected, at the one call site that knows which step actually
    /// produced this stdout.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "ci" => Ok(FindingSource::Ci),
            "warden" => Ok(FindingSource::Warden),
            other if !other.trim().is_empty() => Ok(FindingSource::Role(other.to_string())),
            _ => Err(CoreError::UnknownFindingSource(raw.to_string())),
        }
    }
}

/// Severity of a finding (`FINDINGS.severity`). Only `Blocking` prevents
/// convergence; `Warning`/`Info` are recorded but never trigger a reboucle
/// on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Blocking,
    Warning,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocking => "blocking",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "blocking" => Ok(Severity::Blocking),
            "warning" => Ok(Severity::Warning),
            "info" => Ok(Severity::Info),
            other => Err(CoreError::UnknownSeverity(other.to_string())),
        }
    }
}

/// A single finding raised by a workflow step's agent during a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub source: FindingSource,
    pub severity: Severity,
    pub file: Option<String>,
    pub description: String,
    pub action: Option<String>,
}

/// Wire schema for a single line of the NDJSON findings stream (see
/// `parse_findings`): one finding per line, no wrapping object/array. Field
/// names/values are attacker-controlled (agent output is untrusted,
/// code-standards.md "Agent Subprocess Protocol") so every value is
/// validated against a closed set here, never passed through as a
/// free-form string.
#[derive(Debug, Deserialize)]
struct RawFinding {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

/// Parses an agent's stdout into a validated list of [`Finding`]s.
///
/// Wire format is **line-delimited JSON (NDJSON)**, per code-standards.md
/// "Agent Subprocess Protocol": "Chaque ligne stdout est une valeur JSON
/// validée (parse + schéma) avant d'atteindre la state machine" — one
/// finding object per non-blank line, not a single JSON blob for the whole
/// output. Blank lines are ignored.
///
/// Any non-blank line that isn't parsable JSON, or whose `severity` isn't a
/// known value (or whose `source` is blank), makes the whole call a
/// [`CoreError::MalformedAgentOutput`] — never a panic. We deliberately
/// don't try to salvage the lines that *did* parse: once a stream has shown
/// itself to produce output that doesn't match the protocol, treating the
/// rest of it as trustworthy would contradict code-standards.md "Ne jamais
/// faire confiance à la sortie d'un agent CLI". The caller (the
/// orchestrator) turns a parse failure into a blocking finding of its own,
/// not a crash of the run.
pub fn parse_findings(agent_stdout: &str) -> Result<Vec<Finding>> {
    agent_stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let raw: RawFinding = serde_json::from_str(line).map_err(|e| {
                CoreError::MalformedAgentOutput(format!("invalid JSON line {line:?}: {e}"))
            })?;
            Ok(Finding {
                source: FindingSource::parse(&raw.source)?,
                severity: Severity::parse(&raw.severity)?,
                file: raw.file,
                description: raw.description,
                action: raw.action,
            })
        })
        .collect()
}

/// Rejects any finding whose `source` isn't the one `role` is entitled to
/// claim -- `parse_findings` only validates that `source` is *some* non-blank
/// value (issue #73: roles are open, so there is no fixed set left to
/// validate against there), not that it's the value the role that actually
/// produced this stdout is allowed to use. Without this, an agent running as
/// one role (whose raw output is untrusted input, code-standards.md "Ne
/// jamais faire confiance à la sortie d'un agent CLI") could forge
/// `source: "warden"` -- impersonating the structural finding only Warden
/// itself may raise (`warden::orchestrator::agent_definition_tampering_finding`,
/// issue #24 review M4) -- or `source: "ci"`, or a *different* step's own
/// role name. That last one is the sharper, non-hypothetical case: a tester
/// could mask a real failure by emitting it as `source: "reviewer"` instead
/// of `"tester"`, since `warden::orchestrator::tester_succeeded` (the signal
/// that gates evidence capture) keys off a `Role("tester")` finding
/// specifically -- reviewer-mislabelled output would sail straight past it.
///
/// `expected_role` is the [`Role`] of the workflow step whose invocation this
/// stdout actually came from -- the caller's own responsibility to supply
/// correctly (`warden::orchestrator::run_finding_agent`); this function has
/// no way to independently know which step ran.
///
/// Rejects the *whole* batch on the first mismatch found (index order),
/// rather than silently dropping just the offending line or silently
/// relabelling it to the expected source -- code-standards.md: "Valider
/// toute entrée externe à la frontière, avant qu'elle n'atteigne la state
/// machine", and once a stream has shown itself willing to misrepresent
/// even one finding's own origin, none of the rest of that same stream's
/// claims are trustworthy either (the same "don't salvage the parts that
/// looked fine" stance [`parse_findings`] already takes for a shape error).
pub fn validate_finding_sources_for_role(findings: &[Finding], expected_role: &Role) -> Result<()> {
    let expected = FindingSource::role(expected_role.as_str());
    for (index, finding) in findings.iter().enumerate() {
        if finding.source != expected {
            return Err(CoreError::MalformedAgentOutput(format!(
                "finding at index {index} claims source {:?}, but the {expected_role} role may \
                 only raise findings with source {:?}",
                finding.source.as_str(),
                expected.as_str(),
            )));
        }
    }
    Ok(())
}

/// Decides the next [`RunState`] once a workflow step's cycle findings are
/// known -- called from [`RunState::RunningStep`], never meaningful
/// elsewhere (crash recovery, `StepCyclesExceeded` -> `Failed`, ... do not go
/// through this function).
///
/// **Issue #73**: replaces the old, two-phase-specific `decide_next_state`
/// (ADR-0014's `max_review_cycles`/`max_test_cycles` split) with a single
/// rule that applies uniformly to *any* gated step in a
/// [`crate::workflow::Workflow`], not just the built-in reviewer/tester
/// pair:
///
/// - A blocking finding whose source is this step's own role (`workflow.steps[step_index].role`),
///   or [`FindingSource::Warden`] (the agent-definition-tampering check,
///   `warden::orchestrator::agent_definition_tampering_finding`, always
///   folded into whichever gated step's findings the caller is currently
///   evaluating) -- reboucles to [`RunState::CoderRunning`] within
///   `max_cycles`, or exhausts to [`RunState::StepCyclesExceeded`] at or past
///   it.
/// - No such blocking finding: the step is clean. If it's the workflow's
///   *last* step, the run converges; otherwise the pipeline advances to the
///   next step.
///
/// For the built-in three-step default workflow, this is exactly ADR-0014's
/// old per-phase rule: `step_index = 1` (reviewer) checks `Role("reviewer")`/
/// `Warden` against `max_review_cycles`, reboucling to `CoderRunning` or
/// advancing to `RunningStep(2)` (never converging directly); `step_index = 2`
/// (tester) checks `Role("tester")` against `max_test_cycles`, reboucling or
/// converging (it's the last step) -- issue #73's strict retro-compat
/// requirement, made a property of this one rule rather than two separate
/// ones.
///
/// `current_cycle` is the 1-based count of this step's own invocations made
/// so far *including this one* -- the caller's responsibility to track per
/// step, exactly like `review_cycle`/`test_cycle` used to be tracked
/// separately for the two hardcoded phases.
pub fn decide_next_state_for_step(
    findings: &[Finding],
    workflow: &Workflow,
    step_index: u32,
    current_cycle: u32,
    max_cycles: u32,
) -> RunState {
    let step_role = &workflow.steps[step_index as usize].role;
    let expected_source = FindingSource::role(step_role.as_str());
    let blocking = findings.iter().any(|f| {
        f.severity == Severity::Blocking
            && (f.source == expected_source || f.source == FindingSource::Warden)
    });

    if !blocking {
        return if workflow.is_last_step(step_index) {
            RunState::Converged
        } else {
            RunState::RunningStep(step_index + 1)
        };
    }

    if current_cycle >= max_cycles {
        RunState::StepCyclesExceeded(step_index)
    } else {
        RunState::CoderRunning
    }
}

/// Coarse result of `warden-gated`'s CI watcher polling loop (issue #5),
/// passed in by the caller rather than re-derived here -- this module only
/// ever decides which [`RunState`] a given outcome implies, exactly like
/// [`decide_next_state_for_step`] does for a gated step's findings.
/// Deliberately not the same type as `warden-gated::ci_watcher::WatchOutcome`:
/// that one carries the full per-check `Finding` list for human-readable
/// reporting, while this crate only needs the coarse signal to pick a
/// `RunState` (`warden-gated` must never depend on `warden`, and
/// `warden-core` must never depend on `warden-gated` -- ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiOutcome {
    Merged,
    ChecksPassed,
    Closed,
    ChecksFailed,
    TimedOut,
}

/// Decides the next [`RunState`] once a run's CI watch (issue #5) reaches a
/// terminal outcome. Only meaningful from [`RunState::AwaitingCi`], whose
/// legal next states are exactly `Done` / `CoderRunning` / `Failed`
/// ([`RunState::validate_transition`]) -- notably never
/// `StepCyclesExceeded`, unlike [`decide_next_state_for_step`]'s per-step
/// exhaustion case: a CI failure that exhausts the cycle budget lands on
/// `Failed` here instead. The caller (`warden::orchestrator`) checks this
/// against the review budget -- a CI reboucle re-enters the loop at
/// `CoderRunning` -> `RunningStep(1)`, exactly like any other reboucle to the
/// coder.
///
/// - `Merged` / `ChecksPassed`: `Done`. Warden's own responsibility ends
///   once CI is green -- actually merging the PR is deliberately never
///   automatic (issue #5: "aucun merge automatique n'est déclenché par
///   Warden"), so `ChecksPassed` reaches the same terminal `RunState` as an
///   already-`Merged` PR; the merge itself is left entirely to a human.
/// - `Closed` (closed without merging) / `TimedOut`: `Failed` -- nothing
///   further for Warden to do, and neither represents a working result.
/// - `ChecksFailed`, with cycles remaining: `CoderRunning` (reboucle vers le
///   coder). At the cycle budget: `Failed`.
pub fn decide_next_state_after_ci(
    outcome: CiOutcome,
    current_cycle: u32,
    max_cycles: u32,
) -> RunState {
    match outcome {
        CiOutcome::Merged | CiOutcome::ChecksPassed => RunState::Done,
        CiOutcome::Closed | CiOutcome::TimedOut => RunState::Failed,
        CiOutcome::ChecksFailed => {
            if current_cycle >= max_cycles {
                RunState::Failed
            } else {
                RunState::CoderRunning
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::Workflow;

    fn default_workflow() -> Workflow {
        Workflow::builtin_default()
    }

    fn reviewer_role() -> Role {
        Role::new("reviewer").unwrap()
    }

    fn tester_role() -> Role {
        Role::new("tester").unwrap()
    }

    fn blocking_finding() -> Finding {
        Finding {
            source: FindingSource::role("reviewer"),
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "unchecked unwrap".to_string(),
            action: Some("use ? instead".to_string()),
        }
    }

    /// Issue #24 review M4: the tampering finding
    /// (`warden::orchestrator::agent_definition_tampering_finding`) is
    /// `FindingSource::Warden`, folded in alongside the reviewer's own
    /// findings -- must be charged to the review step's own budget exactly
    /// like a real reviewer finding (decision #37 Q1's imputation rule
    /// applies to it too, not only to role-sourced findings).
    fn tampering_finding() -> Finding {
        Finding {
            source: FindingSource::Warden,
            severity: Severity::Blocking,
            file: Some(".warden/agents/coder.md".to_string()),
            description: "agent definition tampering".to_string(),
            action: None,
        }
    }

    fn tester_blocking_finding() -> Finding {
        Finding {
            source: FindingSource::role("tester"),
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "test fails".to_string(),
            action: Some("fix the panic".to_string()),
        }
    }

    fn info_finding() -> Finding {
        Finding {
            source: FindingSource::role("tester"),
            severity: Severity::Info,
            file: None,
            description: "consider adding a doc comment".to_string(),
            action: None,
        }
    }

    #[test]
    fn no_findings_converges_on_the_last_step() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 2, 1, 5),
            RunState::Converged
        );
    }

    #[test]
    fn no_findings_on_a_non_last_step_advances_to_the_next_step() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 1, 1, 5),
            RunState::RunningStep(2)
        );
    }

    #[test]
    fn only_non_blocking_findings_advances() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[info_finding()], &workflow, 2, 1, 5),
            RunState::Converged
        );
    }

    #[test]
    fn blocking_finding_on_the_reviewer_step_within_budget_reboucles_to_coder() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[blocking_finding()], &workflow, 1, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_finding_on_the_reviewer_step_at_budget_exceeds_its_own_step_cycles() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[blocking_finding()], &workflow, 1, 5, 5),
            RunState::StepCyclesExceeded(1)
        );
    }

    #[test]
    fn blocking_finding_on_the_reviewer_step_past_budget_exceeds_its_own_step_cycles() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[blocking_finding()], &workflow, 1, 6, 5),
            RunState::StepCyclesExceeded(1)
        );
    }

    /// The tampering finding (`FindingSource::Warden`) is charged to
    /// whichever step's own budget the caller is currently evaluating (the
    /// review step's, in the built-in default workflow) exactly like a
    /// role-sourced one.
    #[test]
    fn tampering_finding_is_charged_to_the_step_it_is_folded_into() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tampering_finding()], &workflow, 1, 5, 5),
            RunState::StepCyclesExceeded(1)
        );
    }

    /// A tester finding on the review step's own evaluation never blocks it
    /// (it's a different step's own role) -- proving `decide_next_state_for_step`
    /// really does key off the *specific* step's own role, not "any finding
    /// at all".
    #[test]
    fn a_different_steps_finding_never_blocks_this_step() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 1, 1, 5),
            RunState::RunningStep(2),
            "a tester-sourced finding must not block the reviewer step's own decision"
        );
    }

    /// Decision #37 Q1, the core acceptance criterion of issue #43 (now
    /// generalized, issue #73): a blocking tester finding reboucles/exhausts
    /// against the *tester step's own* budget, never the reviewer step's --
    /// even when the reviewer step's own budget is itself already exhausted,
    /// since a tester finding only ever appears when the caller is
    /// evaluating the tester step, on a cycle whose review already came back
    /// clean this same cycle (`warden::orchestrator::run_convergence_loop`'s
    /// gate, issue #41).
    #[test]
    fn blocking_tester_finding_within_its_own_budget_reboucles_to_coder() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 2, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn blocking_tester_finding_at_its_own_budget_exceeds_step_cycles_not_the_reviewers() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 2, 3, 3),
            RunState::StepCyclesExceeded(2)
        );
    }

    #[test]
    fn blocking_tester_finding_past_its_own_budget_exceeds_step_cycles() {
        let workflow = default_workflow();
        assert_eq!(
            decide_next_state_for_step(&[tester_blocking_finding()], &workflow, 2, 4, 3),
            RunState::StepCyclesExceeded(2)
        );
    }

    #[test]
    fn decide_next_state_mixed_severities_still_reboucles_on_any_blocking() {
        let workflow = default_workflow();
        let findings = vec![info_finding(), blocking_finding()];
        assert_eq!(
            decide_next_state_for_step(&findings, &workflow, 1, 1, 5),
            RunState::CoderRunning
        );
    }

    /// Issue #73's own new-role demonstration: a fourth, custom step
    /// (`techlead`) whose findings aggregate in the loop exactly like the
    /// reviewer's/tester's already do -- a blocking finding attributed to it
    /// reboucles/exhausts against its own budget, and a clean cycle on it (as
    /// the workflow's last step) converges the run.
    #[test]
    fn a_custom_role_beyond_the_default_pipeline_aggregates_like_any_other_step() {
        let yaml = r#"
name: with-techlead
steps:
  - role: coder
    agent: coder
  - role: reviewer
    agent: code-reviewer
    gate: loop-until-clean
  - role: tester
    agent: test-runner
    gate: loop-until-clean
  - role: techlead
    agent: techlead
    gate: loop-until-clean
"#;
        let workflow = Workflow::parse_yaml(yaml).unwrap();

        let techlead_blocking = Finding {
            source: FindingSource::role("techlead"),
            severity: Severity::Blocking,
            file: None,
            description: "architecture concern".to_string(),
            action: Some("reconsider the approach".to_string()),
        };
        assert_eq!(
            decide_next_state_for_step(&[techlead_blocking], &workflow, 3, 1, 5),
            RunState::CoderRunning
        );
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 3, 1, 5),
            RunState::Converged,
            "techlead is this workflow's last step, so a clean cycle converges"
        );
        assert_eq!(
            decide_next_state_for_step(&[], &workflow, 2, 1, 5),
            RunState::RunningStep(3),
            "tester is no longer the last step in this workflow, so a clean cycle advances \
             instead of converging"
        );
    }

    #[test]
    fn parse_findings_empty_stdout_is_no_findings() {
        assert_eq!(parse_findings("").unwrap(), Vec::new());
        assert_eq!(parse_findings("   \n").unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_happy_path() {
        // NDJSON wire format (code-standards.md "Agent Subprocess
        // Protocol"): one finding object per line, no wrapping array.
        let stdout = r#"{"source":"tester","severity":"blocking","file":"src/main.rs","description":"test fails","action":"fix panic"}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, FindingSource::role("tester"));
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert_eq!(findings[0].file.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn parse_findings_multiple_lines_yield_multiple_findings() {
        // The defining property of NDJSON: each line is an independent
        // finding, so a reviewer raising several issues in one invocation
        // just emits several lines.
        let stdout = concat!(
            r#"{"source":"reviewer","severity":"blocking","description":"issue one"}"#,
            "\n",
            r#"{"source":"reviewer","severity":"warning","description":"issue two"}"#,
            "\n",
        );
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].description, "issue one");
        assert_eq!(findings[1].description, "issue two");
    }

    #[test]
    fn parse_findings_rejects_malformed_json() {
        assert!(parse_findings("not json").is_err());
    }

    #[test]
    fn parse_findings_rejects_unknown_severity() {
        let stdout = r#"{"source":"reviewer","severity":"catastrophic","description":"x"}"#;
        assert_eq!(
            parse_findings(stdout),
            Err(CoreError::UnknownSeverity("catastrophic".to_string()))
        );
    }

    /// Issue #73: roles are now open (workflow-defined), so an arbitrary
    /// non-blank `source` string is no longer rejected at this parse layer
    /// -- there is no fixed set of role names left for this crate to
    /// validate against (a `.warden/workflow.yaml` could legitimately name a
    /// step `"ghost"`). Only a *blank* source is still rejected -- see
    /// `parse_findings_rejects_a_blank_source` below. The security property
    /// this test used to pin now lives in `validate_finding_sources_for_role`,
    /// which checks a source against the *specific* role the caller expected
    /// (see that function's own tests).
    #[test]
    fn parse_findings_accepts_any_non_blank_source_as_an_open_role() {
        let stdout = r#"{"source":"ghost","severity":"info","description":"x"}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings[0].source, FindingSource::role("ghost"));
    }

    #[test]
    fn parse_findings_rejects_a_blank_source() {
        let stdout = r#"{"source":"   ","severity":"info","description":"x"}"#;
        assert!(matches!(
            parse_findings(stdout),
            Err(CoreError::UnknownFindingSource(_))
        ));
    }

    #[test]
    fn parse_findings_blank_lines_between_findings_are_ignored() {
        // Reconciled for the NDJSON protocol (M3): the original intent of
        // this test — "an explicitly empty/no-content case still yields no
        // findings" — now maps onto blank-line handling rather than an
        // empty `findings` array, since there's no wrapping array anymore.
        let stdout = "\n   \n\n";
        assert_eq!(parse_findings(stdout).unwrap(), Vec::new());
    }

    #[test]
    fn parse_findings_rejects_missing_required_field() {
        // `description` is required by the wire protocol; an agent that
        // omits it (buggy or malicious output) must be a typed parse error,
        // not a panic or a silently-defaulted empty string.
        let stdout = r#"{"source":"reviewer","severity":"blocking"}"#;
        assert!(matches!(
            parse_findings(stdout),
            Err(CoreError::MalformedAgentOutput(_))
        ));
    }

    #[test]
    fn parse_findings_ignores_unknown_extra_fields_for_forward_compat() {
        let stdout = r#"{"source":"tester","severity":"info","description":"x","confidence":0.9}"#;
        let findings = parse_findings(stdout).unwrap();
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn parse_findings_rejects_trailing_noise_after_the_json_object() {
        // Reconciled for the NDJSON protocol (M3): a valid finding on the
        // first line followed by a stray non-JSON log line is exactly the
        // shape of "trailing noise" a real agent CLI can leak onto stdout.
        // We deliberately don't salvage the line(s) that did parse — a
        // stream that has shown itself to violate the protocol is treated
        // as untrustworthy in full, not partially recovered.
        let stdout = "{\"source\":\"reviewer\",\"severity\":\"info\",\"description\":\"ok\"}\nDEBUG: agent finished in 1.2s\n";
        assert!(matches!(
            parse_findings(stdout),
            Err(CoreError::MalformedAgentOutput(_))
        ));
    }

    #[test]
    fn parse_findings_rejects_a_top_level_json_array_instead_of_object() {
        assert!(parse_findings("[]").is_err());
    }

    // ---- validate_finding_sources_for_role (issue #24 review, cycle 2,
    // MAJOR 2; generalized to open roles, issue #73) ---------------------

    fn finding_with_source(source: FindingSource) -> Finding {
        Finding {
            source,
            severity: Severity::Blocking,
            file: None,
            description: "x".to_string(),
            action: None,
        }
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_a_reviewer_finding_with_the_reviewer_source() {
        let findings = vec![finding_with_source(FindingSource::role("reviewer"))];
        assert!(validate_finding_sources_for_role(&findings, &reviewer_role()).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_a_tester_finding_with_the_tester_source() {
        let findings = vec![finding_with_source(FindingSource::role("tester"))];
        assert!(validate_finding_sources_for_role(&findings, &tester_role()).is_ok());
    }

    /// Issue #73: the same rule applies unchanged to a custom role beyond
    /// the built-in three.
    #[test]
    fn validate_finding_sources_for_role_accepts_a_custom_roles_own_finding() {
        let role = Role::new("techlead").unwrap();
        let findings = vec![finding_with_source(FindingSource::role("techlead"))];
        assert!(validate_finding_sources_for_role(&findings, &role).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_accepts_no_findings_at_all() {
        assert!(validate_finding_sources_for_role(&[], &reviewer_role()).is_ok());
        assert!(validate_finding_sources_for_role(&[], &tester_role()).is_ok());
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_reviewer_finding_claiming_the_warden_source() {
        let findings = vec![finding_with_source(FindingSource::Warden)];
        let error = validate_finding_sources_for_role(&findings, &reviewer_role()).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    #[test]
    fn validate_finding_sources_for_role_rejects_a_reviewer_finding_claiming_the_ci_source() {
        let findings = vec![finding_with_source(FindingSource::Ci)];
        assert!(validate_finding_sources_for_role(&findings, &reviewer_role()).is_err());
    }

    /// The sibling-role impersonation case: a tester claiming the
    /// reviewer's own source (issue #24 review Minor 2, `tester_succeeded`
    /// trusting an agent-controlled `source`).
    #[test]
    fn validate_finding_sources_for_role_rejects_a_tester_finding_claiming_the_reviewer_source() {
        let findings = vec![finding_with_source(FindingSource::role("reviewer"))];
        let error = validate_finding_sources_for_role(&findings, &tester_role()).unwrap_err();
        assert!(matches!(error, CoreError::MalformedAgentOutput(_)));
    }

    /// The whole batch is rejected on the first mismatch, even when an
    /// earlier finding in the same batch was legitimate -- no salvaging
    /// the parts that looked fine, mirroring `parse_findings`' own stance
    /// on a shape error.
    #[test]
    fn validate_finding_sources_for_role_rejects_the_whole_batch_on_one_bad_finding() {
        let findings = vec![
            finding_with_source(FindingSource::role("reviewer")),
            finding_with_source(FindingSource::Warden),
        ];
        assert!(validate_finding_sources_for_role(&findings, &reviewer_role()).is_err());
    }

    // ---- FindingSource::Ci -------------------------------------------------

    #[test]
    fn ci_finding_source_round_trips_through_its_string_form() {
        assert_eq!(FindingSource::Ci.as_str(), "ci");
        assert_eq!(FindingSource::parse("ci").unwrap(), FindingSource::Ci);
    }

    // ---- FindingSource::Warden (issue #24 review, M4) ----------------------

    #[test]
    fn warden_finding_source_round_trips_through_its_string_form() {
        assert_eq!(FindingSource::Warden.as_str(), "warden");
        assert_eq!(
            FindingSource::parse("warden").unwrap(),
            FindingSource::Warden
        );
    }

    // ---- decide_next_state_after_ci (issue #5) -----------------------------

    #[test]
    fn merged_and_checks_passed_both_reach_done() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::Merged, 1, 5),
            RunState::Done
        );
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksPassed, 1, 5),
            RunState::Done
        );
    }

    #[test]
    fn closed_without_merging_and_timed_out_both_fail_the_run() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::Closed, 1, 5),
            RunState::Failed
        );
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::TimedOut, 1, 5),
            RunState::Failed
        );
    }

    #[test]
    fn checks_failed_reboucles_to_coder_within_cycle_budget() {
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksFailed, 1, 5),
            RunState::CoderRunning
        );
    }

    #[test]
    fn checks_failed_at_cycle_budget_fails_the_run_not_a_step_cycles_exceeded_state() {
        // AwaitingCi's only legal next states are Done/CoderRunning/Failed
        // (state.rs) -- `StepCyclesExceeded` is never reachable from here,
        // unlike `decide_next_state_for_step`'s own exhaustion case.
        assert_eq!(
            decide_next_state_after_ci(CiOutcome::ChecksFailed, 5, 5),
            RunState::Failed
        );
    }

    #[test]
    fn every_decide_next_state_after_ci_outcome_is_a_legal_awaiting_ci_transition() {
        for outcome in [
            CiOutcome::Merged,
            CiOutcome::ChecksPassed,
            CiOutcome::Closed,
            CiOutcome::ChecksFailed,
            CiOutcome::TimedOut,
        ] {
            let next = decide_next_state_after_ci(outcome, 1, 5);
            assert!(
                RunState::AwaitingCi.validate_transition(next, 3).is_ok(),
                "{outcome:?} -> {next:?} is not a legal AwaitingCi transition"
            );
        }
    }
}
