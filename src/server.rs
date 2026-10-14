//! Running the browser: Camoufox is driven through Playwright's Juggler
//! protocol, and Playwright has no Rust client, so the launcher starts
//! Playwright's own browser server (`firefox.launchServer`) under Node.js and
//! hands back its websocket endpoint. Any Playwright client (Python, Node,
//! Java, .NET, or a Rust crate speaking the Playwright protocol) connects to
//! it with `firefox.connect(endpoint)`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::launch::{launch_options, Headless, LaunchConfig, LaunchOptions};
use crate::paths::{data, install_dir};
use crate::virtdisplay::VirtualDisplay;

/// The Node.js executable: `RUSTOUFOX_NODE`, or `node` on the PATH.
pub fn node_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("RUSTOUFOX_NODE") {
        return Ok(PathBuf::from(p));
    }
    which::which("node").map_err(|_| {
        Error::runtime("Node.js was not found. Install Node.js 18+ or set RUSTOUFOX_NODE to its executable.")
    })
}

fn resolve_from(node: &Path, cwd: Option<&Path>, package: &str) -> Option<PathBuf> {
    let script = format!("process.stdout.write(require('path').dirname(require.resolve('{package}/package.json')))");
    let mut cmd = Command::new(node);
    cmd.arg("-e").arg(script).stderr(Stdio::null());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().ok()?;
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    (out.status.success() && dir.join("index.js").exists()).then_some(dir)
}

/// The directory of a Playwright package (`playwright-core` or `playwright`):
/// `RUSTOUFOX_PLAYWRIGHT_CORE`, else resolved from the current directory, else
/// from the global npm root.
pub fn playwright_dir() -> Result<PathBuf> {
    static CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);
    if let Some(p) = CACHE.lock().unwrap().clone() {
        return Ok(p);
    }
    let found = if let Some(p) = std::env::var_os("RUSTOUFOX_PLAYWRIGHT_CORE") {
        let p = PathBuf::from(p);
        if !p.join("index.js").exists() {
            return Err(Error::runtime(format!("RUSTOUFOX_PLAYWRIGHT_CORE={} holds no index.js", p.display())));
        }
        p
    } else {
        let node = node_path()?;
        let global = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
            .args(["root", "-g"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
        ["playwright-core", "playwright"]
            .iter()
            .find_map(|pkg| {
                resolve_from(&node, None, pkg)
                    .or_else(|| global.as_ref().map(|g| g.join(pkg)).filter(|d| d.join("index.js").exists()))
            })
            .ok_or_else(|| {
                Error::runtime(
                    "playwright-core was not found. Install it (`npm install playwright-core@\"<1.63\"`, here or with -g) \
                     or set RUSTOUFOX_PLAYWRIGHT_CORE to its directory.",
                )
            })?
    };
    *CACHE.lock().unwrap() = Some(found.clone());
    Ok(found)
}

/// The version of the Playwright package that will drive the browser, if one is found.
pub fn playwright_version() -> Option<String> {
    static CACHE: Mutex<Option<Option<String>>> = Mutex::new(None);
    if let Some(v) = CACHE.lock().unwrap().clone() {
        return v;
    }
    let v = playwright_dir()
        .ok()
        .and_then(|d| std::fs::read(d.join("package.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|p| p["version"].as_str().map(str::to_string));
    *CACHE.lock().unwrap() = Some(v.clone());
    v
}

/// Write a bundled script under the install directory (once per content).
fn script_file(script: &str) -> Result<PathBuf> {
    let dir = install_dir().join("scripts");
    std::fs::create_dir_all(&dir)?;
    let hash = hex::encode(Sha256::digest(script.as_bytes()));
    let path = dir.join(format!("{}.js", &hash[..16]));
    if !path.exists() {
        crate::pkgman::write_atomic(&path, script.as_bytes())?;
    }
    Ok(path)
}

fn b64(value: &Value) -> String {
    base64::engine::general_purpose::STANDARD.encode(value.to_string())
}

/// Run a bundled script with the Playwright directory and base64 JSON options; returns its stdout.
pub fn run_node_script(script: &str, options: &Value) -> Result<String> {
    let out = Command::new(node_path()?)
        .arg(script_file(script)?)
        .arg(playwright_dir()?)
        .arg(b64(options))
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(Error::runtime(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Where the server listens.
#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Port to listen on; a free one when None.
    pub port: Option<u16>,
    /// Path of the websocket endpoint, e.g. "camoufox".
    pub ws_path: Option<String>,
    /// Interface to bind, e.g. "127.0.0.1".
    pub host: Option<String>,
}

/// A running Camoufox behind a Playwright server. Dropping it closes the browser.
pub struct CamoufoxServer {
    child: Child,
    stdin: Option<ChildStdin>,
    ws_endpoint: String,
    launch: LaunchConfig,
    virtual_display: Option<VirtualDisplay>,
}

impl std::fmt::Debug for CamoufoxServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CamoufoxServer").field("ws_endpoint", &self.ws_endpoint).finish()
    }
}

impl CamoufoxServer {
    /// The websocket endpoint to `firefox.connect()` to.
    pub fn ws_endpoint(&self) -> &str {
        &self.ws_endpoint
    }

    /// The launch the browser runs with (identity, prefs, warnings).
    pub fn launch_config(&self) -> &LaunchConfig {
        &self.launch
    }

    /// Block until the server exits.
    pub fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait()?)
    }

    /// The server's exit status, if it has exited.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Close the browser and the server.
    pub fn close(mut self) -> Result<()> {
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        // Closing stdin asks the server to close the browser cleanly, which
        // also removes its temporary profile.
        self.stdin.take();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(mut vd) = self.virtual_display.take() {
            vd.kill();
        }
    }
}

impl Drop for CamoufoxServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_server(
    launch: LaunchConfig,
    server: &ServerOptions,
    virtual_display: Option<VirtualDisplay>,
) -> Result<CamoufoxServer> {
    let mut options = launch.to_playwright();
    if let Some(port) = server.port {
        options["port"] = json!(port);
    }
    if let Some(path) = &server.ws_path {
        options["wsPath"] = json!(if path.starts_with('/') { path.clone() } else { format!("/{path}") });
    }
    if let Some(host) = &server.host {
        options["host"] = json!(host);
    }

    let pw = playwright_dir()?;
    let mut cmd = Command::new(node_path()?);
    cmd.arg(script_file(data::SERVER_JS)?).arg(&pw).current_dir(&pw);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());

    let pin =
        launch.pin_cores.and_then(|n| crate::cpu_affinity::host_cores().map(|c| crate::cpu_affinity::pick(&c, n)));
    #[cfg(target_os = "linux")]
    if let Some(cores) = pin.clone() {
        use std::os::unix::process::CommandExt;
        // The browser inherits the server's mask, so it runs on these cores.
        unsafe {
            cmd.pre_exec(move || crate::cpu_affinity::set_current(&cores));
        }
    }
    let mut child = cmd.spawn()?;
    #[cfg(target_os = "windows")]
    if let Some(cores) = &pin {
        crate::cpu_affinity::set_process(child.id(), cores);
    }
    let _ = &pin;

    let mut stdin = child.stdin.take().ok_or_else(|| Error::runtime("no stdin for the server"))?;
    stdin.write_all(format!("{}\n", b64(&options)).as_bytes())?;
    stdin.flush()?;

    let stdout = child.stdout.take().ok_or_else(|| Error::runtime("no stdout for the server"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
            if !sent {
                if let Some(ep) =
                    serde_json::from_str::<Value>(&line).ok().and_then(|v| v["wsEndpoint"].as_str().map(str::to_string))
                {
                    let _ = tx.send(ep);
                    sent = true;
                    continue;
                }
            }
            println!("{line}");
        }
    });
    let mut server = CamoufoxServer { child, stdin: Some(stdin), ws_endpoint: String::new(), launch, virtual_display };
    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(ep) => {
            server.ws_endpoint = ep;
            Ok(server)
        }
        Err(_) => {
            let status = server.child.try_wait().ok().flatten();
            server.shutdown();
            Err(Error::runtime(format!(
                "The Playwright server did not report an endpoint{}",
                status.map(|s| format!(" (exited with {s})")).unwrap_or_default()
            )))
        }
    }
}

/// Launch Camoufox behind a Playwright server and return once it listens.
/// Takes the same options as [`launch_options`]; `headless: Virtual` starts a
/// private Xvfb display that lives as long as the server.
pub fn launch_server(mut opts: LaunchOptions, server: ServerOptions) -> Result<CamoufoxServer> {
    if opts.extra.contains_key("userDataDir") {
        return Err(Error::value(
            "launch_server() does not support userDataDir: Playwright cannot serve a persistent context over a websocket endpoint.",
        ));
    }
    let mut virtual_display = None;
    if opts.headless == Headless::Virtual {
        let mut vd = VirtualDisplay::new(opts.debug)?;
        opts.virtual_display = Some(vd.get()?);
        opts.headless = Headless::Off;
        virtual_display = Some(vd);
    }
    let launch = launch_options(opts)?;
    spawn_server(launch, &server, virtual_display)
}

/// Serve an already built launch.
pub fn serve(launch: LaunchConfig, server: ServerOptions) -> Result<CamoufoxServer> {
    spawn_server(launch, &server, None)
}

/// Open a headed browser on a page with the Playwright inspector, until it is closed.
pub fn inspect(opts: LaunchOptions, url: Option<&str>) -> Result<()> {
    let launch = launch_options(opts)?;
    let payload = json!({ "launch": launch.to_playwright(), "url": url, "contextOptions": Value::Object(launch.context_defaults()) });
    let status = Command::new(node_path()?)
        .arg(script_file(data::INSPECT_JS)?)
        .arg(playwright_dir()?)
        .arg(b64(&payload))
        .status()?;
    if !status.success() {
        return Err(Error::runtime(format!("the inspector exited with {status}")));
    }
    Ok(())
}
