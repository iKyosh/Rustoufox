//! Pin the browser to as many CPU cores as the identity reports, so a page
//! timing N parallel workers measures the number navigator.hardwareConcurrency
//! claims. The launcher pins the process it spawns (the Playwright server),
//! and the browser inherits the mask. macOS has no affinity API.

/// Whether this host can constrain a process to a subset of its cores.
pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

/// The cores this process may run on, in order.
pub fn host_cores() -> Option<Vec<usize>> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            return Some((0..libc::CPU_SETSIZE as usize).filter(|i| libc::CPU_ISSET(*i, &set)).collect());
        }
    }
    std::thread::available_parallelism().ok().map(|n| (0..n.get()).collect())
}

/// `count` adjacent cores from a random starting point (wrapping), so
/// concurrent browsers on one host do not all stack onto cores 0..count.
pub fn pick(cores: &[usize], count: usize) -> Vec<usize> {
    use rand::Rng;
    if cores.is_empty() {
        return Vec::new();
    }
    let start = rand::thread_rng().gen_range(0..cores.len());
    let mut picked: Vec<usize> = cores[start..].iter().chain(&cores[..start]).copied().take(count).collect();
    picked.sort_unstable();
    picked
}

/// Restrict the calling process to these cores (used in a child before exec).
#[cfg(target_os = "linux")]
pub fn set_current(cores: &[usize]) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for c in cores {
            libc::CPU_SET(*c, &mut set);
        }
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
