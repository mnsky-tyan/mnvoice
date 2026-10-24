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
// Two files, two surfaces, deliberately different artwork. The tray's frames
// come from `assets/mnvoice-tray.ico`, which is the mark ALONE at every size;
// the launcher card is the 256px frame of `assets/mnvoice.ico`, the exe's own
// icon, which carries the wordmark. They are not the same drawing on purpose:
// Plasma's status popup draws the tray at ~94 device px, where a wordmark is
// unreadable mush, while the launcher icon is drawn at 256px, where the
// wordmark is what tells a user which app it belongs to. `build.rs` bakes each
// file on its own, so an unreadable `mnvoice.ico` costs the launcher its
// `Icon=` and leaves the tray's mark alone - which is why each store carries
// its own availability check below rather than one shared one.
//
// The tray keeps its responsive set. All nine frames of the tray ICO are baked
// and served, and `frame_index` picks the nearest, so a 22px panel icon gets
// the 24px drawing instead of a host upscale of the 256px mark. The card side
// carries one frame, because the launcher PNG is written at exactly one size
// (`hicolor/256x256/apps`).
//
// The colours are not recoloured. A StatusNotifierItem host may ask for
// symbolic recolouring; this icon is a full-colour brand mark, so it is served
// as-is and the host is expected to draw it like the Windows tray does.

/// The tray's frame sizes, smallest first, as baked from
/// `assets/mnvoice-tray.ico`.
pub const SIZES: &[u32] = include!(concat!(env!("OUT_DIR"), "/brand_icon_sizes.rs"));

/// Every tray frame's straight (non-premultiplied) RGBA pixels, concatenated
/// in the same order as [`SIZES`]. Mark-only: no wordmark, so it stays legible
/// at the small sizes a panel and its status popup draw.
const PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/brand_icon.rgba"));

/// The launcher card: one 256x256 frame of straight RGBA, the wordmark-bearing
/// artwork from `assets/mnvoice.ico`. Empty when that file could not be baked,
/// which is the one condition [`card_available`] reports.
const CARD_PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/brand_card.rgba"));

/// The card's side in pixels. The launcher PNG is written into
/// `hicolor/256x256/apps`, so 256 is the only size the card is ever asked for.
const CARD_SIDE: u32 = 256;

/// Every baked tray frame, in the size order [`SIZES`] names (smallest first).
///
/// This is what a StatusNotifierItem host is supposed to be handed: the spec's
/// `IconPixmap` is an *array* of (width, height, pixels) precisely so the host
/// can pick the frame that matches the size it actually draws at. Serving one
/// frame instead forces the host to resample: a panel asking for 32px on a
/// HiDPI screen upscales a 24px raster and the icon looks low-resolution,
/// which is exactly what was reported before this existed.
pub fn all_frames() -> Vec<(u32, u32, Vec<u8>)> {
    SIZES.iter().filter_map(|size| frame(*size)).collect()
}

/// The launcher card: the 256px wordmark-bearing artwork, for the desktop
/// entry's icon file. `None` when the build baked no card, which is exactly
/// when [`card_available`] is false.
pub fn card_frame() -> Option<(u32, u32, Vec<u8>)> {
    if CARD_PIXELS.is_empty() {
        return None;
    }
    Some((CARD_SIDE, CARD_SIDE, CARD_PIXELS.to_vec()))
}

/// The tray's brand mark as RGBA for one frame, or `None` when the build baked
/// no tray icon (a checkout whose `mnvoice-tray.ico` was missing: the build
/// script warns instead of failing, and callers fall back to the orb).
pub fn frame(size: u32) -> Option<(u32, u32, Vec<u8>)> {
    let index = frame_index(size)?;
    let side = *SIZES.get(index)? as usize;
    let offset: usize = SIZES
        .iter()
        .take(index)
        .map(|s| (*s as usize) * (*s as usize) * 4)
        .sum();
    let pixels = PIXELS.get(offset..offset + side * side * 4)?;
    Some((side as u32, side as u32, pixels.to_vec()))
}

/// The nearest baked tray frame to `size`, preferring the larger one on a tie.
///
/// Nearest rather than "always the biggest" so a 22px panel icon is handed the
/// 24px drawing rather than the 256px one, which the host would scale down
/// itself. Nearest rather than "smallest that fits" so a 24px request does not
/// get the 16px drawing blown up. Every frame is mark-only, so whichever one is
/// picked is the same drawing at its native resolution.
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

/// Whether this build carries the launcher card, which is the only thing the
/// desktop entry's `Icon=` line needs: naming a file this build cannot write
/// would leave the desktop drawing its generic placeholder, which is worse
/// than no `Icon=` at all. The tray's own fallback is keyed on
/// [`all_frames`] instead, because the two stores are baked independently.
pub fn card_available() -> bool {
    !CARD_PIXELS.is_empty()
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
        // The card store is checked too. It is not sliced through SIZES, so a
        // short card would otherwise pass every length assertion above.
        assert_eq!(
            CARD_PIXELS.len(),
            (CARD_SIDE * CARD_SIDE * 4) as usize,
            "the launcher card is not one whole 256px square"
        );
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

    /// Dark, opaque pixels in the band the wordmark occupies on a card: the
    /// lower-centre. Zero for mark-only artwork, plenty for a card.
    fn wordmark_ink(width: u32, pixels: &[u8]) -> usize {
        let w = width as usize;
        let (top, bottom) = (w * 62 / 100, w * 82 / 100);
        let (left, right) = (w * 25 / 100, w * 78 / 100);
        let mut dark = 0;
        for y in top..bottom {
            for x in left..right {
                let i = (y * w + x) * 4;
                // Ink is dark and opaque; the card's own background is pale.
                if pixels[i + 3] > 128
                    && pixels[i] < 150
                    && pixels[i + 1] < 150
                    && pixels[i + 2] < 150
                {
                    dark += 1;
                }
            }
        }
        dark
    }

    /// The tray set must be MARK-ONLY at every size. A wordmark rendered into
    /// the ~94 device px box Plasma's status popup draws at is unreadable - the
    /// reported blurry tray icon was exactly that: the panel picked the 128px
    /// wordmark card and squeezed "mnvoice" into a smudge. This test pins the
    /// property that fixes it, at the sizes large enough for a card to fit.
    #[test]
    fn no_tray_frame_carries_the_wordmark() {
        for size in SIZES {
            let (w, _, pixels) = frame(*size).expect("frame resolves");
            let dark = wordmark_ink(w, &pixels);
            assert!(
                dark < 4,
                "the {size}px tray frame carries wordmark ink ({dark} dark px in the \
                 lower band); the tray must be mark-only"
            );
        }
    }

    /// The launcher card keeps the wordmark: at 256px it is legible, and it is
    /// what tells a user which app an icon in the app grid belongs to. Feeding
    /// the mark-only drawing in its place fails this, which is the negative
    /// control for the property the tray test pins.
    #[test]
    fn the_launcher_card_still_carries_the_wordmark() {
        let (w, h, pixels) = card_frame().expect("the card resolves");
        assert_eq!((w, h), (CARD_SIDE, CARD_SIDE));
        let dark = wordmark_ink(w, &pixels);
        assert!(
            dark > 200,
            "the 256px launcher card must keep legible wordmark ink, got {dark}"
        );
    }

    /// The `Icon=` gate and the file it points at are the same condition. A
    /// gate that claimed availability the launcher PNG could not back would
    /// name an icon the desktop cannot resolve.
    #[test]
    fn the_icon_gate_agrees_with_the_card_it_points_at() {
        assert_eq!(card_available(), card_frame().is_some());
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
