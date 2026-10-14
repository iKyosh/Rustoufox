//! Draw identities without launching anything: an fpgen fingerprint as
//! Camoufox config keys, its WebGL device, and a per-context identity.
//!
//! cargo run --example identity -- windows

use rustoufox::fingerprints::{from_fpgen, generate_fingerprint, generate_random_font_subset, Screen};
use rustoufox::{coherence, webgl, ContextFingerprintOptions};

fn main() -> rustoufox::Result<()> {
    let os = std::env::args().nth(1).unwrap_or_else(|| "windows".into());
    let key = rustoufox::fingerprints::os_key(&os).expect("windows, macos or linux");

    let fp = generate_fingerprint(None, Some(&Screen::max(1920, 1080)), std::slice::from_ref(&os))?;
    let mut config = from_fpgen(&fp, Some("156"));
    let screen = (coherence::int(&config, "screen.width"), coherence::int(&config, "screen.height"));
    config.extend(webgl::sample_webgl_for_screen(key, screen.0, screen.1, Some(7), None)?);
    config.insert("fonts".into(), generate_random_font_subset(&os, Some(7), false, None).into());
    for violation in coherence::apply(&mut config, key) {
        println!("still incoherent: {} ({})", violation.rule, violation.detail);
    }
    for k in [
        "navigator.userAgent",
        "navigator.platform",
        "navigator.oscpu",
        "navigator.appVersion",
        "screen.width",
        "screen.height",
        "window.outerWidth",
        "window.outerHeight",
        "webGl:vendor",
        "webGl:renderer",
    ] {
        println!("{k:22} {}", config.get(k).map(|v| v.to_string()).unwrap_or_default());
    }
    println!("{:22} {} families", "fonts", config["fonts"].as_array().map(Vec::len).unwrap_or(0));

    let ctx = rustoufox::generate_context_fingerprint(&ContextFingerprintOptions {
        os: Some(os),
        ff_version: Some("156".into()),
        ..Default::default()
    })?;
    println!("\ncontext options: {}", serde_json::to_string(&ctx.context_options)?);
    println!("init script: {} lines", ctx.init_script.lines().count());
    Ok(())
}
