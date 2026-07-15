//! In-app auto-update: checks the public GitHub repo (`Duskrosy/astroshel-lcd`) for a
//! newer release, and (on user click) downloads + launches its installer.
//!
//! Self-contained module: only touches the network via `minreq` (tiny HTTPS client) and
//! parses JSON via `serde_json`. Every failure mode in `check()` is swallowed into `None`
//! -- a flaky network connection or an API hiccup must never surface as an error dialog,
//! since this runs silently in the background on every startup and once a day.

use anyhow::Context;
use std::io::Write;

/// A newer release than the running build, with everything the GUI needs to offer it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    /// The release version, WITHOUT the leading 'v' (e.g. "0.5.0").
    pub version: String,
    /// Direct download URL for the `*-setup.exe` asset.
    pub installer_url: String,
    /// The release's GitHub page (for a "What's new" link).
    pub notes_url: String,
}

const REPO_API: &str = "https://api.github.com/repos/Duskrosy/astroshel-lcd/releases/latest";
const USER_AGENT: &str = "astroshel-lcd";
/// Network timeout (seconds) for both the release-metadata check and the installer
/// download -- generous since the download can be several MB over a slow connection.
const CHECK_TIMEOUT_SECS: u64 = 10;
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// Parses a (possibly `v`-prefixed) semver-ish string into a `(major, minor, patch)`
/// tuple for comparison. Any missing or non-numeric component is treated as 0 --
/// deliberately lenient, since this only ever compares GitHub `tag_name`s and
/// `env!("CARGO_PKG_VERSION")`, both of which are well-formed in practice, and a parse
/// hiccup should degrade to "not newer" rather than panic or reject the release.
fn parse_semver(s: &str) -> (u64, u64, u64) {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

/// True if `remote` (a GitHub tag, e.g. "v0.5.0") is a strictly newer version than
/// `current` (e.g. "0.4.0", no 'v' prefix -- what `env!("CARGO_PKG_VERSION")` gives).
fn is_newer(remote: &str, current: &str) -> bool {
    parse_semver(remote) > parse_semver(current)
}

/// Checks GitHub's "latest release" endpoint for `Duskrosy/astroshel-lcd`. Returns
/// `Some(UpdateInfo)` only if the release is strictly newer than the running build AND
/// has an asset named `*-setup.exe` to offer. Any network error, non-200 response,
/// malformed JSON, or missing field is treated the same: silently `None`. Never panics.
pub fn check() -> Option<UpdateInfo> {
    let resp = minreq::get(REPO_API)
        .with_header("User-Agent", USER_AGENT)
        .with_timeout(CHECK_TIMEOUT_SECS)
        .send()
        .ok()?;
    if resp.status_code != 200 {
        log::warn!("update check: GitHub API returned HTTP {}", resp.status_code);
        return None;
    }
    let body = resp.as_str().ok()?;
    let json: serde_json::Value = serde_json::from_str(body).ok()?;

    let tag_name = json.get("tag_name")?.as_str()?;
    let html_url = json.get("html_url")?.as_str()?.to_string();
    let installer_url = json
        .get("assets")?
        .as_array()?
        .iter()
        .find_map(|asset| {
            let name = asset.get("name")?.as_str()?;
            if name.ends_with("-setup.exe") {
                asset.get("browser_download_url")?.as_str().map(str::to_string)
            } else {
                None
            }
        })?;

    let current = env!("CARGO_PKG_VERSION");
    if !is_newer(tag_name, current) {
        return None;
    }

    Some(UpdateInfo {
        version: tag_name.strip_prefix('v').unwrap_or(tag_name).to_string(),
        installer_url,
        notes_url: html_url,
    })
}

/// Downloads `info.installer_url` (streamed, not buffered whole in memory -- installers
/// can be several MB) to `%TEMP%\astroshel-lean-display-setup.exe`, then launches it as a
/// normal (non-blocking) child process. The installer is a GUI wizard that taskkills the
/// running app itself before replacing files, so this returns as soon as the process is
/// spawned -- it does not wait for the installer to finish.
pub fn download_and_run(info: &UpdateInfo) -> anyhow::Result<()> {
    let dest = std::env::temp_dir().join("astroshel-lean-display-setup.exe");

    let resp = minreq::get(&info.installer_url)
        .with_header("User-Agent", USER_AGENT)
        .with_timeout(DOWNLOAD_TIMEOUT_SECS)
        .send_lazy()
        .with_context(|| format!("GET {}", info.installer_url))?;
    if resp.status_code != 200 {
        anyhow::bail!("download failed: HTTP {}", resp.status_code);
    }

    let file = std::fs::File::create(&dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut w = std::io::BufWriter::new(file);
    for item in resp {
        let (byte, _remaining) = item.map_err(|e| anyhow::anyhow!("download stream error: {e}"))?;
        w.write_all(&[byte])?;
    }
    w.flush().with_context(|| format!("flushing {}", dest.display()))?;
    drop(w);

    std::process::Command::new(&dest)
        .spawn()
        .with_context(|| format!("launching {}", dest.display()))?;
    Ok(())
}

/// Spawns a background thread that checks for an update immediately, writes any found
/// update into `slot`, then re-checks once every 24h for as long as the process runs.
/// Never blocks the caller (the spawn itself is fire-and-forget). A single `None` result
/// (e.g. a transient network blip) intentionally does NOT clear a previously-found
/// update out of `slot` -- only a fresh `Some` overwrites it -- so the GUI's banner
/// doesn't flicker away because one background check happened to fail.
pub fn spawn_check(slot: std::sync::Arc<std::sync::Mutex<Option<UpdateInfo>>>) {
    std::thread::spawn(move || loop {
        if let Some(info) = check() {
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(info);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(24 * 60 * 60));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_version_is_newer() {
        assert!(is_newer("0.4.0", "0.3.0"));
        assert!(is_newer("v0.4.0", "0.3.0"));
    }

    #[test]
    fn identical_version_is_not_newer() {
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("v0.4.0", "0.4.0"));
    }

    #[test]
    fn double_digit_minor_compares_numerically_not_lexically() {
        // Lexical string comparison would (wrongly) say "0.10.0" < "0.9.0" since '1' < '9'.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn older_version_is_not_newer() {
        assert!(!is_newer("0.3.0", "0.4.0"));
    }

    #[test]
    fn malformed_remote_defaults_missing_parts_to_zero() {
        // "v1" -> (1, 0, 0), still comparable rather than panicking.
        assert!(is_newer("v1", "0.9.9"));
        assert!(!is_newer("v0", "0.0.1"));
    }
}
