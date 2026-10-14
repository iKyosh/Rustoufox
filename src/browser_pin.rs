//! The browser build this release of the launcher is paired with.
//!
//! `browser-pin.json` names the one browser release the launcher was built and
//! tested against. By default that build is fetched and launched, so an
//! upgrade of the launcher never silently runs a browser whose Juggler, prefs
//! or patches it was not tested with. A user who explicitly chooses something
//! else (`rustoufox set ...`) keeps that choice, with a warning.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value};

use crate::paths::data;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserPin {
    pub tag: String,
    pub repo: String,
    pub repo_name: String,
    pub version: String,
    pub build: String,
}

impl BrowserPin {
    /// `<firefox version>-<build>`, the form `set` and the cache use.
    pub fn spec(&self) -> String {
        format!("{}-{}", self.version, self.build)
    }
}

/// The stamped pin, or None for an unpinned copy.
pub fn load_pin() -> Option<BrowserPin> {
    let data: Map<String, Value> = serde_json::from_str(data::BROWSER_PIN).ok()?;
    let get = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_string);
    let tag = get("tag").filter(|t| !t.is_empty())?;
    Some(BrowserPin {
        tag,
        repo: get("repo")?,
        repo_name: get("repo_name")?.to_lowercase(),
        version: get("version")?,
        build: get("build")?,
    })
}

/// Whether the user chose a channel or a build themselves.
pub fn is_explicit_choice(config: &Map<String, Value>) -> bool {
    let set = |k: &str| config.get(k).is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
    set("channel") || set("pinned")
}

/// The pin, unless the user explicitly chose otherwise.
pub fn effective_pin(config: Option<&Map<String, Value>>) -> Option<BrowserPin> {
    let pin = load_pin()?;
    let loaded;
    let config = match config {
        Some(c) => c,
        None => {
            loaded = crate::multiversion::load_config();
            &loaded
        }
    };
    if is_explicit_choice(config) {
        None
    } else {
        Some(pin)
    }
}

pub fn matches(pin: &BrowserPin, repo_name: &str, version: &str, build: &str) -> bool {
    repo_name.to_lowercase() == pin.repo_name && version == pin.version && build == pin.build
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// Warn once when an explicitly chosen build is not the paired one.
pub fn warn_if_unpaired(repo_name: &str, version: &str, build: &str) {
    let Some(pin) = load_pin() else { return };
    if matches(&pin, repo_name, version, build) || WARNED.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::warnings::runtime(format!(
        "Launching {} {version}-{build}, which you selected explicitly. This rustoufox release was \
         built and tested with {} {} ({}); other builds may not be compatible. Run \
         `rustoufox set --release` to go back to the paired build.",
        repo_name.to_lowercase(),
        pin.repo_name,
        pin.spec(),
        pin.tag
    ));
}
