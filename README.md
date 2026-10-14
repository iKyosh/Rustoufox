# rustoufox

A Rust launcher for [Camoufox](https://github.com/daijro/camoufox), the
anti-detect Firefox build. It ports the official Python launcher (`camoufox`
on PyPI, tracking the repository's `main` branch) to Rust: one binary and one
library that fetch the browser, draw a coherent device identity on every
launch, and start the browser for any Playwright client.

The browser itself is not rebuilt here. Camoufox spoofs fingerprints in C++
inside its Firefox build; a launcher's job is to pick the identity, check that
its parts agree, and hand it to the browser through `CAMOU_CONFIG`. This crate
does that job, and reads and writes the same install directory and config
files as the Python and TypeScript launchers, so they can share one install.

## What is ported

| Area | Status |
|---|---|
| fpgen (Bayesian network fingerprint generator, pinned model with sha256 checks) | Ported to Rust |
| Identity assembly: fpgen mapping, presets, fonts, voices, media devices, WebGL from recorded devices | Ported |
| Coherence rules (Apple Silicon cores, GPU vs OS, colour depth, DPR, window chrome, ...) | Ported |
| Seeded draws with CPython's Mersenne Twister, so a pinned identity draws the same fonts and voices as the Python launcher | Ported |
| Prefs (canvas placeholder, WebGPU, storage quota, scrollbars, locale, DNT/GPC, cache) | Ported |
| GeoIP (MaxMind databases from repos.yml), public and proxy exit IP, locale selection from CLDR | Ported |
| Build management: repos.yml channels, paired build, `sync`/`fetch`/`set`/`list`/`remove`/`active`/`version`/`path` | Ported |
| Virtual display (Xvfb with `-displayfd`), CPU pinning, Linux fontconfig, `~/.camoufox` | Ported |
| Per-context identities (`NewContext`): init script and context options | Ported |
| Driving the browser | Through Playwright's own server, see below |

## How the browser is driven

Camoufox speaks Playwright's Juggler protocol, and Playwright has no
maintained Rust client. So rustoufox does what the Python package does for its
`launch_server`: it starts `playwright-core`'s `firefox.launchServer()` under
Node.js with the generated options and returns the websocket endpoint. Any
Playwright client, in any language, connects with `firefox.connect(endpoint)`.

That needs Node.js 18+ and `playwright-core` below 1.63:

```bash
npm install -g playwright-core@"<1.63"
```

rustoufox finds `playwright-core` (or `playwright`) from the current directory,
then from the global npm root. `RUSTOUFOX_PLAYWRIGHT_CORE` points it at a
directory and `RUSTOUFOX_NODE` at a Node executable.

If you drive Playwright yourself, `launch_options()` returns the exact
`firefox.launch()` options and starts nothing.

### Without Node.js (experimental)

`rustoufox::juggler` speaks Juggler directly over the browser's pipe, with no
Node.js and no Playwright. It covers contexts, pages, `goto`, `evaluate`,
screenshots, cookies, clicks and text input; it has no selectors,
auto-waiting or network interception. Linux and macOS only for now.

```rust
use rustoufox::juggler::Browser;
use rustoufox::{Headless, LaunchOptions};

let browser = Browser::launch(LaunchOptions { headless: Headless::On, ..Default::default() })?;
let page = browser.new_context()?.new_page()?;
page.goto("https://example.com")?;
println!("{}", page.evaluate("document.title")?);
browser.close()?;
```

`cargo run --release --example juggler` compares its startup with the Node
server. `RUSTOUFOX_JUGGLER_DEBUG=1` shows the browser's own output.

## Install

```bash
cargo install --git https://github.com/iKyosh/Rustoufox
rustoufox fetch
```

`fetch` installs the browser build this release is paired with
(`data/browser-pin.json`), the GeoIP database, uBlock Origin and fpgen's model.

## Command line

| Command | What it does |
|---|---|
| `rustoufox fetch [VERSION]` | Install the paired build (or the chosen channel's latest), or `VERSION`. Syncs first. |
| `rustoufox sync [--spoof-os ...] [--spoof-arch ...]` | Refresh the list of available builds from every repository. |
| `rustoufox set [SPEC] [--geoip] [--release]` | Pin a build (`official/stable/156.0.1-beta.38`), follow a channel (`official/stable`), pick interactively, choose the GeoIP source, or go back to the paired build. |
| `rustoufox active` | Print the active build. |
| `rustoufox list [installed\|all] [--path]` | List installed builds, or every available one. |
| `rustoufox remove [VERSION] [--select] [-y]` | Remove one build, pick one, or the whole data directory. |
| `rustoufox version` | Versions, active build, GeoIP database, storage. |
| `rustoufox path` | Print the data directory. |
| `rustoufox server [launch options] [--port N] [--ws-path P]` | Start a server and print its endpoint. Ctrl+C closes the browser and the virtual display. |
| `rustoufox options [launch options]` | Print the Playwright `firefox.launch()` options as JSON. |
| `rustoufox test [URL]` | Open a headed browser with the Playwright inspector. |

Launch options on the command line: `--os windows` (repeatable),
`--headless true|false|virtual`, `--proxy URL --proxy-username U --proxy-password P`,
`--geoip auto|IP`, `--locale fr-FR`, `--humanize`, `--block-images`,
`--block-webrtc`, `--canvas-noise true|false`, `--pin-cpu-cores`, `--debug`.

```bash
rustoufox server --os windows --headless virtual --geoip auto \
  --proxy http://proxy.example.com:8080 --proxy-username user --proxy-password pass
```

## Library

```rust
use rustoufox::{launch_server, GeoIp, Headless, LaunchOptions, Proxy, ServerOptions};

fn main() -> rustoufox::Result<()> {
    let server = launch_server(
        LaunchOptions {
            os: vec!["windows".into()],
            headless: Headless::Virtual,
            proxy: Some(Proxy::new("http://proxy.example.com:8080").with_auth("user", "pass")),
            geoip: Some(GeoIp::Auto),
            ..Default::default()
        },
        ServerOptions::default(),
    )?;
    println!("connect to {}", server.ws_endpoint());
    // New contexts should start from server.launch_config().context_defaults():
    // the host's media features, and `viewport: null` when the window is spoofed.
    std::io::stdin().read_line(&mut String::new())?;
    server.close()
}
```

`LaunchOptions` carries every option of the Python launcher with the same
meaning and defaults: `config`, `os`, `block_images`, `block_webrtc`,
`block_webgl`, `disable_coop`, `webgl_config`, `geoip`, `geoip_db`,
`humanize`, `locale`, `addons`, `fonts`, `custom_fonts_only`,
`exclude_addons`, `screen`, `window`, `fingerprint`, `fingerprint_preset`,
`ff_version`, `headless`, `main_world_eval`, `allow_addon_new_tab`,
`executable_path`, `browser`, `firefox_user_prefs`, `proxy`, `enable_cache`,
`args`, `env`, `i_know_what_im_doing`, `debug`, `virtual_display`,
`pin_cpu_cores` and `canvas_noise`. Any other Playwright launch option goes
in `extra`, in Playwright's camelCase. Warnings raised while building the
launch are printed and also returned in `LaunchConfig::warnings`.

One browser, many identities: `new_context(&launch, browser_version, opts)`
returns an init script and `newContext()` options that give a context its own
navigator, screen, WebGL, fonts, voices, audio seed, timezone and WebRTC IP.
Apply them with `context.addInitScript()` in your client.

The examples run without Playwright (`identity`, `bench`) and with it (`server`):

```bash
cargo run --example identity -- macos
cargo run --example server
```

## The canvas on another OS

Canvas and WebGL pixels come from the real machine whatever the identity
claims. As in the Python launcher, an identity on the host's own OS claims the
GPU the host renders with, read once from a bare browser (over Juggler on
Linux and macOS, through Playwright on Windows). An identity on another OS or
GPU turns on the canvas protection of `privacy.resistFingerprinting` and
warns. When the host GPU cannot be read (on Windows, no Node.js or
`playwright-core`), rustoufox treats it as unknown and covers the canvas
rather than risk a mismatch.

## Environment variables

| Variable | Effect |
|---|---|
| `CAMOUFOX_INSTALL_DIR` | Data directory (default: the shared `camoufox` cache directory). |
| `CAMOUFOX_EXECUTABLE_PATH` | Launch this binary. |
| `CAMOUFOX_FPGEN_DATA` | Where fpgen's model is kept. |
| `CAMOUFOX_VIRTUAL_DISPLAY_SIZE` | Xvfb screen for `headless = virtual`, e.g. `1920x1080x24`. |
| `RUSTOUFOX_PLAYWRIGHT_CORE`, `RUSTOUFOX_NODE` | Where to find Playwright and Node.js. |
| `RUSTOUFOX_QUIET_WARNINGS` | Do not print warnings (they are still returned). |
| `GITHUB_TOKEN` | Authenticate GitHub API calls during `sync`. |

## Differences from the Python launcher

- The browser runs behind Playwright's server instead of in-process; the
  `Camoufox()` context manager and `NewBrowser()` have no Rust equivalent
  beyond `launch_server`.
- fpgen's model is kept in the camoufox data directory, with `values.dat`
  decompressed once (about 200 MB) so values are read with a seek.
- Monitor size is read with `xrandr` on Linux, `system_profiler` on macOS and
  `GetSystemMetrics` on Windows instead of `screeninfo`.
- Locale tags are validated against the IANA subtag registry
  (`data/language-registry.json`, generated from the `language-tags` package).

## Performance

Measured on 2026-10-14 against the official Python launcher (`camoufox`
0.5.8 from PyPI, Playwright 1.62), on one 4-core Linux container without a
GPU, with the same Camoufox build (v156.0.1-beta.38) and the same request: a
Windows identity, headless, without uBlock Origin. Absolute numbers will
differ on other machines; the ratios are what to read.

The launcher's own work, averaged over 50 calls after a first, cold one. The
Python launcher was given the same decompressed `values.dat` as this crate,
so both read fpgen's values from the same file:

| Operation | Python | Rust | Speed-up |
|---|---|---|---|
| Draw an fpgen fingerprint | 28-30 ms | 9-11 ms | about 3x |
| `launch_options()`, a whole launch's identity, prefs and environment | 30-31 ms | 11-13 ms | about 2.5x |
| Peak memory of the launcher process | 112 MB | 91 MB | -19% |

With its default install, which reads the values from the zstd archive, the
Python launcher takes 87-95 ms per `launch_options()`, about 7x this crate.

Launch to a first loaded page, median of five cold launches:

| Step | Python | Rust |
|---|---|---|
| Browser ready | 2.0 s | 1.3 s |
| First page loaded | 3.19 s | 2.73 s |

The 0.45 s gained is Python's import and identity work. The rest is
Firefox starting, which is the same browser either way, so once it runs each
page costs the same. The Rust figure also includes starting a Node.js client
to load the page, which the Python figure does not.

The gain matters when a process draws many identities (many contexts per
browser, frequent relaunches) or runs on a small machine; for one long-lived
browser it is negligible.

To reproduce, with a Camoufox executable at `$EXE`:

```bash
cargo run --release --example bench -- "$EXE" 50
```

```python
import time, warnings
import camoufox
from camoufox.utils import launch_options

warnings.simplefilter("ignore")
EXE = "/path/to/camoufox-bin"
options = dict(os="windows", headless=True, executable_path=EXE,
               i_know_what_im_doing=True, exclude_addons=[camoufox.DefaultAddons.UBO])
launch_options(**options)
start = time.perf_counter()
for _ in range(50):
    launch_options(**options)
print(f"launch_options: {(time.perf_counter() - start) / 50 * 1e3:.1f} ms")
```

## Development

```bash
cargo test --lib
cargo clippy --all-targets -- -D warnings
```

The data files in `data/` come from the Camoufox repository
(`python/src/camoufox/` and `browser/settings/properties.json`); update them
together with the code that reads them.

## Licence

MIT, like the Camoufox launchers this crate ports. The Camoufox browser is
MPL-2.0, fpgen's model is Apache-2.0, and `territoryInfo.xml` is Unicode CLDR
data under the Unicode licence.
