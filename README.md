# Rust Web Server

This is a Rust program, built with [Actix Web](https://actix.rs), that serves a folder of website files (pages, pictures, CSS, and the rest) to visitors.

Run it on port 80 with Cloudflare in front. Cloudflare connects to your server on port 80, and Cloudflare handles HTTPS for visitors.

**Benchmark:** 599,974 requests/sec (home page with Brotli, 256 connections, `cache-all`). See [Benchmarks](#benchmarks).

**Contents**

- [Which version should I use?](#which-version-should-i-use)
- [Quick start (5 minutes)](#quick-start-5-minutes)
- [Using Cloudflare](#using-cloudflare)
- [Keep it running](#keep-it-running)
- [Settings](#settings)
- [Other systems](#other-systems)
- [Troubleshooting / FAQ](#troubleshooting--faq)
- [Benchmarks](#benchmarks)
- [License](#license)

---

## Which version should I use?

The repository has eight folders. Each folder is its own copy of the same server. Build **one** folder. The only difference is the settings block at the top of that folder's `main.rs` (compression, cache times, and ETags).

Use **`cache-all`** if you are not sure. That is the right pick for most people.

| Folder | In plain words |
|---|---|
| `cache-all` | **Recommended.** Shrinks text files, and browsers may keep every file, including HTML pages, for one year. |
| `cache-assets-only` | Shrinks text files. Browsers check `.html` and `.htm` pages with the server on every visit, and may keep images, CSS, and JavaScript for one year. |
| `cache-1-minute` | Shrinks text files. Browsers may keep HTML for one minute, and other files for one year. Fits a blog or a news page that changes often. |
| `no-cache` | Sends each file unchanged, and tells browsers to keep nothing. Useful while you are editing the site. |
| `no-cache+compression` | Shrinks text files, and tells browsers to keep nothing. |
| `cache-all-no-compression` | Sends files unchanged. Browsers may keep every file, including pages, for one year. Uses less CPU when the server starts. |
| `cache-assets-only-no-compression` | Sends files unchanged. Browsers check HTML on every visit, and may keep other files for one year. |
| `cache-1-minute-no-compression` | Sends files unchanged. Browsers may keep HTML for one minute, and other files for one year. |

"Shrinks text files" means Brotli. Brotli is a compression format. HTML, CSS, JavaScript, SVG, JSON, and similar text are stored a second time in a smaller form, and that smaller form is what a browser gets when it accepts Brotli. Other browsers get the original file. The site is online as soon as the server prints `listening`. The strongest compression finishes in the background. The server then prints `recompressed`. Files that do not fit in the cache budget are read from disk and are not pre-compressed.

The six `cache-*` folders send an ETag, a fingerprint of the file. When the visitor already has that copy, the server answers `304 Not Modified` and skips the body. The two `no-cache` folders do not send ETags.

Every version can serve video and audio, including seeking inside the file. That works for files read from disk too.

With `cache-all`, a browser may keep a page for a year. After you change a file, restart the server. If visitors still see the old file, give it a new name, or add `?v=2` on the link. When Cloudflare sits in front, purge its cache as well (Caching, then Configuration, then Purge Everything). If you edit pages often and want visitors to see changes quickly, use `cache-1-minute` or `cache-assets-only` instead.

In the steps below, replace `cache-all` with another folder name if you picked a different one.

---

## Quick start (5 minutes)

These steps are for Ubuntu and Debian. You need a terminal on the server (SSH is fine). If a command says `sudo: command not found`, you are already root: run that command without `sudo`.

### 1. Install the compiler, git, and Rust

Rust comes from rustup, the official installer. This server needs Rust 1.89 or newer. rustup installs a current Rust.

```bash
sudo apt update
sudo apt install -y build-essential curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo --version
```

### 2. Download the code

This creates a `rustwebserver` folder in your home directory.

```bash
cd "$HOME"
git clone https://github.com/dumptcp/rustwebserver
```

### 3. Open the recommended folder

`cache-all` is the version to run. Every command after this one starts here.

```bash
cd "$HOME/rustwebserver/cache-all"
```

### 4. Build it

The first build downloads libraries and can take a few minutes. `--locked` uses the library versions in `Cargo.lock`, the versions this server was tested with. The finished program is `target/release/rustwebserver`.

```bash
cd "$HOME/rustwebserver/cache-all"
cargo build --release --locked
```

### 5. Put your website in `www`

The home page must be a file named `index.html`. This command creates a test page. Copy your real site over `www` afterwards (FileZilla, WinSCP, or `scp`). Running it again replaces `www/index.html`.

```bash
cd "$HOME/rustwebserver/cache-all"
mkdir -p www
printf '%s\n' '<!doctype html><meta charset="utf-8"><title>It works</title><h1>It works</h1>' > www/index.html
```

You should end up with this layout:

```text
cache-all/
├── Cargo.toml
├── Cargo.lock
├── main.rs
├── target/release/rustwebserver
└── www/
    └── index.html
```

`cloudflare.conf` stays in the parent folder, `~/rustwebserver/cloudflare.conf`.

### 6. Run it

The server listens on port 80. On Linux that needs root, so this command uses `sudo`. If you are already root, delete the word `sudo`. To run it as your own user and still use port 80, grant the binary permission with `setcap` (see [Troubleshooting](#troubleshooting--faq)).

```bash
cd "$HOME/rustwebserver/cache-all"
sudo ROOT="$PWD/www" ./target/release/rustwebserver
```

Leave this window open. The server is ready when it prints `listening on :80`. It also prints `loaded N files from ...` with the folder it read. `Ctrl+C` stops it.

### 7. Check that it works

Open a second SSH window. This asks the server for the home page on port 80. You should see `It works`.

```bash
curl -sS http://127.0.0.1/
```

Visitors use `https://your-domain` after the [Cloudflare](#using-cloudflare) steps below. If curl on the server works and the domain does not, see [Troubleshooting](#troubleshooting--faq).

---

## Using Cloudflare

This is part of the setup. The server speaks plain HTTP on port 80. Cloudflare connects to that port and handles HTTPS for visitors.

In the Cloudflare dashboard, proxy the DNS record (the orange cloud) and set SSL/TLS to **Flexible**. **Full** and **Full (strict)** try HTTPS to your server, and visitors get an error page, because nothing here speaks HTTPS. **Always Use HTTPS** (SSL/TLS, then Edge Certificates) sends `http://` visitors to `https://`.

`cache-all` marks pages as cacheable for a year. Cloudflare still does not cache HTML unless you add a Cache Rule: **Eligible for cache**, Edge TTL **Use cache-control header if present**, Browser TTL **Respect origin**.

Versions that compress files send `no-transform` on every file. That tells Cloudflare to pass the file on unchanged, keep the ETag, and skip adding its own scripts to the page. Versions that do not compress add `no-transform` only on files that are already compressed (images other than SVG and ICO, video, audio, WOFF and WOFF2, PDF, ZIP, and similar). HTML, CSS, JavaScript, SVG, ICO, and TTF/OTF stay open so Cloudflare can compress them. Bot Fight Mode can strip ETags from pages. Turn it off when you want pages answered with `304`.

### The firewall file

`cloudflare.conf` is an nftables firewall (nftables is Linux's firewall tool). Loading it drops new connections except:

- Cloudflare's published addresses, and only on port 80
- SSH, so you can still log in
- connections that are already open, and programs on the server itself (including `curl` to `127.0.0.1`)

Docker's forwarded traffic is left as it is. Loading the file again replaces only the `inet webfw` table.

Load it with the domain proxied. After it is loaded, only Cloudflare can open the site from the internet.

### Set your SSH port

Open `~/rustwebserver/cloudflare.conf`. The two lines at the top are:

```text
define SSH_PORT = 22
define WEB_PORT = 80
```

Leave `WEB_PORT` at 80. That is the port Cloudflare uses to reach this server. Set `SSH_PORT` to the port you use to log in.

**Warning:** a wrong `SSH_PORT` blocks new SSH logins. You can lock yourself out. Check the port before you load the file, and keep this SSH window open until a second SSH window has connected.

```bash
sudo ss -ltnp | grep sshd
```

The address looks like `0.0.0.0:22` or `*:2222`. The number after the colon is `SSH_PORT`. If no `Port` line is set in `/etc/ssh/sshd_config`, SSH is on port 22.

Edit the file with `nano ~/rustwebserver/cloudflare.conf`, save it, then load it:

```bash
sudo apt install -y nftables
sudo nft -f "$HOME/rustwebserver/cloudflare.conf"
sudo nft list table inet webfw
```

The lists of Cloudflare addresses come from [cloudflare.com/ips](https://www.cloudflare.com/ips/). They change rarely. If Cloudflare adds an address, update both lists in the file and load it again.

### Keep the firewall after a reboot

The rules are gone after a reboot until you save them. Save them only after a second SSH session has logged in.

```bash
sudo cp "$HOME/rustwebserver/cloudflare.conf" /etc/nftables.conf
sudo systemctl enable nftables
```

### Remove this firewall

```bash
sudo nft delete table inet webfw
```

If you copied the file to `/etc/nftables.conf`, move it aside too, or the rules return on reboot. `disable` stops the file from loading on the next boot. It leaves the live rules to the `delete table` command above.

```bash
sudo mv /etc/nftables.conf /etc/nftables.conf.bak
sudo systemctl disable nftables
```

On Ubuntu and Debian, `sudo systemctl stop nftables` runs `nft flush ruleset`. That deletes every nftables table, including tables Docker created. `sudo nft delete table inet webfw` removes only this file's rules.

---

## Keep it running

Closing the SSH window stops a server you started in that window. `screen` leaves it running. systemd is the option that also starts the site when the machine boots.

### screen (simple)

`screen` is a small program that keeps your session alive after you disconnect.

```bash
sudo apt install -y screen
cd "$HOME/rustwebserver/cache-all"
screen -S website
```

Inside that window, start the server the same way as in the quick start:

```bash
sudo ROOT="$PWD/www" ./target/release/rustwebserver
```

Detach and leave it running: hold `Ctrl`, press `A`, release both, then press `D`.

Come back later:

```bash
screen -rx website
```

`Ctrl+C` stops the server. Type `exit` to close that screen session. If `screen -S website` says the name is already taken, use `screen -rx website`.

### systemd (starts on boot)

systemd is the service manager on Ubuntu and Debian. This unit runs the server as root, so it can listen on port 80. Paste it as the user who cloned the repo, so `$HOME` is that user's home folder.

```bash
sudo tee /etc/systemd/system/rustwebserver.service >/dev/null <<EOF
[Unit]
Description=Rust static website
After=network.target

[Service]
WorkingDirectory=$HOME/rustwebserver/cache-all
Environment=ROOT=$HOME/rustwebserver/cache-all/www
ExecStart=$HOME/rustwebserver/cache-all/target/release/rustwebserver
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now rustwebserver
sudo systemctl status rustwebserver
```

To run the service as your own user, add `User=yourname` and grant the binary permission with `setcap` so it can still listen on port 80. See [Troubleshooting](#troubleshooting--faq).

Only one program can listen on port 80. Stop the `screen` copy before starting the service, or the service will print that port 80 is in use and keep retrying.

```bash
sudo systemctl stop rustwebserver
sudo systemctl restart rustwebserver
sudo journalctl -u rustwebserver -n 50 --no-pager
```

---

## Settings

Settings are environment variables: names you put on the same line as the command. The server listens on port 80. An empty value is ignored. A bad number prints `bad KEEP_ALIVE:` (using the name you set) and the server exits.

```bash
ROOT="$HOME/rustwebserver/cache-all/www" ./target/release/rustwebserver
```

| Setting | Default | What it does | Example |
|---|---|---|---|
| `ROOT` | `/root/www` | Folder that holds the site. The home page is `index.html` in this folder. A normal user cannot read `/root/www`. Point `ROOT` at a folder you own, such as the `www` folder from the quick start. | `ROOT="$HOME/rustwebserver/cache-all/www"` |
| `KEEP_ALIVE` | `120` seconds | How long an idle connection stays open. The default is 120 seconds so idle sockets are not held for Cloudflare's 900 second origin keep-alive. `0` is treated as 1. | `KEEP_ALIVE=60` |
| `CACHE_MEMORY` | usable RAM / 4 | RAM budget for cached file bytes, including each file's Brotli copy when this version compresses files. A plain number is bytes. A suffix is a power of 1024: `512M` is 512 MiB, `2G` is 2 GiB. `K`, `M`, `G`, `T` and the `KB` / `MB` / `MiB` forms all work. Files past the budget are streamed from disk with the same headers, and that stream is not pre-compressed. The site still serves every file. | `CACHE_MEMORY=512M` |
| `MAX_IN_MEMORY` | `2097152` (2 MiB) | A single file larger than this is always read from disk, even when the cache has room left. Plain number of bytes. | `MAX_IN_MEMORY=1048576` |
| `MAX_CONNECTIONS` | unlimited | How many visitors are served at once. Unset or `0` does not cap connections. A positive number is a process-wide cap: past it the server still accepts the connection, replies `503 Service Unavailable` with `Retry-After: 1` and an empty body, then closes. | `MAX_CONNECTIONS=4096` |
| `HEAD_MEMORY` | usable RAM / 16 | Budget for request headers. The server prints it at startup. Actix still refuses a header larger than 128 KiB, and that cap cannot be lowered. Plain number of bytes. A suffix such as `64M` is rejected. | `HEAD_MEMORY=8388608` |

Usable RAM is the smaller of the machine's memory and any cgroup limit on this process. On macOS, FreeBSD, and Windows it is the machine's physical RAM, or 1 GiB when the system does not report a figure.

On startup the server prints the folder, how many files are in memory, how many are read from disk, the cache budget, the connection cap, the head budget, and the keep-alive. It also raises its open-file soft limit to the hard limit.

`CACHE_MEMORY` is the only setting that accepts a `512M` style suffix.

---

## Other systems

The same folders build on macOS, FreeBSD, and Windows. The server still listens on port 80, with Cloudflare in front. `cloudflare.conf` is a Linux nftables file, so the firewall steps above apply only on Linux. These systems have no `setcap`. Run the server as an administrator so it can listen on port 80.

**macOS.** Install the Xcode Command Line Tools (a C compiler and git), then rustup. Clone the repo the same way as on Linux, create `www` with an `index.html` in it, then build and run:

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cd cache-all
cargo build --release --locked
sudo ROOT="$PWD/www" ./target/release/rustwebserver
```

**FreeBSD.** The base system has a C compiler. Install git and curl, then the same rustup command as on Linux. Run the server as root so it listens on port 80.

```bash
pkg install git curl
```

**Windows.** Install Rust from [https://rustup.rs](https://rustup.rs). The default installer uses the MSVC toolchain, which needs the Visual Studio C++ build tools. Clone the repo, put `index.html` in `cache-all\www`, then from PowerShell in `cache-all`:

```powershell
cargo build --release --locked
mkdir www
$env:ROOT = "$PWD\www"
.\target\release\rustwebserver.exe
```

The program file is `target\release\rustwebserver.exe`. Open PowerShell as Administrator so it can listen on port 80. Check with `curl http://127.0.0.1/`.

---

## Troubleshooting / FAQ

### Permission denied on port 80

The server prints:

```text
listen on :80: permission denied
```

You are not root, so the server cannot listen on port 80 yet. Use `sudo`:

```bash
sudo ROOT="$PWD/www" ./target/release/rustwebserver
```

Or grant this one binary the right to listen on port 80, then run it as yourself. A new build replaces the file and clears that right, so run `setcap` again after every rebuild. If `setcap` is missing, install `libcap2-bin`.

```bash
sudo apt install -y libcap2-bin
sudo setcap cap_net_bind_service=+ep ./target/release/rustwebserver
ROOT="$PWD/www" ./target/release/rustwebserver
```

`cap_net_bind_service` is a Linux permission on that file. macOS, FreeBSD, and Windows do not have it.

### The port is already in use

The server prints `listen on :80: the port is in use` and exits. Another program already has port 80. Stop that program. The server stays on port 80.

```bash
sudo ss -ltnp 'sport = :80'
```

The last column names the program. Apache and nginx are the usual ones:

```bash
sudo systemctl stop apache2
sudo systemctl disable apache2
```

```bash
sudo systemctl stop nginx
sudo systemctl disable nginx
```

Also stop an old copy of this server in `screen` or systemd. Only one program can listen on port 80, so visitors are never split between an old and a new copy.

### 404, or "no files found"

Two different messages:

- The server exits and prints `no files found under /root/www`. `ROOT` is wrong, the folder is empty, or the account you used cannot read it. The default `/root/www` is for a root login. Set `ROOT` to the folder that contains `index.html`. The startup line `loaded N files from ...` shows the folder it actually used.
- The server stays running, and a URL returns `Not Found`. That path is not a file in the site folder. `/` serves `index.html`. `/about/` serves `about/index.html`.

Also:

- Names that start with `.` are skipped, so `/.env` and `/.git/` stay hidden. `/.well-known/` is served (for example `/.well-known/acme-challenge/`). A dotfile inside `.well-known` is still hidden.
- Symbolic links are skipped. Put the real file in the site folder.
- The server reads the folder once, at startup. After you add or edit files, restart it.
- To use your own missing-page design, put `404.html` in the top of the site folder. Inside it, start links with `/` (for example `/style.css`), because that page is shown at many addresses.

### It runs out of memory, or CACHE_MEMORY

The server keeps files in RAM up to `CACHE_MEMORY` (by default, one quarter of usable RAM). Anything past that, and any file bigger than `MAX_IN_MEMORY` (2 MiB), is read from disk when a visitor asks for it. A large site still starts. The `loaded` line shows how many files are in memory and how many come from disk.

If the process disappears with no error, the operating system ran out of memory and killed it. Start again with a smaller budget:

```bash
sudo ROOT="$PWD/www" CACHE_MEMORY=128M ./target/release/rustwebserver
```

`128M` is 128 MiB. Lower it again on a very small VPS. Pages keep working. More files are then read from disk.

### A firewall is blocking visitors

`curl http://127.0.0.1/` on the server works, and a browser from outside does not.

The firewall to use is `cloudflare.conf` in this repo. [The firewall file](#the-firewall-file) covers what it does, installing nftables, loading it, keeping it after a reboot, and removing the `inet webfw` table.

If `cloudflare.conf` is loaded, only Cloudflare can open port 80 from the internet. A browser pointed at the raw IP will not connect. Test on the server with `curl` to `127.0.0.1`, which the firewall allows. From outside, open `https://your-domain`.

To see if it is loaded, run `sudo nft list table inet webfw`. If that says the table is missing, load `cloudflare.conf`.

### Visitors still see the old page

Restart the server after any change in the site folder. Sizes and ETags are fixed at startup.

`cache-all` tells browsers they may keep each file for a year. Rename the file, or link to it as `style.css?v=2`. With Cloudflare, purge the cache (Caching, then Configuration, then Purge Everything).

### Fonts, images, or scripts from another site do not load

The server sends a Content Security Policy: a rule that tells the browser to load things from your own site. Open `main.rs`, find `CONTENT_SECURITY_POLICY_VALUE`, and add the other site on the matching line (`font-src` for fonts, `script-src` for scripts, `img-src` for images, `connect-src` for APIs). Rebuild with `cargo build --release --locked`, then restart.

Google Fonts needs `https://fonts.googleapis.com` on the `style-src` line and `https://fonts.gstatic.com` on the `font-src` line.

Responses also send `Strict-Transport-Security` for one year, including every subdomain. Once a browser has seen that over HTTPS, each subdomain needs to work over HTTPS too.

### A form gets 405

The server answers `GET` and `HEAD`. A form or script that sends `POST`, `PUT`, or `DELETE` gets `405 Method Not Allowed`. It serves files that already exist. It does not run PHP, Python, or other site code.

### `cargo: command not found`

```bash
. "$HOME/.cargo/env"
```

New terminals pick this up from the line rustup added to `~/.bashrc`. If `cargo --version` is older than 1.89, run `rustup update`.

### How to stop it

- In the window where it is running, press `Ctrl+C`. The server stops taking new visitors, lets downloads that are already going finish (up to 30 seconds), then exits.
- From `screen`: `screen -rx website`, then `Ctrl+C`.
- With systemd: `sudo systemctl stop rustwebserver`.

`kill` on the process does the same graceful stop as `Ctrl+C`. If systemd is enabled, use `systemctl stop`. Killing the process by hand makes systemd start it again.

### How to update

You do not need to rebuild when you only change files in `www`. Restart the server so it reads them again.

Rebuild after `git pull`, or after you edit `main.rs` or `Cargo.toml`:

```bash
cd "$HOME/rustwebserver"
git pull
cd cache-all
cargo build --release --locked
```

Then start it again (`Ctrl+C` and the run command, or `sudo systemctl restart rustwebserver`). If a build looks stuck on a bad leftover, run `cargo clean` in that folder and build again. `setcap`, if you used it, must be applied again after the build.

---

## Benchmarks

On Tuesday Nov 24, 2026 (4:16–4:22 AM PT), the phoenixNAP dedicated server in Chicago (Intel Xeon E-2388G, 8 cores / 16 threads, 62 GiB RAM, Ubuntu 26.04.1, kernel 7.0, tuned sysctl and BBR) built each server's `cache-all` from main and ran it alone on 127.0.0.1 with no logging, serving the same 7-file site. The keep-alive default is now 120s.

Commits: [rawrust](https://github.com/dumptcp/rawrustwebserver) `9832e7b`, actix `4ec454c`, [golang](https://github.com/dumptcp/golangwebserver) `d63bff2`. The server was pinned with `taskset` to CPUs 0–3 and 8–11 (4 cores / 8 threads) and the load tool, oha 1.17.0, to CPUs 4–7 and 12–15, so the two never competed for a core.

After a 3s warmup, oha ran three 10-second tests at 256 connections (Rust 1.100.0, Go 1.27.4), and the median is reported. The 4096-connection test is one 10-second run. "Home + Brotli" sends `Accept-Encoding: br` (3,128-byte response). "Home plain" sends `Accept-Encoding: identity` (13,976-byte response); the Nov 22 run left the header out, but oha then sends `gzip, compress, deflate, br` by default, so that row was really Brotli too.

**RPS** is requests per second (higher is better); **p50** is typical latency and **p99** is slow latency. **Transfer** is response bytes per second, and memory is peak RSS.

### Quick winner

Home page with Brotli, 256 connections.

| Server | Requests/sec | Avg latency | Typical latency | Slow latency (p99) | Transfer | Memory |
|---|---:|---:|---:|---:|---:|---:|
| [rawrust](https://github.com/dumptcp/rawrustwebserver) | **715,813** | **0.356 ms** | **0.312 ms** | **0.498 ms** | **2.24 GB/s** | **8.4 MB** |
| actix (this server) | 599,974 | 0.425 ms | 0.402 ms | 0.763 ms | 1.88 GB/s | 11.3 MB |
| [golang](https://github.com/dumptcp/golangwebserver) | 508,184 | 0.502 ms | 0.399 ms | 1.788 ms | 1.59 GB/s | 17.8 MB |

### All numbers

Requests per second. Bold is the fastest in that row.

| What | Connections | rawrust | actix | golang |
|---|---:|---:|---:|---:|
| Home + Brotli | 256 | **715,813** | 599,974 | 508,184 |
| Home plain | 256 | **472,350** | 459,861 | 429,407 |
| Home + Brotli | 4096 | **386,559** | 366,650 | 371,058 |

Home plain, 256 connections, typical latency / slow latency (p99) and transfer: rawrust 0.494 ms / 0.760 ms, **6.60 GB/s**; actix 0.563 ms / **0.723 ms**, 6.43 GB/s; golang **0.468 ms** / 2.177 ms, 6.00 GB/s.

### 4096 connections

Home page with Brotli, one 10-second run.

| Server | Requests/sec | Avg latency | Typical latency | Slow latency (p99) | Transfer | Memory |
|---|---:|---:|---:|---:|---:|---:|
| [rawrust](https://github.com/dumptcp/rawrustwebserver) | **386,559** | **10.57 ms** | **10.63 ms** | **11.97 ms** | **1.21 GB/s** | **24.1 MB** |
| actix (this server) | 366,650 | 11.15 ms | 11.11 ms | 14.13 ms | 1.15 GB/s | 78.6 MB |
| [golang](https://github.com/dumptcp/golangwebserver) | 371,058 | 11.01 ms | 10.84 ms | 20.13 ms | 1.16 GB/s | 44.0 MB |

---

## License

MIT — see [LICENSE](LICENSE).
