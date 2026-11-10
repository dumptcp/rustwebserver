# Rust Web Server

A fast static file server written in Rust ([Actix Web](https://actix.rs)). Run a few commands, and your site is live.

**Benchmark:** 89,423 requests/sec (home page with Brotli, 256 connections, `cache-all`). See [Benchmarks](#benchmarks).

---

## Step 1 — Install Rust + update your server

Run these in your terminal, one at a time:

```bash
sudo apt update
```

```bash
sudo apt upgrade -y
```

```bash
sudo apt autoremove -y
```

> ⚠️ **Warning:** the reboot below will take your server offline for a minute or two while it restarts.

```bash
sudo reboot
```

Reconnect to your server after it comes back up, then continue:

```bash
sudo apt install -y build-essential curl git screen
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
```

```bash
echo '. "$HOME/.cargo/env"' >> ~/.bashrc && source "$HOME/.cargo/env"
```

Check it worked (this server needs Rust 1.89 or newer):

```bash
cargo --version
```

---

## Step 2 — Pick a version

Each folder is a complete, standalone server. Pick **one**:

| Folder | Compression | HTML Cache | Asset Cache | Best For |
|---|---|---|---|---|
| `cache-assets-only/` | ✅ Yes | None (always fresh, revalidated with ETag) | 1 year | **Recommended.** Most sites |
| `cache-all/` | ✅ Yes | None (always fresh, revalidated with ETag) | 1 year for `/assets/`, other files revalidated | Hashed asset names in `/assets/` |
| `cache-1-minute/` | ✅ Yes | 1 minute | 1 year | News sites, blogs |
| `no-cache/` | ❌ No | None | None | Dev, debugging |
| `no-cache+compression/` | ✅ Yes | None | None | Fresh + small downloads |
| `cache-all-no-compression/` | ❌ No | 1 year | 1 year | Cached, low CPU |
| `cache-assets-only-no-compression/` | ❌ No | None (always fresh, revalidated with ETag) | 1 year | Fresh HTML, low CPU |
| `cache-1-minute-no-compression/` | ❌ No | 1 minute | 1 year | Frequent edits, low CPU |

All eight folders contain the same code. The only difference is the **settings** block at the top of each `main.rs`, which sets that version's compression, ETags and cache times.

**Legend:**
- **Compression ✅** = Text files (HTML, CSS, JS, SVG, JSON, ...) are pre-compressed with Brotli at the strongest level, and the server sends that to every request that accepts Brotli; every other request gets the file as-is. Cloudflare always asks your server for Brotli and passes it on to visitors unchanged; for the few that can't read Brotli, Cloudflare sends the uncompressed file. The site is online right away; the strongest compression finishes in the background shortly after.
- **Compression ❌** = The server sends files as-is and does no compressing itself, so it uses less CPU at startup.
- **HTML Cache** = How long browsers keep your `index.html`. "None" means every visit gets the latest version.
- **Asset Cache** = How long browsers keep images, CSS, and JS. "1 year" is standard.
- **Video and audio** = Every version supports seeking (byte ranges), which Safari on iPhone, iPad and Mac needs to play media at all.
- **ETag** = The six `cache-*` folders send an `ETag` with every file and answer `304 Not Modified` (no body) when the browser or Cloudflare already has the current version. The two `no-cache` folders don't, on purpose.
- **Cloudflare** = Files are sent with `Cache-Control: no-transform` wherever Cloudflare changing them would only hurt, so Cloudflare passes them on exactly as the server sent them:
  - **Compression ✅ folders:** every file. The server already compressed them, so Cloudflare keeps each file's `ETag` (304s keep working), doesn't compress them a second time, and doesn't insert its own scripts into your pages.
  - **Compression ❌ folders:** only files that are already compressed: images (except SVG and ICO), video, audio, WOFF/WOFF2 fonts, PDFs, ZIPs and other binary files. HTML, CSS, JS, SVG, ICO, TTF/OTF fonts and other text are left open so Cloudflare can compress them for you.
  - Because of this, Cloudflare's **JavaScript Detections** (Security → Bots) and automatic **Web Analytics** script are not added to pages in the Compression ✅ folders. In the Compression ❌ folders they are, and JavaScript Detections removes the `ETag` from your pages, so turn it off (along with **Bot Fight Mode**, which turns it on) if you want pages answered with `304`.

---

## Step 3 — Add your website files

Download this repository and copy the folder you picked into `/root` (replace `cache-assets-only` with your choice from Step 2):

```bash
cd /root && git clone https://github.com/dumptcp/rustwebserver
```

```bash
cp rustwebserver/cache-assets-only/Cargo.toml rustwebserver/cache-assets-only/Cargo.lock rustwebserver/cache-assets-only/main.rs rustwebserver/cloudflare.conf /root/
```

```bash
mkdir -p /root/www
```

Then upload your `index.html` and other site files into `/root/www` (with `scp`, or an SFTP app such as WinSCP, FileZilla or MobaXterm). It should look like this:

```
/root/
├── Cargo.toml
├── Cargo.lock
├── main.rs
├── cloudflare.conf
└── /root/www/
    └── index.html
```

`Cargo.lock` pins the exact library versions this server was tested with, so your build matches it.

---

## Step 4 — Build

```bash
cd /root && cargo build --release --locked
```

---

## Step 5 — Set up Cloudflare

This server is made to sit behind Cloudflare. In your Cloudflare dashboard:

1. **DNS:** point your domain at your server's IP address and leave the record **Proxied** (orange cloud).
2. **SSL/TLS → Overview:** set the encryption mode to **Flexible**.

Flexible is required. This server speaks plain HTTP, and Cloudflare adds HTTPS for your visitors. With **Full** or **Full (strict)**, Cloudflare tries to reach your server over HTTPS and your visitors get an error page.

Recommended: also turn on **Always Use HTTPS** (SSL/TLS → Edge Certificates), so visitors who type `http://` are moved to `https://`.

Recommended: also turn on **Smart Tiered Cache** (Caching → Tiered Cache). It's free on every plan and makes Cloudflare's data centers fetch files from one another instead of each one asking your server.

Cloudflare caches images, CSS, JS, fonts, video and audio by default, but **not HTML or JSON** (it decides by file extension). With `cache-assets-only` that is exactly right: every page is checked with your server (a `304 Not Modified` when nothing changed) and everything else is answered from Cloudflare's cache. If you picked `cache-1-minute` and want Cloudflare to cache your pages too, add a **Cache Rule** (Caching → Cache Rules) for your site with **Eligible for cache**, Edge TTL **Use cache-control header if present**, and Browser TTL **Respect origin**, so the times this server sends are the ones Cloudflare uses. To have Cloudflare cache `.json` files too, edit or add a Cache Rule for **File extension is in** `json` with **Eligible for cache** and Edge TTL **Use cache-control header if present**.

---

## Step 6 — Open the firewall for Cloudflare

> ⚠️ **Warning:** this firewall lets **only Cloudflare** reach your website. Use it only if your domain goes through Cloudflare (Step 5), or nobody will be able to open your site. SSH (port 22) stays open so you can still log in. Loading it replaces any firewall rules the server already has.

Load the included `cloudflare.conf`:

```bash
sudo nft -f cloudflare.conf
```

Verify it's active:

```bash
sudo nft list ruleset
```

The rules are gone after a reboot unless you save them. To keep them:

```bash
sudo cp cloudflare.conf /etc/nftables.conf
```

```bash
sudo systemctl enable nftables
```

---

## Step 7 — Run the server

```bash
sudo ./target/release/rustwebserver
```

Press `Ctrl+C` to stop it. The server stops taking new visitors, lets downloads that are already running finish (for up to 30 seconds), then exits. `kill` does the same.

To keep the server running after you close your terminal, start it inside `screen`:

```bash
screen -S website
```

Inside it, run `cd /root && sudo ./target/release/rustwebserver`, then press `Ctrl+A` and then `D` to leave it running in the background. To come back to it later (for example to press `Ctrl+C` and restart it after changing your site):

```bash
screen -rx website
```

### Settings

Set these before the command above to change where the server looks and listens (for example `sudo ROOT=/srv/site PORT=8080 ./target/release/rustwebserver`):

| Setting | Default | What it does |
|---|---|---|
| `ROOT` | `/root/www` | The folder your site files are in |
| `PORT` | `80` | The port the server listens on |
| `HOST` | every address | The address it listens on. Use `HOST=127.0.0.1` when something on the same machine passes visitors to it, such as a Cloudflare Tunnel (`cloudflared`) or another web server, so the port can't be reached from outside |

---

## Notes + Tips and Tricks

The server raises its own open-file limit at startup. These can still help (run them before starting the server):

```bash
ulimit -n 999999;ulimit -u unlimited;ulimit -e unlimited;ulimit -r unlimited
```

To clear nftables when you want:

```bash
sudo nft flush ruleset
```

Start from a clean build:

```bash
cargo clean
```

---

- This is a **static** server. It serves HTML, CSS, JS, images, and other files. It does **not** run PHP, Python, or any other server-side code.
- It only answers requests that read a page or a file (`GET` and `HEAD`). A form or script that tries to send data to it (`POST`, `PUT`, `DELETE`, ...) gets `405 Method Not Allowed`.
- Files are loaded into memory once at startup. After changing anything in `www`, restart the server (`Ctrl+C`, then start it again). You only need to rebuild when `main.rs` or `Cargo.toml` changes.
- Only one copy can run on a port. If an old copy is still running, a new one stops with `Address already in use`, so visitors are never split between an old and a new version of your site.
- `/about` and `/about/` both open `about/index.html`.
- To show your own "page not found" page, put a `404.html` in the top of `www`. Inside it, start links to your CSS and images with `/` (for example `/style.css`), because the page is shown at many different addresses.
- Pages can only load things from your own site unless you allow more. If you use Google Fonts, analytics, a CDN or embeds, see the next section.
- Images, CSS, JS and every other file that isn't a page are cached for a year in each `cache-*` version (in `cache-all` only files under `/assets/`; everything else there is revalidated with its ETag). When you change one, give it a new name or link to it with `?v=2`, then purge Cloudflare's cache (Caching → Configuration → Purge Everything), or returning visitors keep the old copy.
- `cache-assets-only` is the safest choice for most people.
- Every response tells browsers to use HTTPS for a year on your domain **and all its subdomains** (`Strict-Transport-Security` with `includeSubDomains`). Make sure every subdomain works over HTTPS (with Cloudflare proxying them, they do). Otherwise, remove `; includeSubDomains` from `STRICT_TRANSPORT_SECURITY` in `main.rs`.
- `cloudflare.conf` lists Cloudflare's addresses as published at [cloudflare.com/ips](https://www.cloudflare.com/ips/). They rarely change, but if Cloudflare adds one, update both lists in the file and load it again.

---

## Using fonts, scripts, or embeds from other sites

To protect your visitors, the server tells browsers to load things only from your own site. Anything hosted somewhere else is blocked until you allow it.

Open `main.rs`, find `CONTENT_SECURITY_POLICY_VALUE`, and add the other site to the matching line:

| You want to use | Line to change | What to add |
|---|---|---|
| Google Fonts | `style-src` and `font-src` | `https://fonts.googleapis.com` to `style-src`, `https://fonts.gstatic.com` to `font-src` |
| A script from a CDN | `script-src` | the CDN, for example `https://cdn.jsdelivr.net` |
| Images or video hosted elsewhere | `img-src` or `media-src` | the site they come from, for example `https://images.example.com` |
| A YouTube embed | add a new line | `frame-src https://www.youtube.com; ` |
| Analytics or an API | `connect-src` (and `script-src` for its script) | the site, for example `https://api.example.com` |

For example, with Google Fonts the two lines become:

```
    style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
    font-src 'self' https://fonts.gstatic.com; \
```

Then rebuild with `cargo build --release --locked` and restart the server.

---

## Benchmarks

On Wednesday Nov 11, 2026 (12:11–12:16 AM PT), a DigitalOcean AMD EPYC 9555P (2 CPUs, 8 GB, Ubuntu 26.04.1) ran every command inside `screen -rx website`, stopped the live server first with Ctrl+C in that session, then clean-built each server's `cache-all` right before its own test (`go clean -cache` for Go, `cargo clean` for Rust) and ran it alone on port 80, serving the same site, one at a time in this order: golang, then actix, then rawrust, each stopped before the next started. After a 3s warmup, oha 1.16.0 ran three 10-second tests at 256 connections (Rust 1.99.1, Go 1.27.4), the median of the three runs is reported, and because the load tool shares those CPUs the numbers are for comparison.

**RPS** is requests per second (higher is better); **p50** is typical latency and **p99** is slow latency.

### Quick winner

Home page with Brotli, 256 connections.

| Server | Requests/sec | Typical latency | Slow latency (p99) | Memory |
|---|---:|---:|---:|---:|
| [rawrust](https://github.com/dumptcp/rawrustwebserver) | **92,934** | **2.68 ms** | **5.67 ms** | 11.7 MB |
| actix (this server) | 89,423 | 2.82 ms | 6.28 ms | **10.0 MB** |
| [golang](https://github.com/dumptcp/golangwebserver) | 79,446 | 3.11 ms | 8.14 ms | 19.4 MB |

### All numbers

Requests per second. Bold is the fastest in that row. Every run had 0 errors.

| What | Connections | rawrust | actix | golang |
|---|---:|---:|---:|---:|
| Home + Brotli | 256 | **92,934** | 89,423 | 79,446 |
| Home plain | 256 | **84,411** | 81,930 | 72,178 |

---

## License

MIT — see [LICENSE](LICENSE).
