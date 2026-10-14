//! Platform constants, the install directory and the bundled data files.
//!
//! The install directory is platformdirs' `user_cache_dir("camoufox")`, the
//! one the Python and TypeScript launchers use, so a browser fetched by any of
//! the three launchers is found by the other two.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::error::{Error, Result};

/// The host OS in the short form the bundled data files use.
pub const OS_NAME: &str = if cfg!(target_os = "macos") {
    "mac"
} else if cfg!(target_os = "windows") {
    "win"
} else {
    "lin"
};

/// The host architecture in the form the release assets are named with.
pub const ARCH: &str = if cfg!(target_arch = "x86_64") {
    "x86_64"
} else if cfg!(target_arch = "x86") {
    "i686"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

pub fn os_arch_matrix(os: &str) -> &'static [&'static str] {
    match os {
        "win" => &["x86_64", "i686"],
        "mac" => &["x86_64", "arm64"],
        "lin" => &["x86_64", "arm64", "i686"],
        _ => &[],
    }
}

/// The launch file of a build, relative to its resources directory.
pub fn launch_file(os: &str) -> &'static str {
    match os {
        "win" => "camoufox.exe",
        "mac" => "../MacOS/camoufox",
        _ => "camoufox-bin",
    }
}

/// Assert the host is one Camoufox has builds for.
pub fn check_host() -> Result<()> {
    if ARCH == "unsupported" {
        return Err(Error::UnsupportedArchitecture(format!(
            "Architecture {} is not supported",
            std::env::consts::ARCH
        )));
    }
    if !os_arch_matrix(OS_NAME).contains(&ARCH) {
        return Err(Error::UnsupportedArchitecture(format!("Architecture {ARCH} is not supported for {OS_NAME}")));
    }
    Ok(())
}

/// platformdirs' `user_cache_dir(app)`, reimplemented so every launcher shares
/// one install directory.
fn user_cache_dir(app: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "windows") {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join("AppData").join("Local"));
        base.join(app).join(app).join("Cache")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Caches").join(app)
    } else {
        let xdg = std::env::var("XDG_CACHE_HOME").unwrap_or_default();
        let base = if xdg.trim().is_empty() { home.join(".cache") } else { PathBuf::from(xdg) };
        base.join(app)
    }
}

/// Where browsers, addons, the GeoIP database and fpgen's model are kept.
/// `CAMOUFOX_INSTALL_DIR` overrides it.
pub fn install_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("CAMOUFOX_INSTALL_DIR").map(PathBuf::from).unwrap_or_else(|| user_cache_dir("camoufox"))
    })
}

/// Values both launchers hard-code, kept in one file shared with the Python
/// and TypeScript packages (`launcher-constants.json`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherConstants {
    pub marker_fonts: std::collections::HashMap<String, Vec<String>>,
    pub windows11_marker_fonts: Vec<String>,
    pub plausible_core_counts: Vec<u32>,
    pub apple_silicon_cores: Vec<u32>,
    pub apple_silicon_panels: serde_json::Map<String, serde_json::Value>,
    pub intel_mac_igp_cores: Vec<u32>,
    pub intel_mac_dgpu_extra_cores: Vec<u32>,
    pub plausible_dpr: serde_json::Map<String, serde_json::Value>,
    pub mac_novelty_voices: Vec<String>,
    pub mac_eloquence_voices: Vec<String>,
    pub baseline_taskbar_height: std::collections::HashMap<String, i64>,
    pub baseline_multi_touch_points: i64,
    pub cache_prefs: serde_json::Map<String, serde_json::Value>,
}

/// The bundled data files, compiled into the binary.
pub mod data {
    pub const BROWSER_PIN: &str = include_str!("../data/browser-pin.json");
    pub const ESSENTIAL_FONTS: &str = include_str!("../data/essential-fonts.json");
    pub const FINGERPRINT_PRESETS: &str = include_str!("../data/fingerprint-presets.json");
    pub const FINGERPRINT_PRESETS_V150: &str = include_str!("../data/fingerprint-presets-v150.json");
    pub const FONT_BASES: &str = include_str!("../data/font-bases.json");
    pub const FONT_GROUPS: &str = include_str!("../data/font-groups.json");
    pub const FONTS: &str = include_str!("../data/fonts.json");
    pub const FPGEN_MODEL: &str = include_str!("../data/fpgen-model.json");
    pub const FPGEN_YML: &str = include_str!("../data/fpgen.yml");
    pub const LAUNCHER_CONSTANTS: &str = include_str!("../data/launcher-constants.json");
    pub const SERVER_JS: &str = include_str!("../data/server.js");
    pub const INSPECT_JS: &str = include_str!("../data/inspect.js");
    pub const HOST_GPU_JS: &str = include_str!("../data/hostGpu.js");
    pub const MEDIA_DEVICES: &str = include_str!("../data/media-devices.json");
    pub const PROPERTIES: &str = include_str!("../data/properties.json");
    pub const REPOS_YML: &str = include_str!("../data/repos.yml");
    pub const TERRITORY_INFO: &str = include_str!("../data/territoryInfo.xml");
    pub const VOICE_MANIFESTS: &str = include_str!("../data/voice-manifests.json");
    pub const VOICE_URIS: &str = include_str!("../data/voice-uris.json");
    pub const WARNINGS_YML: &str = include_str!("../data/warnings.yml");
    pub const LANGUAGE_REGISTRY: &str = include_str!("../data/language-registry.json");
}

pub fn launcher_constants() -> &'static LauncherConstants {
    static C: OnceLock<LauncherConstants> = OnceLock::new();
    C.get_or_init(|| serde_json::from_str(data::LAUNCHER_CONSTANTS).expect("launcher-constants.json"))
}

/// ANSI-styled text, plain when stdout is not a terminal or NO_COLOR is set.
pub fn style(text: &str, fg: Option<&str>, bold: bool) -> String {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
        return text.to_string();
    }
    let mut codes: Vec<&str> = Vec::new();
    if bold {
        codes.push("1");
    }
    if let Some(fg) = fg {
        let code = match fg {
            "red" => "31",
            "green" => "32",
            "yellow" => "33",
            "blue" => "34",
            "cyan" => "36",
            "bright_black" => "90",
            _ => "",
        };
        if !code.is_empty() {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        text.to_string()
    } else {
        format!("\x1b[{}m{}\x1b[0m", codes.join(";"), text)
    }
}

/// Print a bold, optionally coloured message, as the Python twin's `rprint`.
pub fn rprint(msg: &str, fg: Option<&str>) {
    println!("{}", style(msg, fg, true));
}
