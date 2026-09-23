fn main() {
    // The release tag is the single source of truth for the app version.
    // CI pushes tags as refs/tags/vX.Y.Z, so read that when present and fall
    // back to the Cargo version for local builds where no tag exists.
    println!("cargo:rustc-env=MNVOICE_VERSION={}", version());
}

fn version() -> String {
    // GITHUB_REF_NAME is exactly "v0.1.9" on a tag push.
    if let Ok(v) = std::env::var("GITHUB_REF_NAME") {
        if let Some(rest) = v.strip_prefix('v') {
            return rest.to_string();
        }
    }
    env!("CARGO_PKG_VERSION").to_string()
}
