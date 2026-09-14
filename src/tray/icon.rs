//! The tray icon: the Powerhouse logomark, installed into the user's icon
//! theme on demand.
//!
//! The SVG is embedded in the binary and written out on tray start rather than
//! shipped by an installer, so the icon is present however the daemon arrived
//! — a snap, a `cargo build`, or a binary someone copied into `~/.local/bin`.
//! The daemon already embeds its console HTML the same way.

use std::io;
use std::path::{Path, PathBuf};

/// The icon name hosts resolve. Also the `Icon=` in the desktop entry.
pub const ICON_NAME: &str = "ph-reactor";

/// The Powerhouse logomark: the square glyph from the full lockup, with the
/// wordmark removed.
///
/// Recolouring follows the convention Breeze itself uses — a
/// `current-color-scheme` stylesheet plus `fill="currentColor"`. KDE rewrites
/// that stylesheet with the active scheme colour (Breeze light ships
/// `#232629`, breeze-dark `#fcfcfc`), so the mark is black on a light panel
/// and white on a dark one. GNOME recolours symbolic icons by its own
/// mechanism.
///
/// NOTE: this renders blank in renderers that ignore the embedded stylesheet,
/// because `currentColor` then resolves to nothing. That is not a bug — a
/// stock Breeze icon behaves identically. Do not "fix" it with a hard-coded
/// fill; that would defeat the recolouring this exists for.
const LOGOMARK_SVG: &str = include_str!("../../packaging/icons/ph-reactor-symbolic.svg");

/// Writes the icon into the user's hicolor theme when it is missing or stale,
/// and returns the theme root for `IconThemePath`.
///
/// Best-effort by design: a read-only or absent home directory must not stop
/// the daemon, it just means the host falls back to a generic icon.
pub fn ensure_installed() -> Option<PathBuf> {
    let base = dirs::data_dir()?.join("icons/hicolor");
    // `scalable` is what a launcher picks up; `symbolic` is what shells prefer
    // for a tray, and GNOME only recolours icons found under it.
    let targets = [
        base.join("scalable/apps").join(format!("{ICON_NAME}.svg")),
        base.join("symbolic/apps")
            .join(format!("{ICON_NAME}-symbolic.svg")),
    ];
    for path in &targets {
        if let Err(err) = write_if_changed(path, LOGOMARK_SVG) {
            tracing::debug!("could not install tray icon at {}: {err}", path.display());
            return None;
        }
    }
    Some(base)
}

/// Writes `content` to `path` only when it differs, so a restart does not
/// rewrite an unchanged file (and an icon cache is not needlessly invalidated).
fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded asset must be the cropped mark, not the full lockup: the
    /// wordmark paths start at x=144, so a square viewBox is the check that
    /// catches a re-introduced lockup.
    #[test]
    fn embedded_icon_is_the_square_logomark() {
        assert!(LOGOMARK_SVG.contains(r#"viewBox="0 0 114 114""#));
        assert!(!LOGOMARK_SVG.contains("983"), "still the full lockup");
    }

    /// Recolouring depends on both halves of the convention being present.
    /// A hard-coded fill would render everywhere and recolour nowhere, which
    /// is the failure this guards against.
    #[test]
    fn icon_uses_the_breeze_recolouring_convention() {
        assert!(LOGOMARK_SVG.contains(r#"id="current-color-scheme""#));
        assert!(LOGOMARK_SVG.contains("ColorScheme-Text"));
        assert!(LOGOMARK_SVG.contains(r#"fill="currentColor""#));
    }

    #[test]
    fn write_if_changed_is_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("nested/icon.svg");
        write_if_changed(&p, "a").expect("first write");
        let first = std::fs::metadata(&p).expect("stat").modified().ok();
        write_if_changed(&p, "a").expect("second write");
        let second = std::fs::metadata(&p).expect("stat").modified().ok();
        assert_eq!(first, second, "unchanged content should not rewrite");
        write_if_changed(&p, "b").expect("third write");
        assert_eq!(std::fs::read_to_string(&p).expect("read"), "b");
    }
}
