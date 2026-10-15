//! Finding, downloading and installing Camoufox browser builds.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::browser_pin;
use crate::error::{Error, Result};
use crate::multiversion;
use crate::paths::{self, data, eprint_status, launch_file, ARCH, OS_NAME};

/// The camoufox Python release whose behaviour this crate ports. repos.yml
/// keys its browser constraints by that version.
pub const LIBRARY_COMPAT_VERSION: &str = "0.5.8";

/// Browser builds this launcher can drive.
pub const MIN_VERSION: &str = "alpha.1";
pub const MAX_VERSION: &str = "1";
/// From Playwright 1.61 on, the browser must be at least beta.30: that
/// Playwright sends viewport fields only beta.30's Juggler schema accepts.
pub const PLAYWRIGHT_BROWSER_FLOORS: &[((u64, u64), &str)] = &[((1, 61), "beta.30")];
/// The browser interface this launcher speaks (declared in every release's manifest.json).
pub const INTERFACE: i64 = 1;
pub const MIN_INTERFACE: i64 = 1;

fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
}

pub(crate) fn http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("rustoufox/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()?)
}

/// A comparable build string such as `beta.36`.
#[derive(Debug, Clone)]
pub struct Version {
    pub build: String,
    pub version: Option<String>,
    sorted_rel: Vec<i64>,
}

impl Version {
    pub fn new(build: &str, version: Option<&str>) -> Self {
        let mut sorted_rel: Vec<i64> = build
            .split('.')
            .map(|x| {
                if !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()) {
                    x.parse().unwrap_or(0)
                } else {
                    x.chars().next().map(|c| c as i64 - 1024).unwrap_or(-1024)
                }
            })
            .collect();
        let pad = 5usize.saturating_sub(build.matches('.').count());
        sorted_rel.extend(std::iter::repeat_n(0, pad));
        Version { build: build.to_string(), version: version.map(str::to_string), sorted_rel }
    }

    pub fn full_string(&self) -> String {
        format!("{}-{}", self.version.as_deref().unwrap_or("None"), self.build)
    }

    pub fn is_alpha(&self) -> bool {
        self.build.split('.').next().unwrap_or("").eq_ignore_ascii_case("alpha")
    }

    pub fn is_supported(&self) -> bool {
        effective_version_min() <= *self && *self < Version::new(MAX_VERSION, None)
    }

    /// The version recorded in `version.json` of an install.
    pub fn from_path(path: &Path) -> Result<Version> {
        let file = path.join("version.json");
        if !file.exists() {
            return Err(Error::FileNotFound(file));
        }
        let data: Map<String, Value> = serde_json::from_slice(&fs::read(&file)?)?;
        let build = data
            .get("build")
            .or_else(|| data.get("release"))
            .or_else(|| data.get("tag"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::value(format!("{} names no build", file.display())))?;
        Ok(Version::new(build, data.get("version").and_then(Value::as_str)))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.sorted_rel == other.sorted_rel
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.sorted_rel.cmp(&other.sorted_rel))
    }
}

/// Parse a semver-ish string into a comparable tuple; a prerelease compares as its release.
pub fn parse_semver(version: &str) -> Vec<u64> {
    let v = version.trim_start_matches(['^', '~']);
    let re = regex::Regex::new(r"^([0-9.]*[0-9])").unwrap();
    let core = re.captures(v).map(|c| c[1].to_string()).unwrap_or_default();
    let mut parts: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    parts
}

/// The lowest build the installed Playwright can drive.
pub fn effective_version_min() -> Version {
    let mut floor = Version::new(MIN_VERSION, None);
    if let Some(pw) = crate::server::playwright_version() {
        let pw = parse_semver(&pw);
        for ((major, minor), build) in PLAYWRIGHT_BROWSER_FLOORS {
            if pw[..2] >= [*major, *minor][..] && floor < Version::new(build, None) {
                floor = Version::new(build, None);
            }
        }
    }
    floor
}

pub fn interface_supported(interface: i64) -> bool {
    (MIN_INTERFACE..=INTERFACE).contains(&interface)
}

pub fn upgrade_message(version: &Version, interface: i64) -> String {
    format!(
        "Camoufox v{} needs a newer launcher: it uses browser interface {interface}, and this \
         launcher supports {MIN_INTERFACE}-{INTERFACE}. Upgrade rustoufox.",
        version.full_string()
    )
}

pub fn incompatible_message(version: &Version, interface: i64) -> String {
    format!(
        "{} Or run `rustoufox fetch` to install a browser this launcher supports.",
        upgrade_message(version, interface)
    )
}

/// The browser interface a GitHub release declares in its manifest.json.
fn release_interface(release: &Value) -> Result<i64> {
    static CACHE: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
    let Some(manifest) =
        release["assets"].as_array().and_then(|a| a.iter().find(|x| x["name"].as_str() == Some("manifest.json")))
    else {
        return Ok(1);
    };
    let url = manifest["browser_download_url"].as_str().unwrap_or_default().to_string();
    if let Some(v) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&url) {
        return Ok(*v);
    }
    let body: Value = http_client()?.get(&url).send()?.error_for_status()?.json()?;
    let interface = body["interface"].as_i64().unwrap_or(1);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(url, interface);
    Ok(interface)
}

/// Configuration for one repository of browser builds (repos.yml).
#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub repos: Vec<String>,
    pub name: String,
    pub pattern: String,
    pub stable_min: Option<String>,
    pub stable_max: Option<String>,
    pub prerelease_min: Option<String>,
    pub prerelease_max: Option<String>,
}

fn repos_yml() -> Value {
    serde_yaml::from_str(data::REPOS_YML).expect("repos.yml")
}

fn find_version_constraints(versions: &[Value], library_version: &str) -> Option<Value> {
    let lib = parse_semver(library_version);
    let mut newest: Option<(Vec<u64>, Value)> = None;
    for entry in versions {
        let py = &entry["python_library"];
        let min = parse_semver(py["min"].as_str().unwrap_or("0"));
        let max = parse_semver(py["max"].as_str().unwrap_or("999"));
        if min <= lib && lib < max {
            return Some(entry["browser"].clone());
        }
        if newest.as_ref().is_none_or(|(m, _)| min > *m) {
            newest = Some((min, entry["browser"].clone()));
        }
    }
    newest.map(|(_, b)| b)
}

fn channel_bounds(browser: &Value, channel: &str) -> (Option<String>, Option<String>) {
    if browser.is_null() {
        return (None, None);
    }
    let section = if browser.get("stable").is_some() || browser.get("prerelease").is_some() {
        &browser[channel]
    } else {
        browser
    };
    (section["min"].as_str().map(str::to_string), section["max"].as_str().map(str::to_string))
}

impl RepoConfig {
    pub fn load_repos() -> Vec<RepoConfig> {
        repos_yml()["browsers"]
            .as_array()
            .map(|a| a.iter().filter_map(RepoConfig::from_value).collect())
            .unwrap_or_default()
    }

    pub fn default_name() -> String {
        repos_yml()["default"]["browser"].as_str().unwrap_or("Official").to_string()
    }

    fn from_value(d: &Value) -> Option<RepoConfig> {
        let browser = d["versions"]
            .as_array()
            .and_then(|v| find_version_constraints(v, LIBRARY_COMPAT_VERSION))
            .unwrap_or(Value::Null);
        let (stable_min, stable_max) = channel_bounds(&browser, "stable");
        let (prerelease_min, prerelease_max) = channel_bounds(&browser, "prerelease");
        Some(RepoConfig {
            repos: d["repo"].as_str()?.split(',').map(|r| r.trim().to_string()).collect(),
            name: d["name"].as_str()?.to_string(),
            pattern: d["pattern"].as_str()?.to_string(),
            stable_min,
            stable_max,
            prerelease_min,
            prerelease_max,
        })
    }

    pub fn get_default() -> RepoConfig {
        let name = Self::default_name();
        Self::find_by_name(&name).unwrap_or_else(|| Self::load_repos().remove(0))
    }

    pub fn find_by_name(name: &str) -> Option<RepoConfig> {
        Self::load_repos().into_iter().find(|r| r.name.eq_ignore_ascii_case(name))
    }

    /// The asset name pattern as a regex for this OS and architecture.
    pub fn build_pattern(&self, spoof_os: Option<&str>, spoof_arch: Option<&str>) -> regex::Regex {
        let os = spoof_os.unwrap_or(OS_NAME);
        let arch = spoof_arch.unwrap_or(ARCH);
        let mut out = String::from("^");
        let pattern = self.pattern.replace('.', r"\.");
        let re = regex::Regex::new(r"\{(\w+)\}").unwrap();
        let mut last = 0;
        for cap in re.captures_iter(&pattern) {
            let m = cap.get(0).unwrap();
            out.push_str(&pattern[last..m.start()]);
            out.push_str(&match &cap[1] {
                "name" => r"(?P<name>\w+)".to_string(),
                "version" => r"(?P<version>[^-]+)".to_string(),
                "build" => r"(?P<build>[^-]+)".to_string(),
                "os" => regex::escape(os),
                "arch" => regex::escape(arch),
                other => format!("{{{other}}}"),
            });
            last = m.end();
        }
        out.push_str(&pattern[last..]);
        regex::Regex::new(&out).expect("asset pattern")
    }

    pub fn is_version_supported(&self, version: &Version, is_prerelease: bool) -> bool {
        let (min, max) = if is_prerelease {
            (&self.prerelease_min, &self.prerelease_max)
        } else {
            (&self.stable_min, &self.stable_max)
        };
        match (min, max) {
            (Some(min), Some(max)) => Version::new(min, None) <= *version && *version <= Version::new(max, None),
            _ => true,
        }
    }
}

/// A build available on GitHub.
#[derive(Debug, Clone)]
pub struct AvailableVersion {
    pub version: Version,
    pub url: String,
    pub is_prerelease: bool,
    pub asset_id: Option<i64>,
    pub asset_size: Option<i64>,
    pub asset_updated_at: Option<String>,
    pub sha256: Option<String>,
    pub asset_created_at: Option<String>,
    pub interface: i64,
    pub github_repo: String,
}

impl AvailableVersion {
    pub fn sha8(&self) -> String {
        self.sha256.as_deref().unwrap_or("").chars().take(8).collect()
    }

    pub fn to_metadata(&self) -> Value {
        serde_json::json!({
            "version": self.version.version,
            "build": self.version.build,
            "prerelease": self.is_prerelease,
            "asset_id": self.asset_id,
            "asset_size": self.asset_size,
            "asset_updated_at": self.asset_updated_at,
            "sha256": self.sha256,
            "created_at": self.asset_created_at,
            "interface": self.interface,
        })
    }

    /// Rebuild from a repo_cache.json entry.
    pub fn from_cache(entry: &Value, github_repo: &str) -> AvailableVersion {
        let s = |k: &str| entry[k].as_str().map(str::to_string);
        AvailableVersion {
            version: Version::new(entry["build"].as_str().unwrap_or(""), entry["version"].as_str()),
            url: s("url").unwrap_or_default(),
            is_prerelease: entry["is_prerelease"].as_bool().unwrap_or(false),
            asset_id: entry["asset_id"].as_i64(),
            asset_size: entry["asset_size"].as_i64(),
            asset_updated_at: s("asset_updated_at"),
            sha256: s("sha256"),
            asset_created_at: s("created_at"),
            interface: entry["interface"].as_i64().unwrap_or(1),
            github_repo: github_repo.to_string(),
        }
    }

    pub fn to_cache(&self) -> Value {
        serde_json::json!({
            "version": self.version.version,
            "build": self.version.build,
            "url": self.url,
            "is_prerelease": self.is_prerelease,
            "asset_id": self.asset_id,
            "asset_size": self.asset_size,
            "asset_updated_at": self.asset_updated_at,
            "sha256": self.sha256,
            "created_at": self.asset_created_at,
            "interface": self.interface,
        })
    }
}

fn get_releases(repo: &str) -> Result<Vec<Value>> {
    let mut req = http_client()?.get(format!("https://api.github.com/repos/{repo}/releases"));
    if let Some(token) = github_token() {
        req = req.bearer_auth(token);
    }
    Ok(req.timeout(std::time::Duration::from_secs(20)).send()?.error_for_status()?.json()?)
}

/// Every supported build available on GitHub for this platform, newest first.
pub fn list_available_versions(
    config: &RepoConfig,
    include_prerelease: bool,
    spoof_os: Option<&str>,
    spoof_arch: Option<&str>,
    include_incompatible: bool,
) -> Result<Vec<AvailableVersion>> {
    let os = spoof_os.unwrap_or(OS_NAME);
    let arch = spoof_arch.unwrap_or(ARCH);
    if !paths::os_arch_matrix(os).contains(&arch) {
        return Err(Error::UnsupportedArchitecture(format!("Architecture {arch} is not supported for {os}")));
    }
    let pattern = config.build_pattern(spoof_os, spoof_arch);
    let mut releases = Vec::new();
    let mut github_repo = config.repos[0].clone();
    let mut last_error = None;
    for repo in &config.repos {
        match get_releases(repo) {
            Ok(r) => {
                releases = r;
                github_repo = repo.clone();
                last_error = None;
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    if let Some(e) = last_error {
        return Err(e);
    }

    let mut versions = Vec::new();
    for release in &releases {
        let release_pre = release["prerelease"].as_bool().unwrap_or(false);
        if release_pre && !include_prerelease {
            continue;
        }
        for asset in release["assets"].as_array().into_iter().flatten() {
            let name = asset["name"].as_str().unwrap_or("");
            let Some(caps) = pattern.captures(name) else { continue };
            let version = Version::new(&caps["build"], Some(&caps["version"]));
            let is_pre = release_pre || version.is_alpha();
            if is_pre && !include_prerelease {
                continue;
            }
            if !config.is_version_supported(&version, is_pre) {
                continue;
            }
            let interface = release_interface(release)?;
            if !interface_supported(interface) && !include_incompatible {
                eprint_status(&incompatible_message(&version, interface), Some("yellow"));
                continue;
            }
            let digest = asset["digest"].as_str().unwrap_or("");
            versions.push(AvailableVersion {
                version,
                url: asset["browser_download_url"].as_str().unwrap_or("").to_string(),
                is_prerelease: is_pre,
                asset_id: asset["id"].as_i64(),
                asset_size: asset["size"].as_i64(),
                asset_updated_at: asset["updated_at"].as_str().map(str::to_string),
                sha256: digest.strip_prefix("sha256:").map(str::to_string),
                asset_created_at: asset["created_at"].as_str().map(str::to_string),
                interface,
                github_repo: github_repo.clone(),
            });
        }
    }
    versions.sort_by(|a, b| {
        b.version
            .partial_cmp(&a.version)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.asset_created_at.cmp(&a.asset_created_at))
    });
    Ok(versions)
}

/// The newest build a repo offers that this launcher would install: the
/// paired build when there is one, else the newest supported one.
pub fn latest_installable(config: &RepoConfig) -> Result<AvailableVersion> {
    let pin = browser_pin::effective_pin(None);
    let versions = list_available_versions(config, true, None, None, false)?;
    let found = versions.into_iter().find(|v| match &pin {
        Some(pin) => {
            browser_pin::matches(pin, &config.name, v.version.version.as_deref().unwrap_or(""), &v.version.build)
        }
        None => true,
    });
    found.ok_or_else(|| {
        Error::MissingRelease(format!(
            "No matching release found for {OS_NAME} {ARCH} in the supported range. Please update rustoufox."
        ))
    })
}

fn not_installed_message() -> String {
    let config = multiversion::load_config();
    let channel =
        config.get("channel").and_then(Value::as_str).map(str::to_string).unwrap_or_else(multiversion::default_channel);
    let missing = if let Some(pin) = browser_pin::effective_pin(Some(&config)) {
        format!("{} {}, the browser this rustoufox release pairs with,", pin.repo_name, pin.spec())
    } else if let Some(pinned) = config.get("pinned").and_then(Value::as_str) {
        format!("{channel}/{pinned}")
    } else {
        channel
    };
    format!("{missing} is not installed. Please run `rustoufox fetch` to install.")
}

/// The full version string of the active install.
pub fn installed_verstr() -> Result<String> {
    let active = multiversion::get_active_path().ok_or_else(|| Error::CamoufoxNotInstalled(not_installed_message()))?;
    let version = Version::from_path(&active)?;
    if active.parent().and_then(Path::parent).and_then(Path::file_name).is_some_and(|n| n == "browsers") {
        let repo =
            active.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        browser_pin::warn_if_unpaired(&repo, version.version.as_deref().unwrap_or(""), &version.build);
    }
    Ok(version.full_string())
}

pub fn installed_interface(path: &Path) -> i64 {
    fs::read(path.join("version.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["interface"].as_i64())
        .unwrap_or(1)
}

/// The directory of the active install, downloading the paired build if none is installed.
pub fn camoufox_path(download_if_missing: bool) -> Result<PathBuf> {
    multiversion::remove_incompatible_data()?;
    if let Some(active) = multiversion::get_active_path() {
        let installed = Version::from_path(&active)?;
        let interface = installed_interface(&active);
        if !interface_supported(interface) {
            return Err(Error::UnsupportedVersion(incompatible_message(&installed, interface)));
        }
        if installed.is_supported() {
            return Ok(active);
        }
    }
    if !download_if_missing {
        return Err(Error::CamoufoxNotInstalled(not_installed_message()));
    }
    let available = latest_installable(&RepoConfig::get_default())?;
    multiversion::install_versioned(&available, false)?;
    ensure_browser_profile_dir(None)?;
    match multiversion::get_active_path() {
        Some(active) if Version::from_path(&active)?.is_supported() => Ok(active),
        _ => Err(Error::UnsupportedVersion(format!(
            "No available Camoufox build satisfies this launcher's minimum ({MIN_VERSION}). The \
             matching browser release may not be published yet."
        ))),
    }
}

/// The resources directory of an install: where its data files live.
pub fn resources_dir(install: &Path) -> PathBuf {
    if OS_NAME == "mac" {
        install.join("Camoufox.app").join("Contents").join("Resources")
    } else {
        install.to_path_buf()
    }
}

/// A file the active build ships.
pub fn get_path(file: &str) -> Result<PathBuf> {
    Ok(resources_dir(&camoufox_path(true)?).join(file))
}

/// A file a build ships, found from its executable.
pub fn build_file(executable_path: &Path, name: &str) -> PathBuf {
    let mut dir = executable_path.parent().map(Path::to_path_buf).unwrap_or_default();
    if dir.file_name().is_some_and(|n| n == "MacOS") {
        dir = dir.parent().map(|p| p.join("Resources")).unwrap_or(dir);
    }
    dir.join(name)
}

/// The executable of an install, or of the active one (downloading it if needed).
pub fn launch_path(browser_path: Option<&Path>) -> Result<PathBuf> {
    let install = match browser_path {
        Some(p) => p.to_path_buf(),
        None => camoufox_path(true)?,
    };
    let exec = resources_dir(&install).join(launch_file(OS_NAME));
    let exec = exec.canonicalize().unwrap_or(exec);
    if !exec.exists() {
        return Err(Error::CamoufoxNotInstalled(format!(
            "Camoufox is not installed at {}. Please run `rustoufox fetch` to install.",
            install.display()
        )));
    }
    Ok(exec)
}

/// The executable of the installed build `specifier` names. Never downloads.
pub fn selected_launch_path(specifier: &str) -> Result<PathBuf> {
    let path = multiversion::find_installed_version(specifier).ok_or_else(|| {
        Error::value(format!(
            "Browser version '{specifier}' not found. Run `rustoufox list` to see installed versions."
        ))
    })?;
    let version = Version::from_path(&path)?;
    let interface = installed_interface(&path);
    if !interface_supported(interface) {
        return Err(Error::UnsupportedVersion(incompatible_message(&version, interface)));
    }
    let repo = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    browser_pin::warn_if_unpaired(&repo, version.version.as_deref().unwrap_or(""), &version.build);
    launch_path(Some(&path))
}

/// Warn once when the last sync found a browser that needs a newer launcher.
pub fn warn_if_package_outdated() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let cache = multiversion::load_repo_cache();
    let Some(newer) = cache.get("incompatible").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return;
    };
    if WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let newest = newer
        .iter()
        .map(|v| {
            (
                Version::new(v["build"].as_str().unwrap_or(""), v["version"].as_str()),
                v["interface"].as_i64().unwrap_or(1),
            )
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    if let Some((v, i)) = newest {
        crate::warnings::runtime(upgrade_message(&v, i));
    }
}

/// Firefox probes `~/.camoufox` on Linux even with a temporary profile, and
/// stalls on a read-only HOME when it is missing.
pub fn ensure_browser_profile_dir(env: Option<&std::collections::BTreeMap<String, String>>) -> Result<Option<PathBuf>> {
    if OS_NAME != "lin" {
        return Ok(None);
    }
    let home = env
        .and_then(|e| e.get("HOME").cloned())
        .or_else(|| std::env::var("HOME").ok())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = home.join(".camoufox");
    if dir.is_dir() {
        return Ok(Some(dir));
    }
    let created = {
        let mut b = fs::DirBuilder::new();
        b.recursive(true);
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&dir)
    };
    if created.is_err() || !dir.is_dir() {
        return Err(Error::ProfileDirectory(format!(
            "Camoufox requires '{}' to exist before launch, but it could not be created. For a \
             read-only runtime, create this directory before making HOME read-only.",
            dir.display()
        )));
    }
    Ok(Some(dir))
}

/// Download a URL into memory, with a progress bar when `bar` is set.
pub fn webdl(url: &str, desc: Option<&str>, bar: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    webdl_to(url, desc, bar, &mut out)?;
    Ok(out)
}

/// Stream a URL into `out`, returning the sha256 of what was written.
pub fn webdl_to(url: &str, desc: Option<&str>, bar: bool, out: &mut dyn Write) -> Result<String> {
    let mut req = http_client()?.get(url);
    if url.contains("api.github") {
        if let Some(token) = github_token() {
            req = req.bearer_auth(token);
        }
    }
    let mut resp = req.send()?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    let pb = if bar {
        let pb = indicatif::ProgressBar::new(total);
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner} {msg} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} {eta}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        pb.set_message(desc.unwrap_or("Downloading").to_string());
        Some(pb)
    } else {
        None
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        if let Some(pb) = &pb {
            pb.inc(n as u64);
        }
    }
    out.flush()?;
    if let Some(pb) = pb {
        pb.finish_and_clear();
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Write through a temporary file beside `dest`, then replace it in one step.
pub fn write_atomic(dest: &Path, data: &[u8]) -> Result<()> {
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dest.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, dest)?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    result
}

/// Check a download's sha256 against the expected one. Skips, with a
/// warning, when the source publishes no digest.
pub fn verify_sha256(actual: &str, expected: Option<&str>, desc: &str) -> Result<()> {
    let Some(expected) = expected else {
        eprint_status(&format!("Warning: no sha256 published for {desc}; skipping verification."), Some("yellow"));
        return Ok(());
    };
    if actual != expected.to_lowercase() {
        return Err(Error::CorruptedDownload(format!(
            "Checksum mismatch for {desc}.\n  expected sha256: {}\n  actual   sha256: {actual}\n\
             The download was corrupted or tampered with. Installation aborted.",
            expected.to_lowercase()
        )));
    }
    Ok(())
}

/// Extract an in-memory zip archive, refusing members that would land outside `dest`.
pub fn unzip(buffer: &[u8], dest: &Path, desc: Option<&str>) -> Result<()> {
    unzip_from(std::io::Cursor::new(buffer), dest, desc)
}

/// Extract a zip archive, refusing members that would land outside `dest`.
pub fn unzip_from<R: Read + std::io::Seek>(reader: R, dest: &Path, desc: Option<&str>) -> Result<()> {
    let mut zip = zip::ZipArchive::new(reader)?;
    let pb = desc.map(|d| {
        let pb = indicatif::ProgressBar::new(zip.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::with_template("{spinner} {msg} [{bar:30}] {pos}/{len}")
                .unwrap()
                .progress_chars("=> "),
        );
        pb.set_message(d.to_string());
        pb
    });
    let mut links = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::CorruptedDownload(format!(
                "Archive member {} escapes the install directory",
                entry.name()
            )));
        };
        refuse_symlinked_parent(dest, &rel)?;
        let out = dest.join(&rel);
        if entry.is_symlink() {
            // Made once every file is written, so none is written through one.
            let mut target = String::new();
            entry.read_to_string(&mut target)?;
            links.push((rel, target));
        } else if entry.is_dir() {
            fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            remove_non_dir(&out)?;
            let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o7777))?;
            }
        }
        if let Some(pb) = &pb {
            pb.inc(1);
        }
    }
    for (rel, target) in links {
        refuse_symlinked_parent(dest, &rel)?;
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        remove_non_dir(&out)?;
        std::os::unix::fs::symlink(target, &out)?;
    }
    if let Some(pb) = pb {
        pb.finish_and_clear();
    }
    Ok(())
}

/// Refuse a member whose parent directory, inside `dest`, is a symlink: it
/// would be written wherever the link points.
fn refuse_symlinked_parent(dest: &Path, rel: &Path) -> Result<()> {
    let mut dir = dest.to_path_buf();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        dir.push(part);
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::CorruptedDownload(format!("Archive member {} goes through a symlink", rel.display())));
        }
    }
    Ok(())
}

/// Remove a file or symlink already at `path`, so it is replaced rather than
/// followed.
fn remove_non_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_dir() => Ok(fs::remove_file(path)?),
        _ => Ok(()),
    }
}

/// Format an asset timestamp as `Mon D`, or `Mon D, YYYY` when the year differs.
pub fn format_asset_date(iso: Option<&str>) -> String {
    let Some(iso) = iso else { return String::new() };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&iso.replace('Z', "+00:00")) else {
        return String::new();
    };
    use chrono::Datelike;
    if dt.year() == chrono::Local::now().year() {
        dt.format("%b %-d").to_string()
    } else {
        dt.format("%b %-d, %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn archive(build: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        build(&mut zip);
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn symlinks_are_extracted_as_links() {
        let bytes = archive(|z| {
            z.start_file("Contents/MacOS/real", SimpleFileOptions::default()).unwrap();
            z.write_all(b"binary").unwrap();
            z.add_symlink("Contents/MacOS/link", "real", SimpleFileOptions::default()).unwrap();
        });
        let dest = tempfile::tempdir().unwrap();
        unzip(&bytes, dest.path(), None).unwrap();
        let link = dest.path().join("Contents/MacOS/link");
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("real"));
        assert_eq!(fs::read(&link).unwrap(), b"binary");
    }

    #[test]
    fn no_member_is_written_through_a_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let through_dir = archive(|z| {
            z.add_symlink("escape", outside.path().to_str().unwrap(), SimpleFileOptions::default()).unwrap();
            z.start_file("escape/pwned", SimpleFileOptions::default()).unwrap();
            z.write_all(b"x").unwrap();
        });
        let dest = tempfile::tempdir().unwrap();
        let _ = unzip(&through_dir, dest.path(), None);
        assert!(!outside.path().join("pwned").exists());

        // A link already at the path (a previous archive) is replaced, not followed.
        let dest = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("other"), dest.path().join("file")).unwrap();
        let file = archive(|z| {
            z.start_file("file", SimpleFileOptions::default()).unwrap();
            z.write_all(b"x").unwrap();
        });
        unzip(&file, dest.path(), None).unwrap();
        assert!(!outside.path().join("other").exists());
        assert_eq!(fs::read(dest.path().join("file")).unwrap(), b"x");
    }
}
