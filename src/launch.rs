//! Building a launch: the identity (CAMOU_CONFIG), the Firefox prefs, the
//! environment and the Playwright `firefox.launch()` options.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::addons::{self, DefaultAddons};
use crate::coherence::{self, int, Config};
use crate::display;
use crate::error::{Error, Result};
use crate::fingerprints::{self as fp, Screen};
use crate::geolocation;
use crate::host_rendering::{self, Gpu};
use crate::ip::{self, Proxy};
use crate::locales;
use crate::paths::{install_dir, launcher_constants, OS_NAME};
use crate::pkgman::{self, Version};
use crate::warnings::{self, Warning};
use crate::webgl;

/// `headless`: a normal window, Firefox headless, or a headed browser on a private Xvfb display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Headless {
    #[default]
    Off,
    On,
    /// Linux only: a headed browser on a private Xvfb display.
    Virtual,
}

impl From<bool> for Headless {
    fn from(b: bool) -> Self {
        if b {
            Headless::On
        } else {
            Headless::Off
        }
    }
}

/// `geoip`: find the public IP (through the proxy when set), or use this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeoIp {
    Auto,
    Ip(String),
}

/// `humanize`: human cursor movement, optionally capped at a number of seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Humanize {
    On,
    MaxSeconds(f64),
}

/// `fingerprint_preset`: a random bundled preset, or this one.
#[derive(Debug, Clone, PartialEq)]
pub enum FingerprintPreset {
    Random,
    Preset(Value),
}

/// Every option of a Camoufox launch. Fields left at their default behave
/// as the Python launcher's defaults.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// Raw Camoufox properties (properties.json). Overrides the generated identity.
    pub config: Config,
    /// "windows", "macos" and/or "linux". Empty: fpgen's real-world mix.
    pub os: Vec<String>,
    pub block_images: bool,
    pub block_webrtc: bool,
    pub block_webgl: bool,
    pub disable_coop: bool,
    /// A (vendor, renderer) fpgen recorded for Firefox on `os`. Needs `os`.
    pub webgl_config: Option<(String, String)>,
    pub geoip: Option<GeoIp>,
    pub geoip_db: Option<String>,
    pub humanize: Option<Humanize>,
    /// Locales; the first is used for Intl.
    pub locale: Vec<String>,
    /// Paths to extracted addons.
    pub addons: Vec<String>,
    /// Extra font families on top of the OS's own.
    pub fonts: Vec<String>,
    pub custom_fonts_only: bool,
    pub exclude_addons: Vec<DefaultAddons>,
    /// Bounds the generated screen.
    pub screen: Option<Screen>,
    /// Fixed outer window size.
    pub window: Option<(i64, i64)>,
    /// An fpgen fingerprint to use instead of drawing one.
    pub fingerprint: Option<Value>,
    pub fingerprint_preset: Option<FingerprintPreset>,
    /// Firefox version to claim. Warns.
    pub ff_version: Option<u32>,
    pub headless: Headless,
    pub main_world_eval: bool,
    pub allow_addon_new_tab: bool,
    pub executable_path: Option<PathBuf>,
    /// Launch another installed build ("official/beta.20", "beta.20", "134.0.2-beta.20"). Never downloads.
    pub browser: Option<String>,
    pub firefox_user_prefs: Map<String, Value>,
    pub proxy: Option<Proxy>,
    pub enable_cache: bool,
    pub args: Vec<String>,
    /// Environment for the browser. None: a copy of this process's environment.
    pub env: Option<BTreeMap<String, String>>,
    pub i_know_what_im_doing: bool,
    pub debug: bool,
    /// An existing X display, e.g. ":99".
    pub virtual_display: Option<String>,
    /// Pin the browser to navigator.hardwareConcurrency cores (Linux, Windows).
    pub pin_cpu_cores: bool,
    /// Random canvas and WebGL readback. None: on for another OS or GPU.
    pub canvas_noise: Option<bool>,
    /// Any other Playwright `firefox.launch()` option, in Playwright's camelCase.
    pub extra: Map<String, Value>,
}

/// The result of [`launch_options`]: what Playwright needs to launch the browser.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub executable_path: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub firefox_user_prefs: Map<String, Value>,
    pub headless: bool,
    pub proxy: Option<Proxy>,
    pub extra: Map<String, Value>,
    /// The identity sent to the browser (CAMOU_CONFIG).
    pub camou_config: Config,
    /// The identity's OS key ("win", "mac", "lin").
    pub target_os: String,
    /// For a browser without the canvas placeholder, the machine its contexts
    /// must claim: the host's OS and the host GPU it claims.
    pub host_identity: Option<(String, Option<Gpu>)>,
    /// Number of cores to pin the browser to, when pinning applies.
    pub pin_cores: Option<usize>,
    /// Every warning raised while building the launch.
    pub warnings: Vec<Warning>,
}

impl LaunchConfig {
    /// Playwright `firefox.launch()` options, in Playwright's camelCase.
    pub fn to_playwright(&self) -> Value {
        let mut out = Map::new();
        out.insert("executablePath".into(), self.executable_path.to_string_lossy().into());
        out.insert("args".into(), json!(self.args));
        out.insert("env".into(), json!(self.env));
        out.insert("firefoxUserPrefs".into(), Value::Object(self.firefox_user_prefs.clone()));
        out.insert("headless".into(), self.headless.into());
        if let Some(p) = &self.proxy {
            out.insert("proxy".into(), serde_json::to_value(p).unwrap_or(Value::Null));
        }
        out.extend(self.extra.clone());
        Value::Object(out)
    }

    /// Whether the identity spoofs a window dimension: new pages must then
    /// default to no viewport, or Playwright's default viewport deadlocks Juggler.
    pub fn spoofs_window_dimensions(&self) -> bool {
        ["window.outerWidth", "window.outerHeight", "window.innerWidth", "window.innerHeight"]
            .iter()
            .any(|k| self.camou_config.contains_key(*k))
    }

    /// The `newContext()` options Camoufox applies by default, in the
    /// JavaScript client's form: the host's own media features instead of
    /// Playwright's emulated ones, and `viewport: null` (Python's
    /// `no_viewport=True`) when the window is spoofed. Merge your own options
    /// over these.
    pub fn context_defaults(&self) -> Map<String, Value> {
        let mut m = Map::new();
        for k in ["colorScheme", "reducedMotion", "forcedColors", "contrast"] {
            m.insert(k.into(), "no-override".into());
        }
        if self.spoofs_window_dimensions() {
            m.insert("viewport".into(), Value::Null);
        }
        m
    }

    /// The OS and GPU a new context on this browser claims (None: drawn).
    /// On a browser without canvas noise they are the host's.
    pub fn context_identity(&self, os: Option<&str>, preset: Option<&Value>) -> Result<(Option<String>, Option<Gpu>)> {
        let Some((host_os, gpu)) = &self.host_identity else {
            return Ok((os.map(str::to_string), None));
        };
        let host_name = fp::os_name(host_os).unwrap_or("linux");
        let preset_os = preset.and_then(|p| p["navigator"]["platform"].as_str()).map(fp::platform_to_os);
        let preset_gpu = preset.map(|p| {
            (
                p["webgl"]["unmaskedVendor"].as_str().unwrap_or("").to_string(),
                p["webgl"]["unmaskedRenderer"].as_str().unwrap_or("").to_string(),
            )
        });
        let mismatch = os.unwrap_or(host_name) != host_name
            || preset_os.unwrap_or(host_name) != host_name
            || (gpu.is_some() && preset.is_some() && preset_gpu.as_ref() != gpu.as_ref());
        if mismatch {
            let gpu_text = gpu.as_ref().map(|g| format!(" and its GPU {g:?}")).unwrap_or_default();
            return Err(Error::value(format!(
                "This browser renders its canvas as this {host_name} machine, so its contexts claim it{gpu_text}. \
                 Launch it with canvas_noise=true to give a context another OS or GPU."
            )));
        }
        Ok((Some(host_name.to_string()), gpu.clone()))
    }
}

fn host_os_key() -> Option<&'static str> {
    match std::env::consts::OS {
        "macos" => Some("mac"),
        "windows" => Some("win"),
        "linux" => Some("lin"),
        _ => None,
    }
}

const QUOTA_FIXED_LIMIT_PREF: &str = "dom.quotaManager.temporaryStorage.fixedLimit";

fn disk_total_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        Some(s.f_blocks as u64 * s.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut total: u64 = 0;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                &mut total,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(total)
    }
}

/// Half the capacity of the disk a stock Firefox profile would live on, in KB:
/// the number Gecko's own storage quota is derived from.
fn stock_profile_disk_capacity_kb() -> Option<i64> {
    let home = dirs::home_dir()?;
    let candidates: Vec<PathBuf> = match OS_NAME {
        "win" => {
            vec![std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Mozilla")).unwrap_or(home.clone()), home]
        }
        "mac" => vec![home.join("Library").join("Application Support").join("Firefox"), home],
        _ => vec![home.join(".mozilla"), home],
    };
    for candidate in candidates {
        let mut probe = candidate.as_path();
        while !probe.exists() {
            match probe.parent() {
                Some(p) => probe = p,
                None => break,
            }
        }
        if let Some(total) = disk_total_bytes(probe).filter(|t| *t > 0) {
            return Some(((total / 2 / 1024) as i64).min(i32::MAX as i64));
        }
    }
    None
}

fn generate_fontconfig(fontconfig_path: &Path, fonts_dir: &Path, os_dir: &str) -> Result<PathBuf> {
    let mut scan_dirs: Vec<PathBuf> = Vec::new();
    let groups = fonts_dir.join("groups.json");
    if let (Some(key), Ok(text)) = (fp::os_key(os_dir), fs::read_to_string(&groups)) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            scan_dirs = v["readBy"][key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|g| fonts_dir.join(g))
                .filter(|p| p.is_dir())
                .collect();
        }
    }
    if scan_dirs.is_empty() {
        let per_os = fonts_dir.join(os_dir);
        scan_dirs.push(if per_os.is_dir() { per_os } else { fonts_dir.to_path_buf() });
    }
    let conf = fs::read_to_string(fontconfig_path.join("fonts.conf"))?;
    let dirs: Vec<String> = scan_dirs.iter().map(|d| format!("<dir>{}</dir>", d.display())).collect();
    let conf = conf.replace("<dir prefix=\"cwd\">fonts</dir>", &dirs.join("\n\t"));
    let cache = install_dir().join("fontconfig");
    fs::create_dir_all(&cache)?;
    let hash = hex::encode(Sha256::digest(conf.as_bytes()));
    let out = cache.join(format!("fonts-{}.conf", &hash[..12]));
    if !out.exists() {
        fs::write(&out, conf)?;
    }
    Ok(out)
}

fn chunk_env(prefix: &str, data: &str, out: &mut BTreeMap<String, String>) {
    let size = if OS_NAME == "win" { 2047 } else { 32767 };
    let chars: Vec<char> = data.chars().collect();
    for (i, chunk) in chars.chunks(size).enumerate() {
        out.insert(format!("{prefix}_{}", i + 1), chunk.iter().collect());
    }
}

/// The CAMOU_CONFIG_n variables, and FONTCONFIG_FILE on Linux.
fn get_env_vars(config: &Config, target_os: &str, executable: Option<&Path>) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    chunk_env("CAMOU_CONFIG", &serde_json::to_string(config)?, &mut env);
    if OS_NAME == "lin" {
        let os_dir = fp::os_name(target_os).unwrap_or(target_os);
        let bundle = |parts: &[&str]| -> Result<PathBuf> {
            let rel: PathBuf = parts.iter().collect();
            Ok(match executable {
                Some(p) => p.parent().unwrap_or(Path::new(".")).join(rel),
                None => pkgman::get_path(&rel.to_string_lossy())?,
            })
        };
        let mut fc = bundle(&["fontconfig", os_dir])?;
        if !fc.join("fonts.conf").exists() {
            fc = bundle(&["fontconfigs", os_dir])?;
        }
        if !fc.join("fonts.conf").exists() {
            return Err(Error::FileNotFound(fc.join("fonts.conf")));
        }
        let fonts_dir = bundle(&["fonts"])?;
        env.insert(
            "FONTCONFIG_FILE".into(),
            generate_fontconfig(&fc, &fonts_dir, os_dir)?.to_string_lossy().to_string(),
        );
    }
    Ok(env)
}

/// Prefs the browser applies at startup (CAMOU_PREFS_n), ASCII-only.
fn get_pref_env_vars(prefs: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if !prefs.is_empty() {
        chunk_env("CAMOU_PREFS", &crate::pycompat::json_dumps_ascii(&Value::Object(prefs.clone())), &mut env);
    }
    env
}

fn load_properties(executable: Option<&Path>) -> Result<Map<String, Value>> {
    // The build's own properties.json; the copy bundled with this crate for a
    // build that ships none (an unpackaged objdir build).
    let path = match executable {
        Some(p) => Some(pkgman::build_file(p, "properties.json")),
        None => pkgman::get_path("properties.json").ok(),
    };
    let text = match path.filter(|p| p.exists()) {
        Some(p) => fs::read_to_string(p)?,
        None => crate::paths::data::PROPERTIES.to_string(),
    };
    let list: Vec<Value> = serde_json::from_str(&text)?;
    Ok(list.into_iter().filter_map(|p| Some((p["property"].as_str()?.to_string(), p))).collect())
}

fn validate_type(value: &Value, expected: &str) -> bool {
    let integral = |v: &Value| v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0);
    match expected {
        "str" => value.is_string(),
        "int" => integral(value),
        "uint" => integral(value) && value.as_f64().unwrap_or(-1.0) >= 0.0,
        "double" => value.is_number(),
        "bool" => value.is_boolean(),
        "array" => value.is_array(),
        "dict" => value.is_object(),
        _ => false,
    }
}

const VOICE_FIELDS: [&str; 5] = ["lang", "name", "voiceUri", "isDefault", "isLocalService"];

fn validate_voices(voices: &Value) -> Result<()> {
    let list = voices
        .as_array()
        .ok_or_else(|| Error::InvalidPropertyType("Invalid type for property voices. Expected array".into()))?;
    for (i, v) in list.iter().enumerate() {
        let Some(obj) = v.as_object() else {
            return Err(Error::InvalidPropertyType(format!(
                "Invalid voices[{i}]: expected an object with {{{}}}, got {v}. Camoufox needs full voice objects, not names.",
                VOICE_FIELDS.join(", ")
            )));
        };
        let missing: Vec<&str> = VOICE_FIELDS.iter().copied().filter(|f| !obj.contains_key(*f)).collect();
        if !missing.is_empty() {
            return Err(Error::InvalidPropertyType(format!(
                "Invalid voices[{i}]: missing {}. Every voice needs {{{}}}.",
                missing.join(", "),
                VOICE_FIELDS.join(", ")
            )));
        }
    }
    Ok(())
}

/// Check every config key against properties.json.
pub fn validate_config(config: &Config, executable: Option<&Path>) -> Result<()> {
    let props = load_properties(executable)?;
    for (key, value) in config {
        let Some(entry) = props.get(key) else {
            println!("Skipping unknown patch {key} : {value}");
            continue;
        };
        if let Some(removed) = entry.get("removed") {
            println!("Skipping {key}: removed in {}, it has no effect", removed.as_str().unwrap_or(""));
            continue;
        }
        let expected = entry["type"].as_str().unwrap_or("");
        if !validate_type(value, expected) {
            return Err(Error::InvalidPropertyType(format!(
                "Invalid type for property {key}. Expected {expected}, got {value}"
            )));
        }
        if key == "voices" {
            validate_voices(value)?;
        }
    }
    Ok(())
}

/// The OS a user agent presents.
pub fn determine_ua_os(user_agent: &str) -> &'static str {
    if user_agent.contains("Macintosh") || user_agent.contains("Mac OS X") {
        "mac"
    } else if user_agent.contains("Windows") {
        "win"
    } else {
        "lin"
    }
}

fn is_domain_set(config: &Config, props: &[&str]) -> bool {
    props.iter().any(|p| {
        if p.ends_with('.') || p.ends_with(':') {
            config.keys().any(|k| k.starts_with(p))
        } else {
            config.contains_key(*p)
        }
    })
}

fn warn_manual_config(config: &Config) {
    let k = Some(false);
    if is_domain_set(config, &["navigator.language", "headers.Accept-Language", "locale:"]) {
        warnings::leak("locale", k);
    }
    if is_domain_set(config, &["geolocation:", "timezone"]) {
        warnings::leak("geolocation", k);
    }
    if is_domain_set(config, &["headers.User-Agent"]) {
        warnings::leak("header-ua", k);
    }
    if is_domain_set(config, &["navigator."]) {
        warnings::leak("navigator", k);
    }
    if is_domain_set(config, &["navigator.maxTouchPoints"]) {
        warnings::leak("max_touch_points", k);
    }
    if config.get("instantAnimations").and_then(Value::as_bool) == Some(true) {
        warnings::leak("instant_animations", k);
    }
    if is_domain_set(config, &["screen.", "window.", "document.body."]) {
        warnings::leak("viewport", k);
    }
}

fn check_valid_os(os: &[String]) -> Result<()> {
    for o in os {
        if o.to_lowercase() != *o {
            return Err(Error::InvalidOs(format!("OS values must be lowercase: '{o}'")));
        }
        if !["windows", "macos", "linux"].contains(&o.as_str()) {
            return Err(Error::InvalidOs(format!("Camoufox does not support the OS: '{o}'")));
        }
    }
    Ok(())
}

fn merge_into(target: &mut Map<String, Value>, source: Map<String, Value>) {
    for (k, v) in source {
        target.entry(k).or_insert(v);
    }
}

fn set_into(target: &mut Map<String, Value>, key: &str, value: Value) {
    target.entry(key.to_string()).or_insert(value);
}

/// The Firefox version of the build about to launch.
fn resolve_verstr(executable: Option<&Path>) -> Result<String> {
    let Some(exe) = executable else { return pkgman::installed_verstr() };
    let ini = pkgman::build_file(exe, "application.ini");
    let text = fs::read_to_string(&ini).map_err(|_| Error::FileNotFound(ini.clone()))?;
    text.lines()
        .find_map(|l| l.strip_prefix("Version=").map(|v| v.trim().to_string()))
        .ok_or_else(|| Error::value(format!("{} names no Version", ini.display())))
}

fn warn_if_executable_predates_playwright(executable: Option<&Path>) {
    let Some(exe) = executable else { return };
    let Ok(installed) = Version::from_path(exe.parent().unwrap_or(Path::new("."))) else { return };
    let required = pkgman::effective_version_min();
    if installed >= required {
        return;
    }
    warnings::runtime(format!(
        "The Camoufox build at {} is {}, but the installed Playwright needs at least {}. Contexts created \
         with an explicit viewport will fail with \"Protocol error (Browser.setDefaultViewport)\".",
        exe.display(),
        installed.build,
        required.build
    ));
}

/// Whether the canvas is covered, and the host GPU the identity claims.
fn plan_rendering(
    target_os: &str,
    canvas_noise: Option<bool>,
    named_gpu: bool,
    webgl_disabled: bool,
    probe: &dyn Fn() -> Result<Option<Gpu>>,
) -> Result<(bool, Option<Gpu>)> {
    if canvas_noise == Some(true) {
        return Ok((true, None));
    }
    if Some(target_os) == host_os_key() && !named_gpu {
        let host = if webgl_disabled {
            None
        } else {
            match probe() {
                Ok(h) => h,
                Err(e) => {
                    // Fail closed: an unknown host GPU is treated as one fpgen never recorded.
                    warnings::runtime(format!(
                        "Could not read the host GPU ({e}). The canvas is covered as for another machine.{}",
                        if cfg!(unix) {
                            ""
                        } else {
                            " Install Node.js and playwright-core (or set RUSTOUFOX_PLAYWRIGHT_CORE) to let the \
                             identity claim this machine's GPU."
                        }
                    ));
                    return Ok((canvas_noise.is_none(), None));
                }
            }
        };
        if !host_rendering::renders_on_hardware(host.as_ref()) {
            return Ok((false, None));
        }
        let host = host.unwrap();
        if webgl::firefox_gpus(target_os)?.contains(&host) {
            return Ok((false, Some(host)));
        }
    }
    Ok((canvas_noise.is_none(), None))
}

fn str_of(config: &Config, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Build a launch. Warnings are printed and collected on the result.
pub fn launch_options(opts: LaunchOptions) -> Result<LaunchConfig> {
    warnings::start_collecting();
    let result = build(opts);
    let collected = warnings::stop_collecting();
    result.map(|mut c| {
        c.warnings = collected;
        c
    })
}

fn build(opts: LaunchOptions) -> Result<LaunchConfig> {
    crate::paths::check_host()?;
    let LaunchOptions {
        mut config,
        os,
        block_images,
        block_webrtc,
        block_webgl,
        disable_coop,
        webgl_config,
        geoip,
        geoip_db,
        humanize,
        locale,
        mut addons,
        fonts,
        custom_fonts_only,
        exclude_addons,
        screen,
        window,
        fingerprint,
        fingerprint_preset,
        ff_version,
        headless,
        main_world_eval,
        allow_addon_new_tab,
        mut executable_path,
        browser,
        mut firefox_user_prefs,
        proxy,
        enable_cache,
        args,
        env,
        i_know_what_im_doing,
        debug,
        virtual_display,
        pin_cpu_cores,
        canvas_noise,
        extra,
    } = opts;
    let iknow = Some(i_know_what_im_doing);
    let headless = match headless {
        Headless::Virtual => return Err(Error::value(
            "headless=Virtual needs a virtual display: use Camoufox::launch or launch_server, or pass virtual_display.",
        )),
        h => h == Headless::On,
    };

    let mut env: BTreeMap<String, String> = env.unwrap_or_else(|| std::env::vars().collect());
    pkgman::ensure_browser_profile_dir(Some(&env))?;

    let env_executable = std::env::var("CAMOUFOX_EXECUTABLE_PATH").unwrap_or_default().trim().to_string();
    if let Some(b) = &browser {
        if executable_path.is_some() || !env_executable.is_empty() {
            return Err(Error::value(format!(
                "browser={b:?} selects an installed build, but {} names a binary too. Pass one of them.",
                if executable_path.is_some() { "executable_path" } else { "CAMOUFOX_EXECUTABLE_PATH" }
            )));
        }
        executable_path = Some(pkgman::selected_launch_path(b)?);
    } else if executable_path.is_none() && !env_executable.is_empty() {
        executable_path = Some(PathBuf::from(env_executable));
    }
    let executable_path = executable_path.map(|p| std::path::absolute(&p).unwrap_or(p));

    if let Some(display) = &virtual_display {
        env.insert("DISPLAY".into(), display.clone());
        env.insert("GDK_BACKEND".into(), "x11".into());
        env.remove("WAYLAND_DISPLAY");
        env.insert("MOZ_ENABLE_WAYLAND".into(), "0".into());
    }

    if !i_know_what_im_doing {
        warn_manual_config(&config);
    }
    let user_set_navigator = is_domain_set(&config, &["navigator."]);
    let user_set_screen_window = is_domain_set(&config, &["screen.", "window."]);
    let user_set_media_devices = is_domain_set(&config, &["mediaDevices:"]);
    let user_set_fonts = !fonts.is_empty() || is_domain_set(&config, &["fonts"]);
    let user_set_voices = is_domain_set(&config, &["voices"]);
    let user_set_dnt = config.contains_key("navigator.doNotTrack");
    let user_set_gpc = config.contains_key("navigator.globalPrivacyControl");
    let user_set_accept_encoding = config.contains_key("headers.Accept-Encoding");
    let user_set_audio_seed = config.contains_key("audio:seed");

    let salt = if let Some(f) = &fingerprint {
        fp::identity_salt(Some(f))
    } else if let Some(FingerprintPreset::Preset(p)) = &fingerprint_preset {
        fp::identity_salt(Some(p))
    } else if config.contains_key("navigator.userAgent") {
        fp::identity_salt(Some(&Value::Object(config.clone())))
    } else {
        fp::identity_salt(None)
    };

    if !os.is_empty() {
        check_valid_os(&os)?;
    } else if webgl_config.is_some() {
        return Err(Error::value("OS must be set when using webgl_config"));
    }

    addons::add_default_addons(&mut addons, &exclude_addons);
    if !addons.is_empty() {
        addons::confirm_paths(&addons)?;
        config.insert("addons".into(), json!(addons));
    }

    let browser_binary = match &executable_path {
        Some(p) => p.clone(),
        None => pkgman::launch_path(None)?,
    };
    pkgman::warn_if_package_outdated();

    let ff_version_str = match ff_version {
        Some(v) => {
            warnings::leak("ff_version", iknow);
            v.to_string()
        }
        None => resolve_verstr(executable_path.as_deref())?.split('.').next().unwrap_or("").to_string(),
    };

    let mut used_preset = false;
    if let Some(f) = &fingerprint {
        if !i_know_what_im_doing {
            let ua = f["navigator"]["userAgent"].as_str().unwrap_or("");
            if !ua.contains("Firefox/") {
                return Err(Error::NonFirefoxFingerprint(
                    "Non-Firefox fingerprints are not supported in Camoufox. Using fingerprints from a browser other \
                     than Firefox WILL lead to detection. If this is intentional, pass `i_know_what_im_doing=true`."
                        .into(),
                ));
            }
            warnings::leak("custom_fingerprint", Some(false));
        }
    } else if let Some(choice) = &fingerprint_preset {
        let preset = match choice {
            FingerprintPreset::Preset(p) => Some(p.clone()),
            FingerprintPreset::Random => fp::get_random_preset(&os, Some(&ff_version_str)),
        };
        if let Some(p) = preset {
            merge_into(&mut config, fp::from_preset(&p, Some(&ff_version_str), Some(salt)));
            used_preset = true;
        }
    }

    let screen_cons = screen.or_else(|| {
        if headless || !display::has_display(&env) {
            None
        } else {
            display::largest_display(&env).map(|d| Screen::max(d.width, d.height))
        }
    });

    let mut fingerprint = fingerprint;
    if !used_preset && fingerprint.is_none() {
        fingerprint = Some(fp::generate_fingerprint(window, screen_cons.as_ref(), &os)?);
    }
    if !used_preset {
        if let Some(f) = &fingerprint {
            merge_into(&mut config, fp::from_fpgen(f, Some(&ff_version_str)));
        }
    }

    let target_os: String = match str_of(&config, "navigator.userAgent") {
        Some(ua) => determine_ua_os(&ua).to_string(),
        None => OS_NAME.to_string(),
    };

    if !user_set_screen_window && coherence::screen_is_implausible(&config) {
        coherence::repair_screen_orientation(&mut config);
        fp::raise_screen_to_modern_floor(&mut config);
    }
    if !user_set_navigator {
        fp::fix_navigator_arch(&mut config, &target_os);
        fp::fix_hardware_concurrency(&mut config, Some(pin_cpu_cores));
    }
    if !user_set_screen_window {
        if !used_preset {
            fp::raise_screen_to_modern_floor(&mut config);
        }
        if !headless && virtual_display.is_none() {
            if let Some(s) = &screen_cons {
                fp::clamp_screen_to_display(&mut config, s.max_width, s.max_height);
            }
        }
        fp::follow_baseline_protection(&mut config, &target_os);
        fp::clamp_window_dimensions(&mut config);
        fp::clamp_window_position(&mut config);
    }

    if !fonts.is_empty() {
        config.insert("fonts".into(), json!(fonts));
    }
    let os_name = fp::os_name(&target_os).unwrap_or("macos");
    if custom_fonts_only {
        firefox_user_prefs.insert("gfx.bundled-fonts.activate".into(), 0.into());
        if fonts.is_empty() {
            return Err(Error::value("No custom fonts were passed, but `custom_fonts_only` is enabled."));
        }
        warnings::leak("custom_fonts_only", None);
    } else if !user_set_fonts || config.get("fonts").and_then(Value::as_array).is_none_or(Vec::is_empty) {
        let native = (target_os == "mac" || target_os == "win") && host_os_key() == Some(target_os.as_str());
        let drawn = fp::generate_random_font_subset(os_name, Some(fp::identity_seed(&config, salt)), native, None);
        config.insert("fonts".into(), json!(drawn));
    }

    if !user_set_media_devices {
        fp::set_media_devices_defaults(&mut config, salt);
    }

    if target_os == "win" {
        let presented = config.get("fonts").and_then(Value::as_array).cloned().unwrap_or_default();
        let windows_11 = presented.is_empty()
            || fp::windows11_marker_fonts().iter().any(|m| presented.iter().any(|f| f.as_str() == Some(m)));
        set_into(&mut firefox_user_prefs, "ui.useOverlayScrollbars", (if windows_11 { 1 } else { 0 }).into());
    } else {
        set_into(&mut firefox_user_prefs, "ui.useOverlayScrollbars", 1.into());
    }
    if target_os == "lin" {
        set_into(&mut firefox_user_prefs, "gfx.font_rendering.fallback.async", false.into());
    }
    if let Some(kb) = stock_profile_disk_capacity_kb() {
        set_into(&mut firefox_user_prefs, QUOTA_FIXED_LIMIT_PREF, kb.into());
    }

    if !user_set_dnt {
        config.remove("navigator.doNotTrack");
    }
    if !user_set_gpc {
        config.remove("navigator.globalPrivacyControl");
    }
    let dnt = config.get("navigator.doNotTrack").map(crate::pycompat::py_str);
    firefox_user_prefs.insert("privacy.donottrackheader.enabled".into(), (dnt.as_deref() == Some("1")).into());
    let gpc =
        config.get("navigator.globalPrivacyControl").map(|v| v.as_bool().unwrap_or(!v.is_null())).unwrap_or(false);
    firefox_user_prefs.insert("privacy.globalprivacycontrol.enabled".into(), gpc.into());
    if !user_set_accept_encoding {
        config.remove("headers.Accept-Encoding");
    }
    if !user_set_audio_seed {
        let ident = fp::identity_seed(&config, salt) as u64;
        let seed = (ident.wrapping_mul(2_654_435_761).wrapping_add(97)) & 0xFFFF_FFFF;
        config.insert("audio:seed".into(), (if seed == 0 { 1 } else { seed }).into());
    }

    if let Some(g) = &geoip {
        let ip = match g {
            GeoIp::Ip(ip) => ip.clone(),
            GeoIp::Auto => match &proxy {
                Some(p) => ip::public_ip(Some(&p.as_url()?))?,
                None => ip::public_ip(None)?,
            },
        };
        if !block_webrtc {
            if ip::valid_ipv4(&ip) {
                set_into(&mut config, "webrtc:ipv4", ip.clone().into());
                firefox_user_prefs.insert("network.dns.disableIPv6".into(), true.into());
            } else if ip::valid_ipv6(&ip) {
                set_into(&mut config, "webrtc:ipv6", ip.clone().into());
            }
        }
        let geo = geolocation::get_geolocation(&ip, geoip_db.as_deref())?;
        for (k, v) in geo.as_config() {
            if ["timezone", "locale:language", "locale:region", "locale:script"].contains(&k.as_str()) {
                set_into(&mut config, &k, v);
            } else {
                config.insert(k, v);
            }
        }
    }

    if config.contains_key("geolocation:latitude") && config.contains_key("geolocation:longitude") {
        set_into(&mut firefox_user_prefs, "permissions.default.geo", 1.into());
    } else if let Some(p) = &proxy {
        if !p.server.contains("localhost") && !is_domain_set(&config, &["geolocation"]) {
            warnings::leak("proxy_without_geoip", None);
        }
    }

    if !locale.is_empty() {
        locales::handle_locales(&locale, &mut config)?;
    }
    let requested = match str_of(&config, "locale:language") {
        Some(lang) => [Some(lang), str_of(&config, "locale:script"), str_of(&config, "locale:region")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("-"),
        None => "en-US".into(),
    };
    set_into(&mut firefox_user_prefs, "intl.locale.requested", requested.into());

    if !user_set_voices || !config.contains_key("voices") {
        let mut voice_locale = str_of(&config, "navigator.language");
        if let Some(lang) = str_of(&config, "locale:language") {
            voice_locale = Some(
                [Some(lang), str_of(&config, "locale:region")].into_iter().flatten().collect::<Vec<_>>().join("-"),
            );
        }
        let voices = match fp::generate_random_voice_subset(
            os_name,
            voice_locale.as_deref(),
            Some(fp::identity_seed(&config, salt)),
        ) {
            Ok(v) => v,
            Err(e) => {
                warnings::fallback(
                    "Drawing the speech voices",
                    "no speech voices",
                    &e.to_string(),
                    str_of(&config, "navigator.userAgent").as_deref(),
                );
                Vec::new()
            }
        };
        config.insert("voices".into(), json!(voices));
    }
    set_into(&mut config, "voices:blockIfNotDefined", true.into());

    if let Some(h) = humanize {
        set_into(&mut config, "humanize", true.into());
        if let Humanize::MaxSeconds(s) = h {
            set_into(&mut config, "humanize:maxTime", Value::from(s));
        }
    }
    if main_world_eval {
        set_into(&mut config, "allowMainWorld", true.into());
    }
    if allow_addon_new_tab {
        set_into(&mut config, "allowAddonNewtab", true.into());
    }

    if block_images {
        warnings::leak("block_images", iknow);
    }
    if disable_coop {
        warnings::leak("disable_coop", iknow);
    }
    set_into(&mut firefox_user_prefs, "permissions.default.image", (if block_images { 2 } else { 1 }).into());
    set_into(&mut firefox_user_prefs, "media.peerconnection.enabled", (!block_webrtc).into());
    set_into(&mut firefox_user_prefs, "browser.tabs.remote.useCrossOriginOpenerPolicy", (!disable_coop).into());
    if extra.get("isMobile").and_then(Value::as_bool) == Some(true) {
        warnings::leak("is_mobile", iknow);
    }

    coherence::drop_incoherent_source_values(&mut config, &target_os);

    let probe_exe = browser_binary.to_string_lossy().to_string();
    let probe_env = env.clone();
    let probe = || host_rendering::host_gpu(&probe_exe, headless, &probe_env);
    let named_gpu = webgl_config.is_some()
        || (str_of(&config, "webGl:vendor").is_some() && str_of(&config, "webGl:renderer").is_some());
    let (cover_canvas, host) = plan_rendering(&target_os, canvas_noise, named_gpu, block_webgl, &probe)?;
    if cover_canvas {
        host_rendering::add_canvas_placeholder(&mut firefox_user_prefs);
        warnings::leak("canvas_noise", iknow);
    }

    if block_webgl {
        firefox_user_prefs.insert("webgl.disabled".into(), true.into());
        warnings::leak("block_webgl", iknow);
    } else {
        let seed = Some(fp::identity_seed(&config, salt));
        let mut webgl_fp = if let Some((v, r)) = &webgl_config {
            webgl::webgl_for_gpu(&target_os, v, r, seed)?
        } else if let (Some(v), Some(r)) = (str_of(&config, "webGl:vendor"), str_of(&config, "webGl:renderer")) {
            webgl::webgl_for_gpu(&target_os, &v, &r, seed)?
        } else if let Some((v, r)) = &host {
            webgl::webgl_for_gpu(&target_os, v, r, seed)?
        } else {
            webgl::sample_webgl_for_screen(
                &target_os,
                int(&config, "screen.width"),
                int(&config, "screen.height"),
                seed,
                int(&config, "navigator.hardwareConcurrency"),
            )?
        };
        let webgl2 = webgl_fp.remove("webGl2Enabled").unwrap_or(Value::Bool(false));
        merge_into(&mut config, webgl_fp);
        let mut webgl_prefs = Map::new();
        webgl_prefs.insert("webgl.enable-webgl2".into(), webgl2);
        webgl_prefs.insert("webgl.force-enabled".into(), true.into());
        merge_into(&mut firefox_user_prefs, webgl_prefs);
    }

    let incoherent = coherence::apply(&mut config, &target_os);
    if debug {
        for v in &incoherent {
            println!("Incoherent identity ({}): {}", v.rule, v.detail);
        }
    }

    let vendor = str_of(&config, "webGl:vendor");
    let renderer = str_of(&config, "webGl:renderer");
    host_rendering::set_webgpu(
        &mut firefox_user_prefs,
        &target_os,
        (vendor.as_deref(), renderer.as_deref()),
        host.as_ref(),
    );

    if enable_cache {
        merge_into(&mut firefox_user_prefs, launcher_constants().cache_prefs.clone());
    }
    if debug {
        println!("[DEBUG] Config:\n{}", serde_json::to_string_pretty(&config)?);
    }

    warn_if_executable_predates_playwright(executable_path.as_deref());
    validate_config(&config, executable_path.as_deref())?;

    let mut env_vars = get_env_vars(&config, &target_os, executable_path.as_deref())?;
    env_vars.extend(get_pref_env_vars(&firefox_user_prefs));
    env_vars.extend(env);

    // Contexts of a browser without the canvas placeholder claim the host's OS and GPU.
    let host_identity = match (canvas_noise, host_os_key()) {
        (None, Some(host_os)) if !host_rendering::has_canvas_placeholder(&firefox_user_prefs) => {
            let disabled = firefox_user_prefs.get("webgl.disabled").and_then(Value::as_bool).unwrap_or(false);
            let (covered, gpu) = plan_rendering(host_os, None, false, disabled, &probe)?;
            (!covered).then(|| (host_os.to_string(), gpu))
        }
        _ => None,
    };

    let pin_cores = if pin_cpu_cores && crate::cpu_affinity::supported() {
        let cores = crate::cpu_affinity::host_cores().map(|c| c.len()).unwrap_or(0);
        int(&config, "navigator.hardwareConcurrency").filter(|v| *v >= 1 && (*v as usize) < cores).map(|v| v as usize)
    } else {
        None
    };

    Ok(LaunchConfig {
        executable_path: browser_binary,
        args,
        env: env_vars,
        firefox_user_prefs,
        headless,
        proxy,
        extra,
        camou_config: config,
        target_os,
        host_identity,
        pin_cores,
        warnings: Vec::new(),
    })
}

/// Options for [`new_context`].
#[derive(Debug, Clone, Default)]
pub struct NewContextOptions {
    pub preset: Option<Value>,
    pub os: Option<String>,
    pub ff_version: Option<String>,
    pub webrtc_ip: Option<String>,
    pub proxy: Option<Proxy>,
    /// `{"latitude": .., "longitude": ..}`; also grants the geolocation permission.
    pub geolocation: Option<Value>,
    /// Any other `newContext()` option in Playwright's camelCase, overriding the generated value.
    pub context_options: Map<String, Value>,
}

/// A context identity for a browser launched with `launch`: the init script
/// to add with `addInitScript`, and the `newContext()` options to create it with.
#[derive(Debug, Clone)]
pub struct NewContext {
    pub init_script: String,
    pub context_options: Map<String, Value>,
}

/// One identity per browser context (navigator, screen, WebGL, fonts,
/// voices, audio seed, timezone, WebRTC IP), for any Playwright client.
pub fn new_context(launch: &LaunchConfig, browser_version: &str, mut opts: NewContextOptions) -> Result<NewContext> {
    let ff_version =
        opts.ff_version.clone().unwrap_or_else(|| browser_version.split('.').next().unwrap_or("").to_string());
    if let Some(p) = &opts.proxy {
        if opts.webrtc_ip.is_none() || !opts.context_options.contains_key("timezoneId") {
            let (exit_ip, tz) = ip::proxy_exit_geo(&p.as_url()?)?;
            opts.webrtc_ip.get_or_insert(exit_ip);
            opts.context_options.entry("timezoneId").or_insert(tz.into());
        }
    }
    let (os, webgl_config) = launch.context_identity(opts.os.as_deref(), opts.preset.as_ref())?;
    let fp = fp::generate_context_fingerprint(&fp::ContextFingerprintOptions {
        preset: opts.preset.clone(),
        os,
        ff_version: Some(ff_version),
        webrtc_ip: opts.webrtc_ip.clone(),
        webgl_config,
        ..Default::default()
    })?;
    let mut context_options = launch.context_defaults();
    context_options.extend(fp.context_options);
    context_options.extend(opts.context_options);
    if let Some(p) = &opts.proxy {
        context_options.insert("proxy".into(), serde_json::to_value(p)?);
    }
    if let Some(g) = opts.geolocation {
        context_options.insert("geolocation".into(), g);
        context_options.entry("permissions").or_insert(json!(["geolocation"]));
    }
    Ok(NewContext { init_script: fp.init_script, context_options })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_chunks() {
        let mut env = BTreeMap::new();
        let data = "é".repeat(40000);
        chunk_env("CAMOU_CONFIG", &data, &mut env);
        let joined: String = (1..=env.len()).map(|i| env[&format!("CAMOU_CONFIG_{i}")].clone()).collect();
        assert_eq!(joined, data);
    }

    #[test]
    fn types() {
        assert!(validate_type(&json!(3), "uint"));
        assert!(!validate_type(&json!(-3), "uint"));
        assert!(validate_type(&json!(3.0), "int"));
        assert!(!validate_type(&json!(3.5), "int"));
        assert!(validate_type(&json!([]), "array"));
    }

    #[test]
    fn domains() {
        let c: Config = serde_json::from_value(json!({"navigator.platform": "x", "timezone": "UTC"})).unwrap();
        assert!(is_domain_set(&c, &["navigator."]));
        assert!(is_domain_set(&c, &["timezone"]));
        assert!(!is_domain_set(&c, &["screen."]));
    }

    #[test]
    fn bundled_properties_parse() {
        let p = load_properties(Some(Path::new("/nonexistent/camoufox-bin"))).unwrap();
        assert_eq!(p["navigator.userAgent"]["type"], "str");
    }

    #[test]
    fn voices_validation() {
        assert!(validate_voices(&json!(["Alex"])).is_err());
        assert!(validate_voices(
            &json!([{"lang":"en","name":"a","voiceUri":"u","isDefault":false,"isLocalService":true}])
        )
        .is_ok());
    }
}
