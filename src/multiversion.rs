//! Several Camoufox builds installed side by side, under
//! `browsers/<repo>/<version>-<build>[-<sha8>]`, with the same layout and
//! config files as the Python and TypeScript launchers.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::browser_pin;
use crate::error::Result;
use crate::paths::{eprint_status, install_dir, rprint, style};
use crate::pkgman::{self, AvailableVersion, RepoConfig, Version};

pub fn browsers_dir() -> PathBuf {
    install_dir().join("browsers")
}

pub fn config_file() -> PathBuf {
    install_dir().join("config.json")
}

pub fn repo_cache_file() -> PathBuf {
    install_dir().join("repo_cache.json")
}

fn compat_flag() -> PathBuf {
    install_dir().join(".0.5_FLAG")
}

/// Delete a cache directory written before the versioned layout.
pub fn remove_incompatible_data() -> Result<()> {
    let dir = install_dir();
    if dir.exists() && fs::read_dir(dir)?.next().is_some() && !compat_flag().exists() {
        // Anything else in the directory (fpgen's model, addons, GeoIP) is
        // re-downloaded on demand; only an unversioned browser is in the way.
        if dir.join("version.json").exists() {
            eprint_status("Cleaning old data...", Some("yellow"));
            fs::remove_dir_all(dir)?;
        }
    }
    Ok(())
}

fn load_json(path: &Path) -> Map<String, Value> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_json(path: &Path, value: &Map<String, Value>) -> Result<()> {
    fs::create_dir_all(install_dir())?;
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub fn load_config() -> Map<String, Value> {
    load_json(&config_file())
}

pub fn save_config(config: &Map<String, Value>) -> Result<()> {
    save_json(&config_file(), config)
}

pub fn load_repo_cache() -> Map<String, Value> {
    load_json(&repo_cache_file())
}

pub fn save_repo_cache(cache: &Map<String, Value>) -> Result<()> {
    save_json(&repo_cache_file(), cache)
}

/// The default repo's stable channel, like `official/stable`.
pub fn default_channel() -> String {
    format!("{}/stable", RepoConfig::default_name().to_lowercase())
}

/// One cache entry per version-build, the newest by created_at, newest first.
pub fn latest_per_build(versions: &[Value]) -> Vec<Value> {
    let mut best: Vec<(String, Value)> = Vec::new();
    for v in versions {
        let key = format!("{}-{}", v["version"].as_str().unwrap_or(""), v["build"].as_str().unwrap_or(""));
        let created = v["created_at"].as_str().unwrap_or("");
        match best.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cur)) => {
                if created > cur["created_at"].as_str().unwrap_or("") {
                    *cur = v.clone();
                }
            }
            None => best.push((key, v.clone())),
        }
    }
    let mut out: Vec<Value> = best.into_iter().map(|(_, v)| v).collect();
    out.sort_by(|a, b| {
        let key = |v: &Value| {
            (v["version"].as_str().unwrap_or("").to_string(), v["created_at"].as_str().unwrap_or("").to_string())
        };
        key(b).cmp(&key(a))
    });
    out
}

/// Display name for a GitHub repo from repos.yml, lowercased.
pub fn repo_name_for(github_repo: &str) -> String {
    RepoConfig::load_repos()
        .into_iter()
        .find(|r| r.repos.iter().any(|x| x == github_repo))
        .map(|r| r.name.to_lowercase())
        .unwrap_or_else(|| github_repo.split('/').next().unwrap_or(github_repo).to_lowercase())
}

#[derive(Debug, Clone)]
pub struct InstalledVersion {
    pub repo_name: String,
    pub version: Version,
    pub path: PathBuf,
    pub is_active: bool,
    pub is_prerelease: bool,
    pub asset_updated_at: Option<String>,
    pub asset_size: Option<i64>,
    pub sha256: Option<String>,
    pub created_at: Option<String>,
}

impl InstalledVersion {
    pub fn relative_path(&self) -> String {
        format!("browsers/{}/{}", self.repo_name, self.path.file_name().unwrap_or_default().to_string_lossy())
    }

    pub fn channel_path(&self) -> String {
        let ctype = if self.is_prerelease { "prerelease" } else { "stable" };
        format!("{}/{ctype}/{}", self.repo_name, self.version.full_string())
    }

    pub fn changes(&self, available: &AvailableVersion) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.is_prerelease && !available.is_prerelease {
            out.push("prerelease -> stable");
        } else if !self.is_prerelease && available.is_prerelease {
            out.push("stable -> prerelease");
        }
        match (&self.asset_updated_at, &available.asset_updated_at) {
            (Some(a), Some(b)) if a != b => out.push("asset updated"),
            (Some(_), Some(_)) => {}
            _ => {
                if let (Some(a), Some(b)) = (self.asset_size, available.asset_size) {
                    if a != b {
                        out.push("asset updated");
                    }
                }
            }
        }
        out
    }
}

pub fn version_folder_name(version: &str, build: &str, sha8: &str) -> String {
    if sha8.is_empty() {
        format!("{version}-{build}")
    } else {
        format!("{version}-{build}-{sha8}")
    }
}

/// The installed folder for a catalog item, the legacy unsuffixed folder allowed.
pub fn find_install<'a>(
    version_build: &str,
    sha256: Option<&str>,
    installed: &'a [InstalledVersion],
    count: usize,
) -> Option<&'a InstalledVersion> {
    let by_folder = |name: &str| installed.iter().find(|iv| iv.path.file_name().is_some_and(|n| n == name));
    let sha8: String = sha256.unwrap_or("").chars().take(8).collect();
    if !sha8.is_empty() {
        if let Some(exact) = by_folder(&format!("{version_build}-{sha8}")) {
            return Some(exact);
        }
    }
    let legacy = by_folder(version_build)?;
    if let Some(s) = &legacy.sha256 {
        return (Some(s.as_str()) == sha256).then_some(legacy);
    }
    (count <= 1).then_some(legacy)
}

/// Every installed build, sorted by repo then version, descending.
pub fn list_installed() -> Vec<InstalledVersion> {
    let mut installed = Vec::new();
    let active = load_config().get("active_version").and_then(Value::as_str).map(str::to_string);
    let Ok(repos) = fs::read_dir(browsers_dir()) else { return installed };
    for repo_dir in repos.flatten() {
        let name = repo_dir.file_name().to_string_lossy().to_string();
        if !repo_dir.path().is_dir() || name.starts_with('.') {
            continue;
        }
        let Ok(versions) = fs::read_dir(repo_dir.path()) else { continue };
        for version_dir in versions.flatten() {
            let path = version_dir.path();
            if !path.join("version.json").exists() {
                continue;
            }
            let Ok(ver) = Version::from_path(&path) else { continue };
            let data: Value = fs::read(path.join("version.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            let rel = format!("browsers/{name}/{}", version_dir.file_name().to_string_lossy());
            installed.push(InstalledVersion {
                repo_name: name.clone(),
                version: ver,
                path,
                is_active: active.as_deref() == Some(rel.as_str()),
                is_prerelease: data["prerelease"].as_bool().unwrap_or(false),
                asset_updated_at: data["asset_updated_at"].as_str().map(str::to_string),
                asset_size: data["asset_size"].as_i64(),
                sha256: data["sha256"].as_str().map(str::to_string),
                created_at: data["created_at"].as_str().map(str::to_string),
            });
        }
    }
    installed.sort_by(|a, b| {
        b.repo_name
            .cmp(&a.repo_name)
            .then_with(|| b.version.partial_cmp(&a.version).unwrap_or(std::cmp::Ordering::Equal))
    });
    installed
}

/// The active install: the paired build when there is one, else the user's choice.
pub fn get_active_path() -> Option<PathBuf> {
    let mut config = load_config();
    let active = config.get("active_version").and_then(Value::as_str).map(str::to_string);

    if let Some(pin) = browser_pin::effective_pin(Some(&config)) {
        for inst in list_installed() {
            if browser_pin::matches(
                &pin,
                &inst.repo_name,
                inst.version.version.as_deref().unwrap_or(""),
                &inst.version.build,
            ) {
                if active.as_deref() != Some(inst.relative_path().as_str()) {
                    config.insert("active_version".into(), Value::String(inst.relative_path()));
                    let _ = save_config(&config);
                }
                return Some(inst.path);
            }
        }
        return None;
    }

    if let Some(active) = &active {
        let path = install_dir().join(active);
        if path.join("version.json").exists() {
            return Some(path);
        }
    }
    if !browser_pin::is_explicit_choice(&config) {
        if let Some(first) = list_installed().into_iter().next() {
            config.insert("active_version".into(), Value::String(first.relative_path()));
            let _ = save_config(&config);
            return Some(first.path);
        }
    }
    None
}

pub fn set_active(relative_path: &str) -> Result<()> {
    let mut config = load_config();
    config.insert("active_version".into(), Value::String(relative_path.into()));
    save_config(&config)
}

/// An installed build by path, build, full version, or repo/build.
pub fn find_installed_version(specifier: &str) -> Option<PathBuf> {
    let spec = specifier.to_lowercase();
    list_installed().into_iter().find_map(|v| {
        let rel = v.relative_path();
        let hit = rel == specifier
            || rel == format!("browsers/{specifier}")
            || format!("browsers/{}/{}", v.repo_name, v.version.full_string()).ends_with(specifier)
            || format!("{}/{}", v.repo_name, v.version.build).to_lowercase() == spec
            || v.version.build.to_lowercase() == spec
            || v.version.full_string().to_lowercase() == spec
            || v.version.version.as_deref().is_some_and(|x| x.to_lowercase() == spec);
        hit.then_some(v.path)
    })
}

fn chmod_755(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    if path.is_dir() {
        for entry in fs::read_dir(path)?.flatten() {
            let p = entry.path();
            if !p.is_symlink() {
                chmod_755(&p)?;
            }
        }
    }
    Ok(())
}

/// Where a build installs, relative to the install directory:
/// `browsers/<repo>/<version>-<build>[-<sha8>]`.
pub fn install_relative_path(available: &AvailableVersion) -> String {
    let folder = version_folder_name(
        available.version.version.as_deref().unwrap_or(""),
        &available.version.build,
        &available.sha8(),
    );
    format!("browsers/{}/{folder}", repo_name_for(&available.github_repo))
}

/// Install a build to [`install_relative_path`] and make it active.
pub fn install_versioned(available: &AvailableVersion, replace: bool) -> Result<bool> {
    let repo_name = repo_name_for(&available.github_repo);
    let relative = install_relative_path(available);
    let install_path = install_dir().join(&relative);
    let verstr = available.version.full_string();

    if install_path.join("version.json").exists() {
        if !replace {
            let installed = list_installed();
            let changes = installed
                .iter()
                .find(|v| v.version.build == available.version.build && v.repo_name == repo_name)
                .map(|v| v.changes(available))
                .unwrap_or_default();
            let change_msg = if changes.is_empty() { String::new() } else { format!(" ({})", changes.join(", ")) };
            eprint_status(&format!("Version v{verstr} already installed{change_msg}."), Some("yellow"));
            eprint_status(
                if change_msg.is_empty() {
                    "Use --replace to reinstall."
                } else {
                    "Use --replace to update with the new release."
                },
                Some("yellow"),
            );
            set_active(&relative)?;
            return Ok(false);
        }
        eprint_status(&format!("Replacing: {}", install_path.display()), Some("yellow"));
        fs::remove_dir_all(&install_path)?;
    }

    let result = (|| -> Result<()> {
        fs::create_dir_all(&install_path)?;
        eprint_status(&format!("Downloading package: {}", available.url), None);
        let mut tmp = tempfile::tempfile()?;
        let actual = pkgman::webdl_to(&available.url, Some("Downloading Camoufox"), true, &mut tmp)?;
        pkgman::verify_sha256(&actual, available.sha256.as_deref(), &format!("Camoufox v{verstr}"))?;
        eprint_status(&format!("Extracting Camoufox: {}", install_path.display()), None);
        use std::io::Seek;
        tmp.rewind()?;
        pkgman::unzip_from(std::io::BufReader::new(tmp), &install_path, Some("Extracting"))?;
        fs::write(install_path.join("version.json"), serde_json::to_vec(&available.to_metadata())?)?;
        chmod_755(&install_path)?;
        set_active(&relative)?;
        fs::write(compat_flag(), b"")?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&install_path);
        return Err(e);
    }
    eprint_status(&format!("\nCamoufox v{verstr} installed."), Some("green"));
    eprint_status(&format!("Path: {}", install_path.display()), Some("green"));
    Ok(true)
}

/// Remove one installed build.
pub fn remove_version(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    eprint_status(&format!("Removing: {}", path.display()), None);
    fs::remove_dir_all(path)?;
    if let Some(parent) = path.parent() {
        if parent != browsers_dir() && fs::read_dir(parent).map(|mut d| d.next().is_none()).unwrap_or(false) {
            let _ = fs::remove_dir(parent);
        }
    }
    if fs::read_dir(browsers_dir()).map(|mut d| d.next().is_none()).unwrap_or(false) {
        let _ = fs::remove_dir(browsers_dir());
    }
    let mut config = load_config();
    if let Ok(rel) = path.strip_prefix(install_dir()) {
        let rel = rel.to_string_lossy().replace('\\', "/");
        if config.get("active_version").and_then(Value::as_str) == Some(rel.as_str()) {
            let next = list_installed().first().map(|v| Value::String(v.relative_path())).unwrap_or(Value::Null);
            config.insert("active_version".into(), next);
            save_config(&config)?;
        }
    }
    Ok(true)
}

/// A short tag to tell coexisting installs apart: the date, or the sha8.
pub fn installed_label(iv: &InstalledVersion) -> String {
    let date = pkgman::format_asset_date(iv.created_at.as_deref());
    if !date.is_empty() {
        return date;
    }
    iv.sha256.as_deref().unwrap_or("").chars().take(8).collect()
}

/// Print the installed builds as a tree.
pub fn print_tree(show_header: bool, show_paths: bool) {
    let installed = list_installed();
    if installed.is_empty() {
        rprint("No versions installed.", Some("yellow"));
        rprint("Run `rustoufox fetch` to install.", Some("yellow"));
        return;
    }
    if show_header {
        rprint("Installed versions:\n", Some("yellow"));
    }
    let mut current: Option<&str> = None;
    for (i, v) in installed.iter().enumerate() {
        let is_last = i == installed.len() - 1 || installed[i + 1].repo_name != v.repo_name;
        if current != Some(v.repo_name.as_str()) {
            current = Some(&v.repo_name);
            print!("{}", style(&format!("{}/", v.repo_name), Some("cyan"), true));
            if show_paths {
                println!(
                    "{}",
                    style(&format!(" -> {}", browsers_dir().join(&v.repo_name).display()), Some("bright_black"), false)
                );
            } else {
                println!();
            }
        }
        let branch = if is_last { "└── " } else { "├── " };
        let mut line = format!("    {branch}");
        line.push_str(&style(&format!("v{}", v.version.full_string()), v.is_active.then_some("green"), v.is_active));
        line.push_str(&if v.is_prerelease {
            style(" (prerelease)", Some("yellow"), false)
        } else {
            style(" (stable)", Some("blue"), false)
        });
        let tag = installed_label(v);
        if !tag.is_empty() {
            line.push_str(&style(&format!(" ({tag})"), Some("bright_black"), false));
        }
        if v.is_active {
            line.push_str(&style(" (active)", Some("green"), true));
        }
        println!("{line}");
    }
}
