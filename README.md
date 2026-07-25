# npeer

A fast, friendly BitTorrent client for the command line.

[![CI](https://github.com/JohnKesko/npeer/actions/workflows/ci.yml/badge.svg)](https://github.com/JohnKesko/npeer/actions/workflows/ci.yml)

npeer downloads torrent files and magnet links with a live progress display,
automatic resume, bandwidth limits and sensible defaults — no daemon, no
web UI, just a clean CLI that does the right thing.

```text
✓ Read debian-13.6.0-amd64-netinst.iso (755 MiB, 3020 pieces)
⠹ Downloading  debian-13.6.0-amd64-netinst.iso
  to /home/you/Downloads
  [========================>             ]  64%  483 MiB / 755 MiB
  down 3.00 MiB/s (avg 2.96 MiB/s) · up 120 KiB/s · caps ↓3.00 MiB/s
  peers 31 (12 seeds) · discovered 80 · swarm 512 seeds / 98 peers
  elapsed 2m 43s · eta 1m 31s · done ~20:46
```

## Features

- **Torrents and magnets** — `.torrent` files, and magnet links resolved
  through the swarm (BEP 9 / BEP 10 metadata exchange)
- **Automatic resume** — interrupted downloads continue where they left off;
  Ctrl+C saves state and exits cleanly
- **Live progress** — speeds, peers, ETA and expected completion time,
  updating in place without flooding your terminal
- **Bandwidth limits** — `--limit-down 5MiB`, `--limit-up 500KiB`, or set
  them once in the config file
- **HTTP(S) and UDP trackers** — including multi-tracker torrents (BEP 12)
- **Multi-file torrents** — with safe path handling
- **Verification** — every piece is SHA-1 checked as it arrives, and
  `npeer verify` re-checks data on disk any time
- **Seeding** — optional, with ratio and time limits
- **Cross-platform** — Linux, macOS and Windows are first-class
- **Light** — a single small binary with flat memory use; happy on a
  Raspberry Pi or any little always-on box

## Install

npeer is one self-contained binary — download it, put it on your PATH,
done. No runtime, no dependencies.

**Linux (regular PC, x86_64):**

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-x86_64-unknown-linux-musl.tar.gz
tar -xzf npeer-x86_64-unknown-linux-musl.tar.gz
sudo mv npeer-x86_64-unknown-linux-musl/npeer /usr/local/bin/
```

**Linux (ARM64 — Raspberry Pi, ARM servers):**

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-aarch64-unknown-linux-musl.tar.gz
tar -xzf npeer-aarch64-unknown-linux-musl.tar.gz
sudo mv npeer-aarch64-unknown-linux-musl/npeer /usr/local/bin/
```

**macOS:** download `npeer-aarch64-apple-darwin.tar.gz` (Apple Silicon)
or `npeer-x86_64-apple-darwin.tar.gz` (Intel) from the
[latest release](https://github.com/JohnKesko/npeer/releases/latest),
unpack, and move `npeer` somewhere on your PATH. If macOS blocks a
browser-downloaded binary, clear the quarantine flag:

```bash
xattr -d com.apple.quarantine ./npeer
```

**Windows:** download `npeer-x86_64-pc-windows-msvc.zip` from the
[latest release](https://github.com/JohnKesko/npeer/releases/latest),
unzip it, and run `npeer.exe` (add its folder to your PATH to call it
from anywhere).

**With cargo** (any platform with Rust installed):

```bash
cargo install --git https://github.com/JohnKesko/npeer npeer
```

Not sure which Linux build you need? `uname -m` says `x86_64` or
`aarch64` — pick the matching file.

## Quick start

```bash
# Download a torrent file
npeer download ubuntu.torrent

# Download a magnet link (note the quotes!)
npeer download 'magnet:?xt=urn:btih:...&dn=...&tr=...'

# Somewhere specific, with a bandwidth cap
npeer download ubuntu.torrent -o ~/isos --limit-down 5MiB
```

> **Always wrap magnet links in quotes.** Shells treat the `&` characters
> inside a magnet link specially, so an unquoted link is silently cut off
> at the first `&` and loses its trackers. Use single quotes on Linux and
> macOS, double quotes on Windows. If you forget, npeer notices the
> truncated link and reminds you.

Press **Ctrl+C** at any time — the download pauses, state is saved, and
`npeer resume <id>` (or `npeer resume --all`) picks it back up.

## Commands

| Command | What it does |
| --- | --- |
| `npeer download <torrent\|magnet>` | Download (aliases: `add`, `get`) |
| `npeer info <torrent\|magnet>` | Show name, size, pieces, trackers, files |
| `npeer peers <torrent\|magnet>` | Ask the trackers who is in the swarm |
| `npeer list` | All torrents npeer knows about |
| `npeer status [id]` | Detailed status |
| `npeer resume <id>` / `--all` | Continue unfinished downloads |
| `npeer pause <id>` / `stop <id>` | Park a torrent |
| `npeer remove <id> [--delete-files]` | Forget a torrent |
| `npeer verify <id>` | Re-check downloaded data against the piece hashes |
| `npeer config init\|show\|path\|validate` | Manage configuration |
| `npeer completions <shell>` | Shell completions (bash, zsh, fish, ...) |

Torrent ids are an info-hash prefix or any unique part of the name:
`npeer resume 481b` and `npeer resume debian` both work.

## Configuration

npeer reads a TOML file from the platform config directory:

| OS | Path |
| --- | --- |
| Linux | `~/.config/npeer/config.toml` |
| macOS | `~/Library/Application Support/npeer/config.toml` |
| Windows | `%APPDATA%\npeer\config.toml` |

Create a documented starter file with:

```bash
npeer config init
```

```toml
[general]
# download_dir = "/path/to/downloads"   # defaults to your Downloads folder
verify_on_resume = false

[network]
listen_port = 6881
max_peers = 40

[limits]
download = "unlimited"    # e.g. "5MiB", "500KiB", "8MB"
upload = "unlimited"

[seeding]
enabled = false           # keep seeding after completion
max_ratio = 1.0           # stop at this upload ratio (0 = no limit)
max_minutes = 0           # stop after this long (0 = no limit)
```

Precedence, highest first:

1. Command-line flags (`-o`, `--limit-down`, `--port`, ...)
2. Environment variables (`NPEER_DOWNLOAD_DIR`, `NPEER_LIMIT_DOWN`,
   `NPEER_LIMIT_UP`, `NPEER_PORT`, `NPEER_MAX_PEERS`, `NPEER_SEED`)
3. `config.toml`
4. Built-in defaults

Bandwidth values accept binary units (`5MiB`, `500KiB`, `1.5M` — powers of
1024), decimal units (`8MB` — powers of 1000), an optional `/s` suffix, and
`unlimited`.

## Runs great on a Raspberry Pi

A Pi (or any small always-on machine) and npeer are a natural pair:
start a big download over SSH, disconnect, and let it finish overnight.
npeer is one small binary, uses very little memory no matter how large
the torrent is, and `npeer resume --all` picks everything up after a
reboot or power cut — you can even run that automatically at boot.

The step-by-step guide — installing, keeping downloads running after
you log out, and auto-resuming after reboots — is in
[docs/raspberry-pi.md](docs/raspberry-pi.md).

## Where things live

Downloaded files go to your Downloads folder (or wherever you point
`-o` / `download_dir`). Application state — resume data and torrent
metadata — lives separately in the platform data directory, so your
downloads are never mixed with npeer's bookkeeping.

## Architecture

npeer is a two-crate workspace:

- **`npeer-core`** — the engine: bencoding, torrent/magnet parsing,
  HTTP and UDP trackers, the peer wire protocol with extensions,
  piece scheduling (rarest-first with endgame mode), storage,
  verification and session state. It has no CLI or terminal
  dependencies, so it can power other frontends.
- **`npeer`** — the CLI: argument parsing, configuration, and the
  live terminal interface.

## Building from source

```bash
cargo build --release        # binary at target/release/npeer
cargo test                   # unit + end-to-end tests (local swarm, no internet)
```

Requires Rust 1.88 or newer.

## Roadmap

- DHT, PEX and local service discovery (config keys are reserved)
- Accepting incoming peer connections
- A daemon mode with a remote API, and a TUI on top of `npeer-core`
- BitTorrent v2

## Acknowledgements

The protocol behaviour grew out of a hand-written BitTorrent
implementation built for the
[CodeCrafters BitTorrent challenge](https://codecrafters.io), then
reworked into an engine designed for real-world swarms.

## License

Copyright (c) 2026 npeer contributors.

npeer is free software, licensed under the
[GNU General Public License v3.0 or later](LICENSE). You can use, study,
share and improve it freely — and if you distribute a modified version,
it must stay under the same license.
