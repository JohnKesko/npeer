# Running npeer on a Raspberry Pi

A Raspberry Pi is a great home for npeer. It is small, silent and cheap to
leave on, and npeer is a single small program that needs no screen and very
little memory. Start a big download over SSH, close your laptop, and let
the Pi finish it overnight. If the power goes out halfway, one command
picks every download up again.

This guide takes you from a fresh Pi to downloads that survive reboots.
Every command can be copied and pasted as-is.

## What you need

- A Raspberry Pi 3, 4, 5 or Zero 2 W
- A 64-bit operating system (Raspberry Pi OS 64-bit is the default choice
  in the Raspberry Pi Imager)
- A way to reach it: SSH or a keyboard

Not sure if your system is 64-bit? Run:

```bash
uname -m
```

If it prints `aarch64`, you are good. If it prints `armv7l` or `armv6l`,
you have a 32-bit system — see [32-bit systems](#32-bit-systems) at the
bottom.

## Install

Three commands: download, unpack, put it on your PATH.

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-aarch64-unknown-linux-musl.tar.gz
tar -xzf npeer-aarch64-unknown-linux-musl.tar.gz
sudo mv npeer-aarch64-unknown-linux-musl/npeer /usr/local/bin/
```

Check that it works:

```bash
npeer --version
```

The binary is fully self-contained — there is nothing else to install.

## Your first download

```bash
npeer download my-file.torrent
```

Or with a magnet link (the quotes matter — your shell would otherwise cut
the link at the first `&`):

```bash
npeer download 'magnet:?xt=urn:btih:...'
```

Files land in `~/Downloads` by default. Point them somewhere else — an
external drive, for example — with `-o`:

```bash
npeer download my-file.torrent -o /mnt/storage
```

Press Ctrl+C at any time. The download pauses, progress is saved, and
nothing is lost.

## Keep it running after you disconnect

If you start a download over SSH and then close the connection, the
download stops with it. The fix is `tmux`, a tool that keeps programs
running in the background. Install it once:

```bash
sudo apt install tmux
```

Then the routine is:

```bash
# Open a session named "torrents" and start your download inside it
tmux new -s torrents
npeer download 'magnet:?xt=urn:btih:...'
```

Now press **Ctrl+B, then D** to detach. The download keeps running. You
can log out, come back tomorrow, and check on it with:

```bash
tmux attach -t torrents
```

We recommend tmux because it is the only option where you can come back
later and watch the live progress screen again. But it is not the only
way:

- **screen** — the older tool that does the same job. If you already use
  it: `screen -S torrents`, detach with **Ctrl+A, then D**, come back
  with `screen -r torrents`.
- **nohup** — nothing to install: `nohup npeer download '...' &`. The
  trade-off is that you can never see the live screen again; check
  progress with `npeer status` and pause with `kill -INT <pid>`.
- **Nothing at all** — also fine. If your SSH connection drops, npeer
  notices the hangup, pauses cleanly and saves its progress. Next time
  you log in, `npeer resume --all` continues where it stopped. You lose
  the "keeps downloading while I'm away" part, but never any data.

## Continue automatically after a reboot

Power cuts and reboots happen. npeer already remembers where every
download stood; `npeer resume --all` finishes whatever is unfinished. To
run that automatically at every boot, create a small service file:

```bash
sudo tee /etc/systemd/system/npeer-resume.service > /dev/null <<'EOF'
[Unit]
Description=Finish unfinished npeer downloads
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=pi
ExecStart=/usr/local/bin/npeer resume --all

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl enable npeer-resume.service
```

Replace `pi` with your username if it is different (`whoami` tells you).
From now on, every boot quietly finishes any interrupted downloads. See
what it did with:

```bash
journalctl -u npeer-resume
```

## Good settings for a Pi

Two things are worth setting once so the Pi behaves like a polite
housemate. Create a config file:

```bash
npeer config init
```

Then edit it (`npeer config path` shows where it is) and consider:

```toml
[limits]
# Leave room on your home connection for everyone else.
download = "5MiB"
upload = "1MiB"

[seeding]
# A Pi is always on anyway — give back to the swarm.
enabled = true
max_ratio = 2.0
```

Everything can still be overridden per download, for example
`--limit-down 2MiB` or `--no-seed`.

## Handy commands for a headless machine

```bash
npeer list            # what does the Pi know about?
npeer status          # detailed progress of everything
npeer resume --all    # finish everything unfinished
npeer verify debian   # re-check a download against its checksums
```

Torrents can be named by any unique part of their name or id:
`npeer status debian` just works.

## Updating npeer

Same three commands as installing — the new binary replaces the old one:

```bash
curl -LO https://github.com/JohnKesko/npeer/releases/latest/download/npeer-aarch64-unknown-linux-musl.tar.gz
tar -xzf npeer-aarch64-unknown-linux-musl.tar.gz
sudo mv npeer-aarch64-unknown-linux-musl/npeer /usr/local/bin/
```

Your downloads, resume data and configuration are untouched by updates.

## Where things live

| What | Where |
| --- | --- |
| Downloaded files | `~/Downloads` (or wherever you pointed `-o`) |
| Configuration | `~/.config/npeer/config.toml` |
| Resume data | `~/.local/share/npeer/` |

## Troubleshooting

**"cannot execute binary file"** — the binary does not match your system.
Run `uname -m`: `aarch64` needs the `aarch64` download; `x86_64` (a
regular PC) needs the `x86_64` one.

**`armv7l` or `armv6l` (32-bit systems)** <a name="32-bit-systems"></a> —
there is no pre-built 32-bit binary yet. Either reinstall with the 64-bit
Raspberry Pi OS (recommended on a Pi 3 or newer), or build npeer on the
Pi itself:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

```bash
git clone https://github.com/JohnKesko/npeer.git && cd npeer && cargo build --release
```

Building takes a while on a Pi (expect 15–30 minutes); the result is at
`target/release/npeer`.

**Downloads feel slow** — check whether you set a cap (`npeer config
show`), and remember that many home routers throttle sustained uploads;
a modest `upload` limit often makes downloads faster.
