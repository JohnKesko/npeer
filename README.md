# npeer

A simple, fast way to download torrents from the command line.

[![CI](https://github.com/JohnKesko/npeer/actions/workflows/ci.yml/badge.svg)](https://github.com/JohnKesko/npeer/actions/workflows/ci.yml)

Give npeer a torrent file or a magnet link and it downloads it, showing
you how it's going. Stop any time and continue later. There is nothing
to set up first.

```text
✓ Read debian-13.6.0-amd64-netinst.iso (755 MiB, 3020 pieces)
⠹ Downloading: debian-13.6.0-amd64-netinst.iso
To: /home/you/Downloads

[========================>             ]  64%  483 MiB / 755 MiB

down: 3.00 MiB/s (avg 2.96 MiB/s) · cap 3.00 MiB/s
up: 120 KiB/s

peers 31 (12 seeds) · discovered 80 · swarm 512 seeds / 98 peers
verified: 1933/3020 pieces
elapsed 2m 43s · eta 1m 31s · done ~20:46
```

**Contents**

- [What it can do](#what-it-can-do)
- [Install](#install)
- [How to use](#how-to-use)
  - [Download a torrent](#download-a-torrent) ·
    [Download a magnet link](#download-a-magnet-link) ·
    [See what's inside a torrent](#see-whats-inside-a-torrent) ·
    [Download only some files](#download-only-some-files)
  - [Pause and continue](#pause-and-continue) ·
    [See your downloads](#see-your-downloads) ·
    [Choose where files go](#choose-where-files-go) ·
    [Limit the speed](#limit-the-speed)
  - [Check your files](#check-your-files) ·
    [Remove a download](#remove-a-download) ·
    [Keep sharing after you finish](#keep-sharing-after-you-finish)
- [All commands](#all-commands)
- [Settings](#settings)
- [Update npeer](#update-npeer)
- [Raspberry Pi](#raspberry-pi)
- [For developers](#for-developers)

## What it can do

- **Torrent files and magnet links** both work.
- **Pick your files.** From a torrent with many files, download only the
  ones you want.
- **Pause and continue.** Press Ctrl+C to pause. Pick up where you left
  off later, even after restarting your computer.
- **Live progress.** See the speed, how much is done and when it will
  finish.
- **Speed limits**, so downloads don't use up your whole connection.
- **Safe downloads.** Every part is checked as it arrives. Broken or fake
  data is thrown away and fetched again from someone else, so what ends
  up on your disk is exactly what the torrent promised.
- **Sharing back** after your download finishes, if you want to.
- **Works on Linux, macOS and Windows.**
- **Small and light.** One small program that uses little memory. Great
  on a Raspberry Pi.

## Install

npeer is a single program file. Download it and move it to a folder
where your computer looks for programs.

**Linux, regular PC:**

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-x86_64-unknown-linux-musl.tar.gz
tar -xzf npeer-x86_64-unknown-linux-musl.tar.gz
sudo mv npeer-x86_64-unknown-linux-musl/npeer /usr/local/bin/
```

**Linux, Raspberry Pi and other ARM machines:**

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-aarch64-unknown-linux-musl.tar.gz
tar -xzf npeer-aarch64-unknown-linux-musl.tar.gz
sudo mv npeer-aarch64-unknown-linux-musl/npeer /usr/local/bin/
```

Not sure which one you need? Run `uname -m`. It says `x86_64` for a
regular PC or `aarch64` for ARM.

**macOS, Apple Silicon (M1 and newer):**

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-aarch64-apple-darwin.tar.gz
tar -xzf npeer-aarch64-apple-darwin.tar.gz
sudo mkdir -p /usr/local/bin
sudo mv npeer-aarch64-apple-darwin/npeer /usr/local/bin/
```

On an Intel Mac, use `npeer-x86_64-apple-darwin` instead in all three
places. If you download the file with a web browser instead and macOS
refuses to open it, run `xattr -d com.apple.quarantine ./npeer` once.

**Windows:** download `npeer-x86_64-pc-windows-msvc.zip` from the
[latest release](https://github.com/JohnKesko/npeer/releases/latest),
unzip it, and run `npeer.exe`. To run it from any folder, add its folder
to your PATH.

**With Rust's cargo**, on any system:

```bash
cargo install --git https://github.com/JohnKesko/npeer npeer
```

## How to use

### Download a torrent

```bash
npeer download nature-sounds.torrent
```

Files go to your Downloads folder, the same place your web browser puts
things. While it runs you see the speed, how much is done and when it
will finish. Give several torrents and they download one after another.

### Download a magnet link

```bash
npeer download 'magnet:?xt=urn:btih:...&dn=...&tr=...'
```

**Always put magnet links in quotes.** Without them, your terminal cuts
the link short at the first `&` and npeer can't find anyone to download
from. Use single quotes on Linux and macOS, double quotes on Windows. If
you forget, npeer notices and tells you.

### See what's inside a torrent

`npeer info` shows the name, the size and every file in the torrent,
without downloading anything:

```text
$ npeer info nature-sounds.torrent
✓ Read Nature Sounds Collection (413 MiB, 1652 pieces)

Name         Nature Sounds Collection
Info hash    5b1f0c9e7a3d4b2c8e6f1a0d9c7b5e3f2a4c6d8e
Size         413 MiB (1652 pieces of 256 KiB)
Files        6
Trackers     udp://tracker.example.org:6969
Private      no

#  SIZE      FILE
1  98.2 MiB  01 Rain on a Tin Roof.flac
2  87.5 MiB  02 Ocean Waves.flac
3  76.1 MiB  03 Forest at Dawn.flac
4  91.4 MiB  04 Thunderstorm.flac
5  58.6 MiB  05 Mountain Stream.flac
6  1.20 MiB  cover.jpg

Download only some files by their number:
  npeer download nature-sounds.torrent --only 1-3
  npeer download nature-sounds.torrent --skip 6
```

The `#` column is npeer's number for each file. You use it to pick
files, as shown next. Torrents with more than 20 files show the first
20; add `--files` to see them all.

### Download only some files

Pick files by their number from `npeer info`:

```bash
# Only files 1, 2 and 3
npeer download nature-sounds.torrent --only 1-3

# Everything except the cover picture (file 6)
npeer download nature-sounds.torrent --skip 6
```

You can list numbers like `1,3,5-8`.

- Files you skip never show up in your download folder.
- npeer remembers your choice when you continue the download later.
- To change your mind, run the download again with a new `--only` or
  `--skip`. Use `--only all` to get every file.

### Pause and continue

Press **Ctrl+C** to pause. npeer saves where it got to. Later:

```bash
npeer resume            # show unfinished downloads
npeer resume nature     # continue one of them
npeer resume --all      # continue all of them
```

### See your downloads

```bash
npeer list              # every download and how far along it is
npeer status nature     # all the details about one download
```

```text
$ npeer list
ID        NAME                             SIZE     DONE    STATUS
5b1f0c9e  Nature Sounds Collection         262 MiB  100.0%  completed
c7a2e410  debian-13.6.0-amd64-netinst.iso  755 MiB  64.0%   incomplete
```

The first column is the download's id. Whenever a command asks for an
id, you can also type any part of the name instead, like `nature` above.

### Choose where files go

```bash
npeer download nature-sounds.torrent -o ~/Music
```

To change the folder for good, set `download_dir` in the
[settings](#settings).

### Limit the speed

```bash
npeer download nature-sounds.torrent --limit-down 5MiB --limit-up 500KiB
```

`5MiB` means about 5 megabytes per second. To always use a limit, put it
in the [settings](#settings).

### Check your files

```bash
npeer verify nature
```

This checks everything on disk against the torrent. If anything is
broken, `npeer resume nature` downloads just the broken parts again.

### Remove a download

```bash
npeer remove nature                  # npeer forgets it, your files stay
npeer remove nature --delete-files   # also deletes the files
```

### Keep sharing after you finish

By default npeer stops as soon as your download is done. To keep
sharing it with others for a while:

```bash
npeer download nature-sounds.torrent --seed
```

It stops once you have uploaded as much as the download's size. You can
change that in the [settings](#settings).

## All commands

| Command | What it does |
| --- | --- |
| `npeer download <torrent\|magnet>` | Download. `--only` / `--skip` pick files. Also `npeer add` or `npeer get` |
| `npeer info <torrent\|magnet>` | Show what's inside, with numbered files |
| `npeer peers <torrent\|magnet>` | Show who is sharing a torrent right now |
| `npeer list` | List all your downloads |
| `npeer status [id]` | Show details about downloads |
| `npeer resume [id]` / `--all` | Continue unfinished downloads |
| `npeer pause <id>` | Mark a download as paused |
| `npeer stop <id>` | Mark a download as stopped, so `resume --all` leaves it alone |
| `npeer remove <id>` | Forget a download (`--delete-files` deletes its files too) |
| `npeer verify <id>` | Check downloaded files against the torrent |
| `npeer config init\|show\|path\|validate` | Create and inspect the settings file |
| `npeer upgrade` | Update npeer to the newest version (`--beta` for early features) |
| `npeer completions <shell>` | Set up tab completion for bash, zsh, fish and others |

Add `--help` to any command to see all its options.

## Settings

**You don't need a settings file.** Without one, npeer saves to your
Downloads folder, has no speed limits and doesn't keep sharing after a
download. Create one only if you want to change something for good:

```bash
npeer config init
```

This creates a file with every setting explained. The most useful ones:

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
enabled = false           # keep sharing after a download finishes
max_ratio = 1.0           # stop after uploading this many times its size (0 = no limit)
max_minutes = 0           # stop after this many minutes (0 = no limit)
```

The file lives here:

| System | Path |
| --- | --- |
| Linux | `~/.config/npeer/config.toml` |
| macOS | `~/Library/Application Support/npeer/config.toml` |
| Windows | `%APPDATA%\npeer\config.toml` |

Options you type on the command line always win. Next come these
environment variables: `NPEER_DOWNLOAD_DIR`, `NPEER_LIMIT_DOWN`,
`NPEER_LIMIT_UP`, `NPEER_PORT`, `NPEER_MAX_PEERS` and `NPEER_SEED`. The
settings file comes last.

Speeds can be written as `5MiB` or `500KiB` (counting in 1024s), `8MB`
(counting in 1000s), or `unlimited`.

## Update npeer

```bash
npeer upgrade
```

npeer downloads the newest version, checks that it arrived intact, and
replaces itself. `npeer upgrade --check` only tells you if there is a
newer version.

If you installed npeer into `/usr/local/bin` as shown above, run
`sudo npeer upgrade` instead. npeer tells you if it needs that. Updating
never touches your downloads or settings.

### Try new features early

Want new features before everyone else? Get the beta version:

```bash
npeer upgrade --beta
```

Betas are ready to try, but may still have rough edges.

- `npeer upgrade --beta` gets the newest beta, or a newer stable version
  if there is one.
- `npeer upgrade` always gets the stable version. If you're on a beta,
  it offers to switch you back.
- When a beta's new features come out in a stable version,
  `npeer upgrade` gets you that version like everyone else.

## Raspberry Pi

A Raspberry Pi, or any small computer that is always on, is a great home
for npeer. Start a big download, log out, and let it finish overnight.
`npeer resume --all` continues everything after a restart or power cut.

The step-by-step guide covers installing, keeping downloads running
after you log out, and continuing automatically after a restart:
[docs/raspberry-pi.md](docs/raspberry-pi.md).

## Where npeer keeps things

Your downloaded files go to your Downloads folder, or wherever you
choose. npeer's own records are kept in a separate folder, so they never
mix with your files. That includes what it has downloaded so far, a copy
of each torrent, and small leftovers of files you chose to skip.

## For developers

### Build from source

```bash
cargo build --release        # the program ends up in target/release/npeer
cargo test                   # all tests, run locally without internet
```

Requires Rust 1.88 or newer.

Package maintainers can build with `--no-default-features` to leave out
`npeer upgrade`, so updates stay with the package manager.

### How the code is organized

- **`npeer-core`** is the engine: reading torrents and magnet links,
  talking to trackers and peers, choosing which pieces to fetch, saving
  and checking data, and remembering progress. It has no terminal code,
  so other front ends can be built on it.
- **`npeer`** is the command-line program: options, settings and the
  live progress display.

### Planned

- Finding peers without trackers (DHT, PEX and local discovery)
- Accepting incoming connections from other peers
- A background service with a remote control, and a full-screen
  terminal interface
- BitTorrent v2

## Acknowledgements

npeer grew out of a hand-written BitTorrent implementation built for the
[CodeCrafters BitTorrent challenge](https://codecrafters.io), then
reworked into an engine designed for real-world use.

## License

Copyright (c) 2026 npeer contributors.

npeer is free software, licensed under the
[GNU General Public License v3.0 or later](LICENSE). You can use, study,
share and improve it freely. If you share a changed version, it must
stay under the same license.
