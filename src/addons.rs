//! Firefox addons: the default ones (uBlock Origin), downloaded once into a
//! shared folder, and validation of addon paths passed by the caller.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::paths::install_dir;
use crate::pkgman::{unzip, webdl};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefaultAddons {
    /// uBlock Origin.
    Ubo,
}

impl DefaultAddons {
    pub const ALL: [DefaultAddons; 1] = [DefaultAddons::Ubo];

    pub fn name(&self) -> &'static str {
        match self {
            DefaultAddons::Ubo => "UBO",
        }
    }

    pub fn url(&self) -> &'static str {
        match self {
            DefaultAddons::Ubo => "https://addons.mozilla.org/firefox/downloads/latest/ublock-origin/latest.xpi",
        }
    }
}

pub fn addons_dir() -> PathBuf {
    install_dir().join("addons")
}

/// Every path must be an extracted addon (a folder with a manifest.json).
pub fn confirm_paths(paths: &[String]) -> Result<()> {
    for path in paths {
        let p = Path::new(path);
        if !p.is_dir() {
            return Err(Error::InvalidAddonPath(path.clone()));
        }
        if !p.join("manifest.json").exists() {
            return Err(Error::InvalidAddonPath(
                "manifest.json is missing. Addon path must be a path to an extracted addon.".into(),
            ));
        }
    }
    Ok(())
}

/// Download the default addons not yet present, and append the paths of the
/// available ones to `addons`. A failed download is reported and skipped.
pub fn maybe_download_addons(which: &[DefaultAddons], addons: &mut Vec<String>) {
    for addon in which {
        let path = addons_dir().join(addon.name());
        if path.join("manifest.json").exists() {
            addons.push(path.to_string_lossy().to_string());
            continue;
        }
        let result = (|| -> Result<()> {
            fs::create_dir_all(&path)?;
            let buffer = webdl(addon.url(), None, false)?;
            unzip(&buffer, &path, None)
        })();
        match result {
            Ok(()) => addons.push(path.to_string_lossy().to_string()),
            Err(e) => {
                let _ = fs::remove_dir_all(&path);
                eprintln!("Failed to download and extract {}: {e}", addon.name());
            }
        }
    }
}

/// Add the default addons, minus the excluded ones.
pub fn add_default_addons(addons: &mut Vec<String>, exclude: &[DefaultAddons]) {
    let which: Vec<DefaultAddons> = DefaultAddons::ALL.into_iter().filter(|a| !exclude.contains(a)).collect();
    maybe_download_addons(&which, addons);
}
