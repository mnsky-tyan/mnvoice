use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::platform::http::Transport;

/// Repository that publishes mnvoice releases.
const REPO: &str = "mnsky-tyan/mnvoice";

/// GitHub's release feed, `https://github.com/<repo>/releases.atom`.
///
/// This deliberately avoids the GitHub REST API. Unauthenticated API calls are
/// capped at 60 requests per hour *per source IP*, so on a shared, NAT'd or
/// carrier-grade address that budget can already be spent by unrelated traffic
/// and every check would fail with HTTP 403 - exactly the failure observed
/// during verification. The feed is served from the web endpoint: no per-IP
/// quota, no token, and a small machine-readable document instead of a 200 KB
/// page. Releases are listed newest first, but since the three-way split the
/// first entry belongs to whichever platform published last, not necessarily
/// this one - `newest_tag_for_this_platform` picks this platform's entry.
const RELEASES_FEED: &str = "https://github.com/mnsky-tyan/mnvoice/releases.atom";

/// What a published release offers: the version to compare against and the
/// platform's asset to download. The asset name comes from the platform seam,
/// so every platform resolves its own artifact from the same release. The checksum
/// URL names the published `SHA256SUMS` file of the same tag, which is what the
/// download is verified against before anything is installed.
#[derive(Debug)]
pub struct Release {
    pub version: String,
    pub exe_url: String,
    pub sha256_url: String,
}

/// Version baked in at compile time from the release tag.
pub fn current_version() -> &'static str {
    env!("MNVOICE_VERSION")
}

/// True when `a` is strictly newer than `b`, comparing dotted numeric parts.
pub fn is_newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| -> Vec<u32> {
        let cleaned = s.strip_prefix('v').unwrap_or(s);
        cleaned
            .split('.')
            .filter_map(|p| {
                p.trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .collect()
    };
    let (av, bv) = (parts(a), parts(b));
    for i in 0..av.len().max(bv.len()) {
        let (x, y) = (
            av.get(i).copied().unwrap_or(0),
            bv.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Minimal GET against an https URL, returning the whole body.
///
/// Routed through the platform seam so the Windows build and the Linux and
/// macOS ports share one redirect contract. A non-200 is an error here rather
/// than a `Response`, because a 404 or 403 body must not be mistaken for a
/// release that names no version, or for an executable missing its MZ header.
fn http_get(url: &str, accept: &str) -> Result<Vec<u8>, String> {
    let response = crate::platform::http::NativeTransport.get(url, accept)?;
    if response.status != 200 {
        let preview: String = String::from_utf8_lossy(&response.body).chars().take(200).collect();
        return Err(format!(
            "update request returned HTTP {}: {preview}",
            response.status
        ));
    }
    Ok(response.body)
}

/// A downloaded release image is installed only if the release's own published
/// checksum for it verifies: the `SHA256SUMS` file of the same tag is fetched,
/// the line for the asset actually downloaded is read out of it, and the bytes
/// are hashed and compared. Every failure - a missing file, an unlisted asset,
/// a hash that disagrees - is a refusal, so the only outcome of a tampered or
/// truncated download is that nothing installs.
///
/// Called before anything is moved on disk, which is what makes that true: a
/// rejected download never reaches the staged file, let alone the exe path.
fn verify_download(exe_url: &str, sums_url: &str, bytes: &[u8]) -> Result<(), String> {
    let asset = asset_name_from_url(exe_url)?;
    let body = http_get(sums_url, "text/plain")?;
    let expected = expected_hash(&String::from_utf8_lossy(&body), &asset)?;
    let actual = sha256::hex_digest(bytes);
    if actual != expected {
        return Err(format!(
            "downloaded {asset} does not match its published checksum 
             (expected {expected}, got {actual}); refusing to install"
        ));
    }
    Ok(())
}

/// The asset name a URL asks for - its last path segment. Taken from the URL
/// rather than assumed, so the sums lookup and the download can never disagree
/// about what was fetched.
fn asset_name_from_url(url: &str) -> Result<String, String> {
    let name = url
        .rsplit('/')
        .next()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("cannot name the asset in {url}"))?;
    Ok(name.trim().to_string())
}

/// The published sums line for `asset`.
///
/// The format is what `sha256sum` writes: `<64 hex><whitespace><name>` per line,
/// with every released file listed, so the line for this asset has to be picked
/// out. Two spaces are the GNU spelling and one space plus an asterisk is the
/// `-b` spelling; both name the same file, and uppercase hex is accepted because
/// so is it. A file that does not list the asset is a refusal, not a skip: an
/// absent line is how a release that carries no sums would announce itself, and
/// guessing past it would install unchecked.
fn expected_hash(sums: &str, asset: &str) -> Result<String, String> {
    for line in sums.lines() {
        let Some((hash, rest)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let hash = hash.trim().to_ascii_lowercase();
        let is_hex = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if !is_hex {
            continue;
        }
        let name = rest.trim().trim_start_matches('*').trim();
        if name == asset {
            return Ok(hash);
        }
    }
    Err(format!("{asset} is not listed in the published checksums"))
}

/// SHA-256 (FIPS 180-4).
///
/// Hand-rolled on purpose: the update path needs one 32-byte digest of one file
/// once a day, and a hashing crate with its feature tree would cost more than
/// the check is worth. The published FIPS vectors pin the behaviour, so this is
/// thirty-odd lines of well-tested arithmetic rather than a dependency.
mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    /// Lowercase hex of the SHA-256 digest of `data`.
    pub fn hex_digest(data: &[u8]) -> String {
        let mut h = H0;

        // Padding: a single 1 bit, zeros, then the length in bits as a 64-bit
        // big-endian word, all rounded up to a whole number of 64-byte blocks.
        let bit_len = (data.len() as u64).wrapping_mul(8);
        let mut msg = Vec::with_capacity(data.len() + 72);
        msg.extend_from_slice(data);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());

        for block in msg.chunks_exact(64) {
            let mut w = [0u32; 64];
            for (i, word) in block.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }

            let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
            let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *slot = slot.wrapping_add(v);
            }
        }

        let mut out = String::with_capacity(64);
        for word in h {
            for byte in word.to_be_bytes() {
                out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
                out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
            }
        }
        out
    }
}

/// Every `/releases/tag/<tag>` the feed names, in feed order (newest first).
///
/// Each entry contributes exactly one such link - the entry `<id>` is written
/// as `.../Repository/<n>/<tag>`, which does not match - and the list is what
/// makes the three-way release split safe. There are up to three releases per
/// version now, one per platform, so "the newest entry" no longer identifies
/// this platform's release: a Windows install that resolved the newest one
/// could derive a URL from the Linux or macOS release's tag, whose repository
/// carries no `mnvoice.exe`. A hyphen is part of a tag, so `v0.1.15-linux`
/// survives intact, while the `&#39;` entities the real feed escapes still end
/// it and a bare `/releases/tag/` link is dropped for naming no version.
fn feed_tags(feed: &str) -> Vec<String> {
    let needle = "releases/tag/";
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(i) = feed[from..].find(needle) {
        let start = from + i + needle.len();
        // A tag is alphanumerics with dots and hyphens; anything else ends it.
        let stop = |c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-';
        let end = feed[start..]
            .find(stop)
            .map(|e| start + e)
            .unwrap_or(feed.len());
        let tag = &feed[start..end];
        if tag.contains('.') {
            tags.push(tag.to_string());
        }
        from = start;
    }
    tags
}

/// The version a tag names, with the leading `v` and the platform suffix
/// removed: "v0.1.15-win" -> "0.1.15". This is what the binary compares
/// against the version baked in at compile time.
fn version_of_tag(tag: &str) -> String {
    let bare = tag.strip_prefix('v').unwrap_or(tag);
    bare.split('-').next().unwrap_or(bare).to_string()
}

/// The suffix this platform's release tags carry.
#[cfg(windows)]
const fn platform_release_suffix() -> &'static str {
    "win"
}

#[cfg(target_os = "linux")]
const fn platform_release_suffix() -> &'static str {
    "linux"
}

#[cfg(target_os = "macos")]
const fn platform_release_suffix() -> &'static str {
    "macos"
}

/// True when `tag` names this platform's release.
///
/// A bare `vX.Y.Z` tag is accepted on Windows only, because every release
/// published before the three-way split was Windows and those installs must
/// keep updating.
fn tag_is_ours(tag: &str) -> bool {
    let bare = tag.strip_prefix('v').unwrap_or(tag);
    match bare.split_once('-') {
        Some((_, suffix)) => suffix == platform_release_suffix(),
        None => cfg!(windows),
    }
}

/// The newest release tag in the feed that belongs to this platform.
fn newest_tag_for_this_platform(feed: &str) -> Option<String> {
    feed_tags(feed).into_iter().find(|t| tag_is_ours(t))
}

/// Version named by the newest entry of the release feed that belongs to this
/// platform, e.g. the "0.1.10" inside ".../releases/tag/v0.1.10". Tolerates a
/// tag written without the `v`, and one carrying a platform suffix.
///
/// The production path needs the tag, not just the version - it is what the
/// asset URL is derived from - so this is the test-time spelling of
/// `newest_tag_for_this_platform`.
#[cfg(test)]
pub fn parse_version_from_feed(feed: &str) -> Option<String> {
    newest_tag_for_this_platform(feed).map(|t| version_of_tag(&t))
}

/// Download URL for the standalone exe of a specific tag, platform suffix
/// included. GitHub serves release assets from a predictable path, so the URL
/// can be derived rather than parsed out of a listing.
fn asset_url(tag: &str) -> String {
    format!(
        "https://github.com/{REPO}/releases/download/{tag}/{}",
        crate::platform::asset_name()
    )
}

/// Asset holding the published checksums of a release, one SHA-256 per released
/// file. The release workflow writes it for every Windows release it publishes,
/// next to the exe and the zip.
const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// URL of a tag's checksum file, derived from the same tag as the exe, so a
/// download is checked against the sums of the release it came from.
fn checksum_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{CHECKSUMS_ASSET}")
}

/// Ask GitHub what the latest published version is, and where its exe and the
/// release's published checksums live.
pub fn check_latest() -> Result<Release, String> {
    check_latest_from(RELEASES_FEED)
}

/// The lookup proper, split out so the network path can be driven from a test
/// endpoint without reaching github.com.
fn check_latest_from(feed_url: &str) -> Result<Release, String> {
    let body = http_get(feed_url, "application/atom+xml")?;
    let feed = String::from_utf8_lossy(&body);
    // The tag is resolved, not rebuilt from the version: since the split a
    // version alone does not name a release, only a tag does, and the entry it
    // belongs to decides which platform the asset lives under.
    let tag = newest_tag_for_this_platform(&feed)
        .ok_or("release feed names no release for this platform")?;
    Ok(Release {
        version: version_of_tag(&tag),
        exe_url: asset_url(&tag),
        sha256_url: checksum_url(&tag),
    })
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot locate running exe ({e})"))
}

/// Argument that starts a second, short-lived copy of this exe to finish an
/// install the starting process could not complete. The install to repair
/// follows it on the command line. Handled at the top of main, before the
/// single-instance mutex, which the app itself already holds.
pub const FINISH_UPDATE_ARG: &str = "--finish-update";

/// Prefix of the recovery helper's copy of this exe in the temp directory.
///
/// The helper needs an image name of its own because it is a second copy of
/// this exe: every relaunch terminates the other instances of this exe by
/// image name, so a helper that shared the app's image name would be killed by
/// the very restart it exists to outlive, and the install it was started to
/// recover would be left with no exe in it at all.
const HELPER_IMAGE_PREFIX: &str = "mnvoice-updater-";

/// Replace the running exe with the just-downloaded image.
///
/// Windows refuses to overwrite a running image but does allow renaming it, so
/// the current exe is moved to `.old`, the new one is moved into its place, and
/// the running process is relaunched. If the relaunch cannot be started the
/// original is put back, so an interrupted update never leaves a broken install.
///
/// The two renames cannot be made into one operation, so the exe path is
/// briefly absent between them and only a live process could put an image back
/// there. A second, short-lived copy of this exe is therefore started before
/// the swap, holding the read end of a pipe this process keeps open: if this
/// process is killed between the two renames, that copy finishes the swap
/// instead of leaving the install directory with no exe in it.
pub fn install_and_relaunch(rel: &Release, busy: impl Fn() -> bool) -> Result<(), String> {
    let url = &rel.exe_url;
    let version = &rel.version;

    let exe = current_exe()?;

    let bytes = http_get(url, "application/octet-stream")?;

    // Checked against the release's own published checksum before anything
    // moves: a download that cannot be verified never reaches the staged file.
    verify_download(url, &rel.sha256_url, &bytes)?;

    // The helper waits on the other end of this pipe, so keeping the handle open
    // is how it learns this process is still the one installing: exiting, or being
    // killed, closes it all the same. It is in place before anything is moved, so
    // it is already watching when the swap starts, and it is stopped the moment
    // this process has resolved the install either way.
    let mut helper = spawn_helper(&exe)?;

    let old = match stage_and_swap(&bytes, &exe, &busy) {
        Ok(old) => old,
        Err(e) => {
            let _ = helper.kill();
            reap_helpers();
            return Err(e);
        }
    };

    // --restart hands the hotkey and the single-instance mutex over cleanly, and
    // exiting here releases them from this side too - the new image takes this
    // exe's path, so the old process must not keep running the renamed one.
    if let Err(e) = Command::new(&exe).arg("--restart").spawn() {
        let _ = fs::rename(&old, &exe);
        let _ = helper.kill();
        reap_helpers();
        return Err(format!("cannot start the new exe ({e})"));
    }
    crate::windows_app::log(&format!("updated to v{version}, relaunching"));
    let _ = helper.kill();
    reap_helpers();
    std::process::exit(0);
}

/// Start the recovery helper from a copy of this exe that carries its own image
/// name.
///
/// A different image name can only come from a different file, so a copy of
/// this exe is laid down in the temp directory and the install it has to
/// repair is named on its command line, since the copy's own path says nothing
/// about where the real exe lives. Both are taken from the installer, which is
/// the only process that knows either.
fn spawn_helper(exe: &Path) -> Result<std::process::Child, String> {
    let copy = helper_copy_path();
    fs::copy(exe, &copy).map_err(|e| format!("cannot stage the update helper ({e})"))?;
    match Command::new(&copy)
        .arg(FINISH_UPDATE_ARG)
        .arg(exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => Ok(child),
        Err(e) => {
            let _ = fs::remove_file(&copy);
            Err(format!("cannot start the update helper ({e})"))
        }
    }
}

/// Path of the helper's copy of this exe. Every installer takes the path of
/// its own pid, so a second install never has to share one.
fn helper_copy_path() -> PathBuf {
    std::env::temp_dir().join(format!("{HELPER_IMAGE_PREFIX}{}.exe", std::process::id()))
}

/// Take the helper copies left in the temp directory away. Best effort, like
/// clean_stale: Windows will not let a copy still in use go, and one still in
/// use has already had its chance to do its work.
fn reap_helpers() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(HELPER_IMAGE_PREFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Finish an install the process that started this one could not complete.
///
/// This process is not the image being replaced, so it outlives the death of the
/// one that is. That matters only while an install is half-done: the swap has
/// moved the running image aside and has not moved the new one in, which leaves
/// the install directory with no exe at all and nothing running that could
/// repair it. This code repairs it, and does nothing at all for every swap its
/// parent finished or rolled back, so a healthy install is never taken over.
pub fn finish_install(install: Option<&Path>) {
    // The parent holds the only other end of this pipe, so a closed pipe means
    // it is gone: killed, crashed, or finished. Waiting here also means this
    // process never touches the files while its parent could still be swapping.
    let mut pipe = std::io::stdin();
    let mut drain = std::io::sink();
    let _ = std::io::copy(&mut pipe, &mut drain);

    let Some(exe) = install else {
        crate::windows_app::log("update helper: no install to repair");
        return;
    };
    if !swap_interrupted(exe) {
        return;
    }
    if let Err(e) = finish_swap(exe) {
        crate::windows_app::log(&format!("update helper: {e}"));
        return;
    }

    // --restart hands the hotkey and the single-instance mutex over cleanly.
    if let Err(e) = Command::new(exe).arg("--restart").spawn() {
        crate::windows_app::log(&format!("update helper: cannot start the new exe ({e})"));
        return;
    }
    crate::windows_app::log("update helper: finished the interrupted install");
}

/// The state an interrupted swap leaves behind: nothing at the exe path, with
/// the new image still staged beside it.
fn swap_interrupted(exe: &Path) -> bool {
    !exe.exists() && staged_path(exe).exists()
}

/// Move the staged image into the exe path, which the interrupted process could
/// not.
fn finish_swap(exe: &Path) -> Result<(), String> {
    fs::rename(staged_path(exe), exe).map_err(|e| format!("cannot finish the install ({e})"))
}

/// Write the new image beside the running one, then move it over, but only if
/// nothing started recording while the bytes were in flight.
fn stage_and_swap(bytes: &[u8], exe: &Path, busy: &dyn Fn() -> bool) -> Result<PathBuf, String> {
    check_exe_payload(bytes)?;
    let staged = staged_path(exe);
    fs::write(&staged, bytes).map_err(|e| format!("cannot write staged exe ({e})"))?;

    if busy() {
        let _ = fs::remove_file(&staged);
        return Err("install skipped, a dictation session started during the download".into());
    }

    match swap_in(&staged, exe) {
        Ok(old) => Ok(old),
        Err(e) => {
            // The install did not land, so a full copy of the new binary must not
            // be left sitting beside the exe for the next attempt to trip over.
            let _ = fs::remove_file(&staged);
            Err(e)
        }
    }
}

/// Path the download is written to before it is moved over the running exe.
fn staged_path(exe: &Path) -> PathBuf {
    exe.with_extension("new")
}

/// Move the staged image into place, restoring the original if it fails half
/// way through.
fn swap_in(staged: &Path, exe: &Path) -> Result<PathBuf, String> {
    let mut old = exe.as_os_str().to_os_string();
    old.push(".old");
    let old = PathBuf::from(old);

    fs::rename(exe, &old).map_err(|e| format!("cannot move current exe aside ({e})"))?;
    if let Err(e) = fs::rename(staged, exe) {
        // Put the original back so the install is not left half-done.
        let _ = fs::rename(&old, exe);
        return Err(format!("cannot install new exe ({e})"));
    }
    Ok(old)
}

/// A payload that is not a Windows executable must never reach the exe path.
fn check_exe_payload(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 1024 {
        return Err("downloaded file is implausibly small".into());
    }
    if bytes.first() != Some(&b'M') || bytes.get(1) != Some(&b'Z') {
        return Err("downloaded file is not an executable (missing MZ header)".into());
    }
    Ok(())
}

/// Remove what an interrupted or deferred update left behind: the image the
/// running exe was moved aside to, and a download that never got swapped in.
/// Best effort.
fn clean_stale(exe: &Path) {
    let mut old = exe.as_os_str().to_os_string();
    old.push(".old");
    let _ = fs::remove_file(PathBuf::from(old));
    let _ = fs::remove_file(staged_path(exe));
}

fn stamp_path() -> Option<PathBuf> {
    std::env::temp_dir()
        .join("mnvoice-last-update-check")
        .into()
}

/// The cadence decision for one stamp file: a missing, unreadable or nonsense
/// stamp means "never checked", and only a stamp older than a day allows another
/// check.
fn should_check_at(stamp: &Path) -> bool {
    let Ok(text) = fs::read_to_string(stamp) else {
        return true;
    };
    let Ok(last) = text.trim().parse::<u64>() else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(last) > 86_400
}

/// True when a background check has not happened in the last 24 hours. Daily is
/// plenty, and keeps traffic towards github.com trivial.
fn should_check_today() -> bool {
    match stamp_path() {
        Some(path) => should_check_at(&path),
        None => false,
    }
}

fn mark_checked() {
    if let Some(path) = stamp_path() {
        mark_checked_at(&path);
    }
}

fn mark_checked_at(stamp: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = fs::write(stamp, now.to_string());
}

/// Background self-update check. Called once from a temporary thread started
/// at launch, so it can never delay startup. Only installs when idle - see
/// the caller in main.rs.
pub fn background_check_auto() {
    if !should_check_today() {
        return;
    }
    mark_checked();
    // quiet: an automatic run reports only problems, never balloons.
    crate::windows_app::check_for_updates_async(true);
}

/// Called once at startup: tidy up after the previous update, then hand the
/// periodic check to a background thread so nothing blocks the tray. AUTO_UPDATE
/// is read independently of the rest of the config, so the caller passes it in
/// already resolved rather than this deciding what "valid" means.
pub fn startup_cleanup(auto_update: bool) {
    if let Ok(exe) = current_exe() {
        clean_stale(&exe);
    }
    reap_helpers();
    if !auto_update {
        return;
    }
    std::thread::spawn(|| {
        // Give the user time to settle before any network or UI activity.
        std::thread::sleep(std::time::Duration::from_secs(60));
        background_check_auto();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    #[test]
    fn newer_patch_wins() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.2.0", "0.1.99"));
    }

    #[test]
    fn same_or_older_loses() {
        assert!(!is_newer("0.1.9", "0.1.9"));
        assert!(!is_newer("0.1.8", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));
    }

    #[test]
    fn malformed_versions_do_not_panic() {
        assert!(!is_newer("abc", "1.0.0"));
        assert!(!is_newer("", ""));
        assert!(is_newer("v2.0.0", "1.0.0"));
    }

    // --- reading the release feed -----------------------------------------

    /// Shaped like GitHub's real releases.atom: newest entry first, each one
    /// carrying its tag as a `/releases/tag/vX` link, and the summary of older
    /// releases escaping entities the way the feed does.
    fn sample_feed() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom" xml:lang="en-US">
          <id>tag:github.com,2008:https://github.com/mnsky-tyan/mnvoice/releases</id>
          <title>Release notes from mnvoice</title>
          <entry>
            <id>tag:github.com,2008:https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.11</id>
            <link rel="alternate" type="text/html" href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.11"/>
            <title>v0.1.11</title>
          </entry>
          <entry>
            <id>tag:github.com,2008:https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.10</id>
          </entry>
        </feed>"#
            .to_string()
    }

    #[test]
    fn reads_the_tag_out_of_the_feed_and_derives_the_exe_url() {
        let version = parse_version_from_feed(&sample_feed()).unwrap();
        // The `v` is stripped so the tag can be compared against the version
        // build.rs baked from that same tag.
        assert_eq!(version, "0.1.11");
        assert_eq!(
            asset_url(&format!("v{version}")),
            "https://github.com/mnsky-tyan/mnvoice/releases/download/v0.1.11/mnvoice.exe"
        );
        assert_eq!(
            checksum_url(&format!("v{version}")),
            "https://github.com/mnsky-tyan/mnvoice/releases/download/v0.1.11/SHA256SUMS"
        );
    }

    #[test]
    fn a_published_tag_beats_the_version_the_binary_knows_itself_to_be() {
        let version = parse_version_from_feed(&sample_feed()).unwrap();
        assert!(
            is_newer(&version, "0.1.10"),
            "a v0.1.10 build must recognise this release as the update it wants"
        );
    }

    #[test]
    fn a_feed_with_no_tag_is_reported() {
        // A 404 or an error page names no tag, so the caller must hear about it
        // rather than fall back to some assumed version.
        let page = "<!DOCTYPE html><html><body>404 Not Found</body></html>";
        assert!(parse_version_from_feed(page).is_none());
        assert!(parse_version_from_feed("<feed><title>empty</title></feed>").is_none());
    }

    #[test]
    fn a_tag_written_without_the_v_still_resolves() {
        let feed = r#"<entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/1.2.3"/></entry>"#;
        assert_eq!(parse_version_from_feed(feed).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn html_entities_in_the_feed_do_not_bleed_into_the_version() {
        // The real feed writes an id as "...releases/tag/v0.1.10&#39;, <summary>",
        // so the tag is followed immediately by an escaped entity. A scanner
        // that ran past the digits and dots would swallow it into the version.
        let feed = r#"<id>tag:github.com,2008:https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.10&#39;, older release</id>"#;
        assert_eq!(parse_version_from_feed(feed).as_deref(), Some("0.1.10"));
    }

    #[test]
    fn tag_links_with_no_version_are_skipped() {
        // The feed can carry a tag link that names no version - a bare
        // "/releases/tag/" href - ahead of the release it belongs to, so the
        // scan has to keep looking.
        let feed = r#"<a href="/releases/tag/">all tags</a><a href="/mnsky-tyan/mnvoice/releases/tag/v0.1.9">v"#;
        assert_eq!(parse_version_from_feed(feed).as_deref(), Some("0.1.9"));
    }

    /// The feed as it looks since the three-way split: three releases carrying
    /// the same version, newest first, only one of them this platform's.
    fn per_platform_feed() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <feed xmlns="http://www.w3.org/2005/Atom" xml:lang="en-US">
          <entry>
            <id>tag:github.com,2008:Repository/1/v0.1.15-macos</id>
            <link rel="alternate" type="text/html" href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.15-macos"/>
            <title>v0.1.15-macos</title>
          </entry>
          <entry>
            <id>tag:github.com,2008:Repository/2/v0.1.15-linux</id>
            <link rel="alternate" type="text/html" href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.15-linux"/>
            <title>v0.1.15-linux</title>
          </entry>
          <entry>
            <id>tag:github.com,2008:Repository/3/v0.1.15-win</id>
            <link rel="alternate" type="text/html" href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.15-win"/>
            <title>v0.1.15-win</title>
          </entry>
          <entry>
            <link rel="alternate" type="text/html" href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.14"/>
          </entry>
        </feed>"#
            .to_string()
    }

    #[test]
    fn the_suffix_is_removed_from_the_version_it_compares_against() {
        assert_eq!(version_of_tag("v0.1.15-win"), "0.1.15");
        assert_eq!(version_of_tag("v0.1.14"), "0.1.14");
        assert_eq!(version_of_tag("0.1.15-linux"), "0.1.15");
    }

    #[test]
    fn the_split_resolves_this_platforms_release_and_derives_its_url() {
        // The newest entry is another platform's release. Resolving by tag
        // rather than by newest entry is what keeps the derived URL pointing at
        // a repository that actually carries mnvoice.exe.
        let feed = per_platform_feed();
        assert_eq!(
            feed_tags(&feed),
            vec![
                "v0.1.15-macos".to_string(),
                "v0.1.15-linux".to_string(),
                "v0.1.15-win".to_string(),
                "v0.1.14".to_string(),
            ]
        );
        let tag = newest_tag_for_this_platform(&feed).unwrap();
        let bare = tag.strip_prefix('v').unwrap_or(tag.as_str());
        let suffix = bare.split_once('-').map(|(_, s)| s);
        if cfg!(windows) {
            assert_eq!(suffix, Some("win"));
        } else {
            assert_eq!(suffix, Some(platform_release_suffix()));
        }
        assert_eq!(parse_version_from_feed(&feed).as_deref(), Some("0.1.15"));
        assert_eq!(
            asset_url(tag.as_str()),
            format!("https://github.com/mnsky-tyan/mnvoice/releases/download/{tag}/mnvoice.exe")
        );
    }

    #[test]
    fn a_feed_of_other_platforms_only_names_no_release_of_ours() {
        // A Windows install reading a feed whose newest releases all belong to
        // other platforms must report no update, not invent one from a tag
        // whose assets are absent.
        let feed = r#"<feed>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.2.0-macos"/></entry>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.2.0-linux"/></entry>
        </feed>"#;
        assert_eq!(feed_tags(feed).len(), 2);
        if cfg!(windows) {
            assert!(newest_tag_for_this_platform(feed).is_none());
            assert!(parse_version_from_feed(feed).is_none());
        }
    }

    #[test]
    fn a_bare_tag_after_the_split_still_updates_windows() {
        // The legacy shape, and the shape the current Windows installs run:
        // a bare tag is Windows' own release, however new the feed is.
        let feed = r#"<feed>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.2.0-linux"/></entry>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.15-win"/></entry>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.14"/></entry>
        </feed>"#;
        let tag = newest_tag_for_this_platform(feed).unwrap();
        assert_eq!(tag, "v0.1.15-win");
        assert!(is_newer(&version_of_tag(&tag), "0.1.14"));
    }

    #[test]
    fn the_newest_release_of_our_platform_wins_over_older_entries() {
        // Suffixed tags of the same platform: the newest entry is the one taken,
        // exactly as before the split, and it beats the installed version.
        let feed = r#"<feed>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.16-win"/></entry>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.15-win"/></entry>
          <entry><link href="https://github.com/mnsky-tyan/mnvoice/releases/tag/v0.1.14"/></entry>
        </feed>"#;
        assert_eq!(parse_version_from_feed(feed).as_deref(), Some("0.1.16"));
    }

    // --- fetching over the wire -------------------------------------------

    /// A real HTTP endpoint on loopback that answers one request with `body`, so
    /// the production fetch path can be driven without reaching github.com. It
    /// records what the client actually put on the wire, so headers are asserted
    /// from the request rather than from the source.
    struct MockFeed {
        url: String,
        seen: mpsc::Receiver<String>,
        server: std::thread::JoinHandle<()>,
    }

    impl MockFeed {
        /// `path` is what the client asks for and what the URL ends in, so the
        /// release lookup gets a path ending in `/latest` and an asset gets one
        /// ending in `/mnvoice.exe` exactly as GitHub serves them. The response is
        /// whatever `respond` writes on the accepted connection, so a redirect can
        /// be driven exactly the way a plain body is.
        fn serve(
            path: &str,
            respond: impl FnOnce(&mut TcpStream) + Send + 'static,
        ) -> MockFeed {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, seen) = mpsc::channel();
            let server = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                    respond(&mut stream);
                }
            });
            MockFeed {
                url: format!("http://127.0.0.1:{port}/{path}"),
                seen,
                server,
            }
        }

        /// Answers one request with `body`. `status` is the status line to answer
        /// with, so a rejection can be driven the same way a success is.
        fn once(body: &[u8], path: &str, status: &str) -> MockFeed {
            let body = body.to_vec();
            let status = status.to_string();
            MockFeed::serve(path, move |stream| {
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/atom+xml\r\nConnection: close\r\n"
                );
                let _ = stream.write_all(
                    format!("{head}Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
                );
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            })
        }

        /// Answers one request with a redirect to `location` and no body, the way
        /// GitHub answers a request for a release asset: the asset is served from
        /// somewhere else, so the client has to go and get it there.
        fn redirecting_to(location: &str, path: &str) -> MockFeed {
            let location = location.to_string();
            MockFeed::serve(path, move |stream| {
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            })
        }

        /// The request the client really sent, once the response is consumed.
        fn request(self) -> String {
            let _ = self.server.join();
            self.seen.recv().unwrap_or_default()
        }
    }

    #[test]
    fn a_release_feed_is_read_end_to_end_over_http() {
        let feed = MockFeed::once(sample_feed().as_bytes(), "mnsky-tyan/mnvoice/releases.atom", "200 OK");
        let rel = check_latest_from(&feed.url).unwrap();
        assert_eq!(rel.version, "0.1.11");
        assert_eq!(
            rel.exe_url,
            "https://github.com/mnsky-tyan/mnvoice/releases/download/v0.1.11/mnvoice.exe"
        );
        assert_eq!(
            rel.sha256_url,
            "https://github.com/mnsky-tyan/mnvoice/releases/download/v0.1.11/SHA256SUMS",
            "the sums come from the same tag the exe came from"
        );

        // Headers only take effect while the request is still being composed, so
        // the Accept and User-Agent headers have to be on the wire.
        let sent = feed.request().to_lowercase();
        assert!(
            sent.contains("accept: application/atom+xml"),
            "request was: {sent}"
        );
        assert!(sent.contains("user-agent: mnvoice-update"), "request was: {sent}");
    }

    #[test]
    fn a_feed_the_server_rejects_says_so_with_its_status() {
        // A 403 or 404 body is not a release, so naming the status beats a
        // misleading "the feed names no version".
        let feed = MockFeed::once(
            b"<feed><title>rate limited</title></feed>",
            "mnsky-tyan/mnvoice/releases.atom",
            "403 Forbidden",
        );
        let err = check_latest_from(&feed.url).unwrap_err();
        assert!(err.contains("403"), "unexpected error: {err}");
        assert!(
            !err.contains("does not name a version"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_unreachable_feed_reports_an_error_instead_of_a_version() {
        // Bind and immediately release a port, so nothing is listening there.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = check_latest_from(&format!(
            "http://127.0.0.1:{port}/mnsky-tyan/mnvoice/releases.atom"
        ))
        .unwrap_err();
        assert!(!err.is_empty(), "an unreachable feed must produce a message");
    }

    // --- the checksum the download is verified against ---------------------

    #[test]
    fn sha256_reproduces_the_published_vectors() {
        use super::sha256;
        // FIPS 180-4 A.1-A.3: the empty string, "abc" and the 56-byte case that
        // straddles a block boundary, plus the million-'a' case that needs the
        // length field past one block.
        assert_eq!(
            sha256::hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256::hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256::hex_digest(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn the_sums_line_of_the_asset_that_was_asked_for_is_the_one_read() {
        // Real sums files list every released file, in the GNU two-space
        // spelling, and sha256sum -b writes one space and an asterisk instead.
        let sums = "\
b810fff67ec7d67ab0804704ea52b678180dbd6e4d55b02ccb244f167378ab70  other.exe\n\
B810FFF67EC7D67AB0804704EA52B678180DBD6E4D55B02CCB244F167378AB70 *mnvoice.exe\n";
        assert_eq!(
            super::expected_hash(sums, "mnvoice.exe").unwrap(),
            "b810fff67ec7d67ab0804704ea52b678180dbd6e4d55b02ccb244f167378ab70",
            "the line naming the requested asset decides, not the first one"
        );
    }

    #[test]
    fn a_sums_file_that_does_not_list_the_asset_is_a_refusal() {
        // A release without a sums entry for this asset must not be installed
        // through: absent line is how a release with no sums would announce
        // itself, and skipping past it would install unchecked.
        let sums = "deadbeef  some-other-file\n";
        let err = super::expected_hash(sums, "mnvoice.exe").unwrap_err();
        assert!(
            err.contains("mnvoice.exe"),
            "error should name the asset: {err}"
        );
    }

    #[test]
    fn a_download_that_matches_its_published_checksum_is_accepted() {
        // The digest below is an independent sha256sum of these exact bytes, so
        // the check is pinned against a real tool rather than the code under test.
        let bytes = b"mnvoice fake exe payload";
        let sums =
            "b810fff67ec7d67ab0804704ea52b678180dbd6e4d55b02ccb244f167378ab70  mnvoice.exe\n";
        let served = MockFeed::once(
            sums.as_bytes(),
            "mnsky-tyan/mnvoice/releases/download/v0.1.17-win/SHA256SUMS",
            "200 OK",
        );
        super::verify_download(
            &format!("{}/mnvoice.exe", served.url.trim_end_matches("/SHA256SUMS")),
            &served.url,
            bytes,
        )
        .unwrap();
        let asked = served.request();
        assert!(
            asked.contains("/SHA256SUMS"),
            "the sums of the same tag are what must be fetched: {asked}"
        );
    }

    #[test]
    fn a_download_whose_bytes_do_not_match_is_refused() {
        let sums =
            "b810fff67ec7d67ab0804704ea52b678180dbd6e4d55b02ccb244f167378ab70  mnvoice.exe\n";
        let served = MockFeed::once(
            sums.as_bytes(),
            "mnsky-tyan/mnvoice/releases/download/v0.1.17-win/SHA256SUMS",
            "200 OK",
        );
        let err = super::verify_download(
            &format!("{}/mnvoice.exe", served.url.trim_end_matches("/SHA256SUMS")),
            &served.url,
            b"something else entirely",
        )
        .unwrap_err();
        assert!(err.contains("does not match"), "unexpected error: {err}");
        assert!(
            err.contains("b810fff67ec7d67ab0804704ea52b678180dbd6e4d55b02ccb244f167378ab70"),
            "the expected hash belongs in the message: {err}"
        );
        assert!(
            err.contains("refusing to install"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_missing_checksums_file_is_a_refusal() {
        // A 404 on the sums asset is the honest signal that this release cannot
        // be checked. That is a refusal, not a reason to install unverified.
        let served = MockFeed::once(
            b"Not Found",
            "mnsky-tyan/mnvoice/releases/download/v0.1.17-win/SHA256SUMS",
            "404 Not Found",
        );
        let err = super::verify_download(
            &format!("{}/mnvoice.exe", served.url.trim_end_matches("/SHA256SUMS")),
            &served.url,
            b"mnvoice fake exe payload",
        )
        .unwrap_err();
        assert!(err.contains("404"), "unexpected error: {err}");
    }

    #[test]
    fn an_asset_with_no_name_in_its_url_is_a_refusal() {
        // A URL that names no asset cannot be looked up in the sums file, and
        // guessing a name would check the wrong line.
        let err = super::asset_name_from_url(
            "https://github.com/mnsky-tyan/mnvoice/releases/download/v0.1.17-win/",
        )
        .unwrap_err();
        assert!(err.contains("asset"), "unexpected error: {err}");
        assert_eq!(
            super::asset_name_from_url("https://host/tag/mnvoice.exe").unwrap(),
            "mnvoice.exe"
        );
    }

    /// The file the real v0.1.16-win release publishes beside its assets, byte
    /// for byte: one SHA-256 per released file, in the two-space GNU spelling.
    /// Real data, so the check is exercised against the shape and the digests a
    /// real release serves rather than a convenient fiction.
    const PUBLISHED_SUMS: &str = concat!(
        "d154b6145425e8e2886dc57ba4374b2b22aea61958ec140ee401b8194a82450d  mnvoice.exe\n",
        "bf2675cff36f62d220034356839e957c616455ea9b848d87b602162ac715b73f  mnvoice-windows-x64.zip\n",
    );

    /// The digest an independent sha256sum gives of the image below, the bytes
    /// somebody else's build would put on the wire in place of a published one.
    const SUBSTITUTED_IMAGE_DIGEST: &str =
        "d57007075a967423381dfe84ecfc472b4f487b02e1616ef23ffb02c1a2ce5e91";

    #[test]
    fn a_download_that_does_not_match_its_published_checksum_never_reaches_the_staged_file() {
        // The installer's own entry point, driven the way the product drives it:
        // a release resolves, its image is requested over the same wire a real
        // asset arrives on, and what comes back is a plausible Windows
        // executable the release never published. It is exactly the payload the
        // installer would otherwise move over the running exe, so the only thing
        // standing between it and the exe path is the check that runs first.
        let mut substituted = exe_image(2);
        substituted[8192] = 0xff;
        let asset = MockFeed::once(
            &substituted,
            "mnsky-tyan/mnvoice/releases/download/v0.1.16-win/mnvoice.exe",
            "200 OK",
        );
        let sums = MockFeed::once(
            PUBLISHED_SUMS.as_bytes(),
            "mnsky-tyan/mnvoice/releases/download/v0.1.16-win/SHA256SUMS",
            "200 OK",
        );
        let rel = Release {
            version: "0.1.16".into(),
            exe_url: asset.url.clone(),
            sha256_url: sums.url.clone(),
        };
        let exe = current_exe().unwrap();
        let running = fs::read(&exe).unwrap();

        let err = install_and_relaunch(&rel, || false).unwrap_err();

        // A failed check has to be legible: both hashes, and the refusal.
        assert!(err.contains("does not match"), "unexpected error: {err}");
        assert!(
            err.contains("d154b6145425e8e2886dc57ba4374b2b22aea61958ec140ee401b8194a82450d"),
            "the published hash belongs in the message: {err}"
        );
        assert!(
            err.contains(SUBSTITUTED_IMAGE_DIGEST),
            "the hash of what actually arrived belongs in the message: {err}"
        );
        // And nothing on disk moved, which is what checking before the swap
        // buys: no staged download, no image moved aside, and the exe this
        // process is running is still the one it was.
        assert!(
            !staged_path(&exe).exists(),
            "a rejected download reached the staged file"
        );
        let mut old = exe.clone().into_os_string();
        old.push(".old");
        assert!(
            !PathBuf::from(old).exists(),
            "the running image was moved aside"
        );
        assert_eq!(
            fs::read(&exe).unwrap(),
            running,
            "the running image changed"
        );
        let _ = asset.request();
        let _ = sums.request();
    }

    #[test]
    fn the_line_read_is_the_one_the_exe_url_names_never_an_assumed_asset() {
        // A release publishes the zip beside the exe, and a build whose asset
        // is the zip has to be checked against the zip's line. The name comes
        // from the URL that was downloaded, so the same sums file answers both
        // requests correctly - and a request the file does not list is a
        // refusal rather than a fall back to some other line.
        let bytes = b"mnvoice zip asset payload";
        let sums = "d227de818b4a898fc363f853d2946240b30965881941f53d1813104b66eee2b3  mnvoice-windows-x64.zip\n";
        let base = "mnsky-tyan/mnvoice/releases/download/v0.1.16-win";

        let zip = MockFeed::once(sums.as_bytes(), &format!("{base}/SHA256SUMS"), "200 OK");
        super::verify_download(
            &format!(
                "{}/mnvoice-windows-x64.zip",
                zip.url.trim_end_matches("/SHA256SUMS")
            ),
            &zip.url,
            bytes,
        )
        .unwrap();
        let asked = zip.request();
        assert!(asked.contains("/SHA256SUMS"), "request was: {asked}");

        // The same file, asked for the exe it never lists.
        let exe = MockFeed::once(sums.as_bytes(), &format!("{base}/SHA256SUMS"), "200 OK");
        let err = super::verify_download(
            &format!("{}/mnvoice.exe", exe.url.trim_end_matches("/SHA256SUMS")),
            &exe.url,
            b"mnvoice fake exe payload",
        )
        .unwrap_err();
        assert!(
            err.contains("mnvoice.exe is not listed"),
            "the asset that was asked for is the one that has to be listed: {err}"
        );
        let _ = exe.request();
    }

    #[test]
    fn an_asset_download_follows_the_redirect_to_the_host_the_asset_lives_on() {
        // GitHub answers a request for a release asset with a 302 naming the
        // host the asset is actually served from, so a download only succeeds if
        // that redirect is followed. The asset endpoint is bound first, because
        // the redirect has to name its address.
        let cdn = MockFeed::once(
            &downloaded_exe(),
            "release-assets/mnvoice.exe",
            "200 OK",
        );
        let asset = MockFeed::redirecting_to(
            &cdn.url,
            "mnsky-tyan/mnvoice/releases/download/v0.1.11/mnvoice.exe",
        );

        let bytes = http_get(&asset.url, "application/octet-stream").unwrap();
        assert_eq!(
            bytes,
            downloaded_exe(),
            "the bytes must come from the redirect target, not the redirecting host"
        );

        // The asset was really fetched from the target the 302 named, so this
        // fails if the client ever stops following it.
        let asked_of_the_cdn = cdn.request();
        assert!(
            asked_of_the_cdn.contains("/release-assets/mnvoice.exe"),
            "redirect target request was: {asked_of_the_cdn}"
        );
        let _ = asset.request();
    }

    #[test]
    fn the_asset_the_lookup_points_at_is_downloaded_and_swapped_in() {
        // The whole install path short of the relaunch: read the release, follow
        // the asset URL it derives, stage what comes back, move it over the
        // running image - and leave the personal config beside the exe alone.
        let asset = MockFeed::once(
            &downloaded_exe(),
            "mnsky-tyan/mnvoice/releases/download/v0.1.11/mnvoice.exe",
            "200 OK",
        );
        let release = MockFeed::once(
            sample_feed().as_bytes(),
            "mnsky-tyan/mnvoice/releases.atom",
            "200 OK",
        );

        let rel = check_latest_from(&release.url).unwrap();
        assert_eq!(rel.version, "0.1.11");
        assert_eq!(
            rel.exe_url,
            "https://github.com/mnsky-tyan/mnvoice/releases/download/v0.1.11/mnvoice.exe",
            "the url must be the raw exe, never the zip"
        );

        let (dir, exe) = install_folder("swap-live");
        let env_before = fs::read(dir.join("mnvoice.env")).unwrap();
        let kw_before = fs::read(dir.join("keywords.txt")).unwrap();
        let bytes = http_get(&asset.url, "application/octet-stream").unwrap();
        let aside = stage_and_swap(&bytes, &exe, &|| false).unwrap();

        assert_eq!(fs::read(&exe).unwrap(), downloaded_exe());
        assert_eq!(fs::read(&aside).unwrap(), installed_exe());
        assert_eq!(fs::read(dir.join("mnvoice.env")).unwrap(), env_before);
        assert_eq!(fs::read(dir.join("keywords.txt")).unwrap(), kw_before);
        let sent = asset.request();
        assert!(sent.contains("application/octet-stream"), "request was: {sent}");
        let _ = fs::remove_dir_all(&dir);
    }

    // --- swapping the running image ----------------------------------------

    /// Stands in for a published exe: an MZ image large enough to clear the
    /// plausibility floor, with a marker byte so tests can tell which build a file
    /// is.
    fn exe_image(tag: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 16 * 1024];
        bytes[0] = b'M';
        bytes[1] = b'Z';
        bytes[2] = tag;
        bytes
    }

    /// The image that is already running, tagged `1` to tell it from a download.
    fn installed_exe() -> Vec<u8> {
        exe_image(1)
    }

    /// A freshly downloaded image, tagged `2` to tell it from what is running.
    fn downloaded_exe() -> Vec<u8> {
        exe_image(2)
    }

    /// A folder the way mnvoice's exe is really laid out: the running image plus
    /// the personal config files that live beside it.
    fn install_folder(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mnvoice-install-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("mnvoice.exe");
        fs::write(&exe, installed_exe()).unwrap();
        fs::write(dir.join("mnvoice.env"), "API_KEY=not-my-key\nAUTO_UPDATE=1\n").unwrap();
        fs::write(dir.join("keywords.txt"), "kubernetes\nherdr\n").unwrap();
        (dir, exe)
    }

    #[test]
    fn the_new_image_takes_the_exe_path_and_the_running_one_moves_aside() {
        let (dir, exe) = install_folder("swap-ok");
        let aside = stage_and_swap(&downloaded_exe(), &exe, &|| false).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), downloaded_exe());
        assert_eq!(fs::read(&aside).unwrap(), installed_exe());
        // The staged file is consumed, not left lying around.
        assert!(!dir.join("mnvoice.new").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn personal_config_beside_the_exe_is_untouched_by_a_swap() {
        let (dir, exe) = install_folder("swap-config");
        let env_before = fs::read(dir.join("mnvoice.env")).unwrap();
        let kw_before = fs::read(dir.join("keywords.txt")).unwrap();
        stage_and_swap(&downloaded_exe(), &exe, &|| false).unwrap();
        assert_eq!(fs::read(dir.join("mnvoice.env")).unwrap(), env_before);
        assert_eq!(fs::read(dir.join("keywords.txt")).unwrap(), kw_before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_idle_gate_is_read_after_the_download_and_stops_the_swap() {
        // The gate read before the download started said Idle, so the only thing
        // protecting a transcript that begins mid-download is this second read.
        let (dir, exe) = install_folder("swap-busy");
        let err = stage_and_swap(&downloaded_exe(), &exe, &|| {
            // It is read once the payload has landed, not before: the file it
            // decides the fate of is already on disk.
            assert!(dir.join("mnvoice.new").exists(), "gate read too early");
            // A session started while the bytes were in flight.
            true
        })
        .unwrap_err();
        assert!(
            err.contains("dictation session started during the download"),
            "unexpected error: {err}"
        );
        // Nothing was swapped: still the old image, the running file was never
        // moved out of the way, and the download it already paid for is not
        // left behind in the install folder.
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        assert!(!dir.join("mnvoice.exe.old").exists());
        assert!(!dir.join("mnvoice.new").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_payload_that_is_not_a_windows_exe_is_rejected_before_anything_is_written() {
        let (dir, exe) = install_folder("swap-not-exe");
        for bad in [vec![0u8; 16 * 1024], b"MZ".to_vec(), vec![b'P'; 16 * 1024]] {
            let err = stage_and_swap(&bad, &exe, &|| false).unwrap_err();
            assert!(
                err.contains("executable") || err.contains("implausibly small"),
                "unexpected error: {err}"
            );
        }
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        assert!(!dir.join("mnvoice.new").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_blocked_before_the_rename_leaves_the_running_image_intact() {
        let (dir, exe) = install_folder("swap-blocked-aside");
        // Something else already owns the .old slot, so the running image cannot
        // be moved aside.
        fs::create_dir_all(dir.join("mnvoice.exe.old")).unwrap();
        let err = stage_and_swap(&downloaded_exe(), &exe, &|| false).unwrap_err();
        assert!(err.contains("move current exe aside"), "unexpected error: {err}");
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        // The install did not land, so the download is not left on disk either.
        assert!(!dir.join("mnvoice.new").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftovers_from_an_earlier_update_are_reaped_at_startup() {
        let (dir, exe) = install_folder("stale-leftovers");
        fs::write(dir.join("mnvoice.exe.old"), installed_exe()).unwrap();
        fs::write(staged_path(&exe), downloaded_exe()).unwrap();
        clean_stale(&exe);
        assert!(!dir.join("mnvoice.exe.old").exists());
        assert!(!dir.join("mnvoice.new").exists());
        // The running image and the personal config beside it are left alone.
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        assert!(dir.join("mnvoice.env").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repaired_swap_leaves_nothing_for_the_next_start_to_tidy_up() {
        // The helper that repairs an interrupted swap is a copy of this exe under
        // an image name of its own, so repairing it leaves two things behind that
        // both belong to the update and neither to the app: the backup the swap
        // moved the old image to, and the helper copy itself. The next start is
        // what takes them, and it has to take all of them.
        let (dir, exe) = install_folder("swap-repaired-tidy");
        fs::rename(&exe, dir.join("mnvoice.exe.old")).unwrap();
        fs::write(staged_path(&exe), downloaded_exe()).unwrap();
        let helper_copy = helper_copy_path();
        fs::write(&helper_copy, installed_exe()).unwrap();

        finish_swap(&exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), downloaded_exe());

        // What the next start does before the window exists.
        clean_stale(&exe);
        reap_helpers();
        assert!(!dir.join("mnvoice.exe.old").exists());
        assert!(!staged_path(&exe).exists());
        assert_eq!(fs::read(&exe).unwrap(), downloaded_exe());

        // Nothing that names the helper survives either, whatever pid it came
        // from: a copy left in the temp directory is debris the app did not
        // install and the user never asked for.
        let leftovers = fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(HELPER_IMAGE_PREFIX))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "helper copies left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staged_path_that_cannot_be_written_aborts_the_install() {
        let (dir, exe) = install_folder("swap-blocked-stage");
        fs::create_dir_all(dir.join("mnvoice.new")).unwrap();
        let err = stage_and_swap(&downloaded_exe(), &exe, &|| false).unwrap_err();
        assert!(err.contains("cannot write staged exe"), "unexpected error: {err}");
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_that_fails_half_way_puts_the_running_image_back() {
        let (dir, exe) = install_folder("swap-rollback");
        // The running image moves aside fine, but the staged image is nowhere to
        // be found, so the install cannot land.
        let staged = dir.join("not-a-dir").join("mnvoice.new");
        let err = swap_in(&staged, &exe).unwrap_err();
        assert!(err.contains("cannot install new exe"), "unexpected error: {err}");
        // The original is back in place, not stranded as mnvoice.exe.old.
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        assert!(!dir.join("mnvoice.exe.old").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_interrupted_between_the_renames_is_finished_by_another_process() {
        // The swap moved the running image aside and was then interrupted, so
        // there is no exe at all: nothing running could start, and nothing that
        // starts could have repaired it. Another process moving the staged image
        // in is the only repair, and it has to work from this state alone.
        let (dir, exe) = install_folder("swap-interrupted");
        fs::rename(&exe, dir.join("mnvoice.exe.old")).unwrap();
        fs::write(staged_path(&exe), downloaded_exe()).unwrap();
        assert!(swap_interrupted(&exe), "the interrupted state must be recognised");

        finish_swap(&exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), downloaded_exe());
        // The image it was moving aside is still there to go back to, and the
        // staged file is consumed.
        assert_eq!(fs::read(dir.join("mnvoice.exe.old")).unwrap(), installed_exe());
        assert!(!staged_path(&exe).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_that_landed_or_was_rolled_back_is_left_alone() {
        // Both a completed swap and a rolled-back one leave the exe path
        // populated, so there is nothing to finish: taking over an install its
        // parent already resolved is never the finisher's business.
        let (dir, exe) = install_folder("swap-landed");
        stage_and_swap(&downloaded_exe(), &exe, &|| false).unwrap();
        assert!(!swap_interrupted(&exe), "a completed install is not half-done");
        assert_eq!(fs::read(&exe).unwrap(), downloaded_exe());
        let _ = fs::remove_dir_all(&dir);

        let (dir, exe) = install_folder("swap-rolled-back");
        // The staged image cannot be moved in, so the original is put back.
        let staged = dir.join("not-a-dir").join("mnvoice.new");
        assert!(swap_in(&staged, &exe).is_err());
        assert!(!swap_interrupted(&exe), "a rolled-back install is not half-done");
        assert_eq!(fs::read(&exe).unwrap(), installed_exe());
        let _ = fs::remove_dir_all(&dir);
    }

    // --- update cadence ----------------------------------------------------

    fn stamp_file(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("mnvoice-stamp-{name}"));
        let _ = fs::remove_file(&path);
        path
    }

    fn write_stamp(path: &Path, secs_ago: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        fs::write(path, (now - secs_ago).to_string()).unwrap();
    }

    #[test]
    fn a_never_checked_install_checks_today() {
        let stamp = stamp_file("fresh");
        assert!(should_check_at(&stamp));
    }

    #[test]
    fn a_recent_check_suppresses_another_one_and_a_stale_one_does_not() {
        let stamp = stamp_file("cadence");
        mark_checked_at(&stamp);
        assert!(!should_check_at(&stamp), "a check just made must not repeat");

        write_stamp(&stamp, 86_399);
        assert!(!should_check_at(&stamp), "under a day must stay quiet");

        write_stamp(&stamp, 86_401);
        assert!(should_check_at(&stamp), "more than a day must check again");
    }

    #[test]
    fn an_unreadable_or_nonsense_stamp_is_treated_as_never_checked() {
        let stamp = stamp_file("nonsense");
        fs::write(&stamp, "yesterday").unwrap();
        assert!(should_check_at(&stamp));
    }
}
