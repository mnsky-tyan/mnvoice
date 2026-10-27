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
// One file, one artwork. Both surfaces read `assets/mnvoice-linux.ico`: the
// brand card, which is the mark with the "mnvoice" wordmark beneath it. The
// wordmark is part of the icon by design - the user asked for it after a
// revision that served a wordmark-free mark, and the artwork is what the brand
// actually looks like.
//
// Why the file carries seventeen frames instead of the usual nine. A host picks
// the smallest frame >= the size it draws at, so a sparse set makes it resample
// and a resampled wordmark is what looks blurred - that was the original
// report. The ladder has a frame at the sizes hosts actually ask for (panel
// 22/24 at 1.25-2x scaling, the ~94-96px status popup, HiDPI 44/48), so the
// pixels drawn are the pixels drawn for that size.
//
// The whole set is served together and the host picks. `frame` maps a requested
// size to the nearest baked frame through `frame_index`, and production reaches
// that pair twice: `all_frames` asks for each baked size exactly, and the
// launcher asks once for 256, the size its PNG is written at. The ladder carries
// a 256px frame so that second read lands on it rather than on the nearest
// neighbour.
//
// The colours are not recoloured. A StatusNotifierItem host may ask for
// symbolic recolouring; this icon is a full-colour brand mark, so it is served
// as-is and the host is expected to draw it like the Windows tray does.

/// The frame sizes, smallest first, as baked from `assets/mnvoice-linux.ico`.
pub const SIZES: &[u32] = include!(concat!(env!("OUT_DIR"), "/brand_icon_sizes.rs"));

/// Every frame's straight (non-premultiplied) RGBA pixels, concatenated in the
/// same order as [`SIZES`]. Each frame is the brand card - mark plus wordmark -
/// drawn natively at its own size.
const PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/brand_icon.rgba"));

/// Every baked frame, in the size order [`SIZES`] names (smallest first).
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

/// The brand card as RGBA for one frame, or `None` when the build baked no icon
/// (a checkout whose `mnvoice-linux.ico` was missing: the build script warns
/// instead of failing). The two callers differ there: the tray falls back to
/// the orb, while the launcher writes no PNG at all and relies on [`available`]
/// keeping `Icon=` out of the desktop entry, so nothing names a file that was
/// never written.
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

/// The nearest baked frame to `size`, preferring the larger one on a tie.
///
/// A request below the smallest frame clamps up and one above the largest
/// clamps down, so any request resolves against a non-empty store. Production
/// reaches it two ways: [`all_frames`] passes each baked size exactly and lets
/// the host pick, and `ensure_icon` asks for 256 - the one request that must not
/// nearest-match, since a ladder without a 256px frame would hand it the 128px
/// drawing to write into `hicolor/256x256/apps`. That a 256px frame exists is
/// pinned by `the_baked_set_has_the_documented_ladder`, not by this function.
/// Every frame is the same drawing at its native resolution, so whichever one
/// is picked is the artwork made for that size.
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

/// Whether this build carries a usable icon store, which is what the desktop
/// entry's `Icon=` line needs: naming a file this build cannot write would
/// leave the desktop drawing its generic placeholder, which is worse than no
/// `Icon=` at all.
///
/// Not a bare emptiness test: the store must also be the exact whole number of
/// squares [`SIZES`] names, which is the arithmetic [`frame`] slices with.
/// Without that check a short store would pass an emptiness test,
/// `ensure_autostart` would write `Icon=mnvoice`, and `frame(256)` would then
/// return `None` on its own bounds check - a desktop entry naming an icon file
/// that was never written.
pub fn available() -> bool {
    let expected: usize = SIZES
        .iter()
        .map(|s| (*s as usize) * (*s as usize) * 4)
        .sum();
    !SIZES.is_empty() && PIXELS.len() == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The baked set is the ICO's documented ladder. A build that lost frames
    /// would still serve *an* icon, which is why the ladder is pinned: the
    /// dense sizes are what stop a host from resampling the wordmark, so
    /// dropping back to the usual nine would silently undo the fix.
    #[test]
    fn the_baked_set_has_the_documented_ladder() {
        assert_eq!(
            SIZES,
            &[16, 20, 24, 28, 32, 36, 40, 44, 48, 56, 64, 72, 84, 96, 112, 128, 256]
        );
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
        assert_eq!(
            frame_index(1000),
            Some(SIZES.len() - 1),
            "above the largest clamps down"
        );
        let (w, _, _) = frame(22).expect("resolves");
        assert_eq!(w, 24, "the 24px drawing, not the 16px one upscaled");
    }

    /// Pixels of the wordmark's "voice" letters, which are the brand blue
    /// `#4c7390` = (76, 115, 144). Nothing else in the artwork is that hue at
    /// that strength: the card is near-white, the mark is near-black, and the
    /// wash behind it is a very pale blue. Counting them is how a test can tell
    /// "the card with the wordmark" from "the mark alone" without rendering the
    /// SVG.
    fn wordmark_blue(pixels: &[u8]) -> usize {
        let mut blue = 0;
        for px in pixels.as_chunks::<4>().0 {
            let (r, g, b, a) = (px[0] as i32, px[1] as i32, px[2] as i32, px[3]);
            if a > 128
                && (r - 76).abs() < 60
                && (g - 115).abs() < 60
                && (b - 144).abs() < 60
                && b > r + 20
            {
                blue += 1;
            }
        }
        blue
    }

    /// The wordmark is part of the icon at every size that can show it. This is
    /// the user's requirement: a revision that served a wordmark-free mark made
    /// the tray icon stop looking like the brand, so the artwork must keep the
    /// word - and the size a status popup draws at (~96 device px) is where it
    /// has to read.
    #[test]
    fn the_wordmark_is_present_where_it_can_be_read() {
        // 40px is the smallest size measured to still resolve the letters;
        // below that the wordmark antialiases into the card, which is fine,
        // because nothing draws the tray that small and expects text.
        for size in SIZES.iter().filter(|s| **s >= 40) {
            let (_, _, pixels) = frame(*size).expect("frame resolves");
            let blue = wordmark_blue(&pixels);
            assert!(
                blue >= 2,
                "the {size}px frame carries no wordmark (0 blue px); the tray icon \
                 must include the 'mnvoice' wordmark"
            );
        }
        // And at the size the popup actually draws, it must be unmistakable.
        let (_, _, pixels) = frame(96).expect("the popup size resolves");
        let blue = wordmark_blue(&pixels);
        assert!(
            blue > 30,
            "the 96px frame is what Plasma's status popup draws; expected a legible \
             wordmark, got {blue} blue px"
        );
    }

    /// The launcher PNG reads the same store, so the icon a desktop shows in its
    /// app grid carries the wordmark at the size it is drawn.
    #[test]
    fn the_launcher_frame_carries_the_wordmark() {
        let (w, h, pixels) = frame(256).expect("the 256px frame resolves");
        assert_eq!((w, h), (256, 256), "the launcher PNG is written at 256");
        let blue = wordmark_blue(&pixels);
        assert!(
            blue > 200,
            "the 256px launcher icon must keep a legible wordmark, got {blue}"
        );
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
