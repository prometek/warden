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
