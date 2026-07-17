//! Checks GitHub Releases for a newer sshoal build.
//!
//! In keeping with the rest of the crate's "shell out to your system tools"
//! approach (the [`transport`](crate::transport) module drives the system
//! `ssh`), the one HTTP GET this needs is delegated to `curl` — always present
//! on macOS and on the supported Ubuntu — so sshoal pulls in no HTTP/TLS stack
//! for a feature it touches only on launch and on an explicit "Check now".
//!
//! The check is read-only: it reports whether a newer release exists and where
//! to get it, and never installs anything. Pre-releases count (sshoal ships a
//! `vX.Y.Z-beta.N` line), so we list recent releases and pick the newest
//! published (non-draft) tag rather than trusting GitHub's "latest" endpoint,
//! which excludes pre-releases.

use std::cmp::Ordering;
use std::process::Command;

use serde::Deserialize;

/// The GitHub `owner/repo` sshoal releases live under.
pub const REPO: &str = "japananh/sshoal";

/// The human-facing releases page — shown in the "update available" banner and
/// used as a fallback when a release carries no page URL.
pub const RELEASES_URL: &str = "https://github.com/japananh/sshoal/releases";

const RELEASES_API: &str = "https://api.github.com/repos/japananh/sshoal/releases?per_page=10";

/// Bounds one check so a hung network never stalls a UI action.
const CHECK_TIMEOUT_SECS: u32 = 10;

/// The result of a check: whether a newer release exists plus what to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// True only when `latest` is strictly newer than `current`. Equal or older
    /// (a dev build ahead of the last release) is not an update.
    pub available: bool,
    /// The running version that was checked.
    pub current: String,
    /// The newest published release tag (empty when the repo has none yet).
    pub latest: String,
    /// The release's GitHub page (notes + assets), or [`RELEASES_URL`].
    pub url: String,
}

/// Why a check could not produce a result. A check failing is never fatal — the
/// caller surfaces it as a transient status and carries on.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("running curl: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("curl failed (exit {0}) — offline, or GitHub unreachable")]
    Curl(String),
    #[error("parsing GitHub response: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("installing update: {0}")]
    Install(String),
}

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

/// Check whether a release newer than `current` exists. `current` is the running
/// version (e.g. `env!("CARGO_PKG_VERSION")`, `"0.0.1-beta.1"`); a leading `v`
/// is optional, and an unparseable value sorts below any release so a dev build
/// still sees that a release exists. Blocking — call it off the UI thread.
pub fn check_latest(current: &str) -> Result<UpdateInfo, UpdateError> {
    Ok(info_from_releases(current, &fetch_releases()?))
}

/// Fetch the recent releases from the GitHub API (via `curl`). Shared by the
/// check and the installer.
fn fetch_releases() -> Result<Vec<GhRelease>, UpdateError> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &CHECK_TIMEOUT_SECS.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: sshoal-updater",
            RELEASES_API,
        ])
        .output()?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        return Err(UpdateError::Curl(code));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Download the newest release's artifact for this platform and install it over
/// the running app, then return. Blocking — call off the UI thread. The caller
/// relaunches afterwards. (macOS: replaces the running `.app` from the `.dmg`;
/// Linux: replaces the running binary from the tarball.)
pub fn install_latest() -> Result<(), UpdateError> {
    let releases = fetch_releases()?;
    let latest = newest_published(&releases)
        .ok_or_else(|| UpdateError::Install("no published release to install".into()))?;
    let suffix = asset_suffix();
    let asset = latest
        .assets
        .iter()
        .find(|a| a.name.ends_with(suffix))
        .ok_or_else(|| {
            UpdateError::Install(format!("release {} has no {suffix} asset", latest.tag_name))
        })?;
    install_asset(&asset.browser_download_url)
}

/// The release-asset filename suffix for the current platform.
fn asset_suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        ".dmg"
    } else {
        "linux-x86_64.tar.gz"
    }
}

/// A scratch dir under the system temp, removed on drop.
struct ScratchDir(std::path::PathBuf);
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn scratch_dir() -> Result<ScratchDir, UpdateError> {
    let dir = std::env::temp_dir().join(format!("sshoal-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| UpdateError::Install(format!("temp dir: {e}")))?;
    Ok(ScratchDir(dir))
}

fn run(cmd: &str, args: &[&str]) -> Result<std::process::Output, UpdateError> {
    let out = Command::new(cmd).args(args).output()?;
    if !out.status.success() {
        return Err(UpdateError::Install(format!(
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out)
}

/// The path of the running app bundle we should replace, derived from the
/// executable: `.../sshoal.app/Contents/MacOS/sshoal` → `.../sshoal.app`.
#[cfg(target_os = "macos")]
fn running_app_bundle() -> Result<std::path::PathBuf, UpdateError> {
    let exe = std::env::current_exe()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(|p| p.to_path_buf())
        .ok_or_else(|| UpdateError::Install("not running from a .app bundle".into()))
}

#[cfg(target_os = "macos")]
fn install_asset(url: &str) -> Result<(), UpdateError> {
    let app = running_app_bundle()?;
    let app_str = app.to_string_lossy().to_string();
    let scratch = scratch_dir()?;
    let dmg_str = scratch.0.join("sshoal.dmg").to_string_lossy().to_string();
    // Mount at our own path so we never have to parse `hdiutil` output (and so
    // `-quiet`, which suppresses that output, is safe).
    let mnt = scratch.0.join("mnt");
    std::fs::create_dir_all(&mnt).map_err(|e| UpdateError::Install(format!("mount dir: {e}")))?;
    let mnt_str = mnt.to_string_lossy().to_string();

    run("curl", &["-fsSL", url, "-o", &dmg_str])?;
    run(
        "hdiutil",
        &[
            "attach",
            &dmg_str,
            "-nobrowse",
            "-quiet",
            "-mountpoint",
            &mnt_str,
        ],
    )?;

    let result = (|| {
        let src = mnt.join("sshoal.app").to_string_lossy().to_string();
        // Replace the bundle in place. macOS lets you remove a running bundle —
        // the live process keeps its executable inode until it exits.
        let _ = std::fs::remove_dir_all(&app);
        run("cp", &["-R", &src, &app_str])?;
        let _ = Command::new("xattr")
            .args(["-dr", "com.apple.quarantine", &app_str])
            .status();
        Ok(())
    })();

    let _ = Command::new("hdiutil")
        .args(["detach", &mnt_str, "-quiet"])
        .status();
    result
}

#[cfg(not(target_os = "macos"))]
fn install_asset(url: &str) -> Result<(), UpdateError> {
    let exe = std::env::current_exe()?;
    let scratch = scratch_dir()?;
    let tarball = scratch.0.join("sshoal.tar.gz");
    let tar_str = tarball.to_string_lossy().to_string();
    let dir_str = scratch.0.to_string_lossy().to_string();

    run("curl", &["-fsSL", url, "-o", &tar_str])?;
    run("tar", &["-C", &dir_str, "-xzf", &tar_str])?;

    // Atomically replace the running binary: write the new one beside it, then
    // rename over (Linux keeps the old inode for the live process).
    let new_bin = scratch.0.join("sshoal");
    let staged = exe.with_extension("new");
    std::fs::copy(&new_bin, &staged).map_err(|e| UpdateError::Install(format!("copy: {e}")))?;
    let mut perms = std::fs::metadata(&staged)
        .map_err(|e| UpdateError::Install(format!("stat: {e}")))?
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    let _ = std::fs::set_permissions(&staged, perms);
    std::fs::rename(&staged, &exe).map_err(|e| UpdateError::Install(format!("replace: {e}")))?;
    Ok(())
}

/// The pure half of a check: given the running version and the fetched
/// releases, decide whether an update is available. Split out from the `curl`
/// call so the version logic is unit-testable without a network.
fn info_from_releases(current: &str, releases: &[GhRelease]) -> UpdateInfo {
    match newest_published(releases) {
        Some(latest) => UpdateInfo {
            available: compare_semver(current, &latest.tag_name) == Ordering::Less,
            current: current.to_string(),
            latest: latest.tag_name.clone(),
            url: if latest.html_url.is_empty() {
                RELEASES_URL.to_string()
            } else {
                latest.html_url.clone()
            },
        },
        // No published releases at all — nothing to do, not an error.
        None => UpdateInfo {
            available: false,
            current: current.to_string(),
            latest: String::new(),
            url: RELEASES_URL.to_string(),
        },
    }
}

/// The highest-versioned non-draft release (pre-releases included). GitHub
/// returns releases newest-first, but we compare explicitly so an out-of-order
/// publish can't pick the wrong tag.
fn newest_published(releases: &[GhRelease]) -> Option<&GhRelease> {
    releases
        .iter()
        .filter(|r| !r.draft && !r.tag_name.is_empty())
        .max_by(|a, b| compare_semver(&a.tag_name, &b.tag_name))
}

/// Compare two version strings by SemVer 2.0 precedence: a `vX.Y.Z` core with an
/// optional `-beta.N` pre-release suffix. A pre-release ranks below its release,
/// and numeric pre-release identifiers compare numerically (so `beta.9 <
/// beta.10`, which a plain string compare gets wrong). A leading `v` and build
/// metadata (`+sha`) are ignored. A version whose core can't be parsed sorts
/// below any parseable version, so a dev build always sees a release as newer.
fn compare_semver(a: &str, b: &str) -> Ordering {
    let (ca, pa, oka) = parse_semver(a);
    let (cb, pb, okb) = parse_semver(b);
    match (oka, okb) {
        (false, false) => return Ordering::Equal,
        (false, true) => return Ordering::Less,
        (true, false) => return Ordering::Greater,
        (true, true) => {}
    }
    for i in 0..3 {
        match ca[i].cmp(&cb[i]) {
            Ordering::Equal => {}
            ord => return ord,
        }
    }
    compare_prerelease(&pa, &pb)
}

/// Split `"v1.2.3-beta.4"` into core `[1,2,3]` and pre-release `"beta.4"`. `ok`
/// is false when the core has no parseable numeric component.
fn parse_semver(s: &str) -> ([u64; 3], String, bool) {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let s = s.split('+').next().unwrap_or(s); // drop build metadata
    if s.is_empty() {
        return ([0; 3], String::new(), false);
    }
    let (core_part, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, pre.to_string()),
        None => (s, String::new()),
    };
    let mut core = [0u64; 3];
    let mut ok = false;
    for (i, field) in core_part.split('.').take(3).enumerate() {
        match field.parse::<u64>() {
            Ok(n) => {
                core[i] = n;
                ok = true;
            }
            Err(_) => return ([0; 3], String::new(), false),
        }
    }
    (core, pre, ok)
}

/// SemVer pre-release precedence. An empty pre-release (a final release) ranks
/// ABOVE any pre-release.
fn compare_prerelease(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    match (a.is_empty(), b.is_empty()) {
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => {
                if x == y {
                    continue;
                }
                return match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xv), Ok(yv)) => xv.cmp(&yv),
                    // Numeric identifiers rank below alphanumeric ones.
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            html_url: format!("https://example.test/{tag}"),
            draft: false,
            assets: vec![],
        }
    }

    #[test]
    fn semver_orders_core_and_prerelease() {
        assert_eq!(compare_semver("1.0.0", "1.0.1"), Ordering::Less);
        assert_eq!(compare_semver("v1.2.0", "1.1.9"), Ordering::Greater);
        assert_eq!(compare_semver("1.0.0", "v1.0.0"), Ordering::Equal);
        // A pre-release is older than its final release.
        assert_eq!(compare_semver("1.0.0-beta.1", "1.0.0"), Ordering::Less);
        // Numeric pre-release identifiers compare numerically, not lexically.
        assert_eq!(
            compare_semver("0.0.1-beta.9", "0.0.1-beta.10"),
            Ordering::Less
        );
        // Build metadata is ignored.
        assert_eq!(compare_semver("1.2.3+abc", "1.2.3"), Ordering::Equal);
    }

    #[test]
    fn unparseable_current_sorts_below_any_release() {
        // A `dev` build (no version) is treated as older than a real release.
        assert_eq!(compare_semver("dev", "0.0.1"), Ordering::Less);
        assert_eq!(compare_semver("dev", "dev"), Ordering::Equal);
    }

    #[test]
    fn newest_published_skips_drafts_and_ignores_order() {
        let releases = vec![
            rel("v0.0.1-beta.1"),
            GhRelease {
                tag_name: "v9.9.9".into(),
                html_url: String::new(),
                draft: true, // a draft must never win
                assets: vec![],
            },
            rel("v0.0.1-beta.3"),
            rel("v0.0.1-beta.2"),
        ];
        let newest = newest_published(&releases).expect("a published release");
        assert_eq!(newest.tag_name, "v0.0.1-beta.3");
    }

    #[test]
    fn info_flags_available_only_for_newer() {
        let releases = vec![rel("v0.0.2"), rel("v0.0.1")];
        let info = info_from_releases("0.0.1", &releases);
        assert!(info.available);
        assert_eq!(info.latest, "v0.0.2");
        assert_eq!(info.url, "https://example.test/v0.0.2");

        // Running the newest release: not an update.
        let info = info_from_releases("0.0.2", &releases);
        assert!(!info.available);
    }

    #[test]
    fn info_with_no_releases_is_not_an_update() {
        let info = info_from_releases("0.0.1", &[]);
        assert!(!info.available);
        assert_eq!(info.latest, "");
        assert_eq!(info.url, RELEASES_URL);
    }

    #[test]
    fn asset_suffix_matches_platform() {
        if cfg!(target_os = "macos") {
            assert_eq!(asset_suffix(), ".dmg");
        } else {
            assert_eq!(asset_suffix(), "linux-x86_64.tar.gz");
        }
    }

    #[test]
    fn scratch_dir_creates_and_removes_on_drop() {
        let path = {
            let scratch = scratch_dir().unwrap();
            assert!(scratch.0.exists());
            scratch.0.clone()
        };
        assert!(!path.exists(), "ScratchDir should remove its dir on drop");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn running_app_bundle_errs_outside_a_bundle() {
        // The test binary isn't inside a `.app`, so this reports an error.
        assert!(running_app_bundle().is_err());
    }

    #[test]
    fn semver_and_prerelease_edge_cases() {
        // Parseable vs unparseable, both directions.
        assert_eq!(compare_semver("0.0.1", "dev"), Ordering::Greater);
        // A bare "v" strips to an empty (unparseable) core.
        assert_eq!(compare_semver("v", "1.0.0"), Ordering::Less);
        // A final release ranks above its own pre-release.
        assert_eq!(compare_semver("1.0.0", "1.0.0-beta.1"), Ordering::Greater);
        // Numeric pre-release identifiers rank below alphanumeric ones.
        assert_eq!(compare_semver("1.0.0-1", "1.0.0-alpha"), Ordering::Less);
        assert_eq!(compare_semver("1.0.0-alpha", "1.0.0-1"), Ordering::Greater);
        // Two alphanumeric identifiers compare lexically.
        assert_eq!(compare_semver("1.0.0-alpha", "1.0.0-beta"), Ordering::Less);
        // A shorter pre-release sorts below a longer one sharing its prefix.
        assert_eq!(
            compare_semver("1.0.0-alpha", "1.0.0-alpha.1"),
            Ordering::Less
        );
        assert_eq!(
            compare_semver("1.0.0-alpha.1", "1.0.0-alpha"),
            Ordering::Greater
        );
    }
}
