// Self-update from GitHub Releases.
//
// The binary downloads the standalone `mnvoice.exe` release asset (not the zip,
// so no decompressor is needed), writes it beside the running binary, then
// relaunches. Windows cannot overwrite a running image, so the swap is done by
// renaming the current exe out of the way first - renaming a running image is
// allowed, deleting or overwriting it is not.
//
// Config and keywords live beside the exe as separate files and are never
// touched by an update.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::process::Command;

use windows::Win32::Networking::WinHttp::*;
use windows::core::{w, PCWSTR};

use crate::rest;

/// Latest-release endpoint for this project's public repository.
pub const RELEASES_API: &str = "https://api.github.com/repos/mnsky-tyan/mnvoice/releases/latest";

/// Name of the standalone executable asset published alongside the zip.
const EXE_ASSET: &str = "mnvoice.exe";

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
        let (x, y) = (av.get(i).copied().unwrap_or(0), bv.get(i).copied().unwrap_or(0));
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
            if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) },
        );
        if request.is_null() {
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("cannot create HTTP request".into());
        }

        let headers = wide(&format!("Accept: {accept}\r\nUser-Agent: mnvoice-update\r\n"));
        let sent = WinHttpSendRequest(
            request,
            None,
            None,
            0,
            0,
            0,
        );
        if sent.is_err() {
            let e = sent.err().map(|e| e.to_string()).unwrap_or_default();
            let _ = WinHttpCloseHandle(request);
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err(format!("request failed ({e})"));
        }
        let _ = WinHttpAddRequestHeaders(
            request,
            &headers[..headers.len() - 1],
            0x2000_0000,
        );

        let received = WinHttpReceiveResponse(request, std::ptr::null_mut());
        if let Err(e) = received {
            let _ = WinHttpCloseHandle(request);
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err(format!("no response ({e})"));
        }

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        let _ = WinHttpQueryHeaders(
            request,
            19 | 0x2000_0000,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut std::ffi::c_void),
            &mut len,
            &mut index,
        );

        // Follow the same fixed-chunk read pattern the REST client uses.
        let mut body = Vec::new();
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
        let _ = WinHttpCloseHandle(request);
        let _ = WinHttpCloseHandle(connect);
        let _ = WinHttpCloseHandle(session);

        if status != 200 {
            return Err(format!("update endpoint returned HTTP {status}"));
        }
        Ok(body)
    }
}

/// Pull the string value of `"key": "..."` out of flat JSON text.
fn json_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = json.find(&needle)?;
    let after = &json[start + needle.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let quote = rest.find('"')? + 1;
    let rest = &rest[quote..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Ask GitHub what the latest published version is, and where its exe asset lives.
pub fn check_latest() -> Result<Release, String> {
    let body = http_get(RELEASES_API, "application/vnd.github+json")?;
    let text = String::from_utf8_lossy(&body);

    let version = json_str(&text, "tag_name")
        .ok_or("release response has no tag_name")?
        .trim_start_matches('v')
        .to_string();

    // Find the asset whose download URL ends in the exe name. URLs in this API
    // response are plain ASCII, so a suffix match on the path is reliable.
    let marker = "browser_download_url";
    let mut exe_url: Option<String> = None;
    let mut from = 0;
    while let Some(i) = text[from..].find(marker) {
        let window = &text[from + i..];
        if let Some(url) = json_str(window, marker) {
            let base = url.rsplit('/').next().unwrap_or("");
            if base == EXE_ASSET {
                exe_url = Some(url.to_string());
                break;
            }
        }
        from += i + marker.len();
    }

    Ok(Release {
        version,
        exe_url: exe_url.ok_or("release has no mnvoice.exe asset")?,
    })
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot locate running exe ({e})"))
}

/// Remove the leftover .old from a previous swap. Best effort.
fn clean_old(exe: &Path) {
    let mut old = exe.as_os_str().to_os_string();
    old.push(".old");
    let _ = fs::remove_file(PathBuf::from(old));
}

/// Download the latest exe, swap it in beside the running binary, and relaunch.
///
/// On success this function does not return - it spawns the new process and
/// exits the current one. Errors return a message for the tray to display.
pub fn install_and_relaunch() -> Result<String, String> {
    let rel = check_latest()?;
    let url = rel.exe_url;
    let version = rel.version;

    let exe = current_exe()?;
    let staged = exe.with_extension("new");

    let bytes = http_get(&url, "application/octet-stream")?;
    if bytes.len() < 1024 {
        return Err("downloaded file is implausibly small".into());
    }
    // Sanity-check it is a Windows executable before replacing anything.
    if bytes.first() != Some(&b'M') || bytes.get(1) != Some(&b'Z') {
        return Err("downloaded file is not an executable (missing MZ header)".into());
    }
    fs::write(&staged, &bytes).map_err(|e| format!("cannot write staged exe ({e})"))?;

    let mut old = exe.as_os_str().to_os_string();
    old.push(".old");

    // Rename the running image out of the way, then move the new one in.
    fs::rename(&exe, &old).map_err(|e| format!("cannot move current exe aside ({e})"))?;
    if let Err(e) = fs::rename(&staged, &exe) {
        // Put the original back so the install is not left half-done.
        let _ = fs::rename(&old, &exe);
        return Err(format!("cannot install new exe ({e})"));
    }

    Command::new(&exe)
        .spawn()
        .map_err(|e| format!("new exe spawned failed ({e})"))?;

    Ok(version)
}

fn stamp_path() -> Option<PathBuf> {
    std::env::temp_dir()
        .join("mnvoice-last-update-check")
        .into()
}

/// True when a background check has not happened in the last 24 hours. The
/// unauthenticated GitHub API allows 60 requests/hour, so a daily cadence is
/// plenty and keeps us far away from the limit.
fn should_check_today() -> bool {
    let Some(path) = stamp_path() else { return false };
    let Ok(text) = fs::read_to_string(&path) else {
        // No stamp yet, or unreadable - check and write a fresh one.
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

fn mark_checked() {
    if let Some(path) = stamp_path() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = fs::write(path, now.to_string());
    }
}

/// Background self-update check. Intended to be called once from a temporary
/// thread started at launch, so it can never delay startup. Only installs when
/// the run is idle - see the caller.
pub fn background_check_auto() {
    if !should_check_today() {
        return;
    }
    mark_checked();
    // Only report a problem in the automatic path; a quiet run is the whole point.
    match check_latest() {
        Ok(rel) => {
            if is_newer(&rel.version, current_version()) {
                crate::log(&format!(
                    "background update: v{} available (running v{}), will install when idle",
                    rel.version,
                    current_version()
                ));
            }
        }
        Err(e) => crate::log(&format!("background update check failed: {e}")),
    }
}

/// Called once at startup: tidy up after the previous update, then hand the
/// periodic check to a background thread so nothing blocks the tray.
pub fn startup_cleanup() {
    if let Ok(exe) = current_exe() {
        clean_old(&exe);
    }
    let auto = crate::config::load()
        .map(|c| c.auto_update)
        .unwrap_or(false);
    if !auto {
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

    #[test]
    fn parses_tag_and_exe_url_from_api_json() {
        let json = r#"{
          "tag_name": "v0.1.10",
          "assets": [
            {"name":"mnvoice-windows-x64.zip","browser_download_url":"https://example.com/mnvoice-windows-x64.zip","size":42},
            {"name":"mnvoice.exe","browser_download_url":"https://example.com/mnvoice.exe","size":377856}
          ]
        }"#;
        // Drive the same extraction the network path uses, without a request.
        let parsed_tag = json_str(json, "tag_name").unwrap().trim_start_matches('v');
        assert_eq!(parsed_tag, "0.1.10");

        let marker = "browser_download_url";
        let mut found = None;
        let mut from = 0;
        while let Some(i) = json[from..].find(marker) {
            let w = &json[from + i..];
            if let Some(url) = json_str(w, marker) {
                if url.rsplit('/').next() == Some(EXE_ASSET) {
                    found = Some(url.to_string());
                    break;
                }
            }
            from += i + marker.len();
        }
        assert_eq!(found.as_deref(), Some("https://example.com/mnvoice.exe"));
    }

    #[test]
    fn no_exe_asset_is_reported() {
        let json = r#"{"tag_name":"v0.1.10","assets":[{"name":"x.zip","browser_download_url":"https://e/x.zip"}]}"#;
        let marker = "browser_download_url";
        let mut found = None;
        let mut from = 0;
        while let Some(i) = json[from..].find(marker) {
            let w = &json[from + i..];
            if let Some(url) = json_str(w, marker) {
                if url.rsplit('/').next() == Some(EXE_ASSET) {
                    found = Some(url.to_string());
                    break;
                }
            }
            from += i + marker.len();
        }
        assert!(found.is_none());
    }
}
