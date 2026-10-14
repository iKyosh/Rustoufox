//! Proxies, IP validation, and finding the public (or proxy exit) IP.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A proxy in Playwright's format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proxy {
    pub server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bypass: Option<String>,
}

fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Proxy {
    pub fn new(server: impl Into<String>) -> Self {
        Proxy { server: server.into(), ..Default::default() }
    }

    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// (schema, host, port) of the server string.
    pub fn parse_server(server: &str) -> Result<(Option<String>, String, Option<String>)> {
        let re = regex::Regex::new(r"^(?:(?P<schema>\w+)://)?(?P<url>.*?)(?::(?P<port>\d+))?$").unwrap();
        let c = re.captures(server).ok_or_else(|| Error::InvalidProxy(format!("Invalid proxy server: {server}")))?;
        Ok((
            c.name("schema").map(|m| m.as_str().to_string()),
            c["url"].to_string(),
            c.name("port").map(|m| m.as_str().to_string()),
        ))
    }

    /// The proxy as a URL, credentials percent-encoded.
    pub fn as_url(&self) -> Result<String> {
        let (schema, url, port) = Self::parse_server(&self.server)?;
        let mut out = format!("{}://", schema.unwrap_or_else(|| "http".into()));
        if let Some(user) = self.username.as_deref().filter(|u| !u.is_empty()) {
            out.push_str(&quote(user));
            if let Some(pass) = self.password.as_deref().filter(|p| !p.is_empty()) {
                out.push(':');
                out.push_str(&quote(pass));
            }
            out.push('@');
        }
        out.push_str(&url);
        if let Some(port) = port {
            out.push(':');
            out.push_str(&port);
        }
        Ok(out)
    }
}

pub fn valid_ipv4(ip: &str) -> bool {
    regex::Regex::new(r"^(?:[0-9]{1,3}\.){3}[0-9]{1,3}$").unwrap().is_match(ip)
}

pub fn valid_ipv6(ip: &str) -> bool {
    regex::Regex::new(r"^(([0-9a-fA-F]{0,4}:){1,7}[0-9a-fA-F]{0,4})$").unwrap().is_match(ip)
}

pub fn validate_ip(ip: &str) -> Result<()> {
    if valid_ipv4(ip) || valid_ipv6(ip) {
        Ok(())
    } else {
        Err(Error::InvalidIp(format!("Invalid IP address: {ip}")))
    }
}

fn client(proxy: Option<&str>, timeout: u64) -> Result<reqwest::blocking::Client> {
    let mut b = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .user_agent(concat!("rustoufox/", env!("CARGO_PKG_VERSION")));
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| Error::InvalidProxy(e.to_string()))?);
    }
    Ok(b.build()?)
}

pub const PROXY_LOOKUP_FAILED: &str =
    "Could not look up the proxy's exit IP and timezone. Pass webrtc_ip and timezone_id explicitly to skip the lookup";

/// The exit IP of a proxy and that IP's timezone, looked up through the proxy.
pub fn proxy_exit_geo(proxy_url: &str) -> Result<(String, String)> {
    let fail = |e: String| Error::InvalidIp(format!("{PROXY_LOOKUP_FAILED}: {e}"));
    let data: serde_json::Value = client(Some(proxy_url), 10)?
        .get("http://ip-api.com/json?fields=status,message,query,timezone")
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| fail(e.to_string()))?;
    let tz = data["timezone"].as_str().filter(|t| !t.is_empty());
    if data["status"].as_str() != Some("success") || tz.is_none() {
        return Err(fail(data["message"].as_str().map(str::to_string).unwrap_or_else(|| data.to_string())));
    }
    let ip = data["query"].as_str().unwrap_or("").to_string();
    validate_ip(&ip)?;
    Ok((ip, tz.unwrap().to_string()))
}

/// The public IP, through `proxy` when given. Cached per proxy.
pub fn public_ip(proxy: Option<&str>) -> Result<String> {
    static CACHE: Mutex<Option<HashMap<Option<String>, String>>> = Mutex::new(None);
    let key = proxy.map(str::to_string);
    if let Some(ip) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return Ok(ip.clone());
    }
    const URLS: [&str; 6] = [
        "https://api.ipify.org",
        "https://checkip.amazonaws.com",
        "https://ipinfo.io/ip",
        "https://icanhazip.com",
        "https://ifconfig.co/ip",
        "https://ipecho.net/plain",
    ];
    let client = client(proxy, 5)?;
    let mut last = String::new();
    for url in URLS {
        let result = client
            .get(url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
            .map_err(|e| e.to_string())
            .and_then(|t| {
                let ip = t.trim().to_string();
                validate_ip(&ip).map(|_| ip).map_err(|e| e.to_string())
            });
        match result {
            Ok(ip) => {
                CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, ip.clone());
                return Ok(ip);
            }
            Err(e) => last = e,
        }
    }
    Err(Error::InvalidIp(format!("Failed to get IP address: {last}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url() {
        let p = Proxy::new("proxy.example.com:8080").with_auth("us#er", "p@ss/%41");
        assert_eq!(p.as_url().unwrap(), "http://us%23er:p%40ss%2F%2541@proxy.example.com:8080");
        assert_eq!(Proxy::new("socks5://1.2.3.4:1080").as_url().unwrap(), "socks5://1.2.3.4:1080");
    }

    #[test]
    fn ips() {
        assert!(valid_ipv4("1.2.3.4"));
        assert!(!valid_ipv4("1.2.3"));
        assert!(valid_ipv6("2001:db8::1"));
    }
}
