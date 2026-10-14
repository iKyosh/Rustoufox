# Deploying rustoufox

How to run rustoufox on a Linux server or container (an LXC, a VM, Docker),
and how to depend on it from another project.

## On a Linux container

The browser runs headless, so the container needs no GPU, no display and no
special option (no nesting, no privileged mode). As root, on Debian 13 or
Ubuntu 24.04 (x86_64):

```bash
apt update
apt install -y curl ca-certificates \
  libgtk-3-0t64 libx11-xcb1 libasound2t64 libdbus-1-3 libxcomposite1 \
  libxcursor1 libxdamage1 libxfixes3 libxi6 libxrandr2 \
  fontconfig fonts-dejavu-core
# Only for --headless virtual:
apt install -y xvfb

V=v0.1.0-beta.1
curl -fsSL https://github.com/iKyosh/Rustoufox/releases/download/$V/rustoufox-$V-x86_64-unknown-linux-gnu.tar.gz \
  | tar xz -C /tmp
install -m 755 /tmp/rustoufox-$V-x86_64-unknown-linux-gnu/rustoufox /usr/local/bin/
rustoufox fetch
rustoufox version
```

On Debian 12, the packages are `libgtk-3-0` and `libasound2` instead of
`libgtk-3-0t64` and `libasound2t64`. The `v0.1.0-beta.1` binary needs glibc
2.39 (see [Install](../README.md#install)), so on Debian 12 build it in the
container instead of downloading it:

```bash
apt install -y build-essential pkg-config git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo install --git https://github.com/iKyosh/Rustoufox --tag v0.1.0-beta.1
rustoufox fetch
```

Plan for about 3 GB of disk for the browser, the GeoIP database and fpgen's
model.

## As a service

`rustoufox server` starts Playwright's server, which needs Node.js 18+ and
`playwright-core` below 1.63:

```bash
apt install -y nodejs npm
npm install -g playwright-core@"<1.63"
mkdir -p /var/lib/camoufox
CAMOUFOX_INSTALL_DIR=/var/lib/camoufox rustoufox fetch
```

`/etc/systemd/system/rustoufox.service`:

```ini
[Unit]
Description=rustoufox (Camoufox server)
After=network-online.target

[Service]
Environment=CAMOUFOX_INSTALL_DIR=/var/lib/camoufox
ExecStart=/usr/local/bin/rustoufox server --headless true --host 0.0.0.0 --port 9222 --ws-path camoufox
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now rustoufox
journalctl -u rustoufox -f   # shows the ws:// endpoint
```

Any Playwright client connects with
`firefox.connect("ws://<container-ip>:9222/camoufox")`. The server has no
authentication: keep the port on a private network.

## As a dependency

rustoufox is not on crates.io; depend on a git tag.

### From a Rust project

```toml
[dependencies]
rustoufox = { git = "https://github.com/iKyosh/Rustoufox", tag = "v0.1.0-beta.1" }
```

The crate does not embed the browser: the machine that runs your program
needs `rustoufox fetch` (or `CAMOUFOX_EXECUTABLE_PATH`) first.

### From another language

Install the release binary as a build step, run `rustoufox server`, and
connect your Playwright client to its websocket. A Docker image, for
example:

```dockerfile
FROM debian:trixie-slim
ARG V=v0.1.0-beta.1
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
      libgtk-3-0t64 libx11-xcb1 libasound2t64 libdbus-1-3 libxcomposite1 libxcursor1 \
      libxdamage1 libxfixes3 libxi6 libxrandr2 fontconfig fonts-dejavu-core nodejs npm \
 && npm install -g playwright-core@"<1.63" \
 && curl -fsSL https://github.com/iKyosh/Rustoufox/releases/download/$V/rustoufox-$V-x86_64-unknown-linux-gnu.tar.gz \
    | tar xz -C /tmp \
 && install -m 755 /tmp/rustoufox-$V-x86_64-unknown-linux-gnu/rustoufox /usr/local/bin/ \
 && rustoufox fetch \
 && rm -rf /var/lib/apt/lists/* /tmp/rustoufox-*
EXPOSE 9222
CMD ["rustoufox", "server", "--headless", "true", "--host", "0.0.0.0", "--port", "9222", "--ws-path", "camoufox"]
```

Each release page lists the SHA-256 of its archives, to check them in the
build.
