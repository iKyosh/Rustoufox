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

/// Where `require.resolve` from the current directory looks for a package,
/// in Node's order: `node_modules` in the current directory and each parent,
/// then `NODE_PATH`, `~/.node_modules`, `~/.node_libraries` and
/// `<prefix>/lib/node`. Walking them here spares a Node.js start (~0.1 s).
fn node_search_dirs(node: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        for dir in cwd.ancestors() {
            if dir.file_name().is_some_and(|n| n == "node_modules") {
                continue;
            }
            dirs.push(dir.join("node_modules"));
        }
    }
    if let Some(paths) = std::env::var_os("NODE_PATH") {
        dirs.extend(std::env::split_paths(&paths).filter(|p| !p.as_os_str().is_empty()));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".node_modules"));
        dirs.push(home.join(".node_libraries"));
    }
    if let Some(prefix) = node.canonicalize().ok().and_then(|n| Some(n.parent()?.parent()?.to_path_buf())) {
        dirs.push(prefix.join("lib").join("node"));
    }
    dirs
}

/// The directory holding the Node.js executable's `bin`.
fn node_prefix(node: &Path) -> Option<PathBuf> {
    Some(node.canonicalize().ok()?.parent()?.parent()?.to_path_buf())
}

/// Where `npm root -g` usually points, without starting npm (~0.2 s): the
/// prefix from `npm_config_prefix` or `~/.npmrc`, else npm's default.
fn npm_global_roots(node: &Path) -> Vec<PathBuf> {
    let modules = |prefix: PathBuf| prefix.join("lib").join("node_modules");
    let configured = std::env::var_os("npm_config_prefix")
        .or_else(|| std::env::var_os("NPM_CONFIG_PREFIX"))
        .map(PathBuf::from)
        .or_else(|| {
            let rc = std::fs::read_to_string(dirs::home_dir()?.join(".npmrc")).ok()?;
            rc.lines().find_map(|l| {
                let (k, v) = l.split_once('=')?;
                (k.trim() == "prefix").then(|| PathBuf::from(v.trim().trim_matches('"')))
            })
        });
    if let Some(prefix) = configured {
        return vec![modules(prefix)];
    }
    node_prefix(node).map(modules).into_iter().collect()
}

/// `dir/package` if it holds a Playwright package, with symlinks resolved as Node does.
fn package_in(dir: &Path, package: &str) -> Option<PathBuf> {
    let p = dir.join(package);
    p.join("index.js").exists().then(|| p.canonicalize().unwrap_or(p))
}

fn npm_root_global() -> Option<PathBuf> {
    Command::new("npm")
        .args(["root", "-g"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
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
        let local = node_search_dirs(&node);
        let global = npm_global_roots(&node);
        // npm itself is only asked when the usual places do not hold the package.
        let npm_root = std::cell::OnceCell::new();
        ["playwright-core", "playwright"]
            .iter()
            .find_map(|pkg| {
                local.iter().chain(&global).find_map(|d| package_in(d, pkg)).or_else(|| {
                    npm_root.get_or_init(npm_root_global).as_deref().and_then(|d| package_in(d, pkg))
                })
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
            // Node exits within half a second: a short step keeps the
            // close close to that instead of rounding it up.
            std::thread::sleep(Duration::from_millis(10));
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

/// Start the Playwright server under Node.js. It loads Playwright, then
/// waits for its launch options on stdin.
fn start_node(pin: Option<&[usize]>) -> Result<(Child, ChildStdin)> {
    let pw = playwright_dir()?;
    let mut cmd = Command::new(node_path()?);
    cmd.arg(script_file(data::SERVER_JS)?).arg(&pw).current_dir(&pw);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());

    #[cfg(target_os = "linux")]
    if let Some(cores) = pin.map(<[usize]>::to_vec) {
        use std::os::unix::process::CommandExt;
        // The browser inherits the server's mask, so it runs on these cores.
        unsafe {
            cmd.pre_exec(move || crate::cpu_affinity::set_current(&cores));
        }
    }
    let mut child = cmd.spawn()?;
    let _ = &pin;

    match child.stdin.take() {
        Some(stdin) => Ok((child, stdin)),
        None => {
            kill(&mut child);
            Err(Error::runtime("no stdin for the server"))
        }
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Hand the launch to a started server and wait for its endpoint.
fn finish_server(
    mut child: Child,
    mut stdin: ChildStdin,
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

    let stdout = child.stdout.take();
    let mut server = CamoufoxServer { child, stdin: None, ws_endpoint: String::new(), launch, virtual_display };
    if let Err(e) = stdin.write_all(format!("{}\n", b64(&options)).as_bytes()).and_then(|()| stdin.flush()) {
        drop(stdin);
        server.shutdown();
        return Err(e.into());
    }
    server.stdin = Some(stdin);

    let stdout = stdout.ok_or_else(|| Error::runtime("no stdout for the server"))?;
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
            // Keep draining Node's stdout when ours is closed (`| head`):
            // Node would crash on the EPIPE otherwise.
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    });
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

fn spawn_server(
    launch: LaunchConfig,
    server: &ServerOptions,
    virtual_display: Option<VirtualDisplay>,
) -> Result<CamoufoxServer> {
    let pin =
        launch.pin_cores.and_then(|n| crate::cpu_affinity::host_cores().map(|c| crate::cpu_affinity::pick(&c, n)));
    let (child, stdin) = start_node(pin.as_deref())?;
    finish_server(child, stdin, launch, server, virtual_display)
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
    if opts.pin_cpu_cores {
        // The cores to pin depend on the drawn identity and are applied when
        // Node is spawned, so this path keeps the sequential order.
        let launch = launch_options(opts)?;
        return spawn_server(launch, &server, virtual_display);
    }
    // Node spends a while loading Playwright before it reads its options:
    // start it first and draw the identity meanwhile.
    let (mut child, stdin) = start_node(None)?;
    match launch_options(opts) {
        Ok(launch) => finish_server(child, stdin, launch, &server, virtual_display),
        Err(e) => {
            // The waiting server would otherwise wait on stdin forever.
            drop(stdin);
            kill(&mut child);
            Err(e)
        }
    }
}

/// Serve an already built launch.
pub fn serve(launch: LaunchConfig, server: ServerOptions) -> Result<CamoufoxServer> {
    spawn_server(launch, &server, None)
}

/// Open a headed browser on a page with the Playwright inspector, until it is closed.
pub fn inspect(opts: LaunchOptions, url: Option<&str>) -> Result<()> {
    let launch = launch_options(opts)?;
    let payload = json!({ "launch": launch.to_playwright(), "url": url, "contextOptions": Value::Object(launch.context_defaults()) });
    let mut child = Command::new(node_path()?)
        .arg(script_file(data::INSPECT_JS)?)
        .arg(playwright_dir()?)
        .stdin(Stdio::piped())
        .spawn()?;
    let written =
        child.stdin.take().map(|mut stdin| stdin.write_all(&serde_json::to_vec(&payload)?).map_err(Error::from));
    let status = child.wait()?;
    if let Some(Err(e)) = written {
        return Err(e);
    }
    if !status.success() {
        return Err(Error::runtime(format!("the inspector exited with {status}")));
    }
    Ok(())
}
