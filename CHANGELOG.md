# Changelog

## Unreleased

- `rustoufox options` prints in `env` only the variables rustoufox adds or
  changes, no longer the caller's whole environment (tokens, credentials).
  Merge it with your own environment before passing it to Playwright, whose
  `env` replaces the browser's (#20).
- `rustoufox version | head` and other closed pipes end quietly instead of
  panicking with exit code 101 (#21).
- `rustoufox server` handles Ctrl+C or SIGTERM at once and waits for Node in
  10 ms steps instead of 100 ms: about 0.15 s less per close (#22).

## 0.1.0-beta.3

- rustoufox no longer runs on Windows: no Windows binary, no `windows-sys`
  dependency, and the last Node.js script used outside `launch_server`
  (`hostGpu.js`) is gone. Windows identities are unchanged (#17). The last
  Windows binary is attached to `v0.1.0-beta.2`.

## 0.1.0-beta.2

- `launch_server()` starts Node.js before drawing the identity, so Playwright
  loads meanwhile: the endpoint comes about 0.2 to 0.4 s sooner (#6). With
  `pin_cpu_cores`, the order stays sequential.
- The GeoIP database is memory-mapped instead of read whole: about 110 MB
  less peak memory per launch with GeoIP (#7).
- `rustoufox server` drops fpgen's network once the browser runs: about
  12 MB resident while it waits instead of 96 MB (#8).
- `RUSTOUFOX_IP_CACHE_TTL` keeps the public IP on disk, per proxy, for that
  many seconds; off by default (#10).
- The Linux release binary links against glibc 2.28 instead of the runner's
  (2.39), so it runs on Debian 10+, Ubuntu 18.10+ and RHEL 8+ (#11).
- `playwright-core` is found without starting npm or Node.js: `rustoufox
  options` and `rustoufox server` reach their output about 0.2 s sooner
  (#13). npm is still asked when the package is not in the usual places.
- fpgen's network is parsed straight into its tables instead of through
  `serde_json::Value`: loading it takes about 65 ms instead of 135 ms (#13).
- README: the path for Python, Java and .NET clients (#9), `--geoip <IP>`,
  canvas noise, disk footprint and comparing with the Python launcher
  (#12).

## 0.1.0-beta.1

First public beta of rustoufox, a Rust port of the Camoufox launcher. It is
paired with Camoufox v156.0.1-beta.38 (`data/browser-pin.json`).

### What it does

- Fetches and manages Camoufox builds from the same `repos.yml` channels as
  the Python launcher: `sync`, `fetch`, `set`, `list`, `remove`, `active`,
  `version` and `path`, sharing one install directory with the Python and
  TypeScript launchers.
- Draws a coherent device identity on every launch: fpgen's Bayesian network
  (pinned model, sha256 checks), presets, fonts, voices, media devices and
  WebGL from recorded devices, with the same coherence rules as upstream.
- Seeded draws use CPython's Mersenne Twister, so a pinned identity draws the
  same fonts and voices as the Python launcher.
- GeoIP from the MaxMind databases, public and proxy exit IP, and locale
  selection from CLDR.
- Xvfb virtual display, CPU pinning, Linux fontconfig and per-context
  identities (`new_context`).
- Starts Playwright's own server under Node.js for any Playwright client, or
  returns the `firefox.launch()` options with `launch_options()`.

### Experimental

- `rustoufox::juggler`, a native Juggler client that drives the browser over
  its pipe with no Node.js: contexts, pages, `goto`, `evaluate`, screenshots,
  cookies, clicks and text input. Linux and macOS only.
- The host GPU probe runs over Juggler on Linux and macOS; Windows still uses
  Node.js for it.

### Known limits

- The native Juggler client has no selectors, auto-waiting or network
  interception.
- Driving the browser through Playwright still needs Node.js 18+ and
  `playwright-core` below 1.63.
