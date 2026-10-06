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
        res.set_icon("assets/mnvoice.ico");
        res.compile().expect("failed to compile windows resources");
        // Emitting any rerun-if instruction stops Cargo's default scan of the
        // package, so the icon the resource section is built from would
        // otherwise stop being watched and a rebuild after editing it would
        // keep the old orb.
        println!("cargo:rerun-if-changed=assets/mnvoice.ico");
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
