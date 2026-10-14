//! Whole-identity coherence: the checks that look at more than one field.
//!
//! An identity is assembled from several independently sampled pools
//! (navigator and screen from fpgen, the GPU from fpgen's WebGL records, fonts
//! and voices from catalogues), so a machine that never existed can be built
//! from individually plausible parts: an Apple M1 with 2 cores, an Intel Mac
//! GPU behind a notched Apple Silicon panel. Each rule states an invariant that
//! holds on real machines, repairs it where the correct value is determined,
//! and is checked again afterwards.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use crate::paths::launcher_constants;

pub type Config = Map<String, Value>;

pub fn apple_silicon_cores() -> &'static BTreeSet<i64> {
    static S: OnceLock<BTreeSet<i64>> = OnceLock::new();
    S.get_or_init(|| launcher_constants().apple_silicon_cores.iter().map(|c| *c as i64).collect())
}

/// devicePixelRatio steps per OS, ascending: the repair keeps the first of two equally near steps.
pub fn plausible_dpr(os: &str) -> Vec<f64> {
    launcher_constants()
        .plausible_dpr
        .get(os)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

pub const PLAUSIBLE_COLOR_DEPTH: [i64; 2] = [24, 30];
pub const MAX_PLAUSIBLE_TOUCH_POINTS: i64 = 10;
/// The browser's own chrome height in CSS pixels, a property of the binary.
pub const BROWSER_CHROME_HEIGHT: i64 = 86;
const NOT_A_MAC_GPU: [&str; 2] = ["ANGLE", "llvmpipe"];

pub fn intel_mac_igp_cores() -> &'static BTreeSet<i64> {
    static S: OnceLock<BTreeSet<i64>> = OnceLock::new();
    S.get_or_init(|| launcher_constants().intel_mac_igp_cores.iter().map(|c| *c as i64).collect())
}

pub fn intel_mac_dgpu_cores() -> &'static BTreeSet<i64> {
    static S: OnceLock<BTreeSet<i64>> = OnceLock::new();
    S.get_or_init(|| {
        let mut s = intel_mac_igp_cores().clone();
        s.extend(launcher_constants().intel_mac_dgpu_extra_cores.iter().map(|c| *c as i64));
        s
    })
}

pub fn apple_silicon_panels() -> &'static BTreeSet<(i64, i64)> {
    static S: OnceLock<BTreeSet<(i64, i64)>> = OnceLock::new();
    S.get_or_init(|| {
        launcher_constants()
            .apple_silicon_panels
            .values()
            .flat_map(|v| v.as_array().cloned().unwrap_or_default())
            .filter_map(|p| Some((p[0].as_i64()?, p[1].as_i64()?)))
            .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub detail: String,
}

/// An integer config value.
pub fn int(config: &Config, key: &str) -> Option<i64> {
    config.get(key).and_then(Value::as_i64)
}

/// A truthy integer (Python's `if value`).
pub(crate) fn tint(config: &Config, key: &str) -> Option<i64> {
    int(config, key).filter(|v| *v != 0)
}

fn renderer(config: &Config) -> String {
    config.get("webGl:renderer").and_then(Value::as_str).unwrap_or("").to_string()
}

fn is_apple_silicon(config: &Config) -> bool {
    renderer(config).contains("Apple M")
}

fn check_apple_silicon_cores(config: &Config, _os: &str) -> Option<String> {
    if !is_apple_silicon(config) {
        return None;
    }
    let cores = int(config, "navigator.hardwareConcurrency")?;
    (!apple_silicon_cores().contains(&cores))
        .then(|| format!("{:?} with hardwareConcurrency {cores}; Apple Silicon starts at 8", renderer(config)))
}

fn repair_apple_silicon_cores(config: &mut Config, _os: &str) {
    let Some(cores) = int(config, "navigator.hardwareConcurrency") else { return };
    let set = apple_silicon_cores();
    let snapped = set.iter().copied().find(|c| *c >= cores).unwrap_or(*set.iter().max().unwrap());
    config.insert("navigator.hardwareConcurrency".into(), snapped.into());
}

/// Whether this renderer string is one the OS can report.
pub fn gpu_fits_os(renderer: &str, os: &str) -> bool {
    if renderer.is_empty() {
        return true;
    }
    match os {
        "mac" => !NOT_A_MAC_GPU.iter().any(|bad| renderer.contains(bad)),
        "win" => renderer.starts_with("ANGLE"),
        "lin" => !renderer.contains("ANGLE") && !renderer.contains("Apple M"),
        _ => true,
    }
}

fn intel_mac_misfit(renderer: &str, cores: Option<i64>, width: Option<i64>, height: Option<i64>) -> Option<String> {
    if renderer.is_empty() || renderer.contains("Apple M") {
        return None;
    }
    let allowed = if renderer.contains("Intel") { intel_mac_igp_cores() } else { intel_mac_dgpu_cores() };
    if let Some(c) = cores {
        if !allowed.contains(&c) {
            return Some(format!("{renderer:?} with hardwareConcurrency {c}; no Intel Mac with that GPU reports it"));
        }
    }
    if let (Some(w), Some(h)) = (width, height) {
        if apple_silicon_panels().contains(&(w, h)) {
            return Some(format!("{renderer:?} behind a {w}x{h} panel, which only Apple Silicon Macs have"));
        }
    }
    None
}

/// Whether a machine with this GPU has these cores and this screen (Intel Macs only).
pub fn gpu_fits_machine(renderer: &str, os: &str, cores: Option<i64>, width: Option<i64>, height: Option<i64>) -> bool {
    os != "mac" || intel_mac_misfit(renderer, cores, width, height).is_none()
}

fn check_intel_mac_hardware(config: &Config, os: &str) -> Option<String> {
    if os != "mac" {
        return None;
    }
    intel_mac_misfit(
        &renderer(config),
        int(config, "navigator.hardwareConcurrency"),
        int(config, "screen.width"),
        int(config, "screen.height"),
    )
}

fn check_gpu_matches_os(config: &Config, os: &str) -> Option<String> {
    let r = renderer(config);
    if r.is_empty() || gpu_fits_os(&r, os) {
        return None;
    }
    Some(match os {
        "mac" => format!("macOS identity with {r:?}, which no Mac reports"),
        "win" => format!("Windows identity with {r:?}; Firefox on Windows renders through ANGLE"),
        _ => format!("Linux identity with {r:?}"),
    })
}

fn check_color_depth(config: &Config, os: &str) -> Option<String> {
    let depth = config.get("screen.colorDepth")?;
    let d = depth.as_i64();
    if d.is_none_or(|d| !PLAUSIBLE_COLOR_DEPTH.contains(&d)) {
        return Some(format!("screen.colorDepth {depth}; Firefox reports 24 or 30"));
    }
    if os == "mac" && is_apple_silicon(config) && d != Some(30) {
        return Some(format!("Apple Silicon Mac with colorDepth {depth}; deep colour is the macOS default"));
    }
    None
}

fn repair_color_depth(config: &mut Config, os: &str) {
    if let Some(depth) = config.get("screen.colorDepth") {
        if depth.as_i64().is_none_or(|d| !PLAUSIBLE_COLOR_DEPTH.contains(&d)) {
            config.insert("screen.colorDepth".into(), 24.into());
        }
    }
    if os == "mac" && is_apple_silicon(config) {
        config.insert("screen.colorDepth".into(), 30.into());
    }
    if config.contains_key("screen.pixelDepth") || config.contains_key("screen.colorDepth") {
        let depth = config.get("screen.colorDepth").cloned().unwrap_or(24.into());
        config.insert("screen.pixelDepth".into(), depth);
    }
}

fn check_touch_points(config: &Config, os: &str) -> Option<String> {
    let touch = config.get("navigator.maxTouchPoints")?;
    match touch.as_i64() {
        Some(t) if (0..=MAX_PLAUSIBLE_TOUCH_POINTS).contains(&t) => {
            (os == "mac" && t != 0).then(|| format!("macOS identity with maxTouchPoints {t}; no Mac has a touchscreen"))
        }
        _ => {
            Some(format!("navigator.maxTouchPoints {touch}; a digitiser reports at most {MAX_PLAUSIBLE_TOUCH_POINTS}"))
        }
    }
}

fn repair_touch_points(config: &mut Config, os: &str) {
    let touch = config.get("navigator.maxTouchPoints").cloned();
    let too_many = touch.as_ref().and_then(Value::as_i64).is_some_and(|t| t > MAX_PLAUSIBLE_TOUCH_POINTS);
    let not_int = touch.as_ref().is_some_and(|t| !t.is_null() && t.as_i64().is_none());
    if os == "mac" || too_many || not_int {
        config.insert("navigator.maxTouchPoints".into(), 0.into());
    }
}

fn check_device_pixel_ratio(config: &Config, os: &str) -> Option<String> {
    let dpr = config.get("window.devicePixelRatio")?.as_f64()?;
    let allowed = plausible_dpr(os);
    (!allowed.is_empty() && !allowed.contains(&dpr))
        .then(|| format!("window.devicePixelRatio {dpr} is not a display mode {os} offers"))
}

fn repair_device_pixel_ratio(config: &mut Config, os: &str) {
    let Some(dpr) = config.get("window.devicePixelRatio").and_then(Value::as_f64) else { return };
    let allowed = plausible_dpr(os);
    let mut best: Option<f64> = None;
    for v in allowed {
        if best.is_none_or(|b| (v - dpr).abs() < (b - dpr).abs()) {
            best = Some(v);
        }
    }
    if let Some(b) = best {
        config.insert("window.devicePixelRatio".into(), crate::fingerprints::number(b));
    }
}

fn check_window_chrome(config: &Config, _os: &str) -> Option<String> {
    let inner = tint(config, "window.innerHeight")?;
    let outer = tint(config, "window.outerHeight")?;
    (outer - inner < BROWSER_CHROME_HEIGHT).then(|| {
        format!(
            "window.outerHeight {outer} - innerHeight {inner} = {}, less than the {BROWSER_CHROME_HEIGHT}px of \
             chrome the window actually has; the bottom {}px of the claimed viewport cannot receive input",
            outer - inner,
            BROWSER_CHROME_HEIGHT - (outer - inner)
        )
    })
}

fn repair_window_chrome(config: &mut Config, _os: &str) {
    let (Some(inner), Some(outer)) = (tint(config, "window.innerHeight"), tint(config, "window.outerHeight")) else {
        return;
    };
    let avail = tint(config, "screen.availHeight").or_else(|| tint(config, "screen.height"));
    if avail.is_none_or(|a| inner + BROWSER_CHROME_HEIGHT <= a) {
        config.insert("window.outerHeight".into(), (inner + BROWSER_CHROME_HEIGHT).into());
        return;
    }
    config.insert("window.innerHeight".into(), (outer - BROWSER_CHROME_HEIGHT).max(1).into());
}

fn check_screen_shape(config: &Config, _os: &str) -> Option<String> {
    let w = tint(config, "screen.width")?;
    let h = tint(config, "screen.height")?;
    if h > w {
        return Some(format!("portrait screen {w}x{h}; desktop panels are landscape"));
    }
    (w < 1024).then(|| format!("screen {w}x{h} is smaller than any current desktop panel"))
}

fn check_avail_bounds(config: &Config, _os: &str) -> Option<String> {
    let (w, h) = (tint(config, "screen.width"), tint(config, "screen.height"));
    let (aw, ah) = (tint(config, "screen.availWidth"), tint(config, "screen.availHeight"));
    if let (Some(w), Some(aw)) = (w, aw) {
        if aw > w {
            return Some(format!("screen.availWidth {aw} exceeds screen.width {w}"));
        }
    }
    if let (Some(h), Some(ah)) = (h, ah) {
        if ah > h {
            return Some(format!("screen.availHeight {ah} exceeds screen.height {h}"));
        }
    }
    None
}

fn repair_avail_bounds(config: &mut Config, _os: &str) {
    if let Some(w) = tint(config, "screen.width") {
        if int(config, "screen.availWidth").unwrap_or(0) > w {
            config.insert("screen.availWidth".into(), w.into());
        }
    }
    if let Some(h) = tint(config, "screen.height") {
        if int(config, "screen.availHeight").unwrap_or(0) > h {
            config.insert("screen.availHeight".into(), h.into());
        }
    }
}

fn check_arch_agreement(config: &Config, _os: &str) -> Option<String> {
    let s = |k: &str| config.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (ua, platform, oscpu) = (s("navigator.userAgent"), s("navigator.platform"), s("navigator.oscpu"));
    (!ua.is_empty() && ua.contains("x86_64") && (platform.contains("armv") || oscpu.contains("armv")))
        .then(|| format!("user agent claims x86_64 while platform/oscpu say {platform:?}/{oscpu:?}"))
}

type Check = fn(&Config, &str) -> Option<String>;
type Repair = fn(&mut Config, &str);

const RULES: &[(&str, Check, Option<Repair>)] = &[
    ("apple-silicon-cores", check_apple_silicon_cores, Some(repair_apple_silicon_cores)),
    ("gpu-matches-os", check_gpu_matches_os, None),
    ("intel-mac-hardware", check_intel_mac_hardware, None),
    ("color-depth", check_color_depth, Some(repair_color_depth)),
    ("touch-points", check_touch_points, Some(repair_touch_points)),
    ("device-pixel-ratio", check_device_pixel_ratio, Some(repair_device_pixel_ratio)),
    ("window-chrome", check_window_chrome, Some(repair_window_chrome)),
    ("screen-shape", check_screen_shape, None),
    ("avail-bounds", check_avail_bounds, Some(repair_avail_bounds)),
    ("arch-agreement", check_arch_agreement, None),
];

/// Whether the screen is one no desktop reports (portrait, or tiny).
pub fn screen_is_implausible(config: &Config) -> bool {
    check_screen_shape(config, "").is_some()
}

/// Turn a portrait screen landscape, keeping the panel's own dimensions.
pub fn repair_screen_orientation(config: &mut Config) -> bool {
    let (Some(w), Some(h)) = (tint(config, "screen.width"), tint(config, "screen.height")) else { return false };
    if h <= w {
        return false;
    }
    config.insert("screen.width".into(), h.into());
    config.insert("screen.height".into(), w.into());
    if let (Some(aw), Some(ah)) = (tint(config, "screen.availWidth"), tint(config, "screen.availHeight")) {
        config.insert("screen.availWidth".into(), ah.into());
        config.insert("screen.availHeight".into(), aw.into());
    }
    true
}

/// Discard a source's GPU pair the identity cannot keep, so the WebGL pool draws a coherent one.
pub fn drop_incoherent_source_values(config: &mut Config, os: &str) -> Vec<Violation> {
    let r = renderer(config);
    if r.is_empty() {
        return Vec::new();
    }
    let rule = if !gpu_fits_os(&r, os) {
        "gpu-matches-os"
    } else if check_intel_mac_hardware(config, os).is_some() {
        "intel-mac-hardware"
    } else {
        return Vec::new();
    };
    config.remove("webGl:renderer");
    config.remove("webGl:vendor");
    vec![Violation { rule, detail: format!("dropped {r:?} for a {os} identity") }]
}

/// Every invariant this identity breaks. Empty means coherent.
pub fn validate(config: &Config, os: &str) -> Vec<Violation> {
    RULES
        .iter()
        .filter_map(|(name, check, _)| check(config, os).map(|detail| Violation { rule: name, detail }))
        .collect()
}

/// Repair what is determined, and report what is left.
pub fn apply(config: &mut Config, os: &str) -> Vec<Violation> {
    for (_, check, repair) in RULES {
        if let Some(repair) = repair {
            if check(config, os).is_some() {
                repair(config, os);
            }
        }
    }
    validate(config, os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: Value) -> Config {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn apple_silicon_cores_snap_up() {
        let mut c = cfg(
            json!({"webGl:renderer": "Apple M1, or similar", "navigator.hardwareConcurrency": 2, "screen.colorDepth": 24}),
        );
        assert!(!apply(&mut c, "mac").iter().any(|v| v.rule == "apple-silicon-cores"));
        assert_eq!(c["navigator.hardwareConcurrency"], 8);
        assert_eq!(c["screen.pixelDepth"], 30);
    }

    #[test]
    fn window_chrome_grows_window() {
        let mut c = cfg(json!({"window.innerHeight": 717, "window.outerHeight": 801, "screen.availHeight": 1040}));
        apply(&mut c, "lin");
        assert_eq!(c["window.outerHeight"], 717 + BROWSER_CHROME_HEIGHT);
    }

    #[test]
    fn os_gpu_rules() {
        assert!(gpu_fits_os("ANGLE (Intel, Intel(R) HD Graphics Direct3D11 vs_5_0 ps_5_0), or similar", "win"));
        assert!(!gpu_fits_os("Apple M1, or similar", "lin"));
        assert!(!gpu_fits_os("ANGLE (Apple)", "mac"));
    }

    #[test]
    fn dpr_snaps_to_nearest_step() {
        let mut c = cfg(json!({"window.devicePixelRatio": 1.818}));
        apply(&mut c, "win");
        assert_eq!(c["window.devicePixelRatio"], json!(1.75));
    }
}
