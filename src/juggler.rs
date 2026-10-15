//! A native client for Juggler, the protocol Camoufox speaks: drives the
//! browser over Firefox's remote debugging pipe, with no Node.js and no
//! Playwright.
//!
//! Experimental and deliberately small. It covers browser contexts, pages,
//! navigation, evaluation in the page's default world, screenshots, cookies
//! and basic input. It has no selectors, no auto-waiting and no network
//! interception: for those, use [`crate::launch_server`] and any Playwright
//! client.
//!
//! ```no_run
//! use rustoufox::juggler::Browser;
//! use rustoufox::{Headless, LaunchOptions};
//!
//! let browser = Browser::launch(LaunchOptions { headless: Headless::On, ..Default::default() })?;
//! let page = browser.new_context()?.new_page()?;
//! page.goto("https://example.com")?;
//! println!("{}", page.evaluate("document.title")?);
//! browser.close()?;
//! # Ok::<(), rustoufox::Error>(())
//! ```

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::launch::{launch_options, Headless, LaunchConfig, LaunchOptions};
use crate::virtdisplay::VirtualDisplay;
use crate::Proxy;

/// How long one protocol call, page load or wait may take.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

// Playwright's default Firefox profile preferences (`defaultProfilePreferences`
// in playwright-core, Apache-2.0), written to every profile before the
// launch's own preferences, as `firefox.launch()` does.
use crate::paths::data::PLAYWRIGHT_PREFS;

// ---------------------------------------------------------------------------
// Connection: framing, calls and the state events build.

/// What the event stream has told us about one page.
#[derive(Debug, Default)]
struct PageState {
    ready: bool,
    crashed: bool,
    main_frame: Option<String>,
    url: String,
    /// The default world's execution context of each frame.
    contexts: HashMap<String, String>,
    /// The navigation the main frame last committed, and how many it has.
    committed: Option<String>,
    commits: u64,
    /// Navigations whose `load` fired.
    loaded: Vec<String>,
    /// Navigations that failed, with the browser's error text.
    aborted: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    replies: HashMap<u64, std::result::Result<Value, String>>,
    /// Page target id -> session id.
    targets: HashMap<String, String>,
    /// Session id -> page state.
    pages: HashMap<String, PageState>,
    /// Set once the pipe closes.
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// One Juggler connection over any byte stream (the browser's pipe, or a test double).
pub struct Connection {
    shared: Arc<Shared>,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl Connection {
    /// Start reading messages from `reader`; calls are written to `writer`.
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Arc<Connection> {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), changed: Condvar::new() });
        let reading = shared.clone();
        std::thread::spawn(move || read_loop(BufReader::new(reader), &reading));
        Arc::new(Connection { shared, writer: Mutex::new(Box::new(writer)) })
    }

    /// Send a call and wait for its reply. `session` addresses a page.
    pub fn call(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.send(session, method, params)?;
        let reply = self.wait(timeout, method, |st| st.replies.remove(&id))?;
        reply.map_err(|e| Error::runtime(format!("{method}: {e}")))
    }

    /// Send a call without waiting for its reply; returns its id.
    fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<u64> {
        let id = {
            let mut st = self.shared.state.lock().unwrap();
            st.next_id += 1;
            st.next_id
        };
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = s.into();
        }
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(0);
        let mut w = self.writer.lock().unwrap();
        w.write_all(&bytes)?;
        w.flush()?;
        Ok(id)
    }

    /// Close our end of the pipe: Juggler takes that as Browser.close.
    fn close_writer(&self) {
        *self.writer.lock().unwrap() = Box::new(std::io::sink());
    }

    /// Wait until `f` returns a value, the pipe closes or `timeout` runs out.
    fn wait<T>(&self, timeout: Duration, what: &str, mut f: impl FnMut(&mut State) -> Option<T>) -> Result<T> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.state.lock().unwrap();
        loop {
            if let Some(v) = f(&mut st) {
                return Ok(v);
            }
            if st.closed {
                return Err(Error::runtime(format!("{what}: the browser closed the connection")));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::runtime(format!("{what}: timed out after {timeout:?}")));
            }
            st = self.shared.changed.wait_timeout(st, left).unwrap().0;
        }
    }
}

fn read_loop(mut reader: impl std::io::BufRead, shared: &Shared) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(0, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
        let mut st = shared.state.lock().unwrap();
        handle_message(&mut st, msg);
        drop(st);
        shared.changed.notify_all();
    }
    shared.state.lock().unwrap().closed = true;
    shared.changed.notify_all();
}

fn handle_message(st: &mut State, msg: Value) {
    if let Some(id) = msg["id"].as_u64() {
        let reply = match msg.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("protocol error").to_string()),
            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
        };
        st.replies.insert(id, reply);
        return;
    }
    let method = msg["method"].as_str().unwrap_or_default();
    let p = &msg["params"];
    let s = |v: &Value| v.as_str().map(str::to_string);
    match method {
        "Browser.attachedToTarget" => {
            if let (Some(session), Some(target)) = (s(&p["sessionId"]), s(&p["targetInfo"]["targetId"])) {
                st.pages.entry(session.clone()).or_default();
                st.targets.insert(target, session);
            }
            return;
        }
        "Browser.detachedFromTarget" => {
            if let Some(target) = s(&p["targetId"]) {
                st.targets.remove(&target);
            }
            if let Some(session) = s(&p["sessionId"]) {
                st.pages.remove(&session);
            }
            return;
        }
        _ => {}
    }
    let Some(page) = msg["sessionId"].as_str().and_then(|id| st.pages.get_mut(id)) else { return };
    let frame = s(&p["frameId"]);
    let is_main = frame.is_some() && frame == page.main_frame;
    match method {
        "Page.ready" => page.ready = true,
        "Page.crashed" => page.crashed = true,
        "Page.frameAttached" if p["parentFrameId"].is_null() => page.main_frame = frame,
        "Page.navigationCommitted" if is_main => {
            page.url = s(&p["url"]).unwrap_or_default();
            page.committed = s(&p["navigationId"]);
            page.commits += 1;
        }
        "Page.sameDocumentNavigation" if is_main => page.url = s(&p["url"]).unwrap_or_default(),
        "Page.navigationAborted" if is_main => {
            if let Some(nav) = s(&p["navigationId"]) {
                page.aborted.insert(nav, s(&p["errorText"]).unwrap_or_default());
            }
        }
        "Page.eventFired" if is_main && p["name"] == "load" => {
            if let Some(nav) = page.committed.clone() {
                page.loaded.push(nav);
            }
        }
        "Runtime.executionContextCreated" => {
            let name = p["auxData"]["name"].as_str().unwrap_or_default();
            if let (Some(f), Some(ctx), true) =
                (s(&p["auxData"]["frameId"]), s(&p["executionContextId"]), name.is_empty())
            {
                page.contexts.insert(f, ctx);
            }
        }
        "Runtime.executionContextDestroyed" => {
            let id = s(&p["executionContextId"]);
            page.contexts.retain(|_, c| Some(&*c) != id.as_ref());
        }
        "Runtime.executionContextsCleared" => page.contexts.clear(),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Browser, contexts and pages.

/// A running Camoufox driven over Juggler. Closing it (or dropping it) closes
/// the browser and removes its temporary profile.
pub struct Browser {
    conn: Arc<Connection>,
    child: Option<Child>,
    _profile: tempfile::TempDir,
    virtual_display: Option<VirtualDisplay>,
    launch: LaunchConfig,
    version: String,
    user_agent: String,
}

impl Browser {
    /// Build a launch from `opts` and start the browser. `headless: Virtual`
    /// starts a private Xvfb display that lives as long as the browser.
    pub fn launch(mut opts: LaunchOptions) -> Result<Browser> {
        let mut virtual_display = None;
        if opts.headless == Headless::Virtual {
            let mut vd = VirtualDisplay::new(opts.debug)?;
            opts.virtual_display = Some(vd.get()?);
            opts.headless = Headless::Off;
            virtual_display = Some(vd);
        }
        let mut browser = Self::from_config(launch_options(opts)?)?;
        browser.virtual_display = virtual_display;
        Ok(browser)
    }

    /// Start the browser from an already built launch.
    pub fn from_config(launch: LaunchConfig) -> Result<Browser> {
        if let Some(k) = launch.extra.keys().next() {
            return Err(Error::value(format!(
                "the native Juggler client does not support the Playwright launch option {k:?}; use launch_server instead"
            )));
        }
        let profile = tempfile::Builder::new().prefix("rustoufox-profile-").tempdir()?;
        let mut prefs: Map<String, Value> = serde_json::from_str(PLAYWRIGHT_PREFS)?;
        prefs.extend(launch.firefox_user_prefs.clone());
        write_user_js(profile.path(), &prefs)?;

        let mut args: Vec<String> = vec!["-no-remote".into()];
        if launch.headless {
            args.push("-headless".into());
        } else {
            args.extend(["-wait-for-browser".into(), "-foreground".into()]);
        }
        args.extend(["-profile".into(), profile.path().to_string_lossy().into_owned(), "-juggler-pipe".into()]);
        args.extend(launch.args.iter().cloned());
        args.push("-silent".into());

        let mut cmd = Command::new(&launch.executable_path);
        cmd.args(&args).env_clear().envs(&launch.env).env("MOZ_DISABLE_SAFE_MODE_KEY", "1");
        cmd.env_remove("SNAP_NAME").env_remove("SNAP_INSTANCE_NAME");
        // RUSTOUFOX_JUGGLER_DEBUG=1 shows the browser's own output.
        let debug = std::env::var_os("RUSTOUFOX_JUGGLER_DEBUG").is_some();
        let out = || if debug { Stdio::inherit() } else { Stdio::null() };
        cmd.stdin(Stdio::null()).stdout(out()).stderr(out());

        let pin =
            launch.pin_cores.and_then(|n| crate::cpu_affinity::host_cores().map(|c| crate::cpu_affinity::pick(&c, n)));
        let (child, conn) = spawn_with_pipe(cmd, pin)?;
        let mut browser = Browser {
            conn,
            child: Some(child),
            _profile: profile,
            virtual_display: None,
            launch,
            version: String::new(),
            user_agent: String::new(),
        };

        let user_prefs: Vec<Value> = prefs.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect();
        browser.call(
            "Browser.enable",
            json!({ "attachToDefaultContext": false, "userPrefs": user_prefs }),
            Duration::from_secs(60),
        )?;
        if let Some(proxy) = &browser.launch.proxy {
            browser.call("Browser.setBrowserProxy", juggler_proxy(proxy)?, DEFAULT_TIMEOUT)?;
        }
        let info = browser.call("Browser.getInfo", json!({}), DEFAULT_TIMEOUT)?;
        browser.version = info["version"].as_str().unwrap_or_default().into();
        browser.user_agent = info["userAgent"].as_str().unwrap_or_default().into();
        Ok(browser)
    }

    fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.conn.call(None, method, params, timeout)
    }

    /// The browser's version string, as Juggler reports it.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The browser's user agent.
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// The launch this browser was started with.
    pub fn launch_config(&self) -> &LaunchConfig {
        &self.launch
    }

    /// A new, empty browser context. It keeps the host's own media features
    /// and window size (no viewport emulation), as Camoufox's defaults ask.
    pub fn new_context(&self) -> Result<Context> {
        let r = self.call("Browser.createBrowserContext", json!({ "removeOnDetach": true }), DEFAULT_TIMEOUT)?;
        let id = r["browserContextId"]
            .as_str()
            .ok_or_else(|| Error::runtime("Browser.createBrowserContext returned no id"))?;
        Ok(Context { conn: self.conn.clone(), id: id.into() })
    }

    /// Close the browser, then remove its profile and virtual display.
    pub fn close(mut self) -> Result<()> {
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        // Juggler stops the pipe before answering Browser.close, so no reply
        // comes; closing our end as well is what lets the browser quit.
        let _ = self.conn.send(None, "Browser.close", json!({}));
        self.conn.close_writer();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
        if let Some(mut vd) = self.virtual_display.take() {
            vd.kill();
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// An isolated browser context: its own cookies, storage and cache.
pub struct Context {
    conn: Arc<Connection>,
    id: String,
}

impl Context {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Open a page and wait until it is ready.
    pub fn new_page(&self) -> Result<Page> {
        let r = self.conn.call(None, "Browser.newPage", json!({ "browserContextId": self.id }), DEFAULT_TIMEOUT)?;
        let target = r["targetId"].as_str().ok_or_else(|| Error::runtime("Browser.newPage returned no target"))?;
        let session = self.conn.wait(DEFAULT_TIMEOUT, "Browser.newPage", |st| {
            let session = st.targets.get(target)?;
            st.pages.get(session).filter(|p| p.ready && p.main_frame.is_some()).map(|_| session.clone())
        })?;
        Ok(Page { conn: self.conn.clone(), session, target: target.into() })
    }

    /// Every cookie of this context, in Juggler's form.
    pub fn cookies(&self) -> Result<Vec<Value>> {
        let r = self.conn.call(None, "Browser.getCookies", json!({ "browserContextId": self.id }), DEFAULT_TIMEOUT)?;
        Ok(r["cookies"].as_array().cloned().unwrap_or_default())
    }

    /// Add cookies: objects with `name`, `value` and `url` or `domain`/`path`.
    pub fn add_cookies(&self, cookies: &[Value]) -> Result<()> {
        self.conn.call(
            None,
            "Browser.setCookies",
            json!({ "browserContextId": self.id, "cookies": cookies }),
            DEFAULT_TIMEOUT,
        )?;
        Ok(())
    }

    pub fn clear_cookies(&self) -> Result<()> {
        self.conn.call(None, "Browser.clearCookies", json!({ "browserContextId": self.id }), DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// Close the context and every page in it.
    pub fn close(self) -> Result<()> {
        self.conn.call(
            None,
            "Browser.removeBrowserContext",
            json!({ "browserContextId": self.id }),
            DEFAULT_TIMEOUT,
        )?;
        Ok(())
    }
}

/// One tab.
pub struct Page {
    conn: Arc<Connection>,
    session: String,
    target: String,
}

impl Page {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.conn.call(Some(&self.session), method, params, DEFAULT_TIMEOUT)
    }

    fn main_frame(&self) -> Result<String> {
        self.conn.wait(DEFAULT_TIMEOUT, "main frame", |st| st.pages.get(&self.session)?.main_frame.clone())
    }

    pub fn target_id(&self) -> &str {
        &self.target
    }

    /// The main frame's current URL.
    pub fn url(&self) -> String {
        let st = self.conn.shared.state.lock().unwrap();
        st.pages.get(&self.session).map(|p| p.url.clone()).unwrap_or_default()
    }

    /// Navigate the main frame and wait for its `load` event.
    pub fn goto(&self, url: &str) -> Result<()> {
        let frame = self.main_frame()?;
        let commits_before = {
            let st = self.conn.shared.state.lock().unwrap();
            st.pages.get(&self.session).map_or(0, |p| p.commits)
        };
        let r = self.call("Page.navigate", json!({ "frameId": frame, "url": url }))?;
        // No navigation id: a same-document navigation, already done.
        let Some(nav) = r["navigationId"].as_str() else { return Ok(()) };
        let outcome = self.conn.wait(DEFAULT_TIMEOUT, &format!("goto {url}"), |st| {
            let page = st.pages.get(&self.session)?;
            if page.crashed {
                return Some(Err("the page crashed".to_string()));
            }
            if let Some(err) = page.aborted.get(nav) {
                return Some(Err(err.clone()));
            }
            if page.loaded.iter().any(|n| n == nav) {
                return Some(Ok(false));
            }
            // Another navigation committed in its place: an error page, or a
            // script redirect.
            let replaced = page.commits > commits_before && page.committed.as_deref() != Some(nav);
            replaced.then_some(Ok(true))
        })?;
        match outcome {
            Ok(false) => Ok(()),
            Ok(true) => {
                // Firefox shows network errors as an about:neterror page, which
                // commits as its own navigation and never fires `load`.
                let doc = self.evaluate("document.documentURI").unwrap_or_default();
                let doc = doc.as_str().unwrap_or_default();
                match doc.strip_prefix("about:neterror?e=") {
                    Some(rest) => {
                        let code = rest.split('&').next().unwrap_or(rest);
                        Err(Error::runtime(format!("goto {url}: network error ({code})")))
                    }
                    None => Err(Error::runtime(format!("goto {url}: interrupted by another navigation to {doc}"))),
                }
            }
            Err(err) => Err(Error::runtime(format!("goto {url}: {err}"))),
        }
    }

    /// Evaluate a JavaScript expression in the page's default world and
    /// return its value; promises are awaited. Camoufox runs this world apart
    /// from the page's own scripts; prefix with `mw:` (and launch with
    /// `main_world_eval`) to reach the page's globals.
    pub fn evaluate(&self, expression: &str) -> Result<Value> {
        let frame = self.main_frame()?;
        let ctx = self.conn.wait(DEFAULT_TIMEOUT, "execution context", |st| {
            st.pages.get(&self.session)?.contexts.get(&frame).cloned()
        })?;
        let r = self.call(
            "Runtime.evaluate",
            json!({ "executionContextId": ctx, "expression": expression, "returnByValue": true }),
        )?;
        if let Some(e) = r.get("exceptionDetails").filter(|e| !e.is_null()) {
            let text = e["text"].as_str().or(e["value"].as_str()).unwrap_or("exception");
            return Err(Error::runtime(format!("evaluate: {text}")));
        }
        Ok(remote_value(&r["result"]))
    }

    /// A PNG of the viewport.
    pub fn screenshot(&self) -> Result<Vec<u8>> {
        let size = self.evaluate("({ width: innerWidth, height: innerHeight })")?;
        let clip = json!({ "x": 0, "y": 0, "width": size["width"], "height": size["height"] });
        let r = self.call("Page.screenshot", json!({ "mimeType": "image/png", "clip": clip }))?;
        let data = r["data"].as_str().ok_or_else(|| Error::runtime("Page.screenshot returned no data"))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| Error::runtime(format!("Page.screenshot: {e}")))
    }

    /// Left click at viewport coordinates.
    pub fn click(&self, x: f64, y: f64) -> Result<()> {
        let ev = |kind: &str, buttons: u32| json!({ "type": kind, "button": 0, "x": x, "y": y, "modifiers": 0, "clickCount": 1, "buttons": buttons });
        self.call("Page.dispatchMouseEvent", ev("mousemove", 0))?;
        self.call("Page.dispatchMouseEvent", ev("mousedown", 1))?;
        self.call("Page.dispatchMouseEvent", ev("mouseup", 0))?;
        Ok(())
    }

    /// Insert text into the focused element, as an input method would.
    pub fn insert_text(&self, text: &str) -> Result<()> {
        self.call("Page.insertText", json!({ "text": text }))?;
        Ok(())
    }

    pub fn close(self) -> Result<()> {
        self.call("Page.close", json!({ "runBeforeUnload": false }))?;
        Ok(())
    }
}

/// The `[vendor, renderer]` WebGL reports in a bare browser (no identity,
/// WebGL forced on), or null without WebGL. The host GPU probe of
/// [`crate::host_rendering::host_gpu`].
pub(crate) fn read_host_gpu(
    executable_path: &str,
    headless: bool,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Value> {
    let mut prefs = Map::new();
    prefs.insert("webgl.force-enabled".into(), true.into());
    let launch = LaunchConfig {
        executable_path: executable_path.into(),
        args: vec![],
        env: env.clone(),
        firefox_user_prefs: prefs,
        headless,
        proxy: None,
        extra: Map::new(),
        camou_config: Map::new(),
        target_os: String::new(),
        host_identity: None,
        pin_cores: None,
        warnings: vec![],
    };
    let browser = Browser::from_config(launch)?;
    let page = browser.new_context()?.new_page()?;
    let gpu = page.evaluate(
        "(() => {
            const gl = document.createElement('canvas').getContext('webgl');
            const info = gl && gl.getExtension('WEBGL_debug_renderer_info');
            return info
                ? [gl.getParameter(info.UNMASKED_VENDOR_WEBGL), gl.getParameter(info.UNMASKED_RENDERER_WEBGL)]
                : null;
        })()",
    )?;
    browser.close()?;
    Ok(gpu)
}

/// The value of a by-value RemoteObject. JSON has no NaN, Infinity or -0:
/// those come back as their names, as strings.
fn remote_value(obj: &Value) -> Value {
    match obj["unserializableValue"].as_str() {
        Some(name) => Value::String(name.into()),
        None => obj.get("value").cloned().unwrap_or(Value::Null),
    }
}

/// Playwright's `toJugglerProxyOptions`.
fn juggler_proxy(proxy: &Proxy) -> Result<Value> {
    let (scheme, host, port) = Proxy::parse_server(&proxy.server)?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let kind = match scheme.unwrap_or_default().to_ascii_lowercase().as_str() {
        "socks5" => "socks",
        "socks4" => "socks4",
        "https" => "https",
        _ => "http",
    };
    let port = match (port.and_then(|p| p.parse::<u16>().ok()), kind) {
        (Some(p), _) => p,
        (None, "http") => 80,
        (None, "https") => 443,
        _ => return Err(Error::InvalidProxy(format!("the proxy {} needs a port", proxy.server))),
    };
    let bypass: Vec<String> =
        proxy.bypass.as_deref().map(|b| b.split(',').map(|d| d.trim().to_string()).collect()).unwrap_or_default();
    let mut out = json!({ "type": kind, "bypass": bypass, "host": host, "port": port });
    // Juggler wants these absent, not null, when the proxy has no credentials.
    for (key, value) in [("username", &proxy.username), ("password", &proxy.password)] {
        if let Some(v) = value {
            out[key] = v.as_str().into();
        }
    }
    Ok(out)
}

fn write_user_js(dir: &std::path::Path, prefs: &Map<String, Value>) -> Result<()> {
    let mut out = String::new();
    for (k, v) in prefs {
        out.push_str(&format!("user_pref({}, {});\n", Value::String(k.clone()), v));
    }
    std::fs::write(dir.join("user.js"), out)?;
    Ok(())
}

/// A pipe whose ends are close-on-exec from the start: set afterwards, a
/// child forked meanwhile by another thread would inherit them and keep the
/// pipe open past its owner's close.
pub(crate) fn cloexec_pipe() -> std::io::Result<[i32; 2]> {
    let mut fds = [0; 2];
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    // macOS has no pipe2; the window stays open there.
    #[cfg(not(target_os = "linux"))]
    let rc = unsafe {
        let rc = libc::pipe(fds.as_mut_ptr());
        if rc == 0 {
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        rc
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fds)
}

/// Spawn the browser with the Juggler pipe on fds 3 (browser reads) and 4
/// (browser writes).
fn spawn_with_pipe(mut cmd: Command, pin: Option<Vec<usize>>) -> Result<(Child, Arc<Connection>)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
        let [r, w] = cloexec_pipe()?;
        Ok(unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) })
    }

    let (browser_in, to_browser) = pipe()?;
    let (from_browser, browser_out) = pipe()?;
    let (r, w) = (browser_in.as_raw_fd(), browser_out.as_raw_fd());
    unsafe {
        cmd.pre_exec(move || {
            // Move both ends above 4 first, so placing one cannot clobber the other.
            let r2 = libc::fcntl(r, libc::F_DUPFD, 10);
            let w2 = libc::fcntl(w, libc::F_DUPFD, 10);
            if r2 < 0 || w2 < 0 || libc::dup2(r2, 3) < 0 || libc::dup2(w2, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(r2);
            libc::close(w2);
            #[cfg(target_os = "linux")]
            if let Some(cores) = &pin {
                crate::cpu_affinity::set_current(cores)?;
            }
            Ok(())
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = &pin;
    let child = cmd.spawn()?;
    drop(browser_in);
    drop(browser_out);
    let conn = Connection::new(std::fs::File::from(from_browser), std::fs::File::from(to_browser));
    Ok((child, conn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixStream;

    /// A scripted browser on the other end of a socket pair: answers each
    /// call with `respond(method, params, session)` -> (result, events after it).
    fn fake_browser(
        respond: impl Fn(&str, &Value, Option<&str>) -> (Value, Vec<Value>) + Send + 'static,
    ) -> Arc<Connection> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut out = theirs.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(theirs);
            let mut buf = Vec::new();
            while reader.read_until(0, &mut buf).unwrap_or(0) > 0 {
                buf.pop();
                let msg: Value = serde_json::from_slice(&buf).unwrap();
                buf.clear();
                let (result, events) =
                    respond(msg["method"].as_str().unwrap(), &msg["params"], msg["sessionId"].as_str());
                let mut send = |v: Value| {
                    let mut b = serde_json::to_vec(&v).unwrap();
                    b.push(0);
                    out.write_all(&b).unwrap();
                };
                send(json!({ "id": msg["id"], "result": result }));
                for e in events {
                    send(e);
                }
            }
        });
        Connection::new(ours.try_clone().unwrap(), ours)
    }

    fn page_event(method: &str, params: Value) -> Value {
        json!({ "method": method, "params": params, "sessionId": "S1" })
    }

    #[test]
    fn opens_navigates_and_evaluates() {
        let conn = fake_browser(|method, params, session| match method {
            "Browser.createBrowserContext" => (json!({ "browserContextId": "C1" }), vec![]),
            "Browser.newPage" => (
                json!({ "targetId": "T1" }),
                vec![
                    json!({ "method": "Browser.attachedToTarget", "params": {
                        "sessionId": "S1", "targetInfo": { "type": "page", "targetId": "T1", "browserContextId": "C1" } } }),
                    page_event("Page.frameAttached", json!({ "frameId": "F1" })),
                    page_event(
                        "Runtime.executionContextCreated",
                        json!({ "executionContextId": "E0", "auxData": { "frameId": "F1" } }),
                    ),
                    page_event("Page.ready", json!({})),
                ],
            ),
            "Page.navigate" => {
                assert_eq!(session, Some("S1"));
                assert_eq!(params["frameId"], "F1");
                (
                    json!({ "navigationId": "N1" }),
                    vec![
                        page_event("Runtime.executionContextDestroyed", json!({ "executionContextId": "E0" })),
                        page_event(
                            "Page.navigationCommitted",
                            json!({ "frameId": "F1", "navigationId": "N1", "url": params["url"], "name": "" }),
                        ),
                        page_event(
                            "Runtime.executionContextCreated",
                            json!({ "executionContextId": "E1", "auxData": { "frameId": "F1" } }),
                        ),
                        page_event(
                            "Runtime.executionContextCreated",
                            json!({ "executionContextId": "U1", "auxData": { "frameId": "F1", "name": "utility" } }),
                        ),
                        page_event("Page.eventFired", json!({ "frameId": "F1", "name": "load" })),
                    ],
                )
            }
            "Runtime.evaluate" => {
                assert_eq!(params["executionContextId"], "E1");
                (json!({ "result": { "type": "string", "value": "Example" } }), vec![])
            }
            other => panic!("unexpected call {other}"),
        });
        let r = conn.call(None, "Browser.createBrowserContext", json!({}), DEFAULT_TIMEOUT).unwrap();
        let ctx = Context { conn: conn.clone(), id: r["browserContextId"].as_str().unwrap().into() };
        let page = ctx.new_page().unwrap();
        page.goto("https://example.com/").unwrap();
        assert_eq!(page.url(), "https://example.com/");
        assert_eq!(page.evaluate("document.title").unwrap(), json!("Example"));
    }

    #[test]
    fn reports_aborted_navigations_and_errors() {
        let conn = fake_browser(|method, _, _| match method {
            "Page.navigate" => (
                json!({ "navigationId": "N2" }),
                vec![page_event(
                    "Page.navigationAborted",
                    json!({ "frameId": "F1", "navigationId": "N2", "errorText": "NS_ERROR_UNKNOWN_HOST" }),
                )],
            ),
            _ => (json!({}), vec![]),
        });
        {
            let mut st = conn.shared.state.lock().unwrap();
            st.pages
                .insert("S1".into(), PageState { ready: true, main_frame: Some("F1".into()), ..Default::default() });
        }
        let page = Page { conn: conn.clone(), session: "S1".into(), target: "T1".into() };
        let err = page.goto("https://nope.invalid/").unwrap_err().to_string();
        assert!(err.contains("NS_ERROR_UNKNOWN_HOST"), "{err}");
    }

    #[test]
    fn reports_network_error_pages() {
        let conn = fake_browser(|method, _, _| match method {
            "Page.navigate" => (
                json!({ "navigationId": "N3" }),
                vec![
                    page_event("Page.navigationStarted", json!({ "frameId": "F1", "navigationId": "N4" })),
                    page_event(
                        "Runtime.executionContextCreated",
                        json!({ "executionContextId": "E4", "auxData": { "frameId": "F1" } }),
                    ),
                    page_event(
                        "Page.navigationCommitted",
                        json!({ "frameId": "F1", "navigationId": "N4", "url": "http://127.0.0.1:1/", "name": "" }),
                    ),
                ],
            ),
            "Runtime.evaluate" => (
                json!({ "result": { "type": "string", "value": "about:neterror?e=deniedPortAccess&u=http%3A//127.0.0.1%3A1/" } }),
                vec![],
            ),
            _ => (json!({}), vec![]),
        });
        {
            let mut st = conn.shared.state.lock().unwrap();
            st.pages
                .insert("S1".into(), PageState { ready: true, main_frame: Some("F1".into()), ..Default::default() });
        }
        let page = Page { conn: conn.clone(), session: "S1".into(), target: "T1".into() };
        let err = page.goto("http://127.0.0.1:1/").unwrap_err().to_string();
        assert_eq!(err, "goto http://127.0.0.1:1/: network error (deniedPortAccess)");
    }

    #[test]
    fn protocol_errors_and_closed_pipes_surface() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let conn = Connection::new(ours.try_clone().unwrap(), ours);
        let mut theirs_w = theirs.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(theirs);
            let mut buf = Vec::new();
            r.read_until(0, &mut buf).unwrap();
            theirs_w.write_all(b"{\"id\":1,\"error\":{\"message\":\"boom\"}}\0").unwrap();
            // Then the browser goes away.
        });
        let err = conn.call(None, "Browser.getInfo", json!({}), DEFAULT_TIMEOUT).unwrap_err().to_string();
        assert_eq!(err, "Browser.getInfo: boom");
        reader.join().unwrap();
        let err = conn.call(None, "Browser.getInfo", json!({}), Duration::from_secs(5)).unwrap_err().to_string();
        assert!(err.contains("closed") || err.contains("Broken pipe"), "{err}");
    }

    /// A stand-in browser in Python that speaks Juggler on fds 3 and 4,
    /// so the real spawn path (pipe fds, profile, launch calls) runs.
    const FAKE_BROWSER: &str = r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
assert "-juggler-pipe" in args and "-headless" in args, args
profile = args[args.index("-profile") + 1]
assert 'user_pref("app.update.disabledForTesting", true);' in open(os.path.join(profile, "user.js")).read()
assert os.environ.get("CAMOU_CONFIG_1") == "{}", dict(os.environ)
rd, wr = os.fdopen(3, "rb"), os.fdopen(4, "wb")
def send(m):
    wr.write(json.dumps(m).encode() + b"\0"); wr.flush()
def page(method, params):
    send({"method": method, "params": params, "sessionId": "S1"})
buf = b""
while True:
    chunk = rd.read1(65536)
    if not chunk:
        break
    buf += chunk
    while b"\0" in buf:
        raw, buf = buf.split(b"\0", 1)
        m = json.loads(raw); method = m["method"]; p = m["params"]
        result = {}
        if method == "Browser.enable":
            assert any(u["name"] == "app.update.disabledForTesting" for u in p["userPrefs"])
        elif method == "Browser.getInfo":
            result = {"version": "Firefox/156.0", "userAgent": "Mozilla/5.0 test"}
        elif method == "Browser.createBrowserContext":
            result = {"browserContextId": "C1"}
        elif method == "Browser.newPage":
            send({"id": m["id"], "result": {"targetId": "T1"}})
            send({"method": "Browser.attachedToTarget", "params": {"sessionId": "S1", "targetInfo": {"type": "page", "targetId": "T1"}}})
            page("Page.frameAttached", {"frameId": "F1"})
            page("Page.ready", {})
            continue
        elif method == "Page.navigate":
            send({"id": m["id"], "result": {"navigationId": "N1"}})
            page("Page.navigationCommitted", {"frameId": "F1", "navigationId": "N1", "url": p["url"], "name": ""})
            page("Runtime.executionContextCreated", {"executionContextId": "E1", "auxData": {"frameId": "F1"}})
            page("Page.eventFired", {"frameId": "F1", "name": "load"})
            continue
        elif method == "Runtime.evaluate":
            result = {"result": {"type": "number", "value": 42}}
        elif method == "Browser.close":
            send({"id": m["id"], "result": {}})
            sys.exit(0)
        send({"id": m["id"], "result": result})
"#;

    #[test]
    fn launches_over_the_pipe() {
        if which::which("python3").is_err() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("camoufox-bin");
        std::fs::write(&exe, FAKE_BROWSER).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        env.insert("CAMOU_CONFIG_1".into(), "{}".into());
        let launch = LaunchConfig {
            executable_path: exe,
            args: vec![],
            env,
            firefox_user_prefs: Map::new(),
            headless: true,
            proxy: None,
            extra: Map::new(),
            camou_config: Map::new(),
            target_os: "lin".into(),
            host_identity: None,
            pin_cores: None,
            warnings: vec![],
        };
        let browser = Browser::from_config(launch).unwrap();
        assert_eq!(browser.version(), "Firefox/156.0");
        let page = browser.new_context().unwrap().new_page().unwrap();
        page.goto("https://example.com/").unwrap();
        assert_eq!(page.evaluate("6 * 7").unwrap(), json!(42));
        browser.close().unwrap();
    }

    #[test]
    fn proxy_matches_playwright() {
        let p = Proxy::new("socks5://127.0.0.1:1080").with_auth("u", "p");
        let v = juggler_proxy(&p).unwrap();
        assert_eq!(v["type"], "socks");
        assert_eq!(v["host"], "127.0.0.1");
        assert_eq!(v["port"], 1080);
        assert_eq!(v["username"], "u");
        let v = juggler_proxy(&Proxy::new("http://proxy.example.com")).unwrap();
        assert_eq!((v["type"].as_str(), v["port"].as_u64()), (Some("http"), Some(80)));
        assert!(v.get("username").is_none() && v.get("password").is_none(), "{v}");
        let v = juggler_proxy(&Proxy::new("https://[::1]:8443")).unwrap();
        assert_eq!((v["host"].as_str(), v["port"].as_u64()), (Some("::1"), Some(8443)));
    }
}
