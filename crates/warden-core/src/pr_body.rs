//! Evidence section of a run's finalized PR body (ADR-0007 Finalize,
//! ADR-0009 Evidence Capture Adapter): pure markdown rendering of already-
//! captured evidence rows.
//!
//! Lives in `warden-core`, not `warden` (where the Evidence Capture Adapter
//! work originally landed it, alongside `warden`'s own cycles/findings
//! formatting), because `finalize`/`update_body`
//! (`warden_gated::pr_manager`) are the only code in production that
//! actually sets a PR body -- and `warden-gated` must never depend on
//! `warden` (ADR-0006, one-way: `warden-core` is the only crate both
//! `warden` and `warden-gated` can share). Putting the evidence renderer
//! here, instead of in `warden`, is what lets `warden-gated`'s own
//! `finalize` genuinely call it rather than merely receiving pre-rendered
//! markdown it can never independently reproduce or verify.

use crate::EvidenceType;

/// One evidence artifact captured during a run, already committed into the
/// repo (`repo_relative_path` is where it now lives) by the time this is
/// rendered.
#[derive(Debug, Clone)]
pub struct EvidenceRow {
    pub cycle_number: u32,
    pub evidence_type: EvidenceType,
    pub repo_relative_path: String,
    pub description: String,
}

/// Renders the Evidence section (ADR-0009): images embedded inline via the
/// branch's `raw.githubusercontent.com` URL, everything else (video, log,
/// asciinema recordings) as a clickable link, since markdown can't embed
/// those inline. Returns an empty section header with no rows if `evidence`
/// is empty -- callers that only want to include the section when there's
/// something to show should check `evidence.is_empty()` themselves (mirrors
/// `warden::pr_summary::pr_body_from_run`'s own top-level section handling).
///
/// `repo_slug` is `"<owner>/<repo>"` and `branch` the branch the evidence
/// was pushed on -- both needed to build the
/// `raw.githubusercontent.com/<repo_slug>/<branch>/<path>` URLs ADR-0009
/// specifies.
pub fn format_evidence_section(evidence: &[EvidenceRow], repo_slug: &str, branch: &str) -> String {
    let mut body = "## Evidence\n\n".to_string();
    for item in evidence {
        let raw_url = raw_content_url(repo_slug, branch, &item.repo_relative_path);
        match item.evidence_type {
            EvidenceType::Image => {
                body.push_str(&format!(
                    "**Cycle {}** — ![{}]({raw_url})\n\n",
                    item.cycle_number, item.description
                ));
            }
            EvidenceType::Video | EvidenceType::Log | EvidenceType::Other => {
                body.push_str(&format!(
                    "**Cycle {}** — [{}]({raw_url})\n\n",
                    item.cycle_number, item.description
                ));
            }
        }
    }
    body
}

/// Characters left unescaped by [`raw_content_url`]: RFC 3986's "unreserved"
/// set, plus `.` -- everything else (spaces, parentheses, `#`, ...) gets
/// percent-encoded. Deliberately built from `NON_ALPHANUMERIC` rather than
/// listing reserved characters directly, so nothing exotic slips through
/// unescaped by omission.
const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Builds a `raw.githubusercontent.com/<repo_slug>/<branch>/<path>` URL,
/// percent-encoding each path *segment* individually (never the `/`
/// separators between them). Playwright's own artifact names are flattened
/// from nested output directories -- one per test title
/// (`warden::evidence::stage_on_scratch`) -- and test titles routinely
/// contain spaces, parentheses, or other characters that would otherwise
/// break the resulting markdown image/link syntax or produce a dead link.
fn raw_content_url(repo_slug: &str, branch: &str, repo_relative_path: &str) -> String {
    let encoded_path = repo_relative_path
        .split('/')
        .map(|segment| percent_encoding::utf8_percent_encode(segment, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/");
    format!("https://raw.githubusercontent.com/{repo_slug}/{branch}/{encoded_path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Images render inline via the raw.githubusercontent.com URL.
    // -----------------------------------------------------------------

    #[test]
    fn renders_image_evidence_inline_via_the_raw_content_url() {
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: EvidenceType::Image,
            repo_relative_path: ".warden/evidence/1/screenshot.png".to_string(),
            description: "login screen".to_string(),
        }];
        let body = format_evidence_section(&evidence, "acme/widgets", "feature-branch");

        let expected = "![login screen](https://raw.githubusercontent.com/acme/widgets/feature-branch/.warden/evidence/1/screenshot.png)";
        assert!(
            body.contains(expected),
            "expected inline image markdown {expected:?} in body: {body}"
        );
    }

    // -----------------------------------------------------------------
    // Video/log/asciinema (Other) evidence renders as a clickable link,
    // never inline.
    // -----------------------------------------------------------------

    #[test]
    fn renders_video_evidence_as_a_clickable_link_not_inline() {
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: EvidenceType::Video,
            repo_relative_path: ".warden/evidence/1/failure.webm".to_string(),
            description: "failure recording".to_string(),
        }];
        let body = format_evidence_section(&evidence, "acme/widgets", "main");

        let expected = "[failure recording](https://raw.githubusercontent.com/acme/widgets/main/.warden/evidence/1/failure.webm)";
        assert!(body.contains(expected), "body was: {body}");
        assert!(
            !body.contains(&format!("!{expected}")),
            "a video must be a link, not embedded inline: {body}"
        );
    }

    #[test]
    fn renders_asciinema_recording_evidence_as_a_clickable_link() {
        // asciinema recordings are always typed EvidenceType::Other (see
        // AsciinemaAdapter in warden::evidence).
        let evidence = vec![EvidenceRow {
            cycle_number: 2,
            evidence_type: EvidenceType::Other,
            repo_relative_path: ".warden/evidence/2/session.cast".to_string(),
            description: "asciinema recording of the cycle's tester command".to_string(),
        }];
        let body = format_evidence_section(&evidence, "acme/widgets", "main");

        let expected = "[asciinema recording of the cycle's tester command](https://raw.githubusercontent.com/acme/widgets/main/.warden/evidence/2/session.cast)";
        assert!(body.contains(expected), "body was: {body}");
        assert!(!body.contains(&format!("!{expected}")), "body was: {body}");
    }

    #[test]
    fn renders_log_evidence_as_a_clickable_link_not_inline() {
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: EvidenceType::Log,
            repo_relative_path: ".warden/evidence/1/run.log".to_string(),
            description: "test run log".to_string(),
        }];
        let body = format_evidence_section(&evidence, "acme/widgets", "main");

        assert!(body.contains(
            "[test run log](https://raw.githubusercontent.com/acme/widgets/main/.warden/evidence/1/run.log)"
        ));
        assert!(
            !body.contains("!["),
            "a log must never be rendered inline: {body}"
        );
    }

    #[test]
    fn renders_multiple_evidence_items_grouped_by_their_own_cycle_number() {
        let evidence = vec![
            EvidenceRow {
                cycle_number: 1,
                evidence_type: EvidenceType::Image,
                repo_relative_path: ".warden/evidence/1/shot.png".to_string(),
                description: "cycle 1 shot".to_string(),
            },
            EvidenceRow {
                cycle_number: 2,
                evidence_type: EvidenceType::Image,
                repo_relative_path: ".warden/evidence/2/shot.png".to_string(),
                description: "cycle 2 shot".to_string(),
            },
        ];
        let body = format_evidence_section(&evidence, "acme/widgets", "main");

        assert!(body.contains("**Cycle 1** — ![cycle 1 shot]"));
        assert!(body.contains("**Cycle 2** — ![cycle 2 shot]"));
    }

    // -----------------------------------------------------------------
    // MEDIUM finding #3: Playwright artifact names are flattened from test
    // titles (`warden::evidence::stage_on_scratch`) and routinely contain
    // spaces/parentheses/etc -- each path segment must be percent-encoded
    // or the resulting markdown image link is dead.
    // -----------------------------------------------------------------

    #[test]
    fn percent_encodes_path_segments_containing_spaces_and_special_characters() {
        // Mirrors how `warden::evidence::stage_on_scratch` flattens a
        // Playwright test title like "login page (mobile)" into a file
        // name -- spaces and parentheses land verbatim in the file name.
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: EvidenceType::Image,
            repo_relative_path: ".warden/evidence/1/login page (mobile).png".to_string(),
            description: "login page on mobile".to_string(),
        }];
        let body = format_evidence_section(&evidence, "acme/widgets", "main");

        let expected_url = "https://raw.githubusercontent.com/acme/widgets/main/.warden/evidence/1/login%20page%20%28mobile%29.png";
        let expected_line = format!("**Cycle 1** — ![login page on mobile]({expected_url})");
        assert!(
            body.contains(&expected_line),
            "expected the flattened artifact name to be percent-encoded segment-by-segment: {body}"
        );
    }

    #[test]
    fn percent_encoding_never_escapes_the_slash_separators_between_path_segments() {
        let evidence = vec![EvidenceRow {
            cycle_number: 1,
            evidence_type: EvidenceType::Log,
            repo_relative_path: ".warden/evidence/1/run.log".to_string(),
            description: "test run log".to_string(),
        }];
        let body = format_evidence_section(&evidence, "acme/widgets", "main");

        assert!(
            body.contains("acme/widgets/main/.warden/evidence/1/run.log"),
            "path separators must survive percent-encoding unescaped: {body}"
        );
    }
}
