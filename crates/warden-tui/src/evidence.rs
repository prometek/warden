//! Evidence rendering: images render inline when the terminal supports a graphics protocol (see
//! [`crate::capabilities`]).

use std::path::PathBuf;

use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;

use crate::capabilities::GraphicsCapability;
use crate::error::{Result, TuiError};

/// Mirrors `EVIDENCE.type` from Architecture.md §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    Image,
    Video,
    Asciinema,
    Other,
}

impl EvidenceKind {
    /// Maps `RunEvent::EvidenceCaptured.evidence_type`'s free-form string onto a rendering kind.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "image" => EvidenceKind::Image,
            "video" => EvidenceKind::Video,
            "asciinema" => EvidenceKind::Asciinema,
            _ => EvidenceKind::Other,
        }
    }
}

/// One piece of evidence to render, in the shape a future `EVIDENCE` row would carry -- passed in
/// directly by the caller rather than read from a database query that doesn't exist yet.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub file_path: PathBuf,
    pub description: Option<String>,
}

pub enum Rendering {
    Inline(Protocol),
    ExternalViewer { path: PathBuf, reason: &'static str },
}

/// Renders `evidence` for a terminal with the given [`GraphicsCapability`], dispatching per
/// [`EvidenceKind`] as specifies.
pub fn render(
    evidence: &Evidence,
    capability: GraphicsCapability,
    picker: Option<&Picker>,
    area: Size,
) -> Result<Rendering> {
    match evidence.kind {
        EvidenceKind::Image => render_image(evidence, capability, picker, area),
        EvidenceKind::Video => Err(TuiError::NotYetImplemented {
            feature: "inline video frame preview (ffmpeg extraction)",
            reason: "Phase 7 (issue #7) has not landed on this branch: no EVIDENCE producer exists yet to extract a frame from",
        }),
        EvidenceKind::Asciinema => Err(TuiError::NotYetImplemented {
            feature: "asciinema sub-terminal playback",
            reason: "Phase 7 (issue #7) has not landed on this branch: no EVIDENCE producer exists yet to play back",
        }),
        EvidenceKind::Other => Ok(Rendering::ExternalViewer {
            path: evidence.file_path.clone(),
            reason: "this evidence type has no inline rendering, only external viewing",
        }),
    }
}

fn render_image(
    evidence: &Evidence,
    capability: GraphicsCapability,
    picker: Option<&Picker>,
    area: Size,
) -> Result<Rendering> {
    let Some(picker) = picker.filter(|_| capability.supports_inline_images()) else {
        return Ok(Rendering::ExternalViewer {
            path: evidence.file_path.clone(),
            reason: "terminal does not support an inline graphics protocol (Kitty/iTerm2/Sixel)",
        });
    };

    let dyn_image = image::ImageReader::open(&evidence.file_path)
        .map_err(|source| TuiError::ImageDecode {
            path: evidence.file_path.clone(),
            source: source.into(),
        })?
        .decode()
        .map_err(|source| TuiError::ImageDecode {
            path: evidence.file_path.clone(),
            source,
        })?;

    let protocol = picker
        .new_protocol(dyn_image, area, Resize::Fit(None))
        .map_err(|source| TuiError::ImageProtocol {
            path: evidence.file_path.clone(),
            source,
        })?;

    Ok(Rendering::Inline(protocol))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Size;
    use tempfile::TempDir;

    fn sample_area() -> Size {
        Size::new(40, 20)
    }

    #[test]
    fn image_falls_back_to_external_viewer_when_the_terminal_has_no_graphics_capability() {
        let evidence = Evidence {
            kind: EvidenceKind::Image,
            file_path: PathBuf::from("/does/not/matter.png"),
            description: None,
        };

        let rendering = render(&evidence, GraphicsCapability::None, None, sample_area()).unwrap();
        assert!(matches!(rendering, Rendering::ExternalViewer { .. }));
    }

    #[test]
    fn image_decode_failure_on_a_capable_terminal_is_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let bogus_path = dir.path().join("not-an-image.png");
        std::fs::write(&bogus_path, b"not actually a png").unwrap();

        let evidence = Evidence {
            kind: EvidenceKind::Image,
            file_path: bogus_path,
            description: None,
        };
        let picker = Picker::halfblocks();

        let result = render(
            &evidence,
            GraphicsCapability::Kitty,
            Some(&picker),
            sample_area(),
        );
        assert!(matches!(result, Err(TuiError::ImageDecode { .. })));
    }

    #[test]
    fn video_is_a_typed_not_yet_implemented_error() {
        let evidence = Evidence {
            kind: EvidenceKind::Video,
            file_path: PathBuf::from("/does/not/matter.mp4"),
            description: None,
        };

        let result = render(&evidence, GraphicsCapability::Kitty, None, sample_area());
        assert!(matches!(result, Err(TuiError::NotYetImplemented { .. })));
    }

    #[test]
    fn asciinema_is_a_typed_not_yet_implemented_error() {
        let evidence = Evidence {
            kind: EvidenceKind::Asciinema,
            file_path: PathBuf::from("/does/not/matter.cast"),
            description: None,
        };

        let result = render(&evidence, GraphicsCapability::Kitty, None, sample_area());
        assert!(matches!(result, Err(TuiError::NotYetImplemented { .. })));
    }

    #[test]
    fn evidence_kind_parses_every_known_wire_value() {
        assert_eq!(EvidenceKind::parse("image"), EvidenceKind::Image);
        assert_eq!(EvidenceKind::parse("video"), EvidenceKind::Video);
        assert_eq!(EvidenceKind::parse("asciinema"), EvidenceKind::Asciinema);
        assert_eq!(EvidenceKind::parse("other"), EvidenceKind::Other);
    }

    #[test]
    fn evidence_kind_parse_degrades_an_unknown_value_to_other_rather_than_erroring() {
        assert_eq!(EvidenceKind::parse("carrier-pigeon"), EvidenceKind::Other);
    }

    #[test]
    fn other_evidence_kind_always_falls_back_to_an_external_viewer() {
        let evidence = Evidence {
            kind: EvidenceKind::Other,
            file_path: PathBuf::from("/does/not/matter.log"),
            description: None,
        };

        let rendering = render(&evidence, GraphicsCapability::Kitty, None, sample_area()).unwrap();
        assert!(matches!(rendering, Rendering::ExternalViewer { .. }));
    }
}
