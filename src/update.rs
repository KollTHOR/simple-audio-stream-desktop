//! In-app direct updater.
//!
//! Flow: query the GitHub releases API for the newest release that ships a Windows installer,
//! compare its publish time against this build's timestamp (baked by `build.rs`), download the
//! installer to a temp dir, verify its SHA-256 (when the release publishes one), then launch it
//! silently and let the caller exit so the installer can replace the running binaries.
//!
//! HTTP uses the OS `curl` (present on Windows 10+, macOS, Linux) so no TLS stack is linked in;
//! JSON is parsed with `serde_json` and hashing with `sha2`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository whose releases we track.
pub const REPO: &str = "KollTHOR/simple-audio-stream-desktop";
/// Only assets with this prefix (and a `.exe` suffix) are treated as installer payloads.
pub const INSTALLER_PREFIX: &str = "ASLC-Node-Setup-";

/// This build's timestamp (Unix seconds), baked at compile time.
pub fn build_epoch() -> u64 {
    env!("ASLC_BUILD_EPOCH").parse().unwrap_or(0)
}

/// This build's git short SHA (or `unknown`).
pub fn git_sha() -> &'static str {
    env!("ASLC_GIT_SHA")
}

/// This build's crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The release tag this binary was built for (empty for local/dev builds).
pub fn release_tag() -> &'static str {
    env!("ASLC_RELEASE_TAG")
}

/// One published release that carries a usable installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    /// Human version (tag without a leading `v`).
    pub display_version: String,
    pub prerelease: bool,
    /// Publish time as Unix seconds, when parseable.
    pub published_at: Option<u64>,
    pub notes: String,
    pub asset_name: String,
    pub asset_url: String,
    /// URL of the matching `*.sha256` asset, when the release publishes one.
    pub sha_url: Option<String>,
}

impl Release {
    /// True when this release was published meaningfully after this build. A release whose tag is
    /// the very tag this binary was built from is never "newer" (a nightly is published moments
    /// after its commit, so the timestamp alone would otherwise offer the binary its own release).
    pub fn is_newer_than_this_build(&self) -> bool {
        release_is_newer(&self.tag, self.published_at, release_tag(), build_epoch())
    }
}

/// Pure: is `rel_tag` a newer release than a binary stamped `own_tag`/`own_epoch`?
fn release_is_newer(rel_tag: &str, published: Option<u64>, own_tag: &str, own_epoch: u64) -> bool {
    if !own_tag.is_empty() && rel_tag == own_tag {
        return false;
    }
    // A 60 s slack absorbs clock skew between the runner and the local build.
    published.map(|t| t > own_epoch + 60).unwrap_or(false)
}

fn curl_base() -> Command {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "-H", "User-Agent: ASLC-Node"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn curl_text(url: &str) -> Result<Vec<u8>, String> {
    let mut cmd = curl_base();
    cmd.args(["-H", "Accept: application/vnd.github+json", url]);
    let out = cmd
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "request failed (curl {}): {}",
            out.status,
            err.trim()
        ));
    }
    Ok(out.stdout)
}

fn curl_download(url: &str, dest: &Path) -> Result<(), String> {
    let mut cmd = curl_base();
    cmd.arg("-o").arg(dest).arg(url);
    let status = cmd
        .status()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !status.success() {
        return Err(format!("download failed (curl {status})"));
    }
    Ok(())
}

/// Fetch the newest published release that carries a `ASLC-Node-Setup-*.exe` asset.
pub fn fetch_latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let body = curl_text(&url)?;
    let text = std::str::from_utf8(&body).map_err(|e| format!("bad UTF-8 from GitHub: {e}"))?;
    parse_releases(text)?.ok_or_else(|| "no release with a Windows installer was found".to_string())
}

/// Pure: pick the most recently published release that carries an installer asset.
fn parse_releases(json_text: &str) -> Result<Option<Release>, String> {
    let json: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| format!("bad GitHub JSON: {e}"))?;
    let arr = json
        .as_array()
        .ok_or_else(|| "unexpected GitHub response".to_string())?;

    let mut best: Option<Release> = None;
    for r in arr {
        let tag = r.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        let assets = r.get("assets").and_then(|v| v.as_array());
        let Some(assets) = assets else { continue };
        let Some(asset) = assets.iter().find(|x| {
            x.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.starts_with(INSTALLER_PREFIX) && n.ends_with(".exe"))
                .unwrap_or(false)
        }) else {
            continue;
        };
        let asset_name = asset
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let asset_url = asset
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if tag.is_empty() || asset_url.is_empty() {
            continue;
        }
        let want_sha = format!("{asset_name}.sha256");
        let sha_url = assets
            .iter()
            .find(|x| x.get("name").and_then(|n| n.as_str()) == Some(want_sha.as_str()))
            .and_then(|x| x.get("browser_download_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let rel = Release {
            tag: tag.to_string(),
            display_version: tag.trim_start_matches('v').to_string(),
            prerelease: r
                .get("prerelease")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            published_at: r
                .get("published_at")
                .and_then(|v| v.as_str())
                .and_then(parse_iso8601),
            notes: r
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            asset_name,
            asset_url,
            sha_url,
        };
        best = Some(match best {
            // Keep the more recently published one.
            Some(b) if b.published_at.unwrap_or(0) >= rel.published_at.unwrap_or(0) => b,
            _ => rel,
        });
    }
    Ok(best)
}

/// Download the release's installer into a temp dir, verifying its SHA-256 when available.
pub fn download(release: &Release) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("aslc-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create temp dir: {e}"))?;
    let dest = dir.join(&release.asset_name);
    let _ = std::fs::remove_file(&dest);
    curl_download(&release.asset_url, &dest)?;

    if let Some(sha_url) = &release.sha_url {
        if let Ok(text) = curl_text(sha_url) {
            let want = String::from_utf8_lossy(&text)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            if want.len() == 64 {
                let got = sha256_file(&dest)?;
                if got != want {
                    let _ = std::fs::remove_file(&dest);
                    return Err("checksum mismatch — download discarded".into());
                }
            }
        }
    }
    Ok(dest)
}

/// Launch the downloaded installer silently. The caller should exit right after so the installer
/// can replace the running binaries.
pub fn launch_installer(path: &Path) -> Result<(), String> {
    let mut cmd = Command::new(path);
    // ASLCUPDATE=1 tells the installer to relaunch the app once it finishes.
    cmd.args([
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/CLOSEAPPLICATIONS",
        "/ASLCUPDATE=1",
    ]);
    cmd.spawn()
        .map_err(|e| format!("could not start the installer: {e}"))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` into Unix seconds (civil-from-days, Howard Hinnant's algorithm).
fn parse_iso8601(s: &str) -> Option<u64> {
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = days * 86_400 + hour * 3600 + min * 60 + sec;
    if total < 0 {
        None
    } else {
        Some(total as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso8601() {
        // 2024-01-01T00:00:00Z
        assert_eq!(parse_iso8601("2024-01-01T00:00:00Z"), Some(1_704_067_200));
        // 2026-10-08T12:34:56Z
        assert_eq!(parse_iso8601("2026-10-08T12:34:56Z"), Some(1_791_462_896));
        assert_eq!(parse_iso8601("not-a-date"), None);
        assert_eq!(parse_iso8601(""), None);
    }

    #[test]
    fn newer_uses_time_and_never_the_own_tag() {
        let epoch = 1_000_000;
        assert!(!release_is_newer("nightly-x", None, "", epoch));
        assert!(!release_is_newer("nightly-x", Some(epoch), "", epoch));
        assert!(release_is_newer("nightly-x", Some(epoch + 3600), "", epoch));
        // The exact tag we were built from is never "newer", even though it was published later.
        assert!(!release_is_newer(
            "nightly-x",
            Some(epoch + 3600),
            "nightly-x",
            epoch
        ));
        // A different tag published later IS newer.
        assert!(release_is_newer(
            "nightly-y",
            Some(epoch + 3600),
            "nightly-x",
            epoch
        ));
    }

    #[test]
    fn parses_releases_picks_newest_with_installer() {
        let json = r#"[
          {
            "tag_name": "nightly-20260101-b1-aaaaaaa",
            "prerelease": true,
            "published_at": "2026-01-01T00:00:00Z",
            "assets": [
              {"name": "ASLC-Node-Setup-0.1.0-nightly.20260101.aaaaaaa.exe", "browser_download_url": "https://example.test/a.exe"},
              {"name": "ASLC-Node-Setup-0.1.0-nightly.20260101.aaaaaaa.exe.sha256", "browser_download_url": "https://example.test/a.exe.sha256"}
            ]
          },
          {
            "tag_name": "nightly-20260202-b2-bbbbbbb",
            "prerelease": true,
            "published_at": "2026-02-02T00:00:00Z",
            "assets": [
              {"name": "ASLC-Node-Setup-0.1.0-nightly.20260202.bbbbbbb.exe", "browser_download_url": "https://example.test/b.exe"}
            ]
          },
          {
            "tag_name": "docs-only",
            "published_at": "2026-03-03T00:00:00Z",
            "assets": [{"name": "notes.txt", "browser_download_url": "https://example.test/n.txt"}]
          }
        ]"#;
        let rel = parse_releases(json).unwrap().unwrap();
        assert_eq!(rel.tag, "nightly-20260202-b2-bbbbbbb");
        assert!(rel.prerelease);
        assert_eq!(
            rel.asset_name,
            "ASLC-Node-Setup-0.1.0-nightly.20260202.bbbbbbb.exe"
        );
        assert!(rel.sha_url.is_none());
        assert!(rel.published_at.is_some());
    }

    #[test]
    fn parses_releases_none_without_installer() {
        let json = r#"[{"tag_name":"x","assets":[{"name":"a.txt","browser_download_url":"u"}]}]"#;
        assert!(parse_releases(json).unwrap().is_none());
        assert!(parse_releases("not json").is_err());
    }

    /// Manual check against the live GitHub API (network). Run with:
    /// `cargo test --lib -- --ignored live_fetch_latest --nocapture`
    #[test]
    #[ignore = "hits the live GitHub API"]
    fn live_fetch_latest() {
        let rel = fetch_latest().expect("fetch_latest");
        println!(
            "tag={} prerelease={} published={:?}",
            rel.tag, rel.prerelease, rel.published_at
        );
        println!("asset={} url={}", rel.asset_name, rel.asset_url);
        println!("sha_url={:?}", rel.sha_url);
        println!(
            "this build epoch={} newer_than_this_build={}",
            build_epoch(),
            rel.is_newer_than_this_build()
        );
    }
}
