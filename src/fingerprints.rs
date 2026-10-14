//! Device identities: drawing them from fpgen or a recorded preset, and the
//! per-identity draws (fonts, voices, media devices) seeded by the identity so
//! the same identity always presents the same device.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use rand::Rng;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::coherence::{int, tint, Config};
use crate::error::{Error, Result};
use crate::fpgen::{self, Condition};
use crate::ip::{valid_ipv4, validate_ip};
use crate::paths::{data, launcher_constants};
use crate::pycompat::{crc32, json_dumps_ascii, orjson_dumps_sorted, py_str};
use crate::pyrandom::PyRandom;
use crate::warnings;

/// A JSON number from a float.
pub fn number(x: f64) -> Value {
    Value::from(x)
}

/// Camoufox's OS name ("windows") to the data files' key ("win").
pub fn os_key(name: &str) -> Option<&'static str> {
    match name {
        "macos" => Some("mac"),
        "windows" => Some("win"),
        "linux" => Some("lin"),
        _ => None,
    }
}

/// The data files' OS key to Camoufox's OS name.
pub fn os_name(key: &str) -> Option<&'static str> {
    match key {
        "mac" => Some("macos"),
        "win" => Some("windows"),
        "lin" => Some("linux"),
        _ => None,
    }
}

/// fpgen's OS name from either form.
pub fn fpgen_os(name: &str) -> Option<&'static str> {
    match name {
        "lin" | "linux" => Some("Linux"),
        "mac" | "macos" => Some("macOS"),
        "win" | "windows" => Some("Windows"),
        _ => None,
    }
}

/// The OS a navigator.platform value presents, as a Camoufox OS name.
pub fn platform_to_os(platform: &str) -> &'static str {
    if platform == "Win32" {
        "windows"
    } else if platform.contains("Linux") || platform.contains("linux") {
        "linux"
    } else {
        "macos"
    }
}

/// The navigator.oscpu Firefox reports beside a navigator.platform value.
pub fn oscpu_for_platform(platform: &str) -> Option<&'static str> {
    if platform == "MacIntel" {
        return Some("Intel Mac OS X 10.15");
    }
    match platform_to_os(platform) {
        "windows" => Some("Windows NT 10.0; Win64; x64"),
        "linux" => Some("Linux x86_64"),
        _ => None,
    }
}

/// A bound on the screen a generated fingerprint may claim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Screen {
    pub min_width: Option<i64>,
    pub max_width: Option<i64>,
    pub min_height: Option<i64>,
    pub max_height: Option<i64>,
}

impl Screen {
    pub fn max(width: i64, height: i64) -> Self {
        Screen { max_width: Some(width), max_height: Some(height), ..Default::default() }
    }

    fn conditions(&self) -> Vec<(&'static str, Condition)> {
        let mut out = Vec::new();
        let range = |lo: Option<i64>, hi: Option<i64>| -> Condition {
            Condition::Predicate(Box::new(move |v: &Value| {
                v.as_i64().is_some_and(|x| lo.is_none_or(|lo| x >= lo) && hi.is_none_or(|hi| x <= hi))
            }))
        };
        if self.min_width.is_some() || self.max_width.is_some() {
            out.push(("screen.width", range(self.min_width, self.max_width)));
        }
        if self.min_height.is_some() || self.max_height.is_some() {
            out.push(("screen.height", range(self.min_height, self.max_height)));
        }
        out
    }
}

fn load<T: serde::de::DeserializeOwned>(cell: &'static OnceLock<T>, text: &str) -> &'static T {
    cell.get_or_init(|| serde_json::from_str(text).expect("bundled data file"))
}

fn fpgen_mapping() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    C.get_or_init(|| serde_yaml::from_str(data::FPGEN_YML).expect("fpgen.yml"))
}

fn os_fonts() -> &'static HashMap<String, Vec<String>> {
    static C: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    load(&C, data::FONTS)
}

fn essential_fonts() -> &'static HashMap<String, Vec<String>> {
    static C: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    load(&C, data::ESSENTIAL_FONTS)
}

fn font_groups() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    load(&C, data::FONT_GROUPS)
}

fn font_bases() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    load(&C, data::FONT_BASES)
}

fn voice_manifests() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    load(&C, data::VOICE_MANIFESTS)
}

fn voice_uris() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    load(&C, data::VOICE_URIS)
}

fn media_devices() -> &'static Value {
    static C: OnceLock<Value> = OnceLock::new();
    load(&C, data::MEDIA_DEVICES)
}

/// The OS marker fonts, always added back after a draw.
pub fn marker_fonts(os_key: &str) -> &'static [String] {
    launcher_constants().marker_fonts.get(os_key).map(Vec::as_slice).unwrap_or(&[])
}

/// Fonts only a Windows 11 base has.
pub fn windows11_marker_fonts() -> &'static [String] {
    &launcher_constants().windows11_marker_fonts
}

fn ensure_marker_fonts(fonts: &mut Vec<String>, markers: &[String]) {
    let existing: HashSet<String> = fonts.iter().cloned().collect();
    for m in markers {
        if !existing.contains(m) {
            fonts.push(m.clone());
        }
    }
}

/// The entropy that makes `identity_seed` belong to one identity: stable for
/// whatever pinned the identity (an fpgen fingerprint, a preset, a config),
/// random for a fresh one.
pub fn identity_salt(pinned: Option<&Value>) -> u64 {
    match pinned {
        None => rand::thread_rng().gen(),
        Some(v) => {
            let digest = Sha256::digest(orjson_dumps_sorted(v).as_bytes());
            u64::from_be_bytes(digest[..8].try_into().unwrap())
        }
    }
}

/// A seed for the per-identity draws: a pure function of the presented
/// identity and its salt.
pub fn identity_seed(config: &Config, salt: u64) -> u32 {
    let field = |k: &str| config.get(k).map(py_str).unwrap_or_default();
    let parts = [
        field("navigator.userAgent"),
        field("navigator.platform"),
        field("screen.width"),
        field("screen.height"),
        field("navigator.hardwareConcurrency"),
        salt.to_string(),
    ];
    crc32(parts.join("|").as_bytes())
}

fn rng(seed: Option<u32>) -> PyRandom {
    PyRandom::new(seed.map(u128::from))
}

fn f(v: &Value, k: &str, default: f64) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(default)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

fn pick_base(os_key: &str, rng: &mut PyRandom) -> Vec<String> {
    let Some(bases) = font_bases().get(os_key).and_then(Value::as_array).filter(|b| !b.is_empty()) else {
        return Vec::new();
    };
    let roll = rng.random();
    let mut cumulative = 0.0;
    for base in bases {
        cumulative += f(base, "weight", 0.0);
        if roll < cumulative {
            return strings(&base["fonts"]);
        }
    }
    strings(&bases[bases.len() - 1]["fonts"])
}

fn draw_units(os_key: &str, rng: &mut PyRandom, exclude: &HashSet<String>, locale: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for unit in font_groups().get(os_key).and_then(Value::as_array).into_iter().flatten() {
        if let Some(required) = unit.get("requiresLocale").and_then(Value::as_str).filter(|r| !r.is_empty()) {
            if !locale.unwrap_or("").to_lowercase().starts_with(&required.to_lowercase()) {
                continue;
            }
        }
        if rng.random() >= f(unit, "prob", 0.0) {
            continue;
        }
        let mut members: Vec<String> = strings(&unit["fonts"]).into_iter().filter(|m| !exclude.contains(m)).collect();
        if members.is_empty() {
            continue;
        }
        if unit["kind"].as_str() == Some("alacarte") {
            let sizes: Vec<(i64, f64)> = unit
                .get("sizes")
                .and_then(Value::as_array)
                .filter(|s| !s.is_empty())
                .map(|s| s.iter().map(|x| (x["n"].as_i64().unwrap_or(1), f(x, "w", 0.0))).collect())
                .unwrap_or_else(|| vec![(members.len() as i64, 1.0)]);
            let total: f64 = sizes.iter().map(|s| s.1).sum();
            let roll = rng.random() * total;
            let mut cumulative = 0.0;
            let mut count = sizes[sizes.len() - 1].0;
            for (n, w) in &sizes {
                cumulative += w;
                if roll < cumulative {
                    count = *n;
                    break;
                }
            }
            let count = count.clamp(1, members.len() as i64) as usize;
            members = rng.sample(&members, count);
        }
        out.extend(members);
    }
    out
}

fn host_has_variant_fonts(target_os: &str) -> bool {
    if target_os != "windows" {
        return false;
    }
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    std::path::Path::new(&windir).join("Fonts").join("SegUIVar.ttf").exists()
}

/// The font list of one plausible machine of `target_os` ("windows", "macos",
/// "linux"): one OS-version base drawn whole by weight, then each addition
/// unit at its own measured probability, then the marker fonts.
pub fn generate_random_font_subset(
    target_os: &str,
    seed: Option<u32>,
    native: bool,
    locale: Option<&str>,
) -> Vec<String> {
    let mut rng = rng(seed);
    let key = os_key(target_os).unwrap_or("mac");
    let full_list = os_fonts().get(key).or_else(|| os_fonts().get("mac")).cloned().unwrap_or_default();
    let essential: HashSet<String> = essential_fonts().get(key).cloned().unwrap_or_default().into_iter().collect();
    let markers = marker_fonts(key);

    if native {
        let full: HashSet<&String> = full_list.iter().collect();
        let mut result: Vec<String> = full_list.iter().filter(|f| essential.contains(*f)).cloned().collect();
        let mut extra: Vec<String> = essential.iter().filter(|f| !full.contains(f)).cloned().collect();
        extra.sort();
        result.extend(extra);
        if target_os == "windows" && !host_has_variant_fonts(target_os) {
            let w11: HashSet<&String> = windows11_marker_fonts().iter().collect();
            result.retain(|f| !w11.contains(f));
        }
        return result;
    }

    let mut base = pick_base(key, &mut rng);
    if base.is_empty() {
        base = full_list.iter().filter(|f| essential.contains(*f)).cloned().collect();
    }
    let mut result = base;
    let mut chosen: HashSet<String> = result.iter().cloned().collect();
    for font in &full_list {
        if essential.contains(font) && !chosen.contains(font) {
            result.push(font.clone());
            chosen.insert(font.clone());
        }
    }
    for font in draw_units(key, &mut rng, &chosen.clone(), locale) {
        if chosen.insert(font.clone()) {
            result.push(font);
        }
    }
    ensure_marker_fonts(&mut result, markers);
    result
}

fn mac_novelty_voices() -> &'static HashSet<String> {
    static C: OnceLock<HashSet<String>> = OnceLock::new();
    C.get_or_init(|| launcher_constants().mac_novelty_voices.iter().cloned().collect())
}

fn mac_eloquence_voices() -> &'static HashSet<String> {
    static C: OnceLock<HashSet<String>> = OnceLock::new();
    C.get_or_init(|| launcher_constants().mac_eloquence_voices.iter().cloned().collect())
}

fn ascii_alnum_nfkd(name: &str) -> String {
    // NFKD then drop every non-alphanumeric ASCII character: for the Latin
    // voice names this keeps each base letter of an accented one.
    name.chars()
        .filter_map(|c| {
            let base = match c {
                'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
                'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
                'è' | 'é' | 'ê' | 'ë' => 'e',
                'È' | 'É' | 'Ê' | 'Ë' => 'E',
                'ì' | 'í' | 'î' | 'ï' => 'i',
                'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
                'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
                'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'O',
                'ù' | 'ú' | 'û' | 'ü' => 'u',
                'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
                'ý' | 'ÿ' => 'y',
                'Ý' => 'Y',
                'ñ' => 'n',
                'Ñ' => 'N',
                'ç' => 'c',
                'Ç' => 'C',
                c => c,
            };
            base.is_ascii_alphanumeric().then_some(base)
        })
        .collect()
}

fn voice_uri_slug(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::new();
    let mut in_sep = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            in_sep = false;
        } else if !in_sep {
            out.push('.');
            in_sep = true;
        }
    }
    out.trim_matches('.').to_string()
}

/// The voiceURI real Firefox reports for a voice on this OS's speech backend.
pub fn voice_uri(os_key: &str, name: &str, lang: &str) -> String {
    match os_key {
        "lin" => {
            let mut escaped = String::new();
            for ch in name.chars() {
                if ch == ' ' {
                    escaped.push_str("%20");
                } else if ch.is_ascii() {
                    escaped.push(ch);
                } else {
                    let mut buf = [0u8; 4];
                    for b in ch.encode_utf8(&mut buf).bytes() {
                        escaped.push_str(&format!("%{b:02X}"));
                    }
                }
            }
            format!("urn:moz-tts:speechd:{escaped}?{lang}")
        }
        "win" => format!("urn:moz-tts:sapi:{name}?{lang}"),
        "mac" => {
            if let Some(uri) = voice_uris()["mac"].get(format!("{name}|{lang}")).and_then(Value::as_str) {
                return uri.to_string();
            }
            let ascii = ascii_alnum_nfkd(name);
            if mac_novelty_voices().contains(name) {
                format!("urn:moz-tts:osx:com.apple.speech.synthesis.voice.{ascii}")
            } else if mac_eloquence_voices().contains(name) {
                format!("urn:moz-tts:osx:com.apple.eloquence.{lang}.{ascii}")
            } else {
                format!("urn:moz-tts:osx:com.apple.voice.compact.{lang}.{ascii}")
            }
        }
        _ => voice_uri_slug(name),
    }
}

fn split_voice_entry(entry: &str) -> Option<(String, String, String)> {
    let (rest, vtype) = entry.rsplit_once(':')?;
    let (name, lang) = rest.rsplit_once(':')?;
    Some((name.to_string(), lang.to_string(), vtype.to_string()))
}

fn weighted_pick<'a>(rng: &mut PyRandom, items: &'a [Value]) -> &'a Value {
    let total: f64 = items.iter().map(|i| f(i, "w", 0.0)).sum();
    let mut r = rng.random() * total;
    for i in items {
        r -= f(i, "w", 0.0);
        if r <= 0.0 {
            return i;
        }
    }
    &items[items.len() - 1]
}

fn weighted_sample<T: Clone + PartialEq>(
    rng: &mut PyRandom,
    items: &[T],
    k: usize,
    weight: impl Fn(&T) -> f64,
) -> Vec<T> {
    let mut pool: Vec<T> = items.to_vec();
    let mut out = Vec::new();
    while !pool.is_empty() && out.len() < k {
        let total: f64 = pool.iter().map(&weight).sum();
        let mut r = rng.random() * total;
        let mut picked = None;
        for (i, x) in pool.iter().enumerate() {
            r -= weight(x);
            if r <= 0.0 {
                picked = Some(i);
                break;
            }
        }
        match picked {
            Some(i) => out.push(pool.remove(i)),
            None => out.push(pool.pop().unwrap()),
        }
    }
    out
}

fn resolve_display_pack(packs: &Map<String, Value>, fallback: &str, locale: Option<&str>) -> String {
    if let Some(locale) = locale {
        if packs.contains_key(locale) {
            return locale.to_string();
        }
        let lang = locale.split('-').next().unwrap_or("").to_lowercase();
        if let Some(key) = packs.keys().find(|k| k.split('-').next().unwrap_or("").to_lowercase() == lang) {
            return key.clone();
        }
    }
    if packs.contains_key(fallback) {
        fallback.to_string()
    } else {
        packs.keys().next().cloned().unwrap_or_default()
    }
}

/// The speech voice list of one plausible machine of `target_os`, as the
/// voice objects the browser's MaskConfig requires.
pub fn generate_random_voice_subset(target_os: &str, locale: Option<&str>, seed: Option<u32>) -> Result<Vec<Value>> {
    let mut rng = rng(seed);
    let key = os_key(target_os).unwrap_or("mac");
    let manifest = voice_manifests()
        .get(key)
        .filter(|m| !m.is_null())
        .or_else(|| voice_manifests().get("mac"))
        .ok_or_else(|| Error::value("voice-manifests.json has no macOS entry"))?;

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut legacy: Vec<String> = Vec::new();
    let empty = Map::new();
    let packs = manifest.get("langPacks").and_then(Value::as_object).unwrap_or(&empty);

    fn add(entries: &Value, out: &mut Vec<String>, seen: &mut HashSet<String>) {
        for e in strings(entries) {
            if seen.insert(e.clone()) {
                out.push(e);
            }
        }
    }
    let take_pack = |pack: &Value,
                     rng: &mut PyRandom,
                     out: &mut Vec<String>,
                     seen: &mut HashSet<String>,
                     legacy: &mut Vec<String>| {
        add(&pack["oneCore"], out, seen);
        let desktop = strings(&pack["desktop"]);
        if !desktop.is_empty() && rng.random() < f(pack, "desktopProb", 0.0) {
            for e in desktop {
                if !legacy.contains(&e) {
                    legacy.push(e);
                }
            }
        }
    };

    add(&manifest["base"], &mut out, &mut seen);
    let mut chosen: HashSet<String> = HashSet::new();
    if !packs.is_empty() {
        let fallback = manifest["fallbackLocale"].as_str().unwrap_or("en-US");
        let pack_key = resolve_display_pack(packs, fallback, locale);
        chosen.insert(pack_key.clone());
        take_pack(&packs[&pack_key], &mut rng, &mut out, &mut seen, &mut legacy);
    }

    for addition in manifest["additions"].as_array().into_iter().flatten() {
        if addition["deferred"].as_bool().unwrap_or(false) {
            continue;
        }
        if let Some(req) = addition["requiresLocale"].as_str().filter(|r| !r.is_empty()) {
            if !locale.unwrap_or("").to_lowercase().starts_with(&req.to_lowercase()) {
                continue;
            }
        }
        if rng.random() >= f(addition, "prob", 0.0) {
            continue;
        }
        match addition["kind"].as_str() {
            Some("bundle") => add(&addition["voices"], &mut out, &mut seen),
            Some("alacarte") => {
                let default_sizes = vec![json!({"n": 1, "w": 1})];
                let sizes = addition["sizes"].as_array().filter(|s| !s.is_empty()).unwrap_or(&default_sizes);
                let k = weighted_pick(&mut rng, sizes)["n"].as_i64().unwrap_or(1) as usize;
                for e in weighted_sample(&mut rng, &strings(&addition["voices"]), k, |_| 1.0) {
                    if seen.insert(e.clone()) {
                        let lower = e.to_lowercase();
                        let idx = out.iter().position(|v| v.to_lowercase() > lower).unwrap_or(out.len());
                        out.insert(idx, e);
                    }
                }
            }
            Some("groups") if !packs.is_empty() => {
                let eligible: Vec<String> = strings(&addition["groups"])
                    .into_iter()
                    .filter(|g| packs.contains_key(g) && !chosen.contains(g))
                    .collect();
                if eligible.is_empty() {
                    continue;
                }
                let k = match addition["sizes"].as_array().filter(|s| !s.is_empty()) {
                    Some(sizes) => weighted_pick(&mut rng, sizes)["n"].as_i64().unwrap_or(1) as usize,
                    None => eligible.len(),
                };
                let weight =
                    |g: &String| packs[g].get("weight").and_then(Value::as_f64).filter(|w| *w != 0.0).unwrap_or(0.01);
                for g in weighted_sample(&mut rng, &eligible, k, weight) {
                    chosen.insert(g.clone());
                    take_pack(&packs[&g], &mut rng, &mut out, &mut seen, &mut legacy);
                }
            }
            _ => {}
        }
    }

    out.extend(legacy);
    let mut voices = Vec::new();
    for entry in out {
        let (name, lang, vtype) =
            split_voice_entry(&entry).ok_or_else(|| Error::value(format!("malformed voice entry {entry:?}")))?;
        voices.push(json!({
            "name": name,
            "lang": lang,
            "voiceUri": voice_uri(key, &name, &lang),
            "isDefault": false,
            "isLocalService": vtype == "local",
        }));
    }
    Ok(voices)
}

/// Coerce a preset's "Name:lang:type" voice strings into voice objects.
pub fn normalize_preset_voices(voices: &[Value], target_os: &str) -> Vec<Value> {
    let key = os_key(target_os).unwrap_or("mac");
    let mut result: Vec<Value> = Vec::new();
    for entry in voices {
        if entry.is_object() {
            result.push(entry.clone());
            continue;
        }
        let Some((name, lang, vtype)) = entry.as_str().and_then(split_voice_entry) else { continue };
        if name.is_empty() || lang.is_empty() {
            continue;
        }
        result.push(json!({
            "name": name,
            "lang": lang,
            "voiceUri": voice_uri(key, &name, &lang),
            "isDefault": false,
            "isLocalService": vtype == "local",
        }));
    }
    if !result.is_empty() && !result.iter().any(|v| v["isDefault"].as_bool() == Some(true)) {
        result[0]["isDefault"] = Value::Bool(true);
    }
    result
}

/// Logical CPUs this process may run on.
pub fn host_cpu_count() -> Option<i64> {
    std::thread::available_parallelism().ok().map(|n| n.get() as i64)
}

fn plausible_core_counts() -> Vec<i64> {
    launcher_constants().plausible_core_counts.iter().map(|c| *c as i64).collect()
}

/// navigator.hardwareConcurrency = the host's parallelism snapped down into
/// the plausible core counts, or the drawn value when the browser will be
/// pinned to that many cores.
pub fn fix_hardware_concurrency(config: &mut Config, can_pin: Option<bool>) {
    let Some(cap) = host_cpu_count() else { return };
    let table = plausible_core_counts();
    let pinnable = crate::cpu_affinity::supported() && can_pin != Some(false);
    let host_value = table.iter().copied().rfind(|c| *c <= cap).unwrap_or(table[0]);
    if pinnable {
        if let Some(drawn) = int(config, "navigator.hardwareConcurrency").filter(|d| *d >= 1) {
            let target = drawn.min(cap);
            let value = table.iter().copied().rfind(|c| *c <= target).unwrap_or(table[0]);
            config.insert("navigator.hardwareConcurrency".into(), value.into());
            return;
        }
    }
    config.insert("navigator.hardwareConcurrency".into(), host_value.into());
}

/// Force navigator.platform and navigator.oscpu to match a Linux UA's architecture.
pub fn fix_navigator_arch(config: &mut Config, target_os: &str) {
    if target_os != "lin" {
        return;
    }
    let ua = config.get("navigator.userAgent").and_then(Value::as_str).unwrap_or("").to_string();
    let target = if ua.contains("Linux x86_64") {
        "Linux x86_64"
    } else if ua.contains("Linux i686") {
        "Linux i686"
    } else {
        return;
    };
    for key in ["navigator.platform", "navigator.oscpu"] {
        if config.get(key).and_then(Value::as_str) != Some(target) {
            config.insert(key.into(), target.into());
        }
    }
}

/// Claim the available screen and touch points Firefox's baseline
/// fingerprinting protection will report for the claimed OS.
pub fn follow_baseline_protection(config: &mut Config, target_os: &str) {
    if let (Some(w), Some(h)) = (tint(config, "screen.width"), tint(config, "screen.height")) {
        let taskbar = launcher_constants().baseline_taskbar_height.get(target_os).copied().unwrap_or(0);
        config.insert("screen.availWidth".into(), w.into());
        config.insert("screen.availHeight".into(), (h - taskbar).into());
    }
    if int(config, "navigator.maxTouchPoints").unwrap_or(0) > 1 {
        config.insert("navigator.maxTouchPoints".into(), launcher_constants().baseline_multi_touch_points.into());
    }
}

/// Enforce inner <= outer <= avail <= screen on both axes.
pub fn clamp_window_dimensions(config: &mut Config) {
    for axis in ["Width", "Height"] {
        let skey = format!("screen.{}", axis.to_lowercase());
        let akey = format!("screen.avail{axis}");
        let okey = format!("window.outer{axis}");
        let ikey = format!("window.inner{axis}");
        let screen = tint(config, &skey);
        let avail = tint(config, &akey);
        let outer = tint(config, &okey);
        let inner = tint(config, &ikey);
        if let (Some(s), Some(a)) = (screen, avail) {
            if a > s {
                config.insert(akey.clone(), s.into());
            }
        }
        let avail_clamped = if config.contains_key(&akey) { int(config, &akey) } else { screen };
        let outer_cap = avail_clamped.or(screen).filter(|v| *v != 0);
        if let (Some(o), Some(cap)) = (outer, outer_cap) {
            if o > cap {
                let chrome = inner.map(|i| (o - i).max(0)).unwrap_or(0);
                config.insert(okey.clone(), cap.into());
                if inner.is_some() {
                    config.insert(ikey.clone(), (cap - chrome).max(1).into());
                }
            }
        }
        let outer_clamped = if config.contains_key(&okey) { tint(config, &okey) } else { outer };
        if let (Some(i), Some(o)) = (tint(config, &ikey), outer_clamped) {
            if i > o {
                config.insert(ikey.clone(), o.into());
            }
        }
    }
}

/// Shrink the screen to the real display, keeping the taskbar delta.
pub fn clamp_screen_to_display(config: &mut Config, max_width: Option<i64>, max_height: Option<i64>) {
    for (axis, cap, avail_key) in
        [("width", max_width, "screen.availWidth"), ("height", max_height, "screen.availHeight")]
    {
        let key = format!("screen.{axis}");
        let (Some(screen), Some(cap)) = (tint(config, &key), cap.filter(|c| *c != 0)) else { continue };
        if screen <= cap {
            continue;
        }
        let avail = tint(config, avail_key);
        config.insert(key, cap.into());
        if let Some(a) = avail {
            config.insert(avail_key.into(), (cap - (screen - a).max(0)).max(1).into());
        }
    }
}

/// Keep the window box inside the screen.
pub fn clamp_window_position(config: &mut Config) {
    for (axis, pos_key) in [("Width", "window.screenX"), ("Height", "window.screenY")] {
        let screen = tint(config, &format!("screen.{}", axis.to_lowercase()));
        let outer = tint(config, &format!("window.outer{axis}"));
        let Some(pos) = int(config, pos_key) else { continue };
        let (Some(s), Some(o)) = (screen, outer) else { continue };
        config.insert(pos_key.into(), pos.min(s - o).max(0).into());
    }
}

/// Give the identity a plausible set of media devices, unless the caller set any.
pub fn set_media_devices_defaults(config: &mut Config, salt: u64) {
    if config.keys().any(|k| k.starts_with("mediaDevices:")) {
        return;
    }
    let platform = config.get("navigator.platform").map(py_str).unwrap_or_default();
    let key = if platform.starts_with("Win") {
        "win"
    } else if platform.starts_with("Mac") {
        "mac"
    } else {
        "lin"
    };
    let seed = identity_seed(config, salt);
    for (k, v) in draw_media_devices(key, Some(seed)) {
        config.insert(k, v);
    }
}

fn weighted_choice<'a>(rng: &mut PyRandom, items: &'a [Value]) -> &'a Value {
    let w = |i: &Value| i.get("w").and_then(Value::as_f64).unwrap_or(1.0);
    let total: f64 = items.iter().map(w).sum();
    let mut r = rng.random() * total;
    for item in items {
        r -= w(item);
        if r < 0.0 {
            return item;
        }
    }
    &items[items.len() - 1]
}

/// One machine's media devices for an OS key: counts plus aligned label and group lists.
pub fn draw_media_devices(os_key: &str, seed: Option<u32>) -> Map<String, Value> {
    let mut rng = rng(seed);
    let cat = media_devices().get(os_key).filter(|c| c.is_object()).unwrap_or(&media_devices()["win"]);
    let p_no_mic = match os_key {
        "win" => 0.08,
        "lin" => 0.20,
        _ => 0.0,
    };
    let p_builtin_cam = match os_key {
        "win" => 0.78,
        "lin" => 0.45,
        _ => 0.0,
    };
    let mut mics: Vec<(String, String)> = Vec::new();
    let mut outs: Vec<(String, String)> = Vec::new();
    let mut cams: Vec<(String, String)> = Vec::new();
    let mut counter = 0;
    let mut group = || {
        counter += 1;
        format!("hw-{counter}")
    };
    let arr = |k: &str| cat[k].as_array().cloned().unwrap_or_default();

    let cards = arr("cards");
    let card = weighted_choice(&mut rng, &cards).clone();
    let no_mic = rng.random() < p_no_mic;
    let card_grp = group();
    if !no_mic {
        for m in strings(&card["mics"]) {
            mics.push((m, card_grp.clone()));
        }
    }
    for o in strings(&card["outs"]) {
        outs.push((o, card_grp.clone()));
    }
    if let Some(cam) = card["cam"].as_str() {
        cams.push((cam.to_string(), group()));
    }

    let cameras = arr("cameras");
    let p_cam = if no_mic { p_builtin_cam * 0.3 } else { p_builtin_cam };
    if rng.random() < p_cam {
        let builtin: Vec<Value> =
            cameras.iter().filter(|c| c.get("mic").is_none_or(|m| m.is_null() || m == "")).cloned().collect();
        if !builtin.is_empty() {
            let cam = weighted_choice(&mut rng, &builtin)["cam"].as_str().unwrap_or("").to_string();
            cams.push((cam, group()));
        }
    }
    let mut add_item =
        |item: &Value, grp: String, cams: &mut Vec<(String, String)>, group: &mut dyn FnMut() -> String| {
            for m in strings(&item["mics"]) {
                mics.push((m, grp.clone()));
            }
            for o in strings(&item["outs"]) {
                outs.push((o, grp.clone()));
            }
            if let Some(cam) = item["cam"].as_str() {
                cams.push((cam.to_string(), group()));
            }
        };
    if rng.random() < f(cat, "p_headset", 0.0) {
        let headsets = arr("headsets");
        let item = weighted_choice(&mut rng, &headsets).clone();
        let g = group();
        add_item(&item, g, &mut cams, &mut group);
    }
    if rng.random() < f(cat, "p_display", 0.0) {
        let displays = arr("displays");
        let item = weighted_choice(&mut rng, &displays).clone();
        let g = group();
        add_item(&item, g, &mut cams, &mut group);
    }
    if rng.random() < f(cat, "p_extra_camera", 0.0) {
        let with_mic: Vec<Value> = cameras
            .iter()
            .filter(|c| c.get("mic").is_some_and(|m| m.as_str().is_some_and(|s| !s.is_empty())))
            .cloned()
            .collect();
        let external = if with_mic.is_empty() { cameras.clone() } else { with_mic };
        let cam = weighted_choice(&mut rng, &external).clone();
        let g = group();
        cams.push((cam["cam"].as_str().unwrap_or("").to_string(), g.clone()));
        if let Some(mic) = cam["mic"].as_str().filter(|m| !m.is_empty()) {
            mics.push((mic.to_string(), g));
        }
    }
    if cat["monitor_sources"].as_bool().unwrap_or(false) {
        for (label, grp) in outs.clone() {
            mics.push((format!("Monitor of {label}"), grp));
        }
    }

    let labels = |v: &[(String, String)]| Value::from(v.iter().map(|x| x.0.clone()).collect::<Vec<_>>());
    let groups = |v: &[(String, String)]| Value::from(v.iter().map(|x| x.1.clone()).collect::<Vec<_>>());
    let mut out = Map::new();
    out.insert("mediaDevices:enabled".into(), true.into());
    out.insert("mediaDevices:micros".into(), mics.len().into());
    out.insert("mediaDevices:webcams".into(), cams.len().into());
    out.insert("mediaDevices:speakers".into(), outs.len().into());
    out.insert("mediaDevices:microphoneLabels".into(), labels(&mics));
    out.insert("mediaDevices:microphoneGroups".into(), groups(&mics));
    out.insert("mediaDevices:webcamLabels".into(), labels(&cams));
    out.insert("mediaDevices:webcamGroups".into(), groups(&cams));
    out.insert("mediaDevices:speakerLabels".into(), labels(&outs));
    out.insert("mediaDevices:speakerGroups".into(), groups(&outs));
    out
}

const SOFTWARE_RENDERERS: [&str; 4] = ["llvmpipe", "Microsoft Basic Render Driver", "SwiftShader", "Generic Renderer"];
const DISCRETE_GPU_BUCKETS: [&str; 4] =
    ["GeForce 8800 GTX", "GeForce GTX 480", "GeForce GTX 980", "Radeon R9 200 Series"];
const NETBOOK_MAX_PIXELS: i64 = 1024 * 600;
/// The smallest screen mainstream hardware still ships.
pub const MODERN_SCREEN_FLOOR: (i64, i64) = (1366, 768);

/// Reduce a reported renderer to Gecko's sanitized device bucket.
pub fn renderer_bucket(renderer: &str) -> String {
    let mut core = renderer.strip_suffix(", or similar").unwrap_or(renderer).to_string();
    let angle_d3d = regex::Regex::new(r"^ANGLE \([^,]*, (.*?) Direct3D.*\)$").unwrap();
    let angle_vk = regex::Regex::new(r"^ANGLE \((.*)\) on Vulkan$").unwrap();
    let pcie = regex::Regex::new(r"^(.*)/PCIe?/SSE2$").unwrap();
    if let Some(c) = angle_d3d.captures(&core).or_else(|| angle_vk.captures(&core)) {
        core = c[1].to_string();
    }
    if let Some(c) = pcie.captures(&core) {
        core = c[1].to_string();
    }
    core.strip_prefix("NVIDIA ").unwrap_or(&core).to_string()
}

/// Lift netbook-era screen geometry to the modern floor, keeping the avail gaps.
pub fn raise_screen_to_modern_floor(config: &mut Config) {
    let (min_w, min_h) = MODERN_SCREEN_FLOOR;
    let (Some(sw), Some(sh)) = (tint(config, "screen.width"), tint(config, "screen.height")) else { return };
    if sw >= min_w && sh >= min_h {
        return;
    }
    let gap_w = tint(config, "screen.availWidth").map(|a| sw - a);
    let gap_h = tint(config, "screen.availHeight").map(|a| sh - a);
    let (nw, nh) = (sw.max(min_w), sh.max(min_h));
    config.insert("screen.width".into(), nw.into());
    config.insert("screen.height".into(), nh.into());
    if let Some(g) = gap_w {
        config.insert("screen.availWidth".into(), (nw - g.max(0)).max(1).into());
    }
    if let Some(g) = gap_h {
        config.insert("screen.availHeight".into(), (nh - g.max(0)).max(1).into());
    }
}

pub fn is_software_renderer(renderer: &str) -> bool {
    !renderer.is_empty() && SOFTWARE_RENDERERS.iter().any(|n| renderer.contains(n))
}

/// Whether a renderer plausibly drives a screen of this size: discrete GPUs
/// never shipped behind netbook panels.
pub fn gpu_screen_is_plausible(renderer: &str, width: Option<i64>, height: Option<i64>) -> bool {
    let (Some(w), Some(h)) = (width.filter(|w| *w != 0), height.filter(|h| *h != 0)) else { return true };
    if renderer.is_empty() || is_software_renderer(renderer) {
        return true;
    }
    if !DISCRETE_GPU_BUCKETS.contains(&renderer_bucket(renderer).as_str()) {
        return true;
    }
    w * h > NETBOOK_MAX_PIXELS
}

/// Firefox major version from which the v150 preset bundle is preferred.
const PRESETS_V150_MIN_FF: i64 = 149;

/// The bundled presets appropriate for a Firefox version.
pub fn load_presets(ff_version: Option<&str>) -> &'static Value {
    static OLD: OnceLock<Value> = OnceLock::new();
    static V150: OnceLock<Value> = OnceLock::new();
    let major: i64 = ff_version.and_then(|v| v.split('.').next()?.parse().ok()).unwrap_or(0);
    if major >= PRESETS_V150_MIN_FF {
        load(&V150, data::FINGERPRINT_PRESETS_V150)
    } else {
        load(&OLD, data::FINGERPRINT_PRESETS)
    }
}

/// A random bundled preset for one of the given OSes (any OS when empty).
pub fn get_random_preset(os: &[String], ff_version: Option<&str>) -> Option<Value> {
    let presets = &load_presets(ff_version)["presets"];
    let keys: Vec<String> = if os.is_empty() {
        vec!["macos".into(), "windows".into(), "linux".into()]
    } else {
        os.iter().map(|o| os_name(o).map(str::to_string).unwrap_or_else(|| o.clone())).collect()
    };
    let candidates: Vec<&Value> = keys.iter().flat_map(|k| presets[k].as_array().into_iter().flatten()).collect();
    if candidates.is_empty() {
        return None;
    }
    Some(candidates[rand::thread_rng().gen_range(0..candidates.len())].clone())
}

/// The appVersion Firefox reports for a browser sending this user agent.
pub fn app_version_from_user_agent(user_agent: &str) -> Option<String> {
    let re = regex::Regex::new(r"^Mozilla/5\.0 \(([^)]*)\)").unwrap();
    let block = re.captures(user_agent)?;
    let mut kept = Vec::new();
    for token in block[1].split(';').map(str::trim) {
        if token.starts_with("rv:")
            || ["Win64", "x64", "Mobile", "Tablet"].contains(&token)
            || token.starts_with("Linux ")
            || token.starts_with("Intel Mac OS X")
        {
            continue;
        }
        kept.push(if token.starts_with("Windows") { "Windows".to_string() } else { token.to_string() });
    }
    (!kept.is_empty()).then(|| format!("5.0 ({})", kept.join("; ")))
}

fn set_app_version(config: &mut Config) {
    let ua = config.get("navigator.userAgent").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(v) = app_version_from_user_agent(&ua) {
        config.insert("navigator.appVersion".into(), v.into());
    }
}

fn draw_fonts(target_os: &str, seed: u32, instead: &str, identity: Option<&str>) -> Option<Vec<String>> {
    let fonts = generate_random_font_subset(target_os, Some(seed), false, None);
    if fonts.is_empty() {
        warnings::fallback("Drawing the font list", instead, "no fonts were drawn", identity);
        return None;
    }
    Some(fonts)
}

fn draw_voices(target_os: &str, seed: u32, instead: &str, identity: Option<&str>) -> Option<Vec<Value>> {
    match generate_random_voice_subset(target_os, None, Some(seed)) {
        Ok(v) => Some(v),
        Err(e) => {
            warnings::fallback("Drawing the speech voices", instead, &e.to_string(), identity);
            None
        }
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Convert a recorded preset to Camoufox config keys. `salt` keys the font
/// and voice draws; None draws a fresh one.
pub fn from_preset(preset: &Value, ff_version: Option<&str>, salt: Option<u64>) -> Config {
    let salt = salt.unwrap_or_else(|| identity_salt(None));
    let mut config = Config::new();
    let nav = &preset["navigator"];
    if let Some(ua) = nav["userAgent"].as_str().filter(|s| !s.is_empty()) {
        let mut ua = ua.to_string();
        if let Some(ff) = ff_version {
            ua = regex::Regex::new(r"Firefox/\d+\.0").unwrap().replace_all(&ua, format!("Firefox/{ff}.0")).to_string();
            ua = regex::Regex::new(r"rv:\d+\.0").unwrap().replace_all(&ua, format!("rv:{ff}.0")).to_string();
        }
        config.insert("navigator.userAgent".into(), ua.into());
    }
    let platform = nav["platform"].as_str().filter(|s| !s.is_empty());
    if let Some(p) = platform {
        config.insert("navigator.platform".into(), p.into());
    }
    if truthy(nav.get("hardwareConcurrency")) {
        config.insert("navigator.hardwareConcurrency".into(), nav["hardwareConcurrency"].clone());
    }
    let oscpu = nav["oscpu"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| platform.and_then(oscpu_for_platform).map(str::to_string));
    if let Some(o) = oscpu {
        config.insert("navigator.oscpu".into(), o.into());
    }
    set_app_version(&mut config);
    if let Some(t) = nav.get("maxTouchPoints") {
        config.insert("navigator.maxTouchPoints".into(), t.clone());
    }

    let screen = &preset["screen"];
    for (src, dst) in [("width", "screen.width"), ("height", "screen.height")] {
        if truthy(screen.get(src)) {
            config.insert(dst.into(), screen[src].clone());
        }
    }
    if truthy(screen.get("colorDepth")) {
        config.insert("screen.colorDepth".into(), screen["colorDepth"].clone());
        config.insert("screen.pixelDepth".into(), screen["colorDepth"].clone());
    }
    for (src, dst) in [("availWidth", "screen.availWidth"), ("availHeight", "screen.availHeight")] {
        if truthy(screen.get(src)) {
            config.insert(dst.into(), screen[src].clone());
        }
    }
    let webgl = &preset["webgl"];
    if let Some(v) = webgl["unmaskedVendor"].as_str().filter(|s| !s.is_empty()) {
        config.insert("webGl:vendor".into(), v.into());
    }
    if let Some(r) = webgl["unmaskedRenderer"].as_str().filter(|s| !s.is_empty()) {
        config.insert("webGl:renderer".into(), r.into());
    }
    config.insert("audio:seed".into(), rand::thread_rng().gen_range(1..=4_294_967_295u64).into());
    if let Some(tz) = preset["timezone"].as_str().filter(|s| !s.is_empty()) {
        config.insert("timezone".into(), tz.into());
    }

    let target_os = platform_to_os(platform.unwrap_or(""));
    let preset_key = format!(
        "{} / {}",
        config.get("navigator.userAgent").map(py_str).unwrap_or_else(|| "None".into()),
        config.get("webGl:renderer").map(py_str).unwrap_or_else(|| "None".into())
    );
    let has_fonts = truthy(preset.get("fonts"));
    let fonts_instead = if has_fonts { "the preset's recorded fonts" } else { "the browser's own fonts" };
    match draw_fonts(target_os, identity_seed(&config, salt), fonts_instead, Some(&preset_key)) {
        Some(fonts) => {
            config.insert("fonts".into(), fonts.into());
        }
        None if has_fonts => {
            let mut fonts = strings(&preset["fonts"]);
            ensure_marker_fonts(&mut fonts, marker_fonts(os_key(target_os).unwrap_or("mac")));
            config.insert("fonts".into(), fonts.into());
        }
        None => {}
    }
    let has_voices = truthy(preset.get("speechVoices"));
    let voices_instead = if has_voices { "the preset's recorded voices" } else { "the browser's own voices" };
    match draw_voices(target_os, identity_seed(&config, salt), voices_instead, Some(&preset_key)) {
        Some(v) => {
            config.insert("voices".into(), v.into());
        }
        None if has_voices => {
            let voices =
                normalize_preset_voices(preset["speechVoices"].as_array().map(Vec::as_slice).unwrap_or(&[]), target_os);
            config.insert("voices".into(), voices.into());
        }
        None => {}
    }
    config
}

/// Replace a three-digit `1NN.0` Firefox version not touching other digits.
fn replace_ff_versions(s: &str, ff: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let is_match = i + 5 <= b.len()
            && b[i] == b'1'
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
            && b[i + 3] == b'.'
            && b[i + 4] == b'0'
            && (i == 0 || !b[i - 1].is_ascii_digit())
            && (i + 5 == b.len() || !b[i + 5].is_ascii_digit());
        if is_match {
            out.push_str(ff);
            out.push_str(".0");
            i += 5;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn cast_to_properties(out: &mut Config, mapping: &Value, data: &Value, ff_version: Option<&str>) {
    let Some(obj) = data.as_object() else { return };
    for (key, value) in obj {
        if !truthy(Some(value)) {
            continue;
        }
        let Some(target) = mapping.get(key) else { continue };
        if value.is_object() {
            if target.is_object() {
                cast_to_properties(out, target, value, ff_version);
            }
            continue;
        }
        let Some(target) = target.as_str() else { continue };
        let mut value = value.clone();
        if let Value::Array(items) = &value {
            match items.as_slice() {
                [Value::String(s)] => value = Value::String(s.clone()),
                _ => continue,
            }
        }
        if target.starts_with("screen.") && value.as_i64().is_some_and(|v| v < 0) {
            value = 0.into();
        }
        if let (Some(ff), Some(s)) = (ff_version, value.as_str()) {
            value = replace_ff_versions(s, ff).into();
        }
        out.insert(target.into(), value);
    }
}

fn handle_screen_xy(out: &mut Config, fingerprint: &Value) {
    if out.contains_key("window.screenY") {
        return;
    }
    let screen_x = fingerprint["window"]["screenX"].as_i64().unwrap_or(0);
    if screen_x == 0 {
        out.insert("window.screenX".into(), 0.into());
        out.insert("window.screenY".into(), 0.into());
        return;
    }
    if (-50..=50).contains(&screen_x) {
        out.insert("window.screenY".into(), screen_x.into());
        return;
    }
    let screen_y = fingerprint["screen"]["availHeight"].as_i64().unwrap_or(0)
        - fingerprint["window"]["outerHeight"].as_i64().unwrap_or(0);
    let y = match screen_y {
        0 => 0,
        y if y > 0 => rand::thread_rng().gen_range(0..y),
        y => rand::thread_rng().gen_range(y..0),
    };
    out.insert("window.screenY".into(), y.into());
}

/// Convert an fpgen fingerprint to Camoufox config keys.
pub fn from_fpgen(fingerprint: &Value, ff_version: Option<&str>) -> Config {
    let mut out = Config::new();
    cast_to_properties(&mut out, fpgen_mapping(), fingerprint, ff_version);
    handle_screen_xy(&mut out, fingerprint);
    set_app_version(&mut out);
    out
}

/// Set a fixed outer window size, centred on the screen.
pub fn handle_window_size(fp: &mut Value, outer_width: i64, outer_height: i64) {
    if !fp["screen"].is_object() {
        fp["screen"] = json!({});
    }
    if !fp["window"].is_object() {
        fp["window"] = json!({});
    }
    let sw = fp["screen"]["width"].as_i64().filter(|v| *v != 0).unwrap_or(outer_width);
    let sh = fp["screen"]["height"].as_i64().filter(|v| *v != 0).unwrap_or(outer_height);
    let window = fp["window"].as_object_mut().unwrap();
    let get = |w: &Map<String, Value>, k: &str| w.get(k).and_then(Value::as_i64).unwrap_or(0);
    let sx = get(window, "screenX");
    window.insert("screenX".into(), (sx + (sw - outer_width).div_euclid(2)).into());
    window.insert("screenY".into(), (sh - outer_height).div_euclid(2).into());
    let ow = get(window, "outerWidth");
    let oh = get(window, "outerHeight");
    if get(window, "innerWidth") != 0 {
        let iw = get(window, "innerWidth");
        window.insert("innerWidth".into(), (outer_width - ow + iw).max(0).into());
    }
    if get(window, "innerHeight") != 0 {
        let ih = get(window, "innerHeight");
        window.insert("innerHeight".into(), (outer_height - oh + ih).max(0).into());
    }
    window.insert("outerWidth".into(), outer_width.into());
    window.insert("outerHeight".into(), outer_height.into());
}

/// Generate a Firefox fingerprint with fpgen. `screen` bounds the screen
/// (dropped when nothing fits it); `window` overrides the outer window size.
pub fn generate_fingerprint(window: Option<(i64, i64)>, screen: Option<&Screen>, os: &[String]) -> Result<Value> {
    let network = fpgen::network()?;
    let mut conditions: Vec<(&str, Condition)> = vec![("browser", Condition::Equals("Firefox".into()))];
    if !os.is_empty() {
        let resolved = os
            .iter()
            .map(|n| {
                fpgen_os(&n.to_lowercase())
                    .map(Value::from)
                    .ok_or_else(|| Error::value(format!("Unknown OS for fingerprint generation: {n:?}")))
            })
            .collect::<Result<Vec<_>>>()?;
        conditions.push((
            "os",
            if resolved.len() == 1 { Condition::Equals(resolved[0].clone()) } else { Condition::OneOf(resolved) },
        ));
    }
    let screen_conditions = screen.map(Screen::conditions).unwrap_or_default();
    let has_screen = !screen_conditions.is_empty();
    let mut all = conditions;
    all.extend(screen_conditions);
    let mut fingerprint = match network.generate(&all) {
        Ok(fp) => fp,
        Err(Error::InvalidConstraints(_) | Error::RestrictiveConstraints(_)) if has_screen => {
            // The screen bound is best effort: a display the model has
            // nothing to fit (a 1x1 Xvfb) must not stop generation.
            let without: Vec<(&str, Condition)> = all.into_iter().filter(|(k, _)| !k.starts_with("screen.")).collect();
            network.generate(&without)?
        }
        Err(e) => return Err(e),
    };
    if let Some((w, h)) = window {
        handle_window_size(&mut fingerprint, w, h);
    }
    Ok(fingerprint)
}

/// A per-context identity: the init script that applies it and the Playwright
/// `newContext()` options that go with it.
#[derive(Debug, Clone)]
pub struct ContextFingerprint {
    pub init_script: String,
    /// Playwright `newContext()` options, in Playwright's camelCase.
    pub context_options: Map<String, Value>,
    pub config: Config,
}

fn build_init_script(values: &Map<String, Value>) -> Result<String> {
    let mut lines = vec!["(function(v) {".to_string(), "  var w = window;".to_string()];
    let setters = [
        ("audioFingerprintSeed", "setAudioFingerprintSeed"),
        ("navigatorPlatform", "setNavigatorPlatform"),
        ("navigatorOscpu", "setNavigatorOscpu"),
        ("navigatorUserAgent", "setNavigatorUserAgent"),
        ("hardwareConcurrency", "setNavigatorHardwareConcurrency"),
        ("webglVendor", "setWebGLVendor"),
        ("webglRenderer", "setWebGLRenderer"),
    ];
    let present = |k: &str| values.get(k).filter(|v| !v.is_null());
    for (key, func) in setters {
        if let Some(v) = present(key) {
            lines.push(format!("  if (typeof w.{func} === \"function\") w.{func}({});", json_dumps_ascii(v)));
        }
    }
    if let (Some(sw), Some(sh)) =
        (present("screenWidth").filter(|v| truthy(Some(v))), present("screenHeight").filter(|v| truthy(Some(v))))
    {
        lines.push(format!("  if (typeof w.setScreenDimensions === \"function\") w.setScreenDimensions({sw}, {sh});"));
        if let Some(d) = present("screenColorDepth").filter(|v| truthy(Some(v))) {
            lines.push(format!("  if (typeof w.setScreenColorDepth === \"function\") w.setScreenColorDepth({d});"));
        }
    }
    if let Some(tz) = present("timezone").filter(|v| truthy(Some(v))) {
        lines.push(format!("  if (typeof w.setTimezone === \"function\") w.setTimezone({});", json_dumps_ascii(tz)));
    }
    match present("webrtcIP").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(ip) => {
            validate_ip(ip)?;
            let func = if valid_ipv4(ip) { "setWebRTCIPv4" } else { "setWebRTCIPv6" };
            lines.push(format!("  if (typeof w.{func} === \"function\") w.{func}({});", json_dumps_ascii(&ip.into())));
        }
        None => lines.push("  if (typeof w.setWebRTCIPv4 === \"function\") w.setWebRTCIPv4(\"\");".into()),
    }
    if let Some(fonts) = present("fontList").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        let joined = fonts.iter().map(py_str).collect::<Vec<_>>().join(",");
        lines.push(format!(
            "  if (typeof w.setFontList === \"function\") w.setFontList({});",
            json_dumps_ascii(&joined.into())
        ));
    }
    if let Some(voices) = present("speechVoices").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        let joined =
            voices.iter().map(|v| v.get("name").map(py_str).unwrap_or_else(|| py_str(v))).collect::<Vec<_>>().join(",");
        lines.push(format!(
            "  if (typeof w.setSpeechVoices === \"function\") w.setSpeechVoices({});",
            json_dumps_ascii(&joined.into())
        ));
    }
    lines.push("})();".into());
    Ok(lines.join("\n"))
}

/// Options for [`generate_context_fingerprint`].
#[derive(Debug, Clone, Default)]
pub struct ContextFingerprintOptions {
    pub preset: Option<Value>,
    pub os: Option<String>,
    pub ff_version: Option<String>,
    pub webrtc_ip: Option<String>,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub config_overrides: Option<Config>,
    /// The (vendor, renderer) a drawn identity claims, instead of drawing one.
    pub webgl_config: Option<(String, String)>,
}

/// One identity for a browser context, applied by an init script through
/// the browser's self-destructing `window.setXxx()` functions.
pub fn generate_context_fingerprint(opts: &ContextFingerprintOptions) -> Result<ContextFingerprint> {
    let ff = opts.ff_version.as_deref();
    let (mut config, nav, screen, webgl, preset_tz) = if let Some(preset) = &opts.preset {
        (
            from_preset(preset, ff, None),
            preset["navigator"].clone(),
            preset["screen"].clone(),
            preset["webgl"].clone(),
            preset["timezone"].as_str().map(str::to_string),
        )
    } else {
        let os: Vec<String> = opts.os.iter().cloned().collect();
        let fp = generate_fingerprint(None, None, &os)?;
        let mut config = from_fpgen(&fp, ff);
        if config.get("navigator.userAgent").map(py_str).unwrap_or_default().contains("Linux") {
            fix_navigator_arch(&mut config, "lin");
        }
        let salt = identity_salt(None);
        config.entry("audio:seed").or_insert_with(|| rand::thread_rng().gen_range(1..=4_294_967_295u64).into());
        let platform = config.get("navigator.platform").and_then(Value::as_str).unwrap_or("").to_string();
        let target = platform_to_os(&platform);
        let ua = config.get("navigator.userAgent").map(py_str);
        if !config.contains_key("fonts") {
            if let Some(fonts) =
                draw_fonts(target, identity_seed(&config, salt), "the browser's launch-time fonts", ua.as_deref())
            {
                config.insert("fonts".into(), fonts.into());
            }
        }
        if !config.contains_key("voices") {
            if let Some(voices) =
                draw_voices(target, identity_seed(&config, salt), "the browser's launch-time voices", ua.as_deref())
            {
                config.insert("voices".into(), voices.into());
            }
        }
        if !config.contains_key("navigator.oscpu") {
            if let Some(o) = oscpu_for_platform(&platform) {
                config.insert("navigator.oscpu".into(), o.into());
            }
        }
        if !truthy(config.get("webGl:vendor")) || !truthy(config.get("webGl:renderer")) {
            let target_key = opts.os.as_deref().and_then(os_key).or_else(|| os_key(target)).unwrap_or("mac");
            raise_screen_to_modern_floor(&mut config);
            let mut webgl_fp = match &opts.webgl_config {
                Some((v, r)) => crate::webgl::webgl_for_gpu(target_key, v, r, None)?,
                None => crate::webgl::sample_webgl_for_screen(
                    target_key,
                    int(&config, "screen.width"),
                    int(&config, "screen.height"),
                    None,
                    int(&config, "navigator.hardwareConcurrency"),
                )?,
            };
            webgl_fp.remove("webGl2Enabled");
            config.extend(webgl_fp);
        }
        let nav = json!({"platform": config.get("navigator.platform"), "hardwareConcurrency": config.get("navigator.hardwareConcurrency")});
        let screen = json!({
            "width": config.get("screen.width"),
            "height": config.get("screen.height"),
            "colorDepth": config.get("screen.colorDepth"),
            "devicePixelRatio": null,
        });
        let webgl =
            json!({"unmaskedVendor": config.get("webGl:vendor"), "unmaskedRenderer": config.get("webGl:renderer")});
        (config, nav, screen, webgl, None)
    };

    if let Some(tz) = &opts.timezone {
        config.insert("timezone".into(), tz.clone().into());
    }
    if let Some(locale) = &opts.locale {
        let parsed = crate::locales::normalize_locale(locale)?;
        config.insert("locale:language".into(), parsed.language.clone().into());
        config.insert("locale:region".into(), parsed.region.clone().into());
        config.insert("navigator.language".into(), parsed.as_string().into());
        if let Some(script) = &parsed.script {
            config.insert("locale:script".into(), script.clone().into());
        }
    }
    if let Some(overrides) = &opts.config_overrides {
        config.extend(overrides.clone());
    }

    let mut values = Map::new();
    values.insert("audioFingerprintSeed".into(), config.get("audio:seed").cloned().unwrap_or(Value::Null));
    values.insert("navigatorPlatform".into(), nav["platform"].clone());
    values.insert("navigatorOscpu".into(), config.get("navigator.oscpu").cloned().unwrap_or(Value::Null));
    values.insert("navigatorUserAgent".into(), config.get("navigator.userAgent").cloned().unwrap_or(Value::Null));
    let hc = if truthy(nav.get("hardwareConcurrency")) {
        nav["hardwareConcurrency"].clone()
    } else {
        config.get("navigator.hardwareConcurrency").cloned().unwrap_or(Value::Null)
    };
    values.insert("hardwareConcurrency".into(), hc);
    values.insert("webglVendor".into(), webgl["unmaskedVendor"].clone());
    values.insert("webglRenderer".into(), webgl["unmaskedRenderer"].clone());
    values.insert("screenWidth".into(), screen["width"].clone());
    values.insert("screenHeight".into(), screen["height"].clone());
    values.insert("screenColorDepth".into(), screen["colorDepth"].clone());
    let tz = preset_tz.map(Value::from).unwrap_or_else(|| config.get("timezone").cloned().unwrap_or(Value::Null));
    values.insert("timezone".into(), tz);
    values.insert("fontList".into(), config.get("fonts").cloned().unwrap_or(Value::Null));
    values.insert("speechVoices".into(), config.get("voices").cloned().unwrap_or(Value::Null));
    values.insert("webrtcIP".into(), opts.webrtc_ip.clone().unwrap_or_default().into());
    let init_script = build_init_script(&values)?;

    let mut context_options = Map::new();
    if let Some(ua) = config.get("navigator.userAgent").filter(|v| truthy(Some(v))) {
        context_options.insert("userAgent".into(), ua.clone());
    }
    if let (Some(sw), Some(sh)) =
        (screen["width"].as_i64().filter(|v| *v != 0), screen["height"].as_i64().filter(|v| *v != 0))
    {
        context_options.insert("viewport".into(), json!({"width": sw, "height": (sh - 28).max(600)}));
    }
    if truthy(screen.get("devicePixelRatio")) {
        context_options.insert("deviceScaleFactor".into(), screen["devicePixelRatio"].clone());
    }
    let tz = config
        .get("timezone")
        .filter(|v| truthy(Some(v)))
        .cloned()
        .or_else(|| opts.preset.as_ref().and_then(|p| p.get("timezone")).filter(|v| truthy(Some(v))).cloned());
    if let Some(tz) = tz {
        context_options.insert("timezoneId".into(), tz);
    }
    if let Some(lang) = config.get("navigator.language").filter(|v| truthy(Some(v))) {
        context_options.insert("locale".into(), lang.clone());
    }
    Ok(ContextFingerprint { init_script, context_options, config })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_version() {
        assert_eq!(
            app_version_from_user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:150.0) Gecko/20100101 Firefox/150.0"
            )
            .as_deref(),
            Some("5.0 (Windows)")
        );
        assert_eq!(
            app_version_from_user_agent(
                "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:150.0) Gecko/20100101 Firefox/150.0"
            )
            .as_deref(),
            Some("5.0 (X11; Ubuntu)")
        );
    }

    #[test]
    fn ff_version_rewrite() {
        assert_eq!(
            replace_ff_versions("rv:146.0) Gecko/20100101 Firefox/146.0", "156"),
            "rv:156.0) Gecko/20100101 Firefox/156.0"
        );
        assert_eq!(replace_ff_versions("1146.0 146.01", "156"), "1146.0 146.01");
    }

    #[test]
    fn identity_seed_is_stable() {
        let config: Config = serde_json::from_value(json!({
            "navigator.userAgent": "UA", "navigator.platform": "Win32", "screen.width": 1920,
            "screen.height": 1080, "navigator.hardwareConcurrency": 8
        }))
        .unwrap();
        // zlib.crc32(b"UA|Win32|1920|1080|8|42")
        assert_eq!(identity_seed(&config, 42), crc32(b"UA|Win32|1920|1080|8|42"));
    }

    #[test]
    fn font_draw_is_seeded_and_has_markers() {
        let a = generate_random_font_subset("windows", Some(123), false, None);
        let b = generate_random_font_subset("windows", Some(123), false, None);
        assert_eq!(a, b);
        for m in marker_fonts("win") {
            assert!(a.contains(m), "missing marker {m}");
        }
    }

    #[test]
    fn voices_are_full_objects() {
        let v = generate_random_voice_subset("windows", Some("fr-FR"), Some(5)).unwrap();
        assert!(!v.is_empty());
        for voice in &v {
            for field in ["lang", "name", "voiceUri", "isDefault", "isLocalService"] {
                assert!(voice.get(field).is_some());
            }
        }
        assert!(v.iter().any(|x| x["lang"].as_str().unwrap().starts_with("fr")));
    }

    #[test]
    fn media_devices_are_aligned() {
        let m = draw_media_devices("lin", Some(9));
        assert_eq!(
            m["mediaDevices:micros"].as_u64().unwrap() as usize,
            m["mediaDevices:microphoneLabels"].as_array().unwrap().len()
        );
        assert_eq!(
            m["mediaDevices:speakers"].as_u64().unwrap() as usize,
            m["mediaDevices:speakerGroups"].as_array().unwrap().len()
        );
    }

    #[test]
    fn window_dimensions_clamp() {
        let mut c: Config = serde_json::from_value(json!({
            "screen.width": 1366, "screen.height": 768, "screen.availWidth": 1366, "screen.availHeight": 728,
            "window.outerWidth": 1920, "window.innerWidth": 1900, "window.outerHeight": 1000, "window.innerHeight": 900
        }))
        .unwrap();
        clamp_window_dimensions(&mut c);
        assert_eq!(c["window.outerWidth"], 1366);
        assert_eq!(c["window.innerWidth"], 1346);
        assert_eq!(c["window.outerHeight"], 728);
        assert_eq!(c["window.innerHeight"], 628);
    }

    #[test]
    fn preset_conversion() {
        let preset = load_presets(Some("156"))["presets"]["windows"][0].clone();
        let c = from_preset(&preset, Some("156"), Some(1));
        assert!(c["navigator.userAgent"].as_str().unwrap().contains("Firefox/156.0"));
        assert_eq!(c["navigator.appVersion"], "5.0 (Windows)");
        assert!(!c["fonts"].as_array().unwrap().is_empty());
    }
}
