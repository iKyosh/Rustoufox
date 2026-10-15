//! `rustoufox`: the command line of the Rust Camoufox launcher.

use std::collections::HashMap;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use serde_json::{json, Map, Value};

use rustoufox::addons::{maybe_download_addons, DefaultAddons};
use rustoufox::browser_pin::{effective_pin, load_pin};
use rustoufox::geolocation;
use rustoufox::multiversion::{
    self, default_channel, find_install, latest_per_build, list_installed, load_config, load_repo_cache, print_tree,
    remove_version, save_config, save_repo_cache, InstalledVersion,
};
use rustoufox::paths::{install_dir, rprint, style};
use rustoufox::pkgman::{self, AvailableVersion, RepoConfig, Version};
use rustoufox::{Error, GeoIp, Headless, LaunchOptions, Result, ServerOptions};

#[derive(Parser)]
#[command(
    name = "rustoufox",
    version,
    about = "Rust launcher and package manager for the Camoufox anti-detect browser"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum SpoofOs {
    Auto,
    Mac,
    Win,
    Lin,
}

#[derive(Clone, Copy, ValueEnum)]
enum SpoofArch {
    Auto,
    #[value(name = "x86_64")]
    X86_64,
    I686,
    Arm64,
}

#[derive(Clone, Copy, ValueEnum)]
enum ListMode {
    Installed,
    All,
}

#[derive(Subcommand)]
enum Command {
    /// Refresh the list of available builds from every repository.
    Sync {
        #[arg(long, value_enum)]
        spoof_os: Option<SpoofOs>,
        #[arg(long, value_enum)]
        spoof_arch: Option<SpoofArch>,
    },
    /// Install the paired build (or the chosen one), or a specific VERSION.
    Fetch { version: Option<String> },
    /// Pin a build (official/stable/134.0.2-beta.20) or follow a channel (official/stable).
    Set {
        specifier: Option<String>,
        /// Select the GeoIP source instead.
        #[arg(long)]
        geoip: bool,
        /// Go back to the build this release pairs with.
        #[arg(long = "release")]
        paired: bool,
    },
    /// Print the active build.
    Active,
    /// List installed builds, or every available one.
    List {
        #[arg(value_enum, default_value = "installed")]
        mode: ListMode,
        /// Show full paths.
        #[arg(long = "path")]
        show_paths: bool,
    },
    /// Remove one build, pick one interactively, or the whole data directory.
    Remove {
        version: Option<String>,
        #[arg(long)]
        select: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Versions, the active build, the GeoIP database and storage paths.
    Version,
    /// Print the data directory.
    Path,
    /// Open a headed browser with the Playwright inspector.
    Test {
        url: Option<String>,
        #[arg(long)]
        executable_path: Option<std::path::PathBuf>,
    },
    /// Start a Playwright server and print its websocket endpoint.
    Server {
        #[command(flatten)]
        launch: LaunchArgs,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        ws_path: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Print the Playwright firefox.launch() options for a launch, as JSON.
    Options {
        #[command(flatten)]
        launch: LaunchArgs,
    },
}

#[derive(clap::Args)]
struct LaunchArgs {
    /// windows, macos or linux (repeatable).
    #[arg(long)]
    os: Vec<String>,
    /// Run headless (true), headed (false) or on a private Xvfb display (virtual).
    #[arg(long, default_value = "true")]
    headless: String,
    /// Proxy server, e.g. http://host:8080.
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long)]
    proxy_username: Option<String>,
    #[arg(long)]
    proxy_password: Option<String>,
    /// Match timezone, locale and geolocation to the exit IP ("auto" or an IP).
    #[arg(long)]
    geoip: Option<String>,
    /// Locale(s), e.g. fr-FR (repeatable).
    #[arg(long)]
    locale: Vec<String>,
    #[arg(long)]
    humanize: bool,
    #[arg(long)]
    block_images: bool,
    #[arg(long)]
    block_webrtc: bool,
    #[arg(long)]
    canvas_noise: Option<bool>,
    #[arg(long)]
    pin_cpu_cores: bool,
    #[arg(long)]
    debug: bool,
}

impl LaunchArgs {
    fn into_options(self) -> Result<LaunchOptions> {
        let headless = match self.headless.as_str() {
            "true" => Headless::On,
            "false" => Headless::Off,
            "virtual" => Headless::Virtual,
            other => return Err(Error::value(format!("--headless must be true, false or virtual, not {other:?}"))),
        };
        let proxy = self.proxy.map(|server| rustoufox::Proxy {
            server,
            username: self.proxy_username,
            password: self.proxy_password,
            bypass: None,
        });
        Ok(LaunchOptions {
            os: self.os,
            headless,
            proxy,
            geoip: self.geoip.map(|g| if g == "auto" { GeoIp::Auto } else { GeoIp::Ip(g) }),
            locale: self.locale,
            humanize: self.humanize.then_some(rustoufox::Humanize::On),
            block_images: self.block_images,
            block_webrtc: self.block_webrtc,
            canvas_noise: self.canvas_noise,
            pin_cpu_cores: self.pin_cpu_cores,
            debug: self.debug,
            ..Default::default()
        })
    }
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

fn vb(v: &Value) -> String {
    format!("{}-{}", s(v, "version"), s(v, "build"))
}

fn do_sync(spoof_os: Option<&str>, spoof_arch: Option<&str>) -> Result<()> {
    rprint("Syncing repositories...", Some("yellow"));
    let mut repos = Vec::new();
    let mut incompatible = Vec::new();
    for repo in RepoConfig::load_repos() {
        print!("{}", style(&format!("  {}...", repo.name), Some("cyan"), true));
        match pkgman::list_available_versions(&repo, true, spoof_os, spoof_arch, true) {
            Ok(listed) => {
                let (ok, bad): (Vec<AvailableVersion>, Vec<AvailableVersion>) =
                    listed.into_iter().partition(|v| pkgman::interface_supported(v.interface));
                incompatible.extend(bad.iter().map(|v| {
                    json!({"repo": repo.name, "version": v.version.version, "build": v.version.build, "interface": v.interface})
                }));
                println!("{}", style(&format!(" {} versions", ok.len()), Some("green"), true));
                repos.push(json!({"name": repo.name, "repo": repo.repos[0], "versions": ok.iter().map(AvailableVersion::to_cache).collect::<Vec<_>>()}));
            }
            Err(e) => println!("{}", style(&format!(" Error: {e}"), Some("red"), true)),
        }
    }
    let total: usize = repos.iter().map(|r| r["versions"].as_array().map(Vec::len).unwrap_or(0)).sum();
    let count = repos.len();
    let mut cache = Map::new();
    cache.insert("repos".into(), repos.into());
    cache.insert("incompatible".into(), incompatible.clone().into());
    cache.insert("spoof_os".into(), json!(spoof_os));
    cache.insert("spoof_arch".into(), json!(spoof_arch));
    save_repo_cache(&cache)?;
    if let Some(newest) = incompatible.iter().max_by(|a, b| {
        Version::new(&s(a, "build"), a["version"].as_str())
            .partial_cmp(&Version::new(&s(b, "build"), b["version"].as_str()))
            .unwrap()
    }) {
        rprint(
            &pkgman::upgrade_message(
                &Version::new(&s(newest, "build"), newest["version"].as_str()),
                newest["interface"].as_i64().unwrap_or(1),
            ),
            Some("yellow"),
        );
    }
    let platform = spoof_os.map(|o| format!(" ({o}/{})", spoof_arch.unwrap_or("auto"))).unwrap_or_default();
    rprint(&format!("\nSynced {total} versions from {count} repos{platform}."), Some("green"));
    Ok(())
}

fn repo_data<'a>(cache: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    cache.get("repos")?.as_array()?.iter().find(|r| s(r, "name").eq_ignore_ascii_case(name))
}

fn versions(repo: &Value) -> Vec<Value> {
    repo["versions"].as_array().cloned().unwrap_or_default()
}

/// (asset, sha) for a version-build[-sha8] spec.
fn resolve_spec(repo: &Value, spec: &str) -> (Option<Value>, Option<String>) {
    let all = versions(repo);
    for v in &all {
        let sha = s(v, "sha256");
        if !sha.is_empty() && spec == format!("{}-{}", vb(v), &sha[..8.min(sha.len())]) {
            return (Some(v.clone()), Some(sha));
        }
    }
    (latest_per_build(&all).into_iter().find(|v| vb(v) == spec), None)
}

fn pin_target(repo: &Value, pinned: &str, pinned_sha: Option<&str>) -> Option<Value> {
    let all = versions(repo);
    match pinned_sha {
        Some(sha) => all.into_iter().find(|v| v["sha256"].as_str() == Some(sha)),
        None => latest_per_build(&all).into_iter().find(|v| vb(v) == pinned),
    }
}

fn install(repo: &Value, ver: &Value, confirm_prerelease: bool) -> Result<()> {
    let available = AvailableVersion::from_cache(ver, &s(repo, "repo"));
    let current = pkgman::installed_verstr().ok();
    if current.as_deref() == Some(available.version.full_string().as_str()) {
        rprint("Camoufox binaries up to date!", Some("green"));
        rprint(&format!("Current version: v{}", available.version.full_string()), Some("green"));
        return Ok(());
    }
    if available.is_prerelease && confirm_prerelease {
        rprint(&format!("Warning: v{} is a prerelease version!", available.version.full_string()), Some("yellow"));
        if !Confirm::new().with_prompt("Continue with prerelease installation?").interact().unwrap_or(false) {
            rprint("Installation cancelled.", Some("red"));
            return Ok(());
        }
    }
    let action = if current.is_some() { "Installing" } else { "Fetching" };
    rprint(&format!("{action} Camoufox v{}...", available.version.full_string()), Some("yellow"));
    multiversion::install_versioned(&available, false)?;
    pkgman::ensure_browser_profile_dir(None)?;
    Ok(())
}

fn fetch(version: Option<String>) -> Result<()> {
    multiversion::remove_incompatible_data()?;
    do_sync(None, None)?;
    let cache = load_repo_cache();
    let config = load_config();
    let mut paired = false;
    let (repo_name, ver): (String, Option<(Value, Value)>) = if let Some(version) = &version {
        let parts: Vec<String> = version.to_lowercase().split('/').map(str::to_string).collect();
        let (repo, spec) = match parts.len() {
            1 => (RepoConfig::default_name(), parts[0].clone()),
            2 | 3 => (parts[0].clone(), parts[parts.len() - 1].clone()),
            _ => {
                rprint("Format: version-build, repo/version-build, or repo/channel/version-build", Some("red"));
                return Ok(());
            }
        };
        let found = repo_data(&cache, &repo)
            .and_then(|r| resolve_spec(r, spec.trim_start_matches('v')).0.map(|v| (r.clone(), v)));
        if found.is_none() {
            for v in cache.get("incompatible").and_then(Value::as_array).into_iter().flatten() {
                if s(v, "repo").eq_ignore_ascii_case(&repo) && vb(v) == spec.trim_start_matches('v') {
                    rprint(
                        &pkgman::incompatible_message(
                            &Version::new(&s(v, "build"), v["version"].as_str()),
                            v["interface"].as_i64().unwrap_or(1),
                        ),
                        Some("red"),
                    );
                    return Ok(());
                }
            }
        }
        (repo, found)
    } else if let Some(pinned) = config.get("pinned").and_then(Value::as_str) {
        let channel = config.get("channel").and_then(Value::as_str).unwrap_or("");
        let repo = channel.split('/').next().unwrap_or("").to_string();
        let found = repo_data(&cache, &repo).and_then(|r| {
            pin_target(r, pinned, config.get("pinned_sha").and_then(Value::as_str)).map(|v| (r.clone(), v))
        });
        (repo, found)
    } else if let Some(pin) = effective_pin(Some(&config)) {
        let found =
            repo_data(&cache, &pin.repo_name).and_then(|r| pin_target(r, &pin.spec(), None).map(|v| (r.clone(), v)));
        if found.is_none() {
            rprint(&format!("{}, the browser this release pairs with, is not in {}'s releases. Run 'rustoufox sync' and try again.", pin.tag, pin.repo_name), Some("red"));
            return Ok(());
        }
        paired = true;
        (pin.repo_name, found)
    } else {
        let channel = config.get("channel").and_then(Value::as_str).map(str::to_string).unwrap_or_else(default_channel);
        let (repo, ctype) = channel
            .split_once('/')
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .unwrap_or((channel.clone(), "stable".into()));
        let found = repo_data(&cache, &repo).and_then(|r| {
            latest_per_build(&versions(r))
                .into_iter()
                .find(|v| v["is_prerelease"].as_bool().unwrap_or(false) == (ctype == "prerelease"))
                .map(|v| (r.clone(), v))
        });
        (repo, found)
    };
    let Some((repo, ver)) = ver else {
        rprint(
            &format!("Version '{}' not found in cache. Run 'rustoufox sync'.", version.unwrap_or(repo_name)),
            Some("red"),
        );
        return Ok(());
    };
    if let Err(e) = install(&repo, &ver, !paired) {
        let msg = e.to_string();
        if msg.contains("404") {
            rprint("Release not found (404). Asset may have been removed.", Some("red"));
            rprint("Run 'rustoufox sync' to refresh available versions.", Some("yellow"));
        } else {
            rprint(&format!("Error: {msg}"), Some("red"));
        }
        return Ok(());
    }
    if let Err(e) = geolocation::download_mmdb(None, true) {
        rprint(&format!("GeoIP database not downloaded: {e}"), Some("yellow"));
    }
    maybe_download_addons(&DefaultAddons::ALL, &mut Vec::new());
    rustoufox::fpgen::model::ensure_model(false)?;
    Ok(())
}

fn find_installed(spec: &str) -> Option<InstalledVersion> {
    let spec = spec.to_lowercase();
    let installed = list_installed();
    let parts: Vec<&str> = spec.split('/').collect();
    for v in &installed {
        if [v.channel_path(), v.relative_path(), v.version.build.clone(), v.version.full_string()]
            .iter()
            .any(|x| x.to_lowercase() == spec)
        {
            return Some(v.clone());
        }
        if parts.len() == 2 && v.repo_name == parts[0] && v.version.full_string().to_lowercase() == parts[1] {
            return Some(v.clone());
        }
    }
    if parts.len() == 2 && (parts[1] == "stable" || parts[1] == "prerelease") {
        let pre = parts[1] == "prerelease";
        return installed.into_iter().find(|v| v.repo_name == parts[0] && v.is_prerelease == pre);
    }
    None
}

fn set_channel(repo: &str, ctype: &str) -> Result<()> {
    let mut config = load_config();
    config.insert("channel".into(), format!("{repo}/{ctype}").into());
    config.remove("pinned");
    config.remove("pinned_sha");
    let cache = load_repo_cache();
    if let Some(r) = repo_data(&cache, repo) {
        if let Some(latest) =
            versions(r).into_iter().find(|v| v["is_prerelease"].as_bool().unwrap_or(false) == (ctype == "prerelease"))
        {
            if let Some(inst) = list_installed()
                .into_iter()
                .find(|i| i.version.build == s(&latest, "build") && i.repo_name == repo.to_lowercase())
            {
                config.insert("active_version".into(), inst.relative_path().into());
                save_config(&config)?;
                println!("{}", style(&format!("Channel: {}/{ctype}", repo.to_lowercase()), Some("cyan"), true));
                println!(
                    "{}",
                    style(&format!("Using latest: {} (installed)", inst.channel_path()), Some("green"), false)
                );
                return Ok(());
            }
        }
    }
    config.remove("active_version");
    save_config(&config)?;
    println!("{}", style(&format!("Channel: {}/{ctype}", repo.to_lowercase()), Some("cyan"), true));
    println!("{}", style("Run 'rustoufox fetch' to install latest.", Some("yellow"), false));
    Ok(())
}

fn set_pinned(repo: &str, ctype: &str, ver: &Value, inst: Option<&InstalledVersion>, sha: Option<&str>) -> Result<()> {
    let mut config = load_config();
    config.insert("channel".into(), format!("{}/{ctype}", repo.to_lowercase()).into());
    config.insert("pinned".into(), vb(ver).into());
    match sha {
        Some(sha) => config.insert("pinned_sha".into(), sha.into()),
        None => config.remove("pinned_sha"),
    };
    let tag = sha.map(|s| format!(" ({})", &s[..8.min(s.len())])).unwrap_or_default();
    let display = format!("{}/{ctype}/{}{tag}", repo.to_lowercase(), vb(ver));
    match inst {
        Some(inst) => {
            config.insert("active_version".into(), inst.relative_path().into());
            save_config(&config)?;
            println!("{}", style(&format!("Pinned: {display} (installed)"), Some("green"), false));
        }
        None => {
            config.remove("active_version");
            save_config(&config)?;
            println!("{}", style(&format!("Pinned: {display}"), Some("cyan"), true));
            println!("{}", style("Run 'rustoufox fetch' to install.", Some("yellow"), false));
        }
    }
    Ok(())
}

fn select_geoip() -> Result<()> {
    let (repos, _) = geolocation::geoip_repos();
    let current = geolocation::load_geoip_config().map(|c| s(&c, "name")).unwrap_or_default();
    let labels: Vec<String> = repos
        .iter()
        .map(|r| {
            let mut l = s(r, "name");
            if r["deprecated"].as_bool().unwrap_or(false) {
                l.push_str(" (deprecated)");
            }
            if s(r, "name") == current {
                l.push_str(" [active]");
            }
            l
        })
        .collect();
    let Ok(Some(i)) =
        Select::with_theme(&ColorfulTheme::default()).with_prompt("Select GeoIP source").items(&labels).interact_opt()
    else {
        return Ok(());
    };
    geolocation::save_geoip_config(&repos[i], true)?;
    rprint(&format!("GeoIP source: {}", s(&repos[i], "name")), Some("green"));
    geolocation::warn_if_deprecated(&repos[i]);
    Ok(())
}

fn set_cmd(specifier: Option<String>, geoip: bool, paired: bool) -> Result<()> {
    if geoip {
        return select_geoip();
    }
    if paired {
        let Some(pin) = load_pin() else {
            rprint("This rustoufox is a development copy; it pairs with no particular browser.", Some("yellow"));
            return Ok(());
        };
        let mut config = load_config();
        for k in ["channel", "pinned", "pinned_sha", "active_version"] {
            config.remove(k);
        }
        save_config(&config)?;
        println!(
            "{}",
            style(
                &format!("Using the paired browser: {} {} ({})", pin.repo_name, pin.spec(), pin.tag),
                Some("green"),
                false
            )
        );
        println!("{}", style("Run 'rustoufox fetch' if it is not installed.", Some("yellow"), false));
        return Ok(());
    }
    let cache = load_repo_cache();
    if let Some(spec) = specifier {
        let parts: Vec<String> = spec.to_lowercase().split('/').map(str::to_string).collect();
        let (repo, spec) = match parts.len() {
            2 => {
                if parts[1] != "stable" && parts[1] != "prerelease" {
                    rprint(&format!("Unknown channel type '{}'. Use 'stable' or 'prerelease'.", parts[1]), Some("red"));
                    return Ok(());
                }
                return set_channel(&parts[0], &parts[1]);
            }
            1 => (RepoConfig::default_name(), parts[0].clone()),
            3 => {
                if parts[1] != "stable" && parts[1] != "prerelease" {
                    rprint(&format!("Unknown channel type '{}'. Use 'stable' or 'prerelease'.", parts[1]), Some("red"));
                    return Ok(());
                }
                (parts[0].clone(), parts[2].clone())
            }
            _ => {
                rprint(&format!("Invalid specifier '{spec}'."), Some("red"));
                rprint("Use: version-build, repo/channel, or repo/channel/version-build", Some("yellow"));
                return Ok(());
            }
        };
        if !multiversion::repo_cache_file().exists() {
            rprint("No repo cache found. Run 'rustoufox sync' first.", Some("red"));
            return Ok(());
        }
        let Some(r) = repo_data(&cache, &repo) else {
            rprint(&format!("Repo '{}' not in cache. Run 'rustoufox sync'.", repo.to_lowercase()), Some("red"));
            return Ok(());
        };
        let (Some(ver), sha) = resolve_spec(r, spec.trim_start_matches('v')) else {
            rprint(&format!("Version '{spec}' not found in {}.", repo.to_lowercase()), Some("red"));
            return Ok(());
        };
        let ctype = if ver["is_prerelease"].as_bool().unwrap_or(false) { "prerelease" } else { "stable" };
        let count = versions(r).iter().filter(|x| vb(x) == vb(&ver)).count();
        let installed = list_installed();
        let inst = find_install(&vb(&ver), ver["sha256"].as_str(), &installed, count);
        return set_pinned(&s(r, "name"), ctype, &ver, inst, sha.as_deref());
    }

    let Some(repos) = cache.get("repos").and_then(Value::as_array).filter(|r| !r.is_empty()) else {
        rprint("No versions in cache. Run 'rustoufox sync' first.", Some("red"));
        return Ok(());
    };
    let mut channels: Vec<(String, &'static str, Vec<Value>)> = Vec::new();
    for r in repos {
        for (ctype, pre) in [("stable", false), ("prerelease", true)] {
            let list: Vec<Value> =
                versions(r).into_iter().filter(|v| v["is_prerelease"].as_bool().unwrap_or(false) == pre).collect();
            if !list.is_empty() {
                channels.push((s(r, "name"), ctype, list));
            }
        }
    }
    let mut labels = vec!["Set channel".to_string()];
    labels.extend(channels.iter().map(|(n, c, _)| format!("Pin version: {}/{c}", n.to_lowercase())));
    labels.push("Exit".into());
    let Ok(Some(choice)) =
        Select::with_theme(&ColorfulTheme::default()).with_prompt("Select").items(&labels).interact_opt()
    else {
        return Ok(());
    };
    if choice == labels.len() - 1 {
        return Ok(());
    }
    if choice == 0 {
        let items: Vec<String> = channels
            .iter()
            .map(|(n, c, l)| {
                format!(
                    "{}/{c} (latest: v{})",
                    n.to_lowercase(),
                    latest_per_build(l).first().map(vb).unwrap_or_default()
                )
            })
            .collect();
        if let Ok(Some(i)) =
            Select::with_theme(&ColorfulTheme::default()).with_prompt("Set channel").items(&items).interact_opt()
        {
            set_channel(&channels[i].0, channels[i].1)?;
        }
        return Ok(());
    }
    let (name, ctype, list) = &channels[choice - 1];
    let installed = list_installed();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in list {
        *counts.entry(vb(v)).or_default() += 1;
    }
    let items: Vec<String> = list
        .iter()
        .map(|v| {
            let sha = s(v, "sha256");
            let date = pkgman::format_asset_date(v["created_at"].as_str());
            let inst = find_install(&vb(v), v["sha256"].as_str(), &installed, counts[&vb(v)]).is_some();
            let mut cells = vec![format!("v{}", vb(v))];
            if !date.is_empty() {
                cells.push(date);
            }
            if !sha.is_empty() {
                cells.push(format!("({})", &sha[..8.min(sha.len())]));
            }
            if inst {
                cells.push("(installed)".into());
            }
            cells.join("  ")
        })
        .collect();
    if let Ok(Some(i)) = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Pin version ({}/{ctype})", name.to_lowercase()))
        .items(&items)
        .interact_opt()
    {
        let ver = &list[i];
        let inst = find_install(&vb(ver), ver["sha256"].as_str(), &installed, counts[&vb(ver)]);
        set_pinned(name, ctype, ver, inst, ver["sha256"].as_str())?;
    }
    Ok(())
}

fn active_cmd() {
    let config = load_config();
    let channel = config.get("channel").and_then(Value::as_str).map(str::to_string).unwrap_or_else(default_channel);
    let label = |v: &InstalledVersion| match v.sha256.as_deref() {
        Some(sha) if !sha.is_empty() => format!("{} ({})", v.channel_path(), &sha[..8.min(sha.len())]),
        _ => v.channel_path(),
    };
    if let Some(pin) = effective_pin(Some(&config)) {
        match find_installed(&format!("{}/{}", pin.repo_name, pin.spec())) {
            Some(t) => println!("{} (paired with this release)", label(&t)),
            None => println!(
                "{}/{} (paired with this release) {}",
                pin.repo_name,
                pin.spec(),
                style("(not fetched)", Some("yellow"), true)
            ),
        }
        return;
    }
    if let Some(pinned) = config.get("pinned").and_then(Value::as_str) {
        let target = match config.get("pinned_sha").and_then(Value::as_str) {
            Some(sha) => list_installed().into_iter().find(|v| v.sha256.as_deref() == Some(sha)),
            None => find_installed(&format!("{}/{pinned}", channel.to_lowercase())),
        };
        match target {
            Some(t) => println!("{}", label(&t)),
            None => println!("{}/{pinned} {}", channel.to_lowercase(), style("(not fetched)", Some("yellow"), true)),
        }
        return;
    }
    match list_installed().into_iter().find(|v| v.is_active) {
        Some(v) => println!("{}", label(&v)),
        None => println!("{} {}", channel.to_lowercase(), style("(not fetched)", Some("yellow"), true)),
    }
}

fn list_cmd(mode: ListMode, show_paths: bool) {
    match mode {
        ListMode::Installed => {
            print_tree(true, show_paths);
            println!();
            println!("{}", style("geoip/", Some("cyan"), true));
            match geolocation::load_geoip_config() {
                Ok(src) if geolocation::mmdb_path("ipv4", &src).exists() => {
                    let p = geolocation::mmdb_path("ipv4", &src);
                    println!(
                        "    └── {} {}",
                        p.file_name().unwrap_or_default().to_string_lossy(),
                        style(&format!("({})", s(&src, "name")), Some("green"), false)
                    );
                }
                _ => rprint("    └── Not downloaded", Some("yellow")),
            }
        }
        ListMode::All => {
            if !multiversion::repo_cache_file().exists() {
                rprint("No repo cache found. Run 'rustoufox sync' first.", Some("red"));
                return;
            }
            let cache = load_repo_cache();
            let installed: HashMap<String, InstalledVersion> =
                list_installed().into_iter().map(|v| (v.version.build.clone(), v)).collect();
            rprint("Available versions:\n", Some("yellow"));
            for r in cache.get("repos").and_then(Value::as_array).into_iter().flatten() {
                println!("{}", style(&format!("{}/", s(r, "name")), Some("cyan"), true));
                let list = latest_per_build(&versions(r));
                for (i, v) in list.iter().enumerate() {
                    let inst = installed.get(&s(v, "build"));
                    let active = inst.is_some_and(|i| i.is_active);
                    let mut line = format!("    {}", if i == list.len() - 1 { "└── " } else { "├── " });
                    line.push_str(&style(&format!("v{}", vb(v)), active.then_some("green"), active));
                    line.push_str(&if v["is_prerelease"].as_bool().unwrap_or(false) {
                        style(" (prerelease)", Some("yellow"), false)
                    } else {
                        style(" (stable)", Some("blue"), false)
                    });
                    if let Some(inst) = inst {
                        line.push_str(&style(
                            if inst.is_active { " (installed, active)" } else { " (installed)" },
                            Some("green"),
                            inst.is_active,
                        ));
                        if show_paths {
                            line.push_str(&style(&format!(" -> {}", inst.path.display()), Some("bright_black"), false));
                        }
                    }
                    println!("{line}");
                }
                println!();
            }
        }
    }
}

fn remove_cmd(version: Option<String>, select: bool, yes: bool) -> Result<()> {
    let confirm = |msg: &str| yes || Confirm::new().with_prompt(msg).interact().unwrap_or(false);
    if select {
        let installed = list_installed();
        if installed.is_empty() {
            rprint("No browser versions installed.", Some("yellow"));
            return Ok(());
        }
        let labels: Vec<String> = installed
            .iter()
            .map(|v| {
                let tag = multiversion::installed_label(v);
                format!(
                    "{}{}{}",
                    v.channel_path(),
                    if tag.is_empty() { String::new() } else { format!(" ({tag})") },
                    if v.is_active { " [active]" } else { "" }
                )
            })
            .collect();
        let Ok(Some(i)) = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select version to remove")
            .items(&labels)
            .interact_opt()
        else {
            rprint("Cancelled.", Some("yellow"));
            return Ok(());
        };
        if confirm(&format!("Remove {}?", installed[i].channel_path())) {
            remove_version(&installed[i].path)?;
            rprint(&format!("Removed {}", installed[i].channel_path()), Some("green"));
        }
        return Ok(());
    }
    if let Some(v) = version {
        let Some(target) = find_installed(&v) else {
            rprint(&format!("Version '{v}' not found."), Some("red"));
            return Ok(());
        };
        if confirm(&format!("Remove {}?", target.channel_path())) {
            remove_version(&target.path)?;
            rprint(&format!("Removed {}", target.channel_path()), Some("green"));
        }
        return Ok(());
    }
    let dir = install_dir();
    if !dir.exists() || std::fs::read_dir(dir)?.next().is_none() {
        rprint("Nothing to remove.", Some("yellow"));
        return Ok(());
    }
    if confirm(&format!("Remove the camoufox data directory ({})?", dir.display())) {
        std::fs::remove_dir_all(dir)?;
        rprint("Removed camoufox data directory.", Some("green"));
    }
    Ok(())
}

fn dir_size(path: &std::path::Path) -> String {
    fn walk(p: &std::path::Path) -> u64 {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| {
                let p = e.path();
                if p.is_dir() && !p.is_symlink() {
                    walk(&p)
                } else {
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                }
            })
            .sum()
    }
    if !path.exists() {
        return "Nothing here".into();
    }
    let mut total = walk(path) as f64;
    for unit in ["B", "KB", "MB"] {
        if total < 1024.0 {
            return if unit == "B" { format!("{total} B") } else { format!("{total:.1} {unit}") };
        }
        total /= 1024.0;
    }
    format!("{total:.1} GB")
}

fn version_cmd() {
    let row = |label: &str, value: &str, fg: Option<&str>| {
        println!("  {:<28}{}", style(label, None, false), style(value, fg, false))
    };
    println!("{}", style("Packages", None, true));
    row("rustoufox", &format!("v{}", env!("CARGO_PKG_VERSION")), Some("green"));
    row("Python launcher parity", &format!("v{}", pkgman::LIBRARY_COMPAT_VERSION), Some("green"));
    row("fpgen model", &rustoufox::fpgen::model::pin().tag, Some("green"));
    row("Playwright", &rustoufox::server::playwright_version().unwrap_or_else(|| "not found".into()), Some("green"));
    println!("{}", style("Browser", None, true));
    let config = load_config();
    let channel = config.get("channel").and_then(Value::as_str).map(str::to_string).unwrap_or_else(default_channel);
    match config.get("pinned").and_then(Value::as_str) {
        Some(p) => row("Active", &format!("{}/{p}", channel.to_lowercase()), Some("green")),
        None => row("Active", &channel.to_lowercase(), Some("green")),
    }
    if let Some(pin) = load_pin() {
        row("Paired build", &format!("{} {}", pin.repo_name, pin.spec()), Some("green"));
    }
    let active = list_installed().into_iter().find(|v| v.is_active);
    match &active {
        Some(v) => {
            row("Current browser", &format!("v{}", v.version.full_string()), Some("green"));
            if let Some(sha) = &v.sha256 {
                row("SHA256", &sha[..12.min(sha.len())], None);
            }
        }
        None => row("Current browser", "Not installed", Some("red")),
    }
    let sync = std::fs::metadata(multiversion::repo_cache_file())
        .and_then(|m| m.modified())
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| "Never".into());
    row("Last Sync", &sync, None);
    println!("{}", style("GeoIP", None, true));
    match geolocation::load_geoip_config() {
        Ok(src) if geolocation::mmdb_path("ipv4", &src).exists() => row("Database", &s(&src, "name"), Some("green")),
        _ => row("Database", "Not installed", None),
    }
    println!("{}", style("Storage", None, true));
    row("Install path", &install_dir().display().to_string(), Some("cyan"));
    row("Browser(s) directory size", &dir_size(&multiversion::browsers_dir()), None);
    row("GeoIP database size", &dir_size(&geolocation::geoip_dir()), None);
    row("fpgen model size", &dir_size(&rustoufox::fpgen::model::model_dir()), None);
    row("Config file", &multiversion::config_file().display().to_string(), Some("cyan"));
    row("Repo cache", &multiversion::repo_cache_file().display().to_string(), Some("cyan"));
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Sync { spoof_os, spoof_arch } => {
            let os = spoof_os.and_then(|o| match o {
                SpoofOs::Auto => None,
                SpoofOs::Mac => Some("mac"),
                SpoofOs::Win => Some("win"),
                SpoofOs::Lin => Some("lin"),
            });
            let arch = spoof_arch.and_then(|a| match a {
                SpoofArch::Auto => None,
                SpoofArch::X86_64 => Some("x86_64"),
                SpoofArch::I686 => Some("i686"),
                SpoofArch::Arm64 => Some("arm64"),
            });
            do_sync(os, arch)
        }
        Command::Fetch { version } => fetch(version),
        Command::Set { specifier, geoip, paired } => set_cmd(specifier, geoip, paired),
        Command::Active => {
            active_cmd();
            Ok(())
        }
        Command::List { mode, show_paths } => {
            list_cmd(mode, show_paths);
            Ok(())
        }
        Command::Remove { version, select, yes } => remove_cmd(version, select, yes),
        Command::Version => {
            version_cmd();
            Ok(())
        }
        Command::Path => {
            println!("{}", install_dir().display());
            Ok(())
        }
        Command::Test { url, executable_path } => {
            let mut config = rustoufox::coherence::Config::new();
            config.insert("showcursor".into(), false.into());
            rustoufox::server::inspect(
                LaunchOptions { headless: Headless::Off, config, executable_path, ..Default::default() },
                url.as_deref(),
            )
        }
        Command::Server { launch, port, ws_path, host } => {
            // A signal must still close the browser and the virtual display.
            let (stop_tx, stop) = std::sync::mpsc::channel::<()>();
            ctrlc::set_handler(move || {
                let _ = stop_tx.send(());
            })
            .map_err(|e| Error::runtime(format!("cannot install the signal handler: {e}")))?;
            let mut server = rustoufox::launch_server(launch.into_options()?, ServerOptions { port, ws_path, host })?;
            // Nothing else is drawn while the browser runs.
            rustoufox::fpgen::release_network();
            println!("Websocket endpoint: {}", style(server.ws_endpoint(), Some("yellow"), true));
            // A signal wakes this up at once; the timeout only checks
            // whether the server died on its own.
            while server.try_wait()?.is_none() {
                match stop.recv_timeout(std::time::Duration::from_millis(200)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
            server.close()
        }
        Command::Options { launch } => {
            let opts = launch.into_options()?;
            if opts.headless == Headless::Virtual {
                return Err(Error::value("`options` cannot start a virtual display; pass --headless true or false"));
            }
            let config = rustoufox::launch_options(opts)?;
            let mut json = config.to_playwright();
            strip_caller_env(&mut json);
            println!("{}", serde_json::to_string_pretty(&json)?);
            Ok(())
        }
    }
}

/// Keep in `env` only what rustoufox adds or changes, never the caller's own
/// variables (tokens, credentials): the client merges it with its environment.
fn strip_caller_env(json: &mut serde_json::Value) {
    if let Some(env) = json.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.retain(|k, v| std::env::var(k).ok().as_deref() != v.as_str());
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Let `rustoufox … | head` end quietly, like `cat`, instead of panicking
    // on EPIPE. `server` and `test` write to Node's stdin, where a closed pipe
    // must stay an error rather than kill the launcher.
    #[cfg(unix)]
    if !matches!(cli.command, Command::Server { .. } | Command::Test { .. }) {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", style(&format!("Error: {e}"), Some("red"), true));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_env_leaves_out_the_caller_environment() {
        let (home, path) = (std::env::var("HOME").unwrap(), std::env::var("PATH").unwrap());
        let mut json = serde_json::json!({ "env": {
            "HOME": home,
            "PATH": format!("{path}:/extra"),
            "CAMOU_CONFIG_1": "{}",
        }});
        strip_caller_env(&mut json);
        let env = json["env"].as_object().unwrap();
        assert!(!env.contains_key("HOME"));
        assert!(env.contains_key("PATH"), "a changed variable is kept");
        assert!(env.contains_key("CAMOU_CONFIG_1"));
    }
}
