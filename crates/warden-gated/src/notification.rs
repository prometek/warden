//! Parsing of the raw `post-receive` payload.

use crate::error::{GatedError, Result};

/// One `post-receive` ref update, already validated and split into its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushNotification {
    pub old_commit_sha: String,
    pub new_commit_sha: String,
    pub run_id: String,
}

pub const GATE_REF_PREFIX: &str = "refs/heads/warden-run/";

/// Parses one raw `post-receive` line into a [`PushNotification`].
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
