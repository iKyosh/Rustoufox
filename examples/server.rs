//! Launch Camoufox behind a Playwright server and print the endpoint to
//! connect a Playwright client to. Stops on Enter.
//!
//! cargo run --example server
//! node -e "require('playwright-core').firefox.connect(process.argv[1]).then(async b => {
//!   const p = await b.newPage({ viewport: null }); await p.goto('https://example.com');
//!   console.log(await p.evaluate(() => navigator.userAgent)); await b.close() })" <endpoint>

use rustoufox::{launch_server, Headless, LaunchOptions, ServerOptions};

fn main() -> rustoufox::Result<()> {
    let server = launch_server(
        LaunchOptions { headless: Headless::On, os: vec!["windows".into(), "macos".into()], ..Default::default() },
        ServerOptions { port: Some(9222), ws_path: Some("camoufox".into()), ..Default::default() },
    )?;
    let launch = server.launch_config();
    println!("endpoint:  {}", server.ws_endpoint());
    println!("identity:  {}", launch.camou_config["navigator.userAgent"]);
    println!("contexts:  {}", serde_json::to_string(&launch.context_defaults())?);
    println!("Press Enter to stop.");
    std::io::stdin().read_line(&mut String::new())?;
    server.close()
}
