//! Wire form of [`EvidenceRow`] for crossing the `warden` -> `warden-gated`
//! process boundary (issue #15 review, M2): `run-tail`'s `--evidence-json`
//! CLI argument. `EvidenceRow`/`EvidenceType` don't derive
//! `Serialize`/`Deserialize` (mirrors the `source`/`severity` wire
//! convention `ci_channel::CiFindingWire` already uses) so this is the
//! validated string-based wire shape, re-parsed into a real `EvidenceType`
//! at receipt -- never trusted as-is (code-standards.md: "valider toute
//! entrée externe ... à la frontière").

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::evidence::EvidenceType;
use crate::pr_body::EvidenceRow;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvidenceRowWire {
    cycle_number: u32,
    evidence_type: String,
    repo_relative_path: String,
    description: String,
}

impl EvidenceRowWire {
    fn from_evidence_row(row: &EvidenceRow) -> Self {
        Self {
            cycle_number: row.cycle_number,
            evidence_type: row.evidence_type.as_str().to_string(),
            repo_relative_path: row.repo_relative_path.clone(),
            description: row.description.clone(),
        }
    }

    fn into_evidence_row(self) -> Result<EvidenceRow> {
        Ok(EvidenceRow {
            cycle_number: self.cycle_number,
            evidence_type: EvidenceType::parse(&self.evidence_type)?,
            repo_relative_path: self.repo_relative_path,
            description: self.description,
        })
    }
}

/// Serializes `rows` to the exact wire form [`parse_evidence_rows`] parses
/// back -- one JSON array, suitable for a single CLI argument.
pub fn serialize_evidence_rows(rows: &[EvidenceRow]) -> Result<String> {
    let wire: Vec<EvidenceRowWire> = rows
        .iter()
        .map(EvidenceRowWire::from_evidence_row)
        .collect();
    serde_json::to_string(&wire)
        .map_err(|error| CoreError::MalformedEvidenceRows(error.to_string()))
}

/// Parses a `--evidence-json` argument into validated [`EvidenceRow`]s.
/// Malformed JSON or an unknown `evidence_type` is a typed error, never
/// silently dropped or partially trusted -- an empty array (`"[]"`, the CLI
/// default) parses to an empty `Vec`, not an error.
pub fn parse_evidence_rows(raw: &str) -> Result<Vec<EvidenceRow>> {
    let wire: Vec<EvidenceRowWire> = serde_json::from_str(raw)
        .map_err(|error| CoreError::MalformedEvidenceRows(error.to_string()))?;
    wire.into_iter()
        .map(EvidenceRowWire::into_evidence_row)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> EvidenceRow {
        EvidenceRow {
            cycle_number: 2,
            evidence_type: EvidenceType::Image,
            repo_relative_path: ".warden/evidence/2/screenshot.png".to_string(),
            description: "login screen".to_string(),
        }
    }

    #[test]
    fn evidence_rows_round_trip_through_json() {
        let rows = vec![sample_row()];
        let json = serialize_evidence_rows(&rows).unwrap();
        let decoded = parse_evidence_rows(&json).unwrap();

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].cycle_number, 2);
        assert_eq!(decoded[0].evidence_type, EvidenceType::Image);
        assert_eq!(
            decoded[0].repo_relative_path,
            ".warden/evidence/2/screenshot.png"
        );
        assert_eq!(decoded[0].description, "login screen");
    }

    #[test]
    fn an_empty_array_parses_to_no_rows() {
        assert!(parse_evidence_rows("[]").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(
            parse_evidence_rows("not json"),
            Err(CoreError::MalformedEvidenceRows(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_evidence_type() {
        let json = r#"[{"cycle_number":1,"evidence_type":"ghost","repo_relative_path":"x","description":"y"}]"#;
        assert!(parse_evidence_rows(json).is_err());
    }
}
