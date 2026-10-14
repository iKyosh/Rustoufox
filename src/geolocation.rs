//! Geolocation, timezone and locale for an IP, from a GeoIP database
//! (repos.yml's `geoip` sources), refreshed once its data is a week old.

use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::ip::validate_ip;
use crate::locales::{selector, Geolocation};
use crate::paths::{data, install_dir};
use crate::pkgman::{unzip, webdl, write_atomic};
use crate::warnings;

const UPDATE_DAYS: f64 = 8.0;
const RECHECK_DAYS: f64 = 1.0;
const FROZEN_DAYS: f64 = 30.0;

pub fn geoip_dir() -> PathBuf {
    install_dir().join("geoip")
}

fn mmdb_dir() -> PathBuf {
    geoip_dir().join("mmdb")
}

fn config_path() -> PathBuf {
    geoip_dir().join("config.yml")
}

fn find_in<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    let mut v = data;
    for part in key.split('.') {
        v = v.get(part)?;
        if v.is_null() {
            return None;
        }
    }
    Some(v)
}

/// The configured GeoIP sources and the default's name.
pub fn geoip_repos() -> (Vec<Value>, String) {
    let data: Value = serde_yaml::from_str(data::REPOS_YML).expect("repos.yml");
    (
        data["geoip"].as_array().cloned().unwrap_or_default(),
        data["default"]["geoip"].as_str().unwrap_or("GeoIP AIO by daijro").to_string(),
    )
}

/// A source by name, or the default.
pub fn source_by_name(name: Option<&str>) -> Result<Value> {
    let (repos, default) = geoip_repos();
    let target = name.unwrap_or(&default).to_lowercase();
    let validate = |r: &Value| -> Result<Value> {
        if r.get("urls").is_none() || r.get("paths").is_none() {
            return Err(Error::value(format!("GeoIP repo '{}' missing required urls or paths", r["name"])));
        }
        Ok(r.clone())
    };
    if let Some(r) = repos.iter().find(|r| r["name"].as_str().unwrap_or("").to_lowercase() == target) {
        return validate(r);
    }
    if let Some(name) = name {
        let available: Vec<&str> = repos.iter().filter_map(|r| r["name"].as_str()).collect();
        return Err(Error::value(format!("GeoIP database '{name}' not found. Available: {available:?}")));
    }
    repos.first().map(validate).unwrap_or_else(|| Err(Error::value("No GeoIP repos configured in repos.yml")))
}

pub fn warn_if_deprecated(source: &Value) {
    if source["deprecated"].as_bool().unwrap_or(false) {
        let (_, default) = geoip_repos();
        warnings::emit(
            warnings::WarningKind::Future,
            warnings::text("geoip_deprecated")
                .replace("{name}", source["name"].as_str().unwrap_or(""))
                .replace("{default}", &default),
        );
    }
}

fn saved_config() -> Option<Value> {
    serde_yaml::from_str(&fs::read_to_string(config_path()).ok()?).ok()
}

/// The active source: the saved choice, or the default.
pub fn load_geoip_config() -> Result<Value> {
    if let Some(saved) = saved_config() {
        let Ok(source) = source_by_name(saved["name"].as_str()) else { return Ok(saved) };
        if source["deprecated"].as_bool().unwrap_or(false) && !saved["explicit"].as_bool().unwrap_or(false) {
            return source_by_name(None);
        }
        return Ok(source);
    }
    source_by_name(None)
}

pub fn save_geoip_config(source: &Value, explicit: bool) -> Result<()> {
    fs::create_dir_all(geoip_dir())?;
    let mut saved = serde_json::Map::new();
    saved.insert("name".into(), source["name"].clone());
    if explicit {
        saved.insert("explicit".into(), true.into());
    }
    fs::write(config_path(), serde_yaml::to_string(&saved)?)?;
    Ok(())
}

pub fn mmdb_path(ip_version: &str, source: &Value) -> PathBuf {
    let name = source["name"].as_str().unwrap_or("geolite2").to_lowercase();
    if source["urls"].get("combined").is_some() {
        mmdb_dir().join(format!("{name}-combined.mmdb"))
    } else {
        mmdb_dir().join(format!("{name}-{ip_version}.mmdb"))
    }
}

fn build_age_days(path: &std::path::Path) -> Option<f64> {
    let reader = maxminddb::Reader::open_readfile(path).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    Some((now - reader.metadata.build_epoch as f64) / 86400.0)
}

fn chosen_explicitly(source: &Value) -> bool {
    saved_config().is_some_and(|s| s["explicit"].as_bool().unwrap_or(false) && s["name"] == source["name"])
}

/// Download a source's database(s). A named source becomes the explicit choice unless `activate` is false.
pub fn download_mmdb(name: Option<&str>, activate: bool) -> Result<()> {
    let source = match name {
        Some(n) => source_by_name(Some(n))?,
        None => load_geoip_config()?,
    };
    let source_name = source["name"].as_str().unwrap_or("").to_string();
    let lower = source_name.to_lowercase();
    let extract = source["extract"].as_bool().unwrap_or(false);
    let urls = source["urls"].as_object().cloned().unwrap_or_default();
    let combined = urls.contains_key("combined");
    fs::create_dir_all(mmdb_dir())?;

    for (ip_ver, url_list) in &urls {
        let list: Vec<String> = match url_list {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        };
        let suffix = if combined { String::new() } else { format!(" ({ip_ver})") };
        let path = mmdb_dir().join(format!("{lower}-{ip_ver}.mmdb"));
        let mut last_error = None;
        let mut done = false;
        for url in &list {
            let attempt = (|| -> Result<()> {
                let bytes = webdl(url, Some(&format!("Downloading {source_name}{suffix}")), true)?;
                if extract {
                    let tmp = tempfile::tempdir()?;
                    unzip(&bytes, tmp.path(), None)?;
                    let found = walk_mmdb(tmp.path()).ok_or_else(|| Error::value("No .mmdb file found in archive"))?;
                    write_atomic(&path, &fs::read(found)?)?;
                } else {
                    write_atomic(&path, &bytes)?;
                }
                Ok(())
            })();
            match attempt {
                Ok(()) => {
                    done = true;
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
        if !done {
            return Err(last_error.unwrap_or_else(|| Error::runtime(format!("Failed to download {ip_ver}"))));
        }
        if let Some(age) = build_age_days(&path).filter(|a| *a > FROZEN_DAYS) {
            warnings::runtime(
                warnings::text("geoip_frozen")
                    .replace("{name}", &source_name)
                    .replace("{days}", &(age as i64).to_string()),
            );
        }
    }

    // Delete databases of deprecated sources other than this one.
    let (repos, _) = geoip_repos();
    for repo in repos {
        let n = repo["name"].as_str().unwrap_or("");
        if !repo["deprecated"].as_bool().unwrap_or(false) || n == source_name {
            continue;
        }
        let prefix = format!("{}-", n.to_lowercase());
        for entry in fs::read_dir(mmdb_dir())?.flatten() {
            let f = entry.file_name().to_string_lossy().to_string();
            if f.starts_with(&prefix) && f.ends_with(".mmdb") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if activate {
        save_geoip_config(&source, name.is_some() || chosen_explicitly(&source))?;
    }
    Ok(())
}

fn walk_mmdb(dir: &std::path::Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = walk_mmdb(&p) {
                return Some(found);
            }
        } else if p.extension().is_some_and(|e| e == "mmdb") {
            return Some(p);
        }
    }
    None
}

/// Whether the database's data is over a week old and was last checked over a day ago.
pub fn needs_update(source: &Value) -> bool {
    let path = mmdb_path("ipv4", source);
    let Ok(meta) = fs::metadata(&path) else { return true };
    let checked_days =
        meta.modified().ok().and_then(|m| m.elapsed().ok()).map(|d| d.as_secs_f64() / 86400.0).unwrap_or(f64::MAX);
    if checked_days < RECHECK_DAYS {
        return false;
    }
    build_age_days(&path).is_none_or(|d| d > UPDATE_DAYS)
}

/// The geolocation of an IP.
pub fn get_geolocation(ip: &str, geoip_db: Option<&str>) -> Result<Geolocation> {
    validate_ip(ip)?;
    let version = if ip.contains(':') { "ipv6" } else { "ipv4" };
    let source = match geoip_db {
        Some(n) => source_by_name(Some(n))?,
        None => load_geoip_config()?,
    };
    warn_if_deprecated(&source);
    let path = mmdb_path(version, &source);
    if !path.exists() || needs_update(&source) {
        download_mmdb(geoip_db, geoip_db.is_none())?;
    }
    let reader = maxminddb::Reader::open_readfile(&path)?;
    let addr: IpAddr = ip.parse().map_err(|_| Error::InvalidIp(format!("Invalid IP address: {ip}")))?;
    let resp: Value = match reader.lookup::<Value>(addr) {
        Ok(v) => v,
        Err(maxminddb::MaxMindDBError::AddressNotFoundError(_)) => {
            return Err(Error::UnknownIpLocation(format!("IP not found in database: {ip}")))
        }
        Err(e) => return Err(e.into()),
    };
    let paths = &source["paths"];
    let get = |k: &str| paths[k].as_str().and_then(|p| find_in(&resp, p));
    let missing = |k: &str| Error::UnknownIpLocation(format!("The database has no {k} for {ip}"));
    let iso = get("iso_code").and_then(Value::as_str).ok_or_else(|| missing("country"))?.to_uppercase();
    let longitude = get("longitude").and_then(Value::as_f64).ok_or_else(|| missing("longitude"))?;
    let latitude = get("latitude").and_then(Value::as_f64).ok_or_else(|| missing("latitude"))?;
    let timezone = get("timezone").and_then(Value::as_str).ok_or_else(|| missing("timezone"))?.to_string();
    Ok(Geolocation { locale: selector().from_region(&iso)?, longitude, latitude, timezone, accuracy: None })
}
