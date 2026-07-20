//! Wire form of the JSON payload `warden` feeds an agent subprocess over its
//! stdin (ADR-0012, issue #20 Scope B): the run intent for the coder, or the
//! target commit/diff/prior-cycle findings for the reviewer/tester, plus the
//! agent's own role in every payload. This is the channel
//! code-standards.md's Agent Subprocess Protocol already sanctions
//! ("Échange JSON en streaming sur stdin/stdout"), but nothing actually fed
//! one until this issue -- previously the coder received no warden-managed
//! context at all, and the reviewer/tester received none either.
//!
//! Pure/serializable shape only -- the I/O (writing to a child's stdin,
//! closing it) lives in `warden_sandbox::LocalSandbox` (issue #50; this
//! crate has no dependency on it, and never should), mirroring the
//! `warden_core::CiResultMessage` / `warden::ci_channel` split. Constructed
//! here with the same "wire struct + typed constructor + validated parse"
//! convention as `ci_channel`/`evidence_wire`: `AgentInputMessage` is never
//! built by hand from raw strings, and any round-trip through JSON is
//! reparsed and validated, never partially trusted.

use serde::{Deserialize, Serialize};

use crate::convergence::{Finding, FindingSource, Severity};
use crate::error::{CoreError, Result};
use crate::state::AgentRole;

/// Current version of the agent input payload. `parse_agent_input_message`
/// rejects any other value outright rather than guessing at forward/backward
/// compatibility -- bump this if the shape changes in a way an agent-side
/// consumer must branch on.
///
/// **2** (ADR-0013, issue #22): every payload now also carries the
/// `system_prompt` from the role's markdown definition, and a coder payload
/// may carry `findings` (the ones it is being asked to fix on a reboucle).
/// Both are breaking for an agent-side consumer: a v1 agent would drop the
/// prompt on the floor.
///
/// **3** (issue #40, ADR-0012/0013 amendment): every payload now also
/// carries a `scope` ("full" or "correctif", see [`ReviewScope`]) --
/// `"correctif"` is only ever set on a reviewer payload, and narrows `diff`/
/// `findings` from this cycle's full context down to a single correctif
/// under re-review plus the findings that prompted it (decision #37 Q2).
/// Breaking for the same reason 2 was: a v2 agent has never seen this field
/// and has no way to infer "full" from its own absence -- retro-compat is
/// handled the same way every prior bump was, by rejecting the mismatched
/// version outright rather than guessing (see `parse_agent_input_message`),
/// not by accepting both shapes.
pub const AGENT_INPUT_VERSION: u32 = 3;

/// Appended to [`AgentInputMessage::diff`] when `warden` truncated it at its
/// size cap (`warden::orchestrator::MAX_DIFF_BYTES`) before handing it to a
/// reviewer/tester (M1, issue #20 review). Lives here rather than as a
/// private const in `warden::orchestrator` (fix cycle 2, issue #20 review,
/// BUG 4) because this is where an agent-side consumer of the wire contract
/// actually looks: a marker an agent needs to detect but that isn't
/// documented on the field it appears in isn't part of the contract at all,
/// silently cutting the diff without a discoverable marker would be its own
/// silent fallback.
pub const DIFF_TRUNCATED_MARKER: &str = "\n\n[warden: diff truncated at the 8 MiB payload cap]\n";

/// Wire shape of one [`Finding`] riding inside the agent input payload --
/// mirrors `ci_channel::CiFindingWire`'s convention (the already-validated
/// `as_str()` string form for `source`/`severity`, re-validated back into the
/// real enums at receipt): `Finding` itself doesn't derive
/// `Serialize`/`Deserialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AgentFindingWire {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

impl AgentFindingWire {
    fn from_finding(finding: &Finding) -> Self {
        Self {
            source: finding.source.as_str().to_string(),
            severity: finding.severity.as_str().to_string(),
            file: finding.file.clone(),
            description: finding.description.clone(),
            action: finding.action.clone(),
        }
    }

    fn into_finding(self) -> Result<Finding> {
        Ok(Finding {
            source: FindingSource::parse(&self.source)?,
            severity: Severity::parse(&self.severity)?,
            file: self.file,
            description: self.description,
            action: self.action,
        })
    }
}

/// Whether a reviewer/tester `diff`/`findings` pair describes this cycle's
/// full context, or is narrowed to a single correctif under re-review (issue
/// #40, ADR-0012/0013 amendment, decision #37 Q2: "re-review scopée").
///
/// `Full` is the only mode that existed before issue #40, and the only one
/// ever set for `AgentRole::Coder`/`AgentRole::Tester` -- `Correctif` is
/// reviewer-only, enforced by [`AgentInputMessage::for_scoped_review`] at
/// construction and by [`parse_agent_input_message`] on the read side (the
/// same "constructor invariant == parse invariant" convention as
/// `coder_only_violation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewScope {
    /// This cycle's full diff/findings context -- the only mode before
    /// issue #40.
    Full,
    /// `diff`/`findings` are narrowed to a single correctif -- the fix a
    /// coder produced in response to specific findings -- plus the findings
    /// that prompted it, rather than this cycle's whole context. Tells the
    /// reviewer to "look only at this correctif" instead of re-reviewing
    /// everything.
    Correctif,
}

impl ReviewScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewScope::Full => "full",
            ReviewScope::Correctif => "correctif",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "full" => Ok(ReviewScope::Full),
            "correctif" => Ok(ReviewScope::Correctif),
            other => Err(CoreError::MalformedAgentInput(format!(
                "unknown review scope {other:?} (expected \"full\" or \"correctif\")"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentInputWire {
    version: u32,
    role: String,
    /// ADR-0013: required for every role. A payload without one is
    /// malformed, not a payload with an empty prompt -- serde's own
    /// "missing field" error is surfaced as `MalformedAgentInput`.
    system_prompt: String,
    intent: Option<String>,
    target_commit: Option<String>,
    diff: Option<String>,
    findings: Vec<AgentFindingWire>,
    /// Issue #40: required (not defaulted) for the same reason
    /// `system_prompt` became required at v2 -- see `AGENT_INPUT_VERSION`.
    scope: String,
}

/// The payload `warden` feeds an agent subprocess over stdin (ADR-0012): the
/// run intent for the coder, or the target commit/diff/prior findings for
/// the reviewer/tester -- never a mix of both, enforced by the two
/// constructors below rather than left to callers to assemble field-by-field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInputMessage {
    pub role: AgentRole,
    /// The role's system prompt, from the markdown agent definition this
    /// invocation was built from (ADR-0013, issue #22 Scope A:
    /// `AgentDefinition::system_prompt`).
    ///
    /// It rides *here*, in the per-invocation stdin payload, rather than in
    /// argv or a temp prompt file: ADR-0012 already rejected both as
    /// channels for warden-managed agent text (argv leaks arbitrary
    /// multi-line text into `ps`, a temp file adds disk state, a cleanup
    /// race, and a permissions surface), and nothing about a system prompt
    /// weakens those objections. Re-sent every invocation even though it is
    /// static per role -- an accepted, harmless cost (ADR-0013 / Q2), not an
    /// oversight.
    pub system_prompt: String,
    pub intent: Option<String>,
    pub target_commit: Option<String>,
    /// Reviewer/tester only (`None` for coder input, [`Self::for_coder`]):
    /// `git diff base..target` for this cycle's changes. An empty string
    /// is a legitimate value in its own right (a cycle whose coder
    /// committed no changes), distinct from `None`. May be truncated at
    /// `warden::orchestrator::MAX_DIFF_BYTES` -- a truncated diff has
    /// [`DIFF_TRUNCATED_MARKER`] appended, so a reviewer/tester agent can
    /// tell a truncated diff from a genuinely small one rather than acting
    /// on a silently incomplete payload.
    pub diff: Option<String>,
    pub findings: Vec<Finding>,
    /// `Full` for a coder payload and for a full reviewer/tester pass (the
    /// only mode before issue #40); `Correctif` only for a scoped reviewer
    /// re-review (decision #37 Q2), set by [`Self::for_scoped_review`] --
    /// see [`ReviewScope`]'s own docs.
    pub scope: ReviewScope,
}

impl AgentInputMessage {
    /// Coder input: the run intent, plus the findings that triggered this
    /// cycle -- the ones the coder is being asked to fix (ADR-0013 / A2,
    /// issue #22). Empty on a run's first cycle.
    ///
    /// Carries **no** `target_commit`/`diff`, unlike
    /// [`Self::for_finding_agent`], and that asymmetry is deliberate: the
    /// coder already owns a worktree checked out at that very commit and can
    /// run `git diff` itself, so shipping it an up-to-8MiB diff it can read
    /// off its own disk would be redundant, and "target commit" is ambiguous
    /// for the role that is about to *produce* the next one.
    ///
    /// Rejects a blank (empty or all-whitespace) `intent` or `system_prompt`
    /// with the same rigor [`parse_agent_input_message`] applies on the read
    /// side (M2, issue #20 review) -- without this, `to_json` could hand
    /// `parse_agent_input_message`'s own caller a payload the parser refuses
    /// to accept, since it enforces exactly this invariant.
    pub fn for_coder(
        system_prompt: impl Into<String>,
        intent: impl Into<String>,
        findings: Vec<Finding>,
    ) -> Result<Self> {
        let system_prompt = validate_system_prompt(AgentRole::Coder, system_prompt.into())?;
        let intent = intent.into();
        if intent.trim().is_empty() {
            return Err(CoreError::MalformedAgentInput(
                "coder input intent must not be blank".to_string(),
            ));
        }
        Ok(Self {
            role: AgentRole::Coder,
            system_prompt,
            intent: Some(intent),
            target_commit: None,
            diff: None,
            findings,
            scope: ReviewScope::Full,
        })
    }

    /// Reviewer/tester input: the commit under review, the diff this cycle's
    /// coder introduced against the cycle's starting commit, and the
    /// findings that triggered this cycle (including CI findings on a
    /// post-convergence reboucle, ADR-0011) -- empty on a run's first cycle.
    ///
    /// Rejects a blank `target_commit` or `system_prompt` with the same
    /// rigor [`parse_agent_input_message`] applies on the read side (M2,
    /// issue #20 review). `diff` is deliberately not validated the same way
    /// -- an empty diff (a cycle whose coder committed no changes) is a
    /// legitimate value, mirrored by the parser accepting an absent `diff`
    /// as `""`.
    pub fn for_finding_agent(
        role: AgentRole,
        system_prompt: impl Into<String>,
        target_commit: impl Into<String>,
        diff: impl Into<String>,
        findings: Vec<Finding>,
    ) -> Result<Self> {
        if role == AgentRole::Coder {
            return Err(CoreError::MalformedAgentInput(
                "for_finding_agent must be called with Reviewer or Tester, not Coder".to_string(),
            ));
        }
        let system_prompt = validate_system_prompt(role, system_prompt.into())?;
        let target_commit = target_commit.into();
        if target_commit.trim().is_empty() {
            return Err(CoreError::MalformedAgentInput(format!(
                "{} input target_commit must not be blank",
                role.as_str()
            )));
        }
        Ok(Self {
            role,
            system_prompt,
            intent: None,
            target_commit: Some(target_commit),
            diff: Some(diff.into()),
            findings,
            scope: ReviewScope::Full,
        })
    }

    /// Scoped reviewer re-review (issue #40, ADR-0012/0013 amendment,
    /// decision #37 Q2: "re-review scopée"): same wire shape as
    /// [`Self::for_finding_agent`], but `diff`/`findings` are deliberately
    /// **not** this cycle's full context -- `diff` is the single correctif
    /// under re-review, and `findings` is only the findings that prompted
    /// it, telling the reviewer to look only at this correctif rather than
    /// re-reviewing everything. Reviewer-only, exactly like the mode it
    /// implements -- delegates to [`Self::for_finding_agent`] (which rejects
    /// `AgentRole::Coder`) and hardcodes [`AgentRole::Reviewer`], so it can
    /// never be constructed for any other role.
    pub fn for_scoped_review(
        system_prompt: impl Into<String>,
        target_commit: impl Into<String>,
        correctif_diff: impl Into<String>,
        originating_findings: Vec<Finding>,
    ) -> Result<Self> {
        let mut message = Self::for_finding_agent(
            AgentRole::Reviewer,
            system_prompt,
            target_commit,
            correctif_diff,
            originating_findings,
        )?;
        message.scope = ReviewScope::Correctif;
        Ok(message)
    }

    /// Serializes to the exact wire form [`parse_agent_input_message`] parses
    /// back -- one JSON object written to the agent's stdin, then the write
    /// half is closed (`warden_sandbox::LocalSandbox::execute`, issue #50),
    /// never left open waiting for more input.
    pub fn to_json(&self) -> Result<String> {
        let wire = AgentInputWire {
            version: AGENT_INPUT_VERSION,
            role: self.role.as_str().to_string(),
            system_prompt: self.system_prompt.clone(),
            intent: self.intent.clone(),
            target_commit: self.target_commit.clone(),
            diff: self.diff.clone(),
            findings: self
                .findings
                .iter()
                .map(AgentFindingWire::from_finding)
                .collect(),
            scope: self.scope.as_str().to_string(),
        };
        serde_json::to_string(&wire)
            .map_err(|error| CoreError::MalformedAgentInput(error.to_string()))
    }
}

/// Parses one agent-input JSON payload, with the same rigor as
/// `parse_ci_result_message`/`parse_findings`: malformed JSON, an unsupported
/// `version`, an unknown `role`, an unparsable embedded finding, or a
/// role/field combination that violates the invariant `for_coder`/
/// `for_finding_agent` enforce at construction (e.g. a `coder` payload
/// missing `intent`, or a `reviewer`/`tester` payload missing
/// `target_commit`) are all typed errors -- never silently defaulted
/// (code-standards.md: "valider toute entrée externe ... à la frontière").
pub fn parse_agent_input_message(raw: &str) -> Result<AgentInputMessage> {
    let wire: AgentInputWire = serde_json::from_str(raw)
        .map_err(|error| CoreError::MalformedAgentInput(error.to_string()))?;

    if wire.version != AGENT_INPUT_VERSION {
        return Err(CoreError::MalformedAgentInput(format!(
            "unsupported agent input version {} (expected {AGENT_INPUT_VERSION})",
            wire.version
        )));
    }

    let role = AgentRole::parse(&wire.role)?;
    // ADR-0013: required for every role, so validated before the per-role
    // branching rather than duplicated inside each arm.
    let system_prompt = validate_system_prompt(role, wire.system_prompt)?;
    let findings = wire
        .findings
        .into_iter()
        .map(AgentFindingWire::into_finding)
        .collect::<Result<Vec<_>>>()?;
    let scope = ReviewScope::parse(&wire.scope)?;
    // Issue #40: `Correctif` is an invariant `for_scoped_review` enforces by
    // construction (it hardcodes `AgentRole::Reviewer`), so the parse side
    // must enforce it too -- the same "constructor invariant == parse
    // invariant" rule `coder_only_violation` already applies to
    // `target_commit`/`diff` on a coder payload.
    if scope == ReviewScope::Correctif && role != AgentRole::Reviewer {
        return Err(CoreError::MalformedAgentInput(format!(
            "{} input must not carry a \"correctif\" scope (only the reviewer can be scoped)",
            role.as_str()
        )));
    }

    match role {
        AgentRole::Coder => {
            // A2 (ADR-0013): the coder branch is validated with exactly the
            // rigor it always was -- a coder payload still must carry a
            // non-blank intent. `findings` is the only invariant A2 relaxed:
            // they may now be present (the reboucle's fix list) *or* empty (a
            // run's first cycle).
            let intent = wire
                .intent
                .filter(|intent| !intent.trim().is_empty())
                .ok_or_else(|| {
                    CoreError::MalformedAgentInput(
                        "coder input is missing a non-blank intent".to_string(),
                    )
                })?;
            // "Intent + findings only" is an invariant `for_coder` enforces
            // (it hardcodes both to `None`), so the parse side must enforce
            // it too rather than quietly discard whatever a payload carried
            // -- dropping data the sender meant to send is the silent
            // fallback code-standards.md forbids, and the exact
            // construction/parse asymmetry ADR-0012's M2 amendment exists to
            // prevent. Inherited from v1, where the coder arm hardcoded the
            // same `None`s but no constructor contradicted them; A2 is what
            // turned it into a real invariant with a real violation.
            if let Some(field) = coder_only_violation(&wire.target_commit, &wire.diff) {
                return Err(CoreError::MalformedAgentInput(format!(
                    "coder input must not carry a {field} (the coder's own worktree is already \
                     checked out at that commit; it runs `git diff` itself)"
                )));
            }
            Ok(AgentInputMessage {
                role,
                system_prompt,
                intent: Some(intent),
                target_commit: None,
                diff: None,
                findings,
                scope,
            })
        }
        AgentRole::Reviewer | AgentRole::Tester => {
            let target_commit = wire
                .target_commit
                .filter(|commit| !commit.trim().is_empty())
                .ok_or_else(|| {
                    CoreError::MalformedAgentInput(format!(
                        "{} input is missing a non-blank target_commit",
                        role.as_str()
                    ))
                })?;
            Ok(AgentInputMessage {
                role,
                system_prompt,
                intent: None,
                target_commit: Some(target_commit),
                // Absent `diff` is treated as "no diff" (empty string)
                // rather than an error -- a cycle whose coder made no
                // changes has a legitimately empty diff to report.
                diff: Some(wire.diff.unwrap_or_default()),
                findings,
                scope,
            })
        }
    }
}

/// Names the reviewer/tester-only field a coder payload wrongly carries, if
/// any (A2, ADR-0013) -- so the error can say *which* field was rejected
/// rather than just that something was wrong.
fn coder_only_violation(
    target_commit: &Option<String>,
    diff: &Option<String>,
) -> Option<&'static str> {
    match (target_commit, diff) {
        (Some(_), _) => Some("target_commit"),
        (_, Some(_)) => Some("diff"),
        _ => None,
    }
}

/// Shared blank-check for the one field every role's payload must carry
/// (ADR-0013): a payload whose `system_prompt` says nothing tells the agent
/// nothing about what it is -- the same typed error on both the construction
/// and the parse side, never an empty default.
fn validate_system_prompt(role: AgentRole, system_prompt: String) -> Result<String> {
    if system_prompt.trim().is_empty() {
        return Err(CoreError::MalformedAgentInput(format!(
            "{} input system_prompt must not be blank",
            role.as_str()
        )));
    }
    Ok(system_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM_PROMPT: &str = "You are Warden's agent.";

    fn sample_finding() -> Finding {
        Finding {
            source: FindingSource::Ci,
            severity: Severity::Blocking,
            file: Some("src/lib.rs".to_string()),
            description: "build failed".to_string(),
            action: Some("fix the build".to_string()),
        }
    }

    #[test]
    fn coder_input_round_trips_through_json() {
        let message =
            AgentInputMessage::for_coder(SYSTEM_PROMPT, "implement the thing", Vec::new()).unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();
        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Coder);
        assert_eq!(decoded.system_prompt, SYSTEM_PROMPT);
        assert_eq!(decoded.intent.as_deref(), Some("implement the thing"));
        assert!(decoded.target_commit.is_none());
        assert!(decoded.diff.is_none());
        assert!(decoded.findings.is_empty());
    }

    /// A2 (ADR-0013, issue #22): the reboucle payload the coder never used
    /// to get -- the findings it is being asked to fix ride alongside the
    /// intent, and still no `target_commit`/`diff` (it has its own worktree
    /// checked out at that commit and can `git diff` itself).
    #[test]
    fn coder_input_round_trips_with_the_findings_it_must_fix() {
        let message = AgentInputMessage::for_coder(
            SYSTEM_PROMPT,
            "implement the thing",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Coder);
        assert_eq!(decoded.intent.as_deref(), Some("implement the thing"));
        assert_eq!(decoded.findings.len(), 1);
        assert_eq!(decoded.findings[0].source, FindingSource::Ci);
        assert_eq!(decoded.findings[0].description, "build failed");
        assert!(decoded.target_commit.is_none());
        assert!(decoded.diff.is_none());
    }

    #[test]
    fn finding_agent_input_round_trips_through_json_with_findings() {
        let message = AgentInputMessage::for_finding_agent(
            AgentRole::Reviewer,
            SYSTEM_PROMPT,
            "abc123",
            "diff --git a/x b/x\n+added line\n",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Reviewer);
        assert_eq!(decoded.system_prompt, SYSTEM_PROMPT);
        assert!(decoded.intent.is_none());
        assert_eq!(decoded.target_commit.as_deref(), Some("abc123"));
        assert_eq!(
            decoded.diff.as_deref(),
            Some("diff --git a/x b/x\n+added line\n")
        );
        assert_eq!(decoded.findings.len(), 1);
        assert_eq!(decoded.findings[0].source, FindingSource::Ci);
        assert_eq!(decoded.scope, ReviewScope::Full);
    }

    /// Issue #40 / decision #37 Q2: a scoped reviewer re-review carries the
    /// correctif's own diff and the findings that prompted it, not this
    /// cycle's full context -- and round-trips with `scope: Correctif`, the
    /// marker that tells the reviewer to look only at this correctif.
    #[test]
    fn scoped_review_input_round_trips_through_json_as_correctif_scope() {
        let message = AgentInputMessage::for_scoped_review(
            SYSTEM_PROMPT,
            "def456",
            "diff --git a/x b/x\n+fixed the bug\n",
            vec![sample_finding()],
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.role, AgentRole::Reviewer);
        assert_eq!(decoded.scope, ReviewScope::Correctif);
        assert_eq!(decoded.target_commit.as_deref(), Some("def456"));
        assert_eq!(
            decoded.diff.as_deref(),
            Some("diff --git a/x b/x\n+fixed the bug\n")
        );
        assert_eq!(decoded.findings.len(), 1);
        assert!(json.contains(r#""scope":"correctif""#));
    }

    /// M2 counterpart for `for_scoped_review`: same construction-side rigor
    /// as `for_finding_agent_rejects_a_blank_target_commit`, since
    /// `for_scoped_review` delegates to `for_finding_agent` internally.
    #[test]
    fn for_scoped_review_rejects_a_blank_target_commit() {
        assert!(matches!(
            AgentInputMessage::for_scoped_review(SYSTEM_PROMPT, "   ", "diff", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn review_scope_round_trips_through_its_string_form() {
        for scope in [ReviewScope::Full, ReviewScope::Correctif] {
            assert_eq!(ReviewScope::parse(scope.as_str()).unwrap(), scope);
        }
        assert!(ReviewScope::parse("ghost").is_err());
    }

    #[test]
    fn tester_input_with_no_prior_findings_round_trips_to_an_empty_list() {
        let message = AgentInputMessage::for_finding_agent(
            AgentRole::Tester,
            SYSTEM_PROMPT,
            "abc123",
            "",
            Vec::new(),
        )
        .unwrap();
        let json = message.to_json().unwrap();
        let decoded = parse_agent_input_message(&json).unwrap();
        assert!(decoded.findings.is_empty());
        assert_eq!(decoded.diff.as_deref(), Some(""));
    }

    #[test]
    fn for_finding_agent_rejects_the_coder_role() {
        let result = AgentInputMessage::for_finding_agent(
            AgentRole::Coder,
            SYSTEM_PROMPT,
            "abc123",
            "",
            vec![],
        );
        assert!(matches!(result, Err(CoreError::MalformedAgentInput(_))));
    }

    /// M2 (issue #20 review): construction must validate with the same
    /// rigor `parse_agent_input_message` applies on the read side --
    /// otherwise `for_coder` could hand its own caller a payload
    /// `to_json`+`parse_agent_input_message` round-trips into something the
    /// parser rejects outright.
    #[test]
    fn for_coder_rejects_a_blank_intent() {
        assert!(matches!(
            AgentInputMessage::for_coder(SYSTEM_PROMPT, "", Vec::new()),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_coder(SYSTEM_PROMPT, "   \n\t ", Vec::new()),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// M2 counterpart for the reviewer/tester constructor.
    #[test]
    fn for_finding_agent_rejects_a_blank_target_commit() {
        assert!(matches!(
            AgentInputMessage::for_finding_agent(
                AgentRole::Reviewer,
                SYSTEM_PROMPT,
                "",
                "",
                vec![]
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_finding_agent(
                AgentRole::Tester,
                SYSTEM_PROMPT,
                "   ",
                "",
                vec![]
            ),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// ADR-0013 / Q2: the system prompt is the definition's whole point --
    /// a blank one is a typed error on the construction side of every role,
    /// exactly as the parser enforces on the read side below.
    #[test]
    fn both_constructors_reject_a_blank_system_prompt() {
        assert!(matches!(
            AgentInputMessage::for_coder("  \n\t", "do the thing", Vec::new()),
            Err(CoreError::MalformedAgentInput(_))
        ));
        assert!(matches!(
            AgentInputMessage::for_finding_agent(AgentRole::Reviewer, "", "abc123", "", vec![]),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(
            parse_agent_input_message("not json"),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let json = r#"{"version":99,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// The 1 -> 2 bump (ADR-0013) is a real break, not a formality: a
    /// payload announcing v1 is refused on its *version*, never read as a
    /// best-effort current version.
    ///
    /// The fixture deliberately carries every field the *current* (v3)
    /// wire shape requires, even though no real v1 payload ever did: without
    /// them, `serde_json` would reject the payload on a missing field before
    /// the version gate is ever reached, and this test would pass for a
    /// reason that has nothing to do with what it claims to prove. Everything
    /// here is valid v3 except the `version` itself.
    #[test]
    fn rejects_a_version_1_payload() {
        let json = r#"{"version":1,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// The 2 -> 3 bump (issue #40) is its own real break: a genuine v2
    /// payload -- valid under the *old* shape, and missing `scope` entirely,
    /// since that field did not exist yet -- must be refused outright, never
    /// silently read as `scope: "full"`. This is what "retro-compat handled"
    /// means throughout this module: reject the mismatched version with a
    /// clear, typed error, not guess at forward/backward compatibility (same
    /// convention the 1 -> 2 bump already established above).
    #[test]
    fn rejects_a_genuine_version_2_payload_with_no_scope_field() {
        let json = r#"{"version":2,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[]}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_role() {
        let json = r#"{"version":3,"role":"ghost","system_prompt":"x","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::UnknownRole(_))
        ));
    }

    #[test]
    fn rejects_a_payload_missing_system_prompt() {
        let json = r#"{"version":3,"role":"coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_payload_whose_system_prompt_is_blank() {
        let json = r#"{"version":3,"role":"reviewer","system_prompt":"   ","target_commit":"abc","diff":"","findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// A2 relaxed `for_coder`'s findings invariant, not its intent one: a
    /// coder payload is still validated with the same rigor it always was.
    #[test]
    fn rejects_a_coder_payload_missing_intent() {
        let json = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":null,"target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_a_coder_payload_whose_intent_is_blank_even_when_it_carries_findings() {
        let json = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"   ","target_commit":null,"diff":null,"findings":[{"source":"reviewer","severity":"blocking","file":null,"description":"x","action":null}],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// A2 (issue #22): "the coder gets **intent + findings ONLY**:
    /// `target_commit` and `diff` MUST be null/None for the coder", and the
    /// ticket asks for that invariant to hold *both* at construction and in
    /// `parse_agent_input_message`. A coder payload that carries a
    /// `target_commit`/`diff` violates the invariant the constructor
    /// enforces, so the parser must refuse it with the same rigor rather
    /// than quietly discarding the fields (code-standards.md: no silent
    /// fallback -- dropping data the sender meant is exactly that).
    #[test]
    fn rejects_a_coder_payload_that_carries_a_target_commit_or_diff() {
        let with_commit = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":"abc123","diff":null,"findings":[],"scope":"full"}"#;
        assert!(
            matches!(
                parse_agent_input_message(with_commit),
                Err(CoreError::MalformedAgentInput(_))
            ),
            "a coder payload with a target_commit must be rejected, not silently stripped: {:?}",
            parse_agent_input_message(with_commit)
        );

        let with_diff = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":"diff --git a/x b/x","findings":[],"scope":"full"}"#;
        assert!(
            matches!(
                parse_agent_input_message(with_diff),
                Err(CoreError::MalformedAgentInput(_))
            ),
            "a coder payload with a diff must be rejected, not silently stripped: {:?}",
            parse_agent_input_message(with_diff)
        );
    }

    #[test]
    fn rejects_a_reviewer_payload_missing_target_commit() {
        let json = r#"{"version":3,"role":"reviewer","system_prompt":"be a reviewer","intent":null,"target_commit":null,"diff":null,"findings":[],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_finding_source_inside_findings() {
        let json = r#"{"version":3,"role":"tester","system_prompt":"be a tester","intent":null,"target_commit":"abc","diff":"","findings":[{"source":"ghost","severity":"blocking","file":null,"description":"x","action":null}],"scope":"full"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::UnknownFindingSource(_))
        ));
    }

    /// Issue #40: `Correctif` is reviewer-only, enforced on the read side the
    /// same way `coder_only_violation` already enforces "no target_commit/
    /// diff on a coder payload" -- a tester payload claiming a `"correctif"`
    /// scope must be rejected, not silently coerced to `Full`.
    #[test]
    fn rejects_a_tester_payload_with_a_correctif_scope() {
        let json = r#"{"version":3,"role":"tester","system_prompt":"be a tester","intent":null,"target_commit":"abc","diff":"","findings":[],"scope":"correctif"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    /// Same invariant, coder side.
    #[test]
    fn rejects_a_coder_payload_with_a_correctif_scope() {
        let json = r#"{"version":3,"role":"coder","system_prompt":"be a coder","intent":"x","target_commit":null,"diff":null,"findings":[],"scope":"correctif"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_scope_string() {
        let json = r#"{"version":3,"role":"reviewer","system_prompt":"be a reviewer","intent":null,"target_commit":"abc","diff":"","findings":[],"scope":"ghost"}"#;
        assert!(matches!(
            parse_agent_input_message(json),
            Err(CoreError::MalformedAgentInput(_))
        ));
    }
}
