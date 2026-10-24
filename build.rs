fn main() {
    // The resource section is a Windows concept; winres would try to run
    // rc.exe everywhere, so this only happens for Windows targets. The tag
    // env is emitted unconditionally because the CLI prints the version on
    // every platform.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        // Embed the application icon so it lands in the exe's resource section.
        // Without this, the tray and window class icons fall back to the generic
        // OS application icon and the orb never reaches the binary.
        let mut res = winres::WindowsResource::new();
        const ICON: &str = "assets/mnvoice.ico";
        res.set_icon(ICON);
        // A real diagnostic, not `expect`. A build-script panic reaches the
        // user as "failed to run custom build command", which names neither
        // the file nor the reason; the rest of this file is careful enough
        // (see the rerun-if note below) that an opaque failure here is the one
        // place a broken checkout gives nothing to act on. The common cause is
        // a missing or unreadable .ico, so that is what it says.
        if let Err(e) = res.compile() {
            println!("cargo:warning=resource compilation failed for {ICON}: {e}");
            println!(
                "cargo:warning=mnvoice embeds its icon from {ICON} at compile \
                 time; a failed checkout, a missing file, or an unreadable .ico \
                 all land here"
            );
            panic!("failed to compile windows resources from {ICON}: {e}");
        }
        // Emitting any rerun-if instruction stops Cargo's default scan of the
        // package, so the icon the resource section is built from would
        // otherwise stop being watched and a rebuild after editing it would
        // keep the old orb.
        println!("cargo:rerun-if-changed=assets/mnvoice.ico");
    }

    // The brand mark reaches the Linux tray and the desktop entry as pixels,
    // not as runtime data files: the exe resource and the launcher card come
    // from `assets/mnvoice.ico`, the tray from the mark-only
    // `assets/mnvoice-tray.ico` (the reason for two files is below), so both
    // are decoded here and their frames are baked in as bytes. The
    // alternative - shipping the .ico files as runtime data and parsing them
    // on every start - would make a released binary depend on a
    // path beside it, which is exactly what the single-file release asset does
    // not have.
    //
    // Only the Linux build pays for this: macOS has no tray icon and Windows
    // gets its icon from winres above.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // Two files, because the two surfaces need different artwork at the
        // same size. Both are mark-only/card renders of the same brand source,
        // and the split exists for a measured reason:
        //
        // `assets/mnvoice-tray.ico` draws the MARK ALONE at every size. The tray
        // is displayed small - Plasma's status popup drew it at ~94 device px
        // (a 47px logical icon) - and a wordmark rendered into a 94px box is
        // unreadable mush. Serving the wordmark card at tray sizes was the
        // reported "blurry" icon: the panel picked the 128px frame and squeezed
        // "mnvoice" into a smudge. Mark-only stays crisp at every size.
        //
        // `assets/mnvoice.ico` carries the full card WITH the wordmark at 64px
        // and above, and mark-only below. That is right for the launcher, where
        // the icon is drawn at 256px and the wordmark is legible - the icon a
        // user reads the app name from in the dock or app grid.
        let tray_icon = "assets/mnvoice-tray.ico";
        let app_icon = "assets/mnvoice.ico";
        let out = std::path::PathBuf::from(
            std::env::var("OUT_DIR").expect("OUT_DIR is always set for a build script"),
        );
        // Each file is baked on its own, so one unreadable .ico costs only its
        // own surface: the tray keeps its mark if the launcher's card cannot be
        // read, and the warning names the file, because "some icon failed" is
        // not actionable. An empty store is written all the same - linux_icon
        // includes both unconditionally, and a reused target dir keeps a
        // previous build's pixels unless they are overwritten here, so an
        // unwriteable .ico would otherwise fail the compile outright or
        // silently ship the wrong artwork.
        let tray = bake_brand_frames(tray_icon);
        let app = bake_brand_frames(app_icon);
        for (path, result, note) in [
            (tray_icon, &tray, "the tray falls back to the orb"),
            (app_icon, &app, "the desktop entry gets no Icon="),
        ] {
            if let Err(e) = result {
                println!("cargo:warning=no brand icon baked in ({path}: {e}); {note}");
            }
        }
        let (sizes, pixels) = tray.unwrap_or_default();
        let (app_sizes, app_pixels) = app.unwrap_or_default();
        // The launcher card is sliced with the tray's size list, so the two
        // files must name the same frames in the same order: a drift would not
        // be visible until card_frame() handed the launcher a misaligned
        // slice. Checked here, where it is a loud warning, instead of a
        // silently wrong launcher icon.
        if !sizes.is_empty() && !app_sizes.is_empty() && sizes != app_sizes {
            println!(
                "cargo:warning={tray_icon} and {app_icon} disagree on frame sizes \
                 ({sizes:?} vs {app_sizes:?}); the launcher icon would be garbled"
            );
        }
        // The pixels travel as a file in OUT_DIR, not as an env var:
        // `cargo:rustc-env` lands on rustc's command line, and a few
        // hundred kilobytes of comma-separated bytes overflowed it
        // ("Argument list too long", measured). `include_bytes!` has no
        // such limit and costs one read at compile time.
        std::fs::write(out.join("brand_icon.rgba"), &pixels).expect("write the baked brand pixels");
        // Only the expression: linux_icon.rs declares the const and
        // includes this, so a `pub const` here would be a declaration
        // inside an expression.
        let sizes_rs = format!(
            "&[{}]",
            sizes
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        std::fs::write(out.join("brand_icon_sizes.rs"), sizes_rs)
            .expect("write the baked brand sizes");
        // The launcher card, same sizes, wordmark included at 64px+.
        std::fs::write(out.join("brand_card.rgba"), &app_pixels)
            .expect("write the baked launcher card");
        println!("cargo:rerun-if-changed={tray_icon}");
        println!("cargo:rerun-if-changed={app_icon}");
    }

    // The release tag is the source of truth for the app version, and it is
    // emitted raw: platform::version_of_tag is the convention for what a tag's
    // version is, and the publish side owns the release shape (and the version
    // checks) in one place - .github/actions/verify-release-tag. This build
    // cannot call it (separate compilation unit), so the filter below is not
    // that rule and is looser than the workflow's: it only stops a non-tag ref
    // name from being baked into the exe, and any wrong version that survives
    // it is the workflow's to catch before it publishes.
    //
    // GITHUB_REF_NAME is the tag name on a tag push ("v0.1.18-win"), but on a
    // branch or pull-request build it is a branch name ("main", "7/merge", and
    // a workflow_dispatch can select any branch at all), so only refs that
    // name a tag are accepted here; everything else gets the crate version,
    // which version_of_tag passes through unchanged.
    //
    // "Names a tag" is v followed by a digit: a branch named "v-new-hotkey"
    // would otherwise bake that name in, and version_of_tag reduces it to an
    // empty version the updater would compare against happily.
    let tag = std::env::var("GITHUB_REF_NAME")
        .ok()
        .filter(|name| name.starts_with('v') && name[1..].starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=MNVOICE_TAG={tag}");
    // Without this, a rebuild in the same target directory keeps the version
    // baked by an earlier build with a different tag, and the updater compares
    // against a baseline that was never compiled in.
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
}

/// Decodes every frame of a brand .ico into straight RGBA and returns its size
/// list plus the concatenated pixels. Called once per file: the tray icon and
/// the launcher card are baked separately, each from its own .ico.
///
/// The pixels travel as a binary file in `OUT_DIR` (read with `include_bytes!`)
/// and the sizes as a small Rust source file (read with `include!`): a
/// `cargo:rustc-env` var would put the bytes on rustc's command line, and a few
/// hundred kilobytes of them overflowed it ("Argument list too long", measured).
///
/// The ICO directory is parsed here rather than trusted from a hardcoded table:
/// a frame count that disagreed with the file would bake a truncated buffer and
/// the icon would be garbage rather than missing. Every frame in both of this
/// repo's icons is a PNG (checked: all nine in each), so the PNG decoder is the
/// only format handled; an embedded BMP frame is refused by name instead of
/// decoded wrongly.
fn bake_brand_frames(path: &str) -> Result<(Vec<usize>, Vec<u8>), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 6 || &data[0..4] != b"\0\0\x01\0" {
        return Err("not an ICO (bad header)".into());
    }
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    let dir_end = 6 + count * 16;
    if count == 0 || data.len() < dir_end {
        return Err("ICO directory truncated".into());
    }

    let mut sizes = Vec::with_capacity(count);
    let mut all = Vec::new();
    for i in 0..count {
        let entry = &data[6 + i * 16..6 + (i + 1) * 16];
        // A zero byte means 256, the format's encoding for the largest size.
        let width = if entry[0] == 0 {
            256
        } else {
            entry[0] as usize
        };
        let height = if entry[1] == 0 {
            256
        } else {
            entry[1] as usize
        };
        let bytes = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let frame = data
            .get(offset..offset.checked_add(bytes).ok_or("frame size overflow")?)
            .ok_or("frame runs past the end of the file")?;
        if frame.len() < 8 || frame[1..4] != *b"PNG" {
            return Err(format!(
                "{width}x{height} frame is not PNG; only PNG frames are supported"
            ));
        }
        let mut decoded = png::Decoder::new(frame)
            .read_info()
            .map_err(|e| format!("{width}x{height}: {e}"))?;
        // The header is available before any buffer is allocated, so a frame
        // that disagrees with its own directory entry is refused without
        // first sizing a buffer from it.
        let info = decoded.info();
        if info.width as usize != width || info.height as usize != height {
            return Err(format!(
                "{width}x{height} frame decodes as {}x{}",
                info.width, info.height
            ));
        }
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err(format!(
                "{width}x{height} frame is {:?}/{:?}, expected RGBA8",
                info.color_type, info.bit_depth
            ));
        }
        let mut buf = vec![0u8; decoded.output_buffer_size()];
        let info = decoded
            .next_frame(&mut buf)
            .map_err(|e| format!("{width}x{height}: {e}"))?;
        buf.truncate(info.buffer_size());
        sizes.push(width);
        all.extend_from_slice(&buf);
    }
    Ok((sizes, all))
}
