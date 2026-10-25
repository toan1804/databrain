//! "A new version is available": asks GitHub for the latest DataBrain
//! release and picks the download for this computer. Nothing is installed:
//! the UI shows a link. Only the version is sent (in the User-Agent).

use databrain_query_engine::{EngineError, Result};
use serde::{Deserialize, Serialize};

/// GitHub repository the releases come from.
pub const REPO: &str = "toan1804/databrain";
/// Setting: check at startup (default on).
pub const UPDATE_CHECK_SETTING: &str = "update_check";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub newer: bool,
    /// The release page.
    pub url: String,
    /// The installer for this computer, when the release has one.
    pub download: Option<String>,
    pub download_name: Option<String>,
    pub notes: Option<String>,
    pub published_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// `v0.10.2` → (0, 10, 2); None for other tags (`oracle-agent/v0.1.2`).
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let v = tag.strip_prefix('v').unwrap_or(tag);
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

/// The asset for this OS / CPU / build (with or without bundled DuckDB).
fn pick_asset<'a>(assets: &'a [Asset], os: &str, arch: &str, no_duckdb: bool) -> Option<&'a Asset> {
    let arch_tags: &[&str] = match arch {
        "aarch64" => &["aarch64", "arm64"],
        _ => &["x64", "x86_64", "amd64"],
    };
    let exts: &[&str] = match os {
        "macos" => &[".dmg"],
        "windows" => &["-setup.exe", ".msi"],
        _ => &[".AppImage", ".deb"],
    };
    for ext in exts {
        if let Some(a) = assets.iter().find(|a| {
            let n = a.name.to_ascii_lowercase();
            n.ends_with(&ext.to_ascii_lowercase()) && arch_tags.iter().any(|t| n.contains(t)) && n.contains("no-duckdb") == no_duckdb
        }) {
            return Some(a);
        }
    }
    None
}

fn latest_of(releases: Vec<Release>) -> Option<Release> {
    releases.into_iter().filter(|r| !r.draft && !r.prerelease && parse_version(&r.tag_name).is_some() && r.tag_name.starts_with('v')).max_by_key(|r| parse_version(&r.tag_name))
}

fn info_for(current: &str, releases: Vec<Release>, os: &str, arch: &str, no_duckdb: bool) -> Option<UpdateInfo> {
    let r = latest_of(releases)?;
    let newer = parse_version(&r.tag_name) > parse_version(current);
    let asset = pick_asset(&r.assets, os, arch, no_duckdb);
    Some(UpdateInfo {
        current: current.to_string(),
        latest: r.tag_name.trim_start_matches('v').to_string(),
        newer,
        url: r.html_url,
        download: asset.map(|a| a.browser_download_url.clone()),
        download_name: asset.map(|a| a.name.clone()),
        notes: r.body.map(|b| b.chars().take(2000).collect()),
        published_at: r.published_at,
    })
}

/// The latest release compared with `current` (the running app's version).
pub async fn check(current: &str) -> Result<Option<UpdateInfo>> {
    let client = reqwest::Client::builder()
        .user_agent(format!("DataBrain/{current}"))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| EngineError::new("internal", e.to_string()))?;
    let res = client
        .get(format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| EngineError::new("network", format!("Could not reach GitHub: {e}")))?;
    if !res.status().is_success() {
        return Err(EngineError::new("network", format!("GitHub answered {}", res.status())));
    }
    let releases: Vec<Release> = res.json().await.map_err(|e| EngineError::new("network", e.to_string()))?;
    let no_duckdb = cfg!(feature = "duckdb-system");
    Ok(info_for(current, releases, std::env::consts::OS, std::env::consts::ARCH, no_duckdb))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            html_url: format!("https://github.com/x/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
            body: None,
            published_at: None,
            assets: assets.iter().map(|n| Asset { name: (*n).into(), browser_download_url: format!("https://dl/{n}") }).collect(),
        }
    }

    #[test]
    fn picks_the_newest_app_release_and_this_computers_installer() {
        let assets = ["DataBrain_0.3.0_aarch64.dmg", "DataBrain-no-duckdb_0.3.0_aarch64.dmg", "DataBrain_0.3.0_x64-setup.exe", "DataBrain_0.3.0_x64_en-US.msi", "DataBrain_0.3.0_amd64.AppImage", "DataBrain_0.3.0_amd64.deb"];
        let list = || vec![rel("oracle-agent/v0.9.0", &[]), rel("v0.3.0", &assets), rel("v0.2.0", &[]), Release { draft: true, ..rel("v0.4.0", &[]) }];
        let i = info_for("0.2.0", list(), "macos", "aarch64", false).unwrap();
        assert_eq!((i.latest.as_str(), i.newer, i.download_name.as_deref()), ("0.3.0", true, Some("DataBrain_0.3.0_aarch64.dmg")));
        assert_eq!(info_for("0.2.0", list(), "macos", "aarch64", true).unwrap().download_name.as_deref(), Some("DataBrain-no-duckdb_0.3.0_aarch64.dmg"));
        assert_eq!(info_for("0.2.0", list(), "windows", "x86_64", false).unwrap().download_name.as_deref(), Some("DataBrain_0.3.0_x64-setup.exe"));
        assert_eq!(info_for("0.2.0", list(), "linux", "x86_64", false).unwrap().download_name.as_deref(), Some("DataBrain_0.3.0_amd64.AppImage"));
        assert_eq!(info_for("0.2.0", list(), "macos", "x86_64", false).unwrap().download, None, "no Intel build: the release page only");
        assert!(!info_for("0.3.0", list(), "macos", "aarch64", false).unwrap().newer);
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
    }
}
