//! fpgen's model, installed from the pinned release (`fpgen-model.json`).
//!
//! The archive and each of its files are checked against their sha256 before
//! anything is written. `values.dat` is decompressed once beside the archive's
//! files and checked against its own pinned hash, so values are read with a
//! seek instead of a 200 MB decompression per process.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::paths::{data, install_dir};
use crate::pkgman::{webdl, write_atomic};

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPin {
    pub tag: String,
    pub url: String,
    pub sha256: String,
    pub file_sha256: std::collections::BTreeMap<String, String>,
    pub decompressed_sha256: std::collections::BTreeMap<String, String>,
}

pub fn pin() -> ModelPin {
    serde_json::from_str(data::FPGEN_MODEL).expect("fpgen-model.json")
}

/// Holds the archive's sha256, written last: a stamp means the install finished.
const STAMP: &str = ".pinned-model";
/// Holds values.dat's sha256 once it was decompressed and verified.
const VALUES_STAMP: &str = ".values-dat";

/// Where the model lives. `CAMOUFOX_FPGEN_DATA` overrides it, as in the
/// TypeScript launcher.
pub fn model_dir() -> PathBuf {
    std::env::var_os("CAMOUFOX_FPGEN_DATA").map(PathBuf::from).unwrap_or_else(|| install_dir().join("fpgen"))
}

static LOCK: Mutex<()> = Mutex::new(());

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn read_stamp(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Whether the pinned model's archive files are in place.
pub fn is_pinned(dir: &Path) -> bool {
    let pin = pin();
    if read_stamp(&dir.join(STAMP)).as_deref() == Some(pin.sha256.as_str()) {
        return pin.file_sha256.keys().all(|name| dir.join(name).exists());
    }
    pin.file_sha256.iter().all(|(name, digest)| sha256_file(&dir.join(name)).map(|h| &h == digest).unwrap_or(false))
}

/// Install the pinned model unless it is already there, then make sure
/// `values.dat` is decompressed. Returns the model directory.
pub fn ensure_model(force: bool) -> Result<PathBuf> {
    let dir = model_dir();
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let pin = pin();
    let installed = force || !is_pinned(&dir);
    if installed {
        install(&dir, &pin)?;
    }
    // The stamp only saves hashing the archive next time: a read-only model
    // another account installed (a container image) is fine without it.
    if read_stamp(&dir.join(STAMP)).as_deref() != Some(pin.sha256.as_str()) {
        if let Err(e) = fs::write(dir.join(STAMP), format!("{}\n", pin.sha256)) {
            if installed {
                return Err(Error::FpgenModel(format!(
                    "Cannot write fpgen's model directory {}: {e}. Install the model once as the account \
                     that owns it, e.g. with `rustoufox fetch`.",
                    dir.display()
                )));
            }
        }
    }
    decompress_values(&dir, &pin)?;
    Ok(dir)
}

fn install(dir: &Path, pin: &ModelPin) -> Result<()> {
    eprintln!("Downloading fpgen model {}: {}", pin.tag, pin.url);
    let buffer = webdl(&pin.url, None, false)?;
    let actual = hex::encode(Sha256::digest(&buffer));
    if actual != pin.sha256.to_lowercase() {
        return Err(Error::CorruptedDownload(format!(
            "Checksum mismatch for fpgen model.\n  expected sha256: {}\n  actual   sha256: {actual}\n\
             The download was corrupted or tampered with. Installation aborted.",
            pin.sha256
        )));
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(buffer))?;
    let mut names: Vec<String> = zip.file_names().map(str::to_string).collect();
    names.sort();
    let expected: Vec<String> = pin.file_sha256.keys().cloned().collect();
    if names != expected {
        return Err(Error::CorruptedDownload(format!("fpgen model archive holds {names:?}, expected {expected:?}.")));
    }
    // Members are read by name, never extracted by path, and every one is
    // verified before any is written.
    let mut members = Vec::new();
    for (name, digest) in &pin.file_sha256 {
        let mut file = zip.by_name(name)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if &hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(Error::CorruptedDownload(format!(
                "fpgen model member {name} does not match its pinned sha256."
            )));
        }
        members.push((name.clone(), bytes));
    }
    fs::create_dir_all(dir)?;
    let _ = fs::remove_file(dir.join(STAMP));
    let _ = fs::remove_file(dir.join(VALUES_STAMP));
    for (name, bytes) in members {
        write_atomic(&dir.join(name), &bytes)?;
    }
    Ok(())
}

fn decompress_values(dir: &Path, pin: &ModelPin) -> Result<()> {
    let expected = pin
        .decompressed_sha256
        .get("values.dat")
        .ok_or_else(|| Error::FpgenModel("fpgen-model.json pins no values.dat hash".into()))?;
    let dat = dir.join("values.dat");
    if dat.exists() && read_stamp(&dir.join(VALUES_STAMP)).as_deref() == Some(expected.as_str()) {
        return Ok(());
    }
    if dat.exists() && &sha256_file(&dat)? == expected {
        let _ = fs::write(dir.join(VALUES_STAMP), format!("{expected}\n"));
        return Ok(());
    }
    let src = fs::File::open(dir.join("values.dat.zst"))?;
    let tmp = dir.join(format!(".values.dat.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut decoder = zstd::stream::read::Decoder::new(src)?;
        let mut out = std::io::BufWriter::new(fs::File::create(&tmp)?);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = decoder.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        drop(out);
        let actual = hex::encode(hasher.finalize());
        if &actual != expected {
            return Err(Error::CorruptedDownload(
                "values.dat decompressed from the pinned model does not match its pinned sha256.".into(),
            ));
        }
        fs::rename(&tmp, &dat)?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    result?;
    fs::write(dir.join(VALUES_STAMP), format!("{expected}\n"))?;
    Ok(())
}

/// Decompress a zst file of the model into memory.
pub fn read_zst(path: &Path) -> Result<Vec<u8>> {
    let f = fs::File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(f)?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}
