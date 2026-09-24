use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Networking::WinHttp::*;
use windows::core::{w, PCWSTR};

use crate::rest::{self, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS};

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
/// page. Releases are listed newest first, so the first entry names the current
/// version.
const RELEASES_FEED: &str = "https://github.com/mnsky-tyan/mnvoice/releases.atom";

/// Name of the standalone executable asset published alongside the zip.
const EXE_ASSET: &str = "mnvoice.exe";

/// What a published release offers: the version to compare against and the
/// raw exe asset to download.
#[derive(Debug)]
pub struct Release {
    pub version: String,
    pub exe_url: String,
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wptr(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}

/// Minimal GET against an https URL, returning the whole body.
fn http_get(url: &str, accept: &str) -> Result<Vec<u8>, String> {
    unsafe {
        let (host, port, secure, path) = rest::parse_base_url(url)?;
        let session = WinHttpOpen(
            w!("mnvoice-update"),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return Err("cannot create HTTP session".into());
        }
        WinHttpSetTimeouts(session, 0, 15_000, 45_000, 45_000)
            .map_err(|e| format!("set timeouts ({e})"))?;

        let host_w = wide(&host);
        let connect = WinHttpConnect(session, wptr(&host_w), port, 0);
        if connect.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err(format!("cannot connect to {host}"));
        }

        let path_w = wide(&path);
        let request = WinHttpOpenRequest(
            connect,
            w!("GET"),
            wptr(&path_w),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            },
        );
        if request.is_null() {
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("cannot create HTTP request".into());
        }

        // Headers must be attached before the send: anything added afterwards is
        // never put on the wire. Mirrors the REST client's ordering.
        let headers = wide(&format!(
            "Accept: {accept}\r\nUser-Agent: mnvoice-update\r\n"
        ));
        let mut body = Vec::new();
        let result = (|| {
            WinHttpAddRequestHeaders(
                request,
                &headers[..headers.len() - 1],
                WINHTTP_ADDREQ_FLAG_ADD,
            )?;
            WinHttpSendRequest(request, None, None, 0, 0, 0)?;
            WinHttpReceiveResponse(request, std::ptr::null_mut())?;
            // The status line is only readable while the response is still open,
            // so it is collected here next to the body it describes.
            let mut status: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            let mut index = 0u32;
            let _ = WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut std::ffi::c_void),
                &mut len,
                &mut index,
            );
            let mut chunk = [0u8; 16 * 1024];
            loop {
                let mut read = 0u32;
                let ok = WinHttpReadData(
                    request,
                    chunk.as_mut_ptr() as *mut std::ffi::c_void,
                    chunk.len() as u32,
                    &mut read,
                );
                if ok.is_err() || read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read as usize]);
            }
            Ok::<(u32, Vec<u8>), windows::core::Error>((status, body))
        })();
        let _ = WinHttpCloseHandle(request);
        let _ = WinHttpCloseHandle(connect);
        let _ = WinHttpCloseHandle(session);
        let (status, body) = result.map_err(|e| format!("request failed ({e})"))?;

        // A 404 or 403 body must not be mistaken for a release that names no
        // version, or for an executable missing its MZ header.
        if status != 200 {
            let preview: String = String::from_utf8_lossy(&body).chars().take(200).collect();
            return Err(format!("update request returned HTTP {status}: {preview}"));
        }

        Ok(body)
    }
}

/// Version named by the newest entry of the release feed, e.g. the "0.1.10"
/// inside ".../releases/tag/v0.1.10". Tolerates a tag written without the `v`.
pub fn parse_version_from_feed(feed: &str) -> Option<String> {
    let needle = "releases/tag/";
    let mut from = 0;
    while let Some(i) = feed[from..].find(needle) {
        let mut start = from + i + needle.len();
        // Tags are normally written with a leading v, sometimes without.
        if feed[start..].starts_with('v') || feed[start..].starts_with('V') {
            start += 1;
        }
        // A version is digits and dots; stop at the first character that is not.
        let end = feed[start..]
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|e| start + e)
            .unwrap_or(feed.len());
        let version = &feed[start..end];
        if !version.is_empty() && version.contains('.') {
            return Some(version.to_string());
        }
        from = start;
    }
    None
}

/// Download URL for the standalone exe of a specific tag. GitHub serves release
/// assets from a predictable path, so the URL can be derived rather than parsed
/// out of a listing.
fn asset_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{EXE_ASSET}")
}

/// Ask GitHub what the latest published version is, and where its exe lives.
pub fn check_latest() -> Result<Release, String> {
    check_latest_from(RELEASES_FEED)
}

/// The lookup proper, split out so the network path can be driven from a test
/// endpoint without reaching github.com.
fn check_latest_from(feed_url: &str) -> Result<Release, String> {
    let body = http_get(feed_url, "application/atom+xml")?;
    let feed = String::from_utf8_lossy(&body);
    let version =
        parse_version_from_feed(&feed).ok_or("release feed does not name a version")?;
    let tag = format!("v{version}");
    Ok(Release {
        version,
        exe_url: asset_url(&tag),
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
    crate::log(&format!("updated to v{version}, relaunching"));
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
    let copy = std::env::temp_dir().join(format!(
        "{HELPER_IMAGE_PREFIX}{}.exe",
        std::process::id()
    ));
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
        crate::log("update helper: no install to repair");
        return;
    };
    if !swap_interrupted(exe) {
        return;
    }
    if let Err(e) = finish_swap(exe) {
        crate::log(&format!("update helper: {e}"));
        return;
    }
    // This process runs from a copy of the exe, and a running image cannot be
    // deleted, so the copy is renamed aside where the next start's cleanup can
    // take it.
    if let Ok(self_image) = current_exe() {
        let mut aside = self_image.clone().into_os_string();
        aside.push(".old");
        let _ = fs::rename(&self_image, PathBuf::from(aside));
    }

    // --restart hands the hotkey and the single-instance mutex over cleanly.
    if let Err(e) = Command::new(exe).arg("--restart").spawn() {
        crate::log(&format!("update helper: cannot start the new exe ({e})"));
        return;
    }
    crate::log("update helper: finished the interrupted install");
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
    crate::check_for_updates_async(true);
}

/// Called once at startup: tidy up after the previous update, then hand the
/// periodic check to a background thread so nothing blocks the tray. The
/// caller already holds the loaded config, so its AUTO_UPDATE flag is taken
/// from there rather than parsed off disk a second time.
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
    use std::net::TcpListener;
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
        /// ending in `/mnvoice.exe` exactly as GitHub serves them. `status` is
        /// the status line to answer with, so a rejection can be driven the
        /// same way a success is.
        fn once(body: &[u8], path: &str, status: &str) -> MockFeed {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let body = body.to_vec();
            let status = status.to_string();
            let (tx, seen) = mpsc::channel();
            let server = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/atom+xml\r\nConnection: close\r\n"
                    );
                    let _ = stream.write_all(
                        format!("{head}Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
                    );
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                }
            });
            MockFeed {
                url: format!("http://127.0.0.1:{port}/{path}"),
                seen,
                server,
            }
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
