//! Time the launcher's own work: drawing a fingerprint and building a whole
//! launch, averaged over N calls after a first, cold one. Starts no browser.
//!
//! cargo run --release --example bench -- /path/to/camoufox-bin 50

use std::time::Instant;

use rustoufox::fingerprints::{from_fpgen, generate_fingerprint};
use rustoufox::{launch_options, DefaultAddons, Headless, LaunchOptions};

/// Peak resident memory of this process, where the OS reports it.
fn max_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status.lines().find(|l| l.starts_with("VmHWM:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

fn main() -> rustoufox::Result<()> {
    let mut args = std::env::args().skip(1);
    let exe = args.next().expect("usage: bench <camoufox executable> [calls]");
    let n: u32 = args.next().map(|n| n.parse().expect("calls must be a number")).unwrap_or(50);
    let options = || LaunchOptions {
        os: vec!["windows".into()],
        headless: Headless::On,
        executable_path: Some(exe.clone().into()),
        i_know_what_im_doing: true,
        exclude_addons: vec![DefaultAddons::Ubo],
        ..Default::default()
    };

    let t = Instant::now();
    launch_options(options())?;
    let first = t.elapsed();

    let t = Instant::now();
    for _ in 0..n {
        launch_options(options())?;
    }
    let launch = t.elapsed() / n;

    let t = Instant::now();
    for _ in 0..n {
        from_fpgen(&generate_fingerprint(None, None, std::slice::from_ref(&"windows".to_string()))?, Some("156"));
    }
    let generate = t.elapsed() / n;

    println!("first launch_options (cold): {:.0} ms", first.as_secs_f64() * 1e3);
    println!("launch_options:              {:.1} ms", launch.as_secs_f64() * 1e3);
    println!("generate fingerprint:        {:.1} ms", generate.as_secs_f64() * 1e3);
    if let Some(mb) = max_rss_mb() {
        println!("peak memory:                 {mb:.0} MB");
    }
    Ok(())
}
