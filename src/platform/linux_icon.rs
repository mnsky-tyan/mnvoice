// The brand mark on the surfaces a Linux desktop reads from a file or a
// pixmap: the tray's StatusNotifierItem icon and the `.desktop` entry the app
// writes for its launcher and autostart row.
//
// Why this module exists at all. On Windows the icon is the exe's resource
// section, so `build.rs`'s winres call is the whole story. Linux has no such
// section, and the two places a desktop looks for an icon are a named file
// (`Icon=` in the entry, resolved through the icon theme) and the tray's own
// `IconPixmap` property. Before this module the Linux tray painted the *orb*
// from `orb_render` and the entry carried no `Icon=` at all, so the brand mark
// - the thing the Windows app and the README show - appeared nowhere on a
// Linux desktop.
//
// One source of truth. The pixels are not a second copy of the artwork: the
// build script decodes `assets/mnvoice.ico` (the same file winres bakes into
// the exe) and writes the frames into `OUT_DIR`, and this module includes them.
// A re-render of the brand asset therefore reaches every platform from one
// file, and the two platforms cannot drift.
//
// The responsive set is preserved. The ICO carries a mark-only drawing at tray
// sizes and the full card with the wordmark at large sizes; both are kept and
// `frame_for` picks the nearest one, exactly as Windows does at the same
// sizes. A single downscaled 256px card would put an illegible wordmark in the
// panel.
//
// The colours are not recoloured. A StatusNotifierItem host may ask for
// symbolic recolouring; this icon is a full-colour brand mark, so it is served
// as-is and the host is expected to draw it like the Windows tray does.

/// Frame sizes, largest last, as baked from `assets/mnvoice.ico`.
pub const SIZES: &[u32] = include!(concat!(env!("OUT_DIR"), "/brand_icon_sizes.rs"));

/// Every frame's straight (non-premultiplied) RGBA pixels, concatenated in the
/// same order as [`SIZES`].
const PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/brand_icon.rgba"));

/// The brand mark as RGBA for one frame, or `None` when the build baked no
/// icon (a checkout whose `.ico` was missing: the build script warns instead
/// of failing, and callers fall back to the orb).
pub fn frame(size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let index = frame_index(size)?;
    let offset = SIZES
        .iter()
        .take(index)
        .map(|s| (*s as usize) * (*s as usize) * 4)
        .sum::<usize>();
    let side = SIZES[index] as usize;
    let end = offset + side * side * 4;
    let pixels = PIXELS.get(offset..end)?;
    Some((side as u32, side as u32, pixels.to_vec()))
}

/// The nearest baked frame to `size`, preferring the larger one on a tie.
///
/// Nearest rather than "always the biggest" for the reason the ICO carries nine
/// sizes: a panel asks for 16 or 22px, and handing it the 256px card makes the
/// host resample a wordmark it cannot resolve. Nearest rather than "smallest
/// that fits" so a 24px request does not get the 16px drawing blown up.
fn frame_index(size: u32) -> Option<usize> {
    if SIZES.is_empty() {
        return None;
    }
    let mut best = 0usize;
    let mut best_distance = u32::MAX;
    for (index, baked) in SIZES.iter().enumerate() {
        let distance = baked.abs_diff(size);
        if distance < best_distance || (distance == best_distance && *baked > SIZES[best]) {
            best = index;
            best_distance = distance;
        }
    }
    Some(best)
}

/// Whether a build carries the brand mark at all.
pub fn available() -> bool {
    !SIZES.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baked set is the ICO's documented nine sizes. A build that lost
    /// frames would still serve *an* icon, which is why the count is pinned.
    #[test]
    fn the_baked_set_has_the_nine_documented_sizes() {
        assert_eq!(SIZES, &[16, 20, 24, 32, 40, 48, 64, 128, 256]);
    }

    /// Every frame is a full square of RGBA bytes, so an offset arithmetic
    /// slip shows up as a short buffer rather than as garbage pixels.
    #[test]
    fn every_frame_is_a_whole_square_of_rgba() {
        let expected: usize = SIZES
            .iter()
            .map(|s| (*s as usize) * (*s as usize) * 4)
            .sum();
        assert_eq!(
            PIXELS.len(),
            expected,
            "frame data does not match the size list"
        );
        for size in SIZES {
            let (w, h, pixels) = frame(*size).expect("every baked size resolves");
            assert_eq!((w, h), (*size, *size));
            assert_eq!(pixels.len(), (w * h * 4) as usize);
        }
    }

    /// A tray request lands on the nearest frame, and both ends of the range
    /// clamp instead of wrapping or returning nothing.
    #[test]
    fn a_requested_size_resolves_to_the_nearest_baked_frame() {
        assert_eq!(frame_index(16), Some(0));
        assert_eq!(frame_index(1), Some(0), "below the smallest clamps up");
        assert_eq!(frame_index(22), Some(2), "22px is nearest 24px");
        assert_eq!(frame_index(1000), Some(8), "above the largest clamps down");
        let (w, _, _) = frame(22).expect("resolves");
        assert_eq!(w, 24, "the 24px drawing, not the 16px one upscaled");
    }

    /// The mark must be visible: an all-transparent or all-one-colour frame
    /// would pass every length check and still ship a blank tray icon.
    #[test]
    fn the_tray_sized_frame_has_visible_ink() {
        let (_, _, pixels) = frame(22).expect("resolves");
        let opaque = pixels.chunks(4).filter(|px| px[3] > 40).count();
        assert!(
            opaque > 100,
            "expected visible ink, got {opaque} opaque pixels"
        );
        let colours = pixels
            .chunks(4)
            .filter(|px| px[3] > 40)
            .map(|px| (px[0], px[1], px[2]))
            .collect::<std::collections::HashSet<_>>();
        assert!(
            colours.len() > 8,
            "a flat fill is not the brand mark: {} distinct colours",
            colours.len()
        );
    }
}
