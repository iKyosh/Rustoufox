//! A private Xvfb display for `headless = "virtual"` (Linux only).

use crate::error::{Error, Result};
use crate::paths::OS_NAME;

/// Xvfb screen geometry; `CAMOUFOX_VIRTUAL_DISPLAY_SIZE` overrides it.
const DEFAULT_SCREEN: &str = "1x1x24";
const SCREEN_ENV_VAR: &str = "CAMOUFOX_VIRTUAL_DISPLAY_SIZE";
/// Setting this to 1 enables the Composite extension.
const COMPOSITE_ENV_VAR: &str = "CAMOUFOX_VIRTUAL_DISPLAY_COMPOSITE";

fn resolve_screen() -> Result<String> {
    let value = std::env::var(SCREEN_ENV_VAR).unwrap_or_default().trim().to_lowercase();
    if value.is_empty() {
        return Ok(DEFAULT_SCREEN.into());
    }
    let mut parts: Vec<&str> = value.split('x').collect();
    let ok = (parts.len() == 2 || parts.len() == 3) && parts.iter().all(|p| p.parse::<u32>().is_ok_and(|n| n > 0));
    if !ok {
        return Err(Error::VirtualDisplay(format!(
            "{SCREEN_ENV_VAR} must look like '1920x1080' or '1920x1080x24', got {value:?}"
        )));
    }
    if parts.len() == 2 {
        parts.push("24");
    }
    Ok(parts.join("x"))
}

#[derive(Debug)]
pub struct VirtualDisplay {
    pub debug: bool,
    pub screen: String,
    pub composite: bool,
    proc: Option<std::process::Child>,
    display: Option<u32>,
}

impl VirtualDisplay {
    pub fn new(debug: bool) -> Result<Self> {
        let composite = matches!(std::env::var(COMPOSITE_ENV_VAR).unwrap_or_default().trim(), "1" | "true");
        Ok(VirtualDisplay { debug, screen: resolve_screen()?, composite, proc: None, display: None })
    }

    fn xvfb_args(&self) -> Vec<String> {
        [
            "-screen",
            "0",
            &self.screen,
            "-ac",
            "-nolisten",
            "tcp",
            "-extension",
            "RENDER",
            "+extension",
            "GLX",
            if self.composite { "+extension" } else { "-extension" },
            "COMPOSITE",
            "-extension",
            "XVideo",
            "-extension",
            "XVideo-MotionCompensation",
            "-extension",
            "XINERAMA",
            "-fp",
            "built-ins",
            "-nocursor",
            "-br",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Start Xvfb if needed and return its display, e.g. ":99".
    pub fn get(&mut self) -> Result<String> {
        if OS_NAME != "lin" {
            return Err(Error::VirtualDisplay("Virtual display is only supported on Linux.".into()));
        }
        if self.proc.is_none() {
            self.start()?;
        }
        Ok(format!(":{}", self.display.unwrap_or(0)))
    }

    fn start(&mut self) -> Result<()> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let xvfb = which::which("Xvfb")
            .map_err(|_| Error::VirtualDisplay("Please install Xvfb to use headless mode.".into()))?;
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        unsafe { libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC) };

        let mut cmd = Command::new(&xvfb);
        cmd.arg("-displayfd").arg(write_fd.to_string()).args(self.xvfb_args());
        cmd.stdin(Stdio::null());
        if !self.debug {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        // Mesa software GLX: the GPU is not used anyway.
        cmd.env("__GLX_VENDOR_LIBRARY_NAME", "mesa").env("LIBGL_ALWAYS_SOFTWARE", "1");
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        if self.debug {
            eprintln!(
                "Starting virtual display: {} -displayfd {write_fd} {}",
                xvfb.display(),
                self.xvfb_args().join(" ")
            );
        }
        let spawned = cmd.spawn();
        unsafe { libc::close(write_fd) };
        let child = match spawned {
            Ok(c) => c,
            Err(e) => {
                unsafe { libc::close(read_fd) };
                return Err(Error::VirtualDisplay(format!("Cannot execute Xvfb: {e}")));
            }
        };
        self.proc = Some(child);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut buf = Vec::new();
        let result = loop {
            if buf.contains(&b'\n') {
                break Ok(());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
            let mut pfd = libc::pollfd { fd: read_fd, events: libc::POLLIN, revents: 0 };
            if remaining <= 0 || unsafe { libc::poll(&mut pfd, 1, remaining) } <= 0 {
                break Err(Error::VirtualDisplay("Xvfb did not report a display within 10000ms".into()));
            }
            let mut chunk = [0u8; 64];
            let n = unsafe { libc::read(read_fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                break Err(Error::VirtualDisplay(format!(
                    "Xvfb did not report a display (got {:?})",
                    String::from_utf8_lossy(&buf)
                )));
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        };
        unsafe { libc::close(read_fd) };
        if let Err(e) = result {
            self.kill();
            return Err(e);
        }
        match String::from_utf8_lossy(&buf).trim().parse() {
            Ok(n) => {
                self.display = Some(n);
                Ok(())
            }
            Err(_) => {
                self.kill();
                Err(Error::VirtualDisplay(format!(
                    "Xvfb wrote non-integer display: {:?}",
                    String::from_utf8_lossy(&buf)
                )))
            }
        }
    }

    /// Stop Xvfb and remove its lock and socket, whether or not it was still running.
    pub fn kill(&mut self) {
        let Some(mut proc) = self.proc.take() else { return };
        if matches!(proc.try_wait(), Ok(None)) {
            let _ = proc.kill();
        }
        let _ = proc.wait();
        if let Some(d) = self.display {
            let _ = std::fs::remove_file(format!("/tmp/.X{d}-lock"));
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{d}"));
        }
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.kill();
    }
}
