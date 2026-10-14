//! What this machine renders, and covering it when the identity claims another machine.
//!
//! Canvas and WebGL pixels come from the real OS, driver and GPU whatever the
//! identity claims. On the host's own OS the identity claims the GPU the host
//! renders with. Any other identity turns on the canvas protection of
//! `privacy.resistFingerprinting`, so readback returns random data.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::fingerprints::is_software_renderer;

pub const BASELINE_OVERRIDES_PREF: &str = "privacy.baselineFingerprintingProtection.overrides";
pub const CANVAS_PLACEHOLDER_TARGETS: [&str; 3] = [
    "+CanvasImageExtractionPrompt",
    "+CanvasExtractionBeforeUserInputIsBlocked",
    "+CanvasExtractionFromThirdPartiesIsBlocked",
];
pub const WEBGPU_BLOCKLIST_PREF: &str = "gfx.blacklist.webgpu";
pub const FEATURE_STATUS_OK: i64 = 1;
pub const FEATURE_BLOCKED_DEVICE: i64 = 4;

/// A (vendor, renderer) pair.
pub type Gpu = (String, String);

/// Turn on the canvas placeholder, keeping any baseline overrides already set.
pub fn add_canvas_placeholder(prefs: &mut Map<String, Value>) {
    let existing = prefs.get(BASELINE_OVERRIDES_PREF).and_then(Value::as_str).unwrap_or("");
    let mut overrides: Vec<String> = existing.split(',').filter(|o| !o.is_empty()).map(str::to_string).collect();
    overrides.extend(CANVAS_PLACEHOLDER_TARGETS.iter().map(|s| s.to_string()));
    prefs.insert(BASELINE_OVERRIDES_PREF.into(), overrides.join(",").into());
}

pub fn has_canvas_placeholder(prefs: &Map<String, Value>) -> bool {
    prefs.get(BASELINE_OVERRIDES_PREF).and_then(Value::as_str).unwrap_or("").contains(CANVAS_PLACEHOLDER_TARGETS[0])
}

/// Whether the host's canvas comes from a GPU rather than a software rasterizer.
pub fn renders_on_hardware(gpu: Option<&Gpu>) -> bool {
    gpu.is_some_and(|g| !is_software_renderer(&g.1))
}

/// Expose navigator.gpu where Firefox on the claimed device has it (Windows,
/// Apple Silicon), with the host's adapter only when the identity claims the
/// host's hardware GPU. Prefs the caller set are kept.
pub fn set_webgpu(
    prefs: &mut Map<String, Value>,
    target_os: &str,
    gpu: (Option<&str>, Option<&str>),
    host: Option<&Gpu>,
) {
    let default = target_os == "win" || (target_os == "mac" && gpu.0 == Some("Apple"));
    let enabled = prefs.entry("dom.webgpu.enabled").or_insert(default.into()).as_bool().unwrap_or(false);
    if enabled {
        let own = renders_on_hardware(host)
            && host.is_some_and(|h| gpu.0 == Some(h.0.as_str()) && gpu.1 == Some(h.1.as_str()));
        prefs
            .entry(WEBGPU_BLOCKLIST_PREF)
            .or_insert(if own { FEATURE_STATUS_OK } else { FEATURE_BLOCKED_DEVICE }.into());
    }
}

/// The (vendor, renderer) this machine's Firefox reports with no identity
/// applied, read from the binary about to launch under the same display.
/// Needs Node.js and playwright-core; None when the browser has no WebGL.
pub fn host_gpu(executable_path: &str, headless: bool, env: &BTreeMap<String, String>) -> crate::Result<Option<Gpu>> {
    static CACHE: Mutex<Option<HashMap<String, Option<Gpu>>>> = Mutex::new(None);
    let env: BTreeMap<String, String> = env
        .iter()
        .filter(|(k, _)| !k.starts_with("CAMOU_") && k.as_str() != "FONTCONFIG_FILE")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let key = format!("{executable_path}|{headless}|{env:?}");
    if let Some(hit) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return Ok(hit.clone());
    }
    let options = serde_json::json!({ "executablePath": executable_path, "headless": headless, "env": env });
    let output = crate::server::run_node_script(crate::paths::data::HOST_GPU_JS, &options)?;
    let parsed: Value = serde_json::from_str(output.trim())?;
    let gpu = parsed.as_array().and_then(|a| Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_str()?.to_string())));
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, gpu.clone());
    Ok(gpu)
}
