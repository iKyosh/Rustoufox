//! # rustoufox
//!
//! A Rust launcher for [Camoufox](https://github.com/daijro/camoufox), the
//! anti-detect Firefox build. It is a port of the official Python and
//! TypeScript launchers: on every launch it draws a complete device identity
//! from [fpgen](https://github.com/scrapfly/fingerprint-generator) (a Bayesian
//! network of real-world browser fingerprints), checks that its parts agree,
//! and hands it to the browser, which spoofs it in C++.
//!
//! ```no_run
//! use rustoufox::{launch_server, Headless, LaunchOptions, ServerOptions};
//!
//! let server = launch_server(
//!     LaunchOptions { headless: Headless::On, os: vec!["windows".into()], ..Default::default() },
//!     ServerOptions::default(),
//! )?;
//! // Connect any Playwright client: firefox.connect(server.ws_endpoint())
//! println!("{}", server.ws_endpoint());
//! # Ok::<(), rustoufox::Error>(())
//! ```
//!
//! [`launch_options`] alone builds the Playwright `firefox.launch()` options
//! without starting anything, for a Playwright you drive yourself.

#[cfg(not(unix))]
compile_error!("rustoufox runs on Linux and macOS only");

pub mod addons;
pub mod browser_pin;
pub mod coherence;
pub mod cpu_affinity;
pub mod display;
pub mod error;
pub mod fingerprints;
pub mod fpgen;
pub mod geolocation;
pub mod host_rendering;
pub mod ip;
pub mod juggler;
pub mod launch;
pub mod locales;
pub mod multiversion;
pub mod paths;
pub mod pkgman;
pub mod pycompat;
pub mod pyrandom;
pub mod server;
pub mod virtdisplay;
pub mod warnings;
pub mod webgl;

pub use addons::DefaultAddons;
pub use error::{Error, Result};
pub use fingerprints::{generate_context_fingerprint, ContextFingerprint, ContextFingerprintOptions, Screen};
pub use ip::Proxy;
pub use launch::{
    launch_options, new_context, FingerprintPreset, GeoIp, Headless, Humanize, LaunchConfig, LaunchOptions, NewContext,
    NewContextOptions,
};
pub use server::{launch_server, serve, CamoufoxServer, ServerOptions};
