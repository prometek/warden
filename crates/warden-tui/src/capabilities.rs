//! Terminal graphics capability detection (ADR-0010). Kitty, iTerm2, and
//! Sixel are the three inline image protocols in scope; anything else --
//! including `ratatui-image`'s own "halfblocks" approximation, which draws
//! colored Unicode block characters rather than a real image -- falls back
//! to an external viewer, per ADR-0010's explicit protocol list.

use ratatui_image::picker::{Picker, ProtocolType};

/// What this terminal can render evidence images with, as decided at
/// startup. [`GraphicsCapability::supports_inline_images`] is what
/// [`crate::evidence`] checks before attempting an inline render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsCapability {
    Kitty,
    Iterm2,
    Sixel,
    /// No graphics protocol detected (or the query failed) -- includes
    /// `ratatui-image`'s halfblocks fallback, deliberately treated as "not
    /// inline-capable" per ADR-0010's scoped protocol list.
    None,
}

impl GraphicsCapability {
    pub fn supports_inline_images(self) -> bool {
        !matches!(self, GraphicsCapability::None)
    }
}

/// Maps `ratatui-image`'s own protocol guess onto ADR-0010's narrower
/// "inline-capable or not" question. Pure and unit-testable independent of
/// the actual stdio query in [`detect`].
fn classify(protocol_type: ProtocolType) -> GraphicsCapability {
    match protocol_type {
        ProtocolType::Kitty => GraphicsCapability::Kitty,
        ProtocolType::Iterm2 => GraphicsCapability::Iterm2,
        ProtocolType::Sixel => GraphicsCapability::Sixel,
        ProtocolType::Halfblocks => GraphicsCapability::None,
    }
}

/// Queries the connected terminal for its graphics capability. This writes
/// and reads a handful of escape sequences on stdio (`Picker`'s own
/// documented behaviour) -- callers must invoke this after entering the
/// alternate screen but before reading terminal input events, exactly as
/// `ratatui_image::picker::Picker::from_query_stdio` requires.
///
/// A failed query (not a real terminal, unsupported, timed out) is treated
/// identically to a successful query that found nothing: `None` capability,
/// never a propagated error -- ADR-0010's own fallback rule ("si le
/// terminal ne supporte aucun protocole graphique : fallback systématique")
/// already covers this case.
pub fn detect() -> (GraphicsCapability, Option<Picker>) {
    match Picker::from_query_stdio() {
        Ok(picker) => (classify(picker.protocol_type()), Some(picker)),
        Err(error) => {
            tracing::warn!(
                %error,
                "terminal graphics capability query failed; evidence will fall back to an external viewer"
            );
            (GraphicsCapability::None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_iterm2_and_sixel_are_classified_as_inline_capable() {
        assert_eq!(classify(ProtocolType::Kitty), GraphicsCapability::Kitty);
        assert_eq!(classify(ProtocolType::Iterm2), GraphicsCapability::Iterm2);
        assert_eq!(classify(ProtocolType::Sixel), GraphicsCapability::Sixel);
        assert!(classify(ProtocolType::Kitty).supports_inline_images());
        assert!(classify(ProtocolType::Iterm2).supports_inline_images());
        assert!(classify(ProtocolType::Sixel).supports_inline_images());
    }

    #[test]
    fn halfblocks_is_classified_as_not_inline_capable() {
        assert_eq!(classify(ProtocolType::Halfblocks), GraphicsCapability::None);
        assert!(!classify(ProtocolType::Halfblocks).supports_inline_images());
    }
}
