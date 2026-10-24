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
    // not as a second asset: `assets/mnvoice.ico` is the one brand file this
    // repo owns (the Windows resource section, the tray icon and the desktop
    // icon all come from it), so it is decoded here and the frames are baked
    // in as bytes. The alternative - shipping the .ico as a runtime data file
    // and parsing it on every start - would make a released binary depend on a
    // path beside it, which is exactly what the single-file release asset does
    // not have.
    //
    // Only the Linux build pays for this: macOS has no tray icon and Windows
    // gets its icon from winres above.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        const ICON: &str = "assets/mnvoice.ico";
        match bake_brand_frames(ICON) {
            Ok((sizes, pixels)) => {
                // The pixels travel as a file in OUT_DIR, not as an env var:
                // `cargo:rustc-env` lands on rustc's command line, and a few
                // hundred kilobytes of comma-separated bytes overflowed it
                // ("Argument list too long", measured). `include_bytes!` has no
                // such limit and costs one read at compile time.
                let out = std::path::PathBuf::from(
                    std::env::var("OUT_DIR").expect("OUT_DIR is always set for a build script"),
                );
                std::fs::write(out.join("brand_icon.rgba"), &pixels)
                    .expect("write the baked brand pixels");
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
            }
            // A brand icon that cannot be read must not turn into a build
            // failure: the app still dictates without it. Saying so loudly is
            // the difference between a missing icon and a mystery.
            Err(e) => println!(
                "cargo:warning=no brand icon baked in ({ICON}: {e}); the tray \
                 falls back to the orb and the desktop entry gets no Icon="
            ),
        }
        println!("cargo:rerun-if-changed={ICON}");
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

/// Decodes every frame of `assets/mnvoice.ico` into straight RGBA and returns
/// the size list plus the concatenated pixels, both as decimal strings the
/// crate can read back with `env!` and `include!`-free arithmetic.
///
/// The ICO directory is parsed here rather than trusted from a hardcoded table:
/// a frame count that disagreed with the file would bake a truncated buffer and
/// the icon would be garbage rather than missing. Every frame in this repo's
/// icon is a PNG (checked: all nine), so the PNG decoder is the only format
/// handled; an embedded BMP frame is refused by name instead of decoded wrongly.
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
        let mut buf = vec![0u8; decoded.output_buffer_size()];
        let info = decoded
            .next_frame(&mut buf)
            .map_err(|e| format!("{width}x{height}: {e}"))?;
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
        buf.truncate(info.buffer_size());
        sizes.push(width);
        all.extend_from_slice(&buf);
    }
    Ok((sizes, all))
}
