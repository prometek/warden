//! Evidence types and pure classification logic (ADR-0009, issue #7): the
//! `EVIDENCE.type` values persisted in SQLite, the two capture tools Warden
//! chooses between, and the project-type detection that picks a default
//! between them. No I/O here -- gathering the filesystem facts a
//! [`ProjectMarkers`] is built from is the `warden` crate's job (scanning a
//! repo is I/O); this module only classifies facts it's handed.

use crate::error::{CoreError, Result};

/// Kind of artifact an evidence adapter produced (`EVIDENCE.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceType {
    Image,
    Video,
    Log,
    Other,
}

impl EvidenceType {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceType::Image => "image",
            EvidenceType::Video => "video",
            EvidenceType::Log => "log",
            EvidenceType::Other => "other",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "image" => Ok(EvidenceType::Image),
            "video" => Ok(EvidenceType::Video),
            "log" => Ok(EvidenceType::Log),
            "other" => Ok(EvidenceType::Other),
            other => Err(CoreError::UnknownEvidenceType(other.to_string())),
        }
    }
}

/// Which capture tool produces the evidence for a cycle's successful e2e
/// test (ADR-0009): Playwright for web/UI projects, asciinema for CLI
/// projects. Selected automatically from [`ProjectType`], with an explicit
/// override always winning (`evidence.tool` config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceTool {
    Playwright,
    Asciinema,
}

impl EvidenceTool {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceTool::Playwright => "playwright",
            EvidenceTool::Asciinema => "asciinema",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "playwright" => Ok(EvidenceTool::Playwright),
            "asciinema" => Ok(EvidenceTool::Asciinema),
            other => Err(CoreError::UnknownEvidenceTool(other.to_string())),
        }
    }
}

/// Coarse classification of the target repository, used only to pick a
/// default [`EvidenceTool`] (ADR-0009: "présence d'un serveur/framework
/// front dans le repo → Playwright, sinon → asciinema").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectType {
    Web,
    Cli,
}

/// The filesystem facts [`detect_project_type`] classifies: a shallow scan
/// of the repo root (marker files) plus, if present, the union of
/// `package.json`'s `dependencies`/`devDependencies` keys. Gathering this is
/// I/O and lives in the `warden` crate (`evidence::scan_project_markers`);
/// this struct is the validated boundary value that crosses into pure logic.
#[derive(Debug, Clone, Default)]
pub struct ProjectMarkers {
    /// Entry names found directly at the repo root (file or directory
    /// names, not full paths).
    pub root_entries: Vec<String>,
    /// `package.json` dependency/dev-dependency keys, empty if the repo has
    /// no `package.json` at its root.
    pub package_json_dependencies: Vec<String>,
}

/// Root-level file/directory names that, on their own, indicate a web/UI
/// project regardless of `package.json` contents.
const WEB_MARKER_ENTRIES: &[&str] = &[
    "index.html",
    "next.config.js",
    "next.config.mjs",
    "next.config.ts",
    "vite.config.js",
    "vite.config.ts",
    "angular.json",
    "svelte.config.js",
    "nuxt.config.js",
    "nuxt.config.ts",
    "webpack.config.js",
    "gatsby-config.js",
];

/// `package.json` dependency names that indicate a web/UI or web-serving
/// project (front-end frameworks and the servers that render for a
/// browser -- ADR-0009's "serveur/framework front").
const WEB_FRAMEWORK_DEPENDENCIES: &[&str] = &[
    "react",
    "vue",
    "svelte",
    "next",
    "nuxt",
    "@angular/core",
    "vite",
    "express",
    "fastify",
    "koa",
    "playwright",
    "@playwright/test",
];

/// Classifies a repo as [`ProjectType::Web`] if any known web/UI marker is
/// present, [`ProjectType::Cli`] otherwise. Deliberately simple ("détection
/// simple" per ADR-0009) -- `evidence.tool` always overrides this when the
/// heuristic guesses wrong.
pub fn detect_project_type(markers: &ProjectMarkers) -> ProjectType {
    let has_web_marker_file = markers
        .root_entries
        .iter()
        .any(|entry| WEB_MARKER_ENTRIES.contains(&entry.as_str()));
    let has_web_dependency = markers
        .package_json_dependencies
        .iter()
        .any(|dep| WEB_FRAMEWORK_DEPENDENCIES.contains(&dep.as_str()));

    if has_web_marker_file || has_web_dependency {
        ProjectType::Web
    } else {
        ProjectType::Cli
    }
}

/// Picks the [`EvidenceTool`] to capture with: `override_tool` always wins
/// (`evidence.tool` config, ADR-0009), otherwise it follows
/// [`ProjectType`]'s default mapping.
pub fn select_evidence_tool(
    project_type: ProjectType,
    override_tool: Option<EvidenceTool>,
) -> EvidenceTool {
    override_tool.unwrap_or(match project_type {
        ProjectType::Web => EvidenceTool::Playwright,
        ProjectType::Cli => EvidenceTool::Asciinema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Project-type detection (ADR-0009 acceptance criterion 1: "web
    // project -> Playwright selected; CLI project -> asciinema selected").
    // -----------------------------------------------------------------

    #[test]
    fn detect_project_type_returns_web_for_a_known_marker_file() {
        let markers = ProjectMarkers {
            root_entries: vec!["index.html".to_string(), "src".to_string()],
            package_json_dependencies: vec![],
        };
        assert_eq!(detect_project_type(&markers), ProjectType::Web);
    }

    #[test]
    fn detect_project_type_returns_web_for_a_known_framework_dependency() {
        let markers = ProjectMarkers {
            root_entries: vec!["package.json".to_string()],
            package_json_dependencies: vec!["react".to_string(), "lodash".to_string()],
        };
        assert_eq!(detect_project_type(&markers), ProjectType::Web);
    }

    #[test]
    fn detect_project_type_returns_cli_when_no_web_signal_is_present() {
        let markers = ProjectMarkers {
            root_entries: vec!["Cargo.toml".to_string(), "src".to_string()],
            package_json_dependencies: vec![],
        };
        assert_eq!(detect_project_type(&markers), ProjectType::Cli);
    }

    #[test]
    fn detect_project_type_ignores_unrelated_package_json_dependencies() {
        // A package.json exists (e.g. a Node-based CLI tool) but declares
        // none of the front-end/web-serving frameworks ADR-0009 lists --
        // must not be misclassified as Web just because *some*
        // package.json is present.
        let markers = ProjectMarkers {
            root_entries: vec!["package.json".to_string(), "bin".to_string()],
            package_json_dependencies: vec!["commander".to_string(), "chalk".to_string()],
        };
        assert_eq!(detect_project_type(&markers), ProjectType::Cli);
    }

    #[test]
    fn detect_project_type_with_no_markers_at_all_defaults_to_cli() {
        assert_eq!(
            detect_project_type(&ProjectMarkers::default()),
            ProjectType::Cli
        );
    }

    // -----------------------------------------------------------------
    // Tool selection (acceptance criterion 2: "config override
    // evidence.tool always wins over auto-detection").
    // -----------------------------------------------------------------

    #[test]
    fn select_evidence_tool_follows_detected_project_type_when_no_override_is_given() {
        assert_eq!(
            select_evidence_tool(ProjectType::Web, None),
            EvidenceTool::Playwright
        );
        assert_eq!(
            select_evidence_tool(ProjectType::Cli, None),
            EvidenceTool::Asciinema
        );
    }

    #[test]
    fn select_evidence_tool_override_wins_over_a_web_detected_project() {
        assert_eq!(
            select_evidence_tool(ProjectType::Web, Some(EvidenceTool::Asciinema)),
            EvidenceTool::Asciinema,
            "an explicit evidence.tool override must always win over auto-detection"
        );
    }

    #[test]
    fn select_evidence_tool_override_wins_over_a_cli_detected_project() {
        assert_eq!(
            select_evidence_tool(ProjectType::Cli, Some(EvidenceTool::Playwright)),
            EvidenceTool::Playwright,
            "an explicit evidence.tool override must always win over auto-detection"
        );
    }

    // -----------------------------------------------------------------
    // EvidenceTool / EvidenceType parse+as_str -- the closed-set
    // boundary validation `main.rs`'s `--evidence-tool` and `db.rs`'s
    // stored `evidence.type` column both rely on.
    // -----------------------------------------------------------------

    #[test]
    fn evidence_tool_as_str_and_parse_round_trip() {
        for tool in [EvidenceTool::Playwright, EvidenceTool::Asciinema] {
            assert_eq!(EvidenceTool::parse(tool.as_str()).unwrap(), tool);
        }
    }

    #[test]
    fn evidence_tool_parse_rejects_an_unknown_value() {
        let result = EvidenceTool::parse("selenium");
        assert!(
            matches!(result, Err(CoreError::UnknownEvidenceTool(value)) if value == "selenium")
        );
    }

    #[test]
    fn evidence_type_as_str_and_parse_round_trip() {
        for evidence_type in [
            EvidenceType::Image,
            EvidenceType::Video,
            EvidenceType::Log,
            EvidenceType::Other,
        ] {
            assert_eq!(
                EvidenceType::parse(evidence_type.as_str()).unwrap(),
                evidence_type
            );
        }
    }

    #[test]
    fn evidence_type_parse_rejects_an_unknown_value() {
        let result = EvidenceType::parse("audio");
        assert!(matches!(result, Err(CoreError::UnknownEvidenceType(value)) if value == "audio"));
    }
}
