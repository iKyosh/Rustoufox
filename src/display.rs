//! Host display geometry, in the CSS pixels Firefox lays its windows out in.

use std::collections::BTreeMap;

use crate::paths::OS_NAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplaySize {
    pub width: i64,
    pub height: i64,
}

/// Whether there is a desktop session for the window to open on. Only Linux
/// can lack one.
pub fn has_display(env: &BTreeMap<String, String>) -> bool {
    if OS_NAME != "lin" {
        return true;
    }
    env.get("DISPLAY").is_some_and(|v| !v.is_empty()) || env.get("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

/// The roomiest attached monitor, or None when it cannot be probed.
pub fn largest_display(env: &BTreeMap<String, String>) -> Option<DisplaySize> {
    #[cfg(target_os = "windows")]
    {
        let _ = env;
        windows_primary()
    }
    #[cfg(target_os = "macos")]
    {
        let _ = env;
        macos_displays()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        xrandr(env)
    }
}

/// Parse `xrandr --current`: every connected output's mode, e.g. "1920x1080+0+0".
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn xrandr(env: &BTreeMap<String, String>) -> Option<DisplaySize> {
    let out = std::process::Command::new("xrandr").arg("--current").env_clear().envs(env).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_xrandr(&String::from_utf8_lossy(&out.stdout))
}

#[cfg_attr(any(target_os = "windows", target_os = "macos"), allow(dead_code))]
fn parse_xrandr(text: &str) -> Option<DisplaySize> {
    let re = regex::Regex::new(r" connected (?:primary )?(\d+)x(\d+)\+\d+\+\d+").unwrap();
    re.captures_iter(text)
        .filter_map(|c| Some(DisplaySize { width: c[1].parse().ok()?, height: c[2].parse().ok()? }))
        .max_by_key(|d| d.width * d.height)
        .map(|d| DisplaySize { width: d.width.max(1), height: d.height.max(1) })
}

/// The primary monitor as a DPI-unaware process sees it: already scaled to CSS pixels.
#[cfg(target_os = "windows")]
fn windows_primary() -> Option<DisplaySize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    (w > 0 && h > 0).then_some(DisplaySize { width: w as i64, height: h as i64 })
}

/// macOS reports points, which are CSS pixels: "UI Looks like: 1512 x 982".
#[cfg(target_os = "macos")]
fn macos_displays() -> Option<DisplaySize> {
    let out = std::process::Command::new("system_profiler").arg("SPDisplaysDataType").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let looks = regex::Regex::new(r"UI Looks like: (\d+) x (\d+)").unwrap();
    let resolution = regex::Regex::new(r"Resolution: (\d+) x (\d+)").unwrap();
    let re = if looks.is_match(&text) { looks } else { resolution };
    re.captures_iter(&text)
        .filter_map(|c| Some(DisplaySize { width: c[1].parse().ok()?, height: c[2].parse().ok()? }))
        .max_by_key(|d| d.width * d.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrandr_output() {
        let text = "Screen 0: minimum 8 x 8\nHDMI-1 connected primary 1920x1080+0+0 (normal)\nDP-1 connected 2560x1440+1920+0\nDP-2 disconnected\n";
        assert_eq!(parse_xrandr(text), Some(DisplaySize { width: 2560, height: 1440 }));
    }
}
