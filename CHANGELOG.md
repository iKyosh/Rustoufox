# Changelog

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
