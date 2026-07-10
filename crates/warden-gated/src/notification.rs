//! Parsing of the raw `post-receive` payload (ADR-0002). Git invokes the
//! `post-receive` hook with one line per updated ref on stdin, formatted
//! `<old-sha> <new-sha> <ref-name>`. This module is the *only* place that
//! interprets that payload -- the hook script itself (`hook.rs`) and the
//! `notify` relay (`relay.rs`) never do, so there is exactly one, testable,
//! Rust-typed place where "what does this push mean" is decided
//! (code-standards.md: "aucune logique métier dans les ... callbacks
//! d'event").

use crate::error::{GatedError, Result};

/// One `post-receive` ref update, already validated and split into its
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushNotification {
    pub old_commit_sha: String,
    pub new_commit_sha: String,
    pub run_id: String,
}

/// Ref prefix `warden` pushes a converged run's branch under in the local
/// bare gate repo, mirroring the `refs/warden/runs/<run_id>/...` local-ref
/// convention `warden::orchestrator` already uses to protect cycle commits
/// from `git gc`.
pub const GATE_REF_PREFIX: &str = "refs/heads/warden-run/";

/// Parses one raw `post-receive` line into a [`PushNotification`]. Any line
/// that isn't exactly three whitespace-separated fields, or whose ref
/// doesn't match [`GATE_REF_PREFIX`], is a boundary error -- never silently
/// ignored (code-standards.md: "valider toute entrée externe ... à la
/// frontière").
pub fn parse_post_receive_line(line: &str) -> Result<PushNotification> {
    let mut fields = line.split_whitespace();
    let (Some(old_commit_sha), Some(new_commit_sha), Some(refname), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(GatedError::MalformedPushNotification(line.to_string()));
    };

    let run_id = refname
        .strip_prefix(GATE_REF_PREFIX)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| GatedError::MalformedPushNotification(line.to_string()))?;

    Ok(PushNotification {
        old_commit_sha: old_commit_sha.to_string(),
        new_commit_sha: new_commit_sha.to_string(),
        run_id: run_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_post_receive_line() {
        let line = "old111 new222 refs/heads/warden-run/run-abc";
        let notification = parse_post_receive_line(line).unwrap();
        assert_eq!(
            notification,
            PushNotification {
                old_commit_sha: "old111".to_string(),
                new_commit_sha: "new222".to_string(),
                run_id: "run-abc".to_string(),
            }
        );
    }

    #[test]
    fn rejects_a_line_with_the_wrong_number_of_fields() {
        assert!(matches!(
            parse_post_receive_line("only two fields"),
            Err(GatedError::MalformedPushNotification(_))
        ));
        assert!(matches!(
            parse_post_receive_line("a b c d"),
            Err(GatedError::MalformedPushNotification(_))
        ));
    }

    #[test]
    fn rejects_a_ref_outside_the_gate_naming_convention() {
        let line = "old111 new222 refs/heads/main";
        assert!(matches!(
            parse_post_receive_line(line),
            Err(GatedError::MalformedPushNotification(_))
        ));
    }

    #[test]
    fn rejects_a_ref_with_an_empty_run_id() {
        let line = "old111 new222 refs/heads/warden-run/";
        assert!(matches!(
            parse_post_receive_line(line),
            Err(GatedError::MalformedPushNotification(_))
        ));
    }
}
