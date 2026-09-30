fn main() {
    // The resource section is a Windows concept; winres would try to run
    // rc.exe everywhere, so this only happens for Windows targets. The version
    // env is emitted unconditionally because the CLI prints it too.
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        // Embed the application icon so it lands in the exe's resource section.
        // Without this, the tray and window class icons fall back to the generic
        // OS application icon and the orb never reaches the binary.
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/mnvoice.ico");
        res.compile().expect("failed to compile windows resources");
        // Emitting any rerun-if instruction stops Cargo's default scan of the
        // package, so the icon the resource section is built from would
        // otherwise stop being watched and a rebuild after editing it would
        // keep the old orb.
        println!("cargo:rerun-if-changed=assets/mnvoice.ico");
    }

    // The release tag is the single source of truth for the app version.
    // CI pushes tags as refs/tags/vX.Y.Z[-platform], so read that when present
    // and fall back to the Cargo version for local builds where no tag exists.
    println!("cargo:rustc-env=MNVOICE_VERSION={}", version());
    // Without this, a rebuild in the same target directory keeps the version
    // baked by an earlier build with a different tag, and the updater compares
    // against a baseline that was never compiled in.
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
}

fn version() -> String {
    // GITHUB_REF_NAME is the tag name on a tag push: "v0.1.9", or with a
    // platform suffix since the three-way split, "v0.1.15-win".
    if let Ok(v) = std::env::var("GITHUB_REF_NAME") {
        if let Some(rest) = v.strip_prefix('v') {
            // Since the three-way split a release tag carries the platform as a
            // suffix ("v0.1.15-win"), but the suffix is not part of the version:
            // update.rs::version_of_tag is the definition of what a tag's version
            // is, and the updater compares what it resolves out of the feed
            // against this value, so both must cut at the same hyphen.
            let cut = rest.find('-').unwrap_or(rest.len());
            return rest[..cut].to_string();
        }
    }
    env!("CARGO_PKG_VERSION").to_string()
}
