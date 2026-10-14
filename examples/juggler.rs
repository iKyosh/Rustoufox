//! Compare the native Juggler client with the Node.js server path: time until
//! the browser can be driven, then a first page load with the native client.
//!
//! cargo run --release --example juggler -- [URL] [runs]

use std::time::Instant;

use rustoufox::juggler::Browser;
use rustoufox::{launch_server, Headless, LaunchOptions, ServerOptions};

fn options() -> LaunchOptions {
    LaunchOptions { headless: Headless::On, os: vec!["windows".into()], ..Default::default() }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() -> rustoufox::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "https://example.com".into());
    let runs: u32 = args.next().map(|n| n.parse().expect("runs must be a number")).unwrap_or(5);

    let (mut native, mut node, mut load) = (0.0, 0.0, 0.0);
    for run in 0..runs {
        let t = Instant::now();
        let browser = Browser::launch(options())?;
        native += ms(t);
        let page = browser.new_context()?.new_page()?;
        let t = Instant::now();
        page.goto(&url)?;
        load += ms(t);
        if run == 0 {
            println!("{}: {}", browser.version(), page.evaluate("document.title")?);
        }
        browser.close()?;

        let t = Instant::now();
        let server = launch_server(options(), ServerOptions::default())?;
        node += ms(t);
        server.close()?;
    }
    let n = f64::from(runs);
    println!("ready to drive, native Juggler: {:.0} ms", native / n);
    println!("ready to drive, Node server:    {:.0} ms", node / n);
    println!("goto {url} (native):  {:.0} ms", load / n);
    Ok(())
}
