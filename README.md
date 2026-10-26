# Rust Web Server

A fast static file server written in Rust ([Actix Web](https://actix.rs)). Run a few commands, and your site is live.

**Benchmark:** about 90,000 requests per second from one CPU core of a 2-CPU DigitalOcean server, with no failed requests. See [Benchmarks](#benchmarks).

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
sudo apt install -y build-essential curl
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
```

```bash
echo '. "$HOME/.cargo/env"' >> ~/.bashrc && source "$HOME/.cargo/env"
```

Check it worked:

```bash
cargo --version
```

---

## Step 2 — Pick a version

Each folder is a complete, standalone server. Pick **one**:

| Folder | Compression | HTML Cache | Asset Cache | Best For |
|---|---|---|---|---|
| `cache-assets-only/` | ✅ Yes | None (always fresh, revalidated with ETag) | 1 year | **Recommended.** Most sites |
| `cache-all/` | ✅ Yes | 1 year | 1 year | Sites that never change |
| `cache-1-minute/` | ✅ Yes | 1 minute | 1 year | News sites, blogs |
| `no-cache/` | ❌ No | None | None | Dev, debugging |
| `no-cache+compression/` | ✅ Yes | None | None | Fresh + small downloads |
| `cache-all-no-compression/` | ❌ No | 1 year | 1 year | Cached, low CPU |
| `cache-assets-only-no-compression/` | ❌ No | None (always fresh, revalidated with ETag) | 1 year | Fresh HTML, low CPU |
| `cache-1-minute-no-compression/` | ❌ No | 1 minute | 1 year | Frequent edits, low CPU |

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

Put the `Cargo.toml` and `main.rs` from the folder you picked in `/root`, and your `index.html` and other site files in `/root/www`:

```
/root/
├── Cargo.toml
├── main.rs
└── /root/www/
    └── index.html
```

---

## Step 4 — Build

```bash
cargo build --release
```

---

## Step 5 — Set up Cloudflare

This server is made to sit behind Cloudflare. In your Cloudflare dashboard:

1. **DNS:** point your domain at your server's IP address and leave the record **Proxied** (orange cloud).
2. **SSL/TLS → Overview:** set the encryption mode to **Flexible**.

Flexible is required. This server speaks plain HTTP, and Cloudflare adds HTTPS for your visitors. With **Full** or **Full (strict)**, Cloudflare tries to reach your server over HTTPS and your visitors get an error page.

Recommended: also turn on **Always Use HTTPS** (SSL/TLS → Edge Certificates), so visitors who type `http://` are moved to `https://`.

Recommended: also turn on **Smart Tiered Cache** (Caching → Tiered Cache). It's free on every plan and makes Cloudflare's data centers fetch files from one another instead of each one asking your server.

Cloudflare caches images, CSS, JS, fonts, video and audio by default, but **not HTML** (it decides by file extension). With `cache-assets-only` that is exactly right: every page is checked with your server (a `304 Not Modified` when nothing changed) and everything else is answered from Cloudflare's cache. If you picked `cache-all` or `cache-1-minute` and want Cloudflare to cache your pages too, add a **Cache Rule** (Caching → Cache Rules) for your site with **Eligible for cache**, Edge TTL **Use cache-control header if present**, and Browser TTL **Respect origin**, so the times this server sends are the ones Cloudflare uses.

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

Press `Ctrl+C` to stop it.

### Settings

Set these before the command above to change where the server looks and listens (for example `sudo ROOT=/srv/site PORT=8080 ./target/release/rustwebserver`):

| Setting | Default | What it does |
|---|---|---|
| `ROOT` | `/root/www` | The folder your site files are in |
| `PORT` | `80` | The port the server listens on |
| `HOST` | every address | The address it listens on. Use `HOST=127.0.0.1` when something on the same machine passes visitors to it, such as a Cloudflare Tunnel (`cloudflared`) or another web server, so the port can't be reached from outside |

---

## Notes + Tips and Tricks

Can help gain more performance (run it before starting the server):

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
- Files are loaded into memory once at startup. After changing anything in `www`, restart the server. You only need to rebuild when `main.rs` or `Cargo.toml` changes.
- `/about` and `/about/` both open `about/index.html`.
- To show your own "page not found" page, put a `404.html` in the top of `www`. Inside it, start links to your CSS and images with `/` (for example `/style.css`), because the page is shown at many different addresses.
- Pages can only load things from your own site unless you allow more. If you use Google Fonts, analytics, a CDN or embeds, see the next section.
- If you update a file while using `cache-all`, you may need to clear your browser cache or Cloudflare cache to see the change.
- `cache-assets-only` is the safest choice for most people.

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

Then rebuild with `cargo build --release` and restart the server.

---

## Benchmarks

`cache-all` on a DigitalOcean server with 2 CPUs (AMD EPYC 9555P, Ubuntu 26.04, Rust 1.99.1), built from source on the server and serving a real site: one 14 KB page (sent as 3.3 KB with Brotli) and a 2 KB icon. Measured on October 26, 2026.

| Test | Requests per second | Requests per CPU-second | Half of responses within | 99% of responses within |
|---|---|---|---|---|
| Home page, Brotli (256 connections) | 89,319 | 99,194 | 1.4 ms | 3.5 ms |
| `304 Not Modified` (256 connections) | 96,001 | 111,055 | 1.3 ms | 3.3 ms |
| Both files in turn (256 connections) | 88,812 | 98,900 | 1.4 ms | 3.5 ms |
| New connection every 10 requests (256 connections) | 57,230 | 70,985 | 2.1 ms | 4.1 ms |
| Home page, Brotli (1,000 connections) | 85,661 | 96,056 | 5.8 ms | 12.1 ms |

- **Requests per CPU-second** is how many requests one CPU core can answer. With only 2 CPUs, the load generator had one of them and could not quite push the server to its limit, so this is a little higher than the requests per second.
- **No failed requests** in 18.8 million.
- **Memory:** 5.3 MB idle, 23.2 MB at most with 1,000 connections open.
- **Faster version:** [dumptcp/rawrustwebserver](https://github.com/dumptcp/rawrustwebserver) is the same server without a web framework. On the same server it answered 4–8% more requests per CPU-second.

**How it was measured:** the server on CPU 0 (one worker), the load generator (wrk 4.1.0, one thread) on CPU 1, over the loopback interface. HTTP/1.1 with keep-alive, requests sent the way Cloudflare sends them (`Accept-Encoding: br, gzip`), 15 seconds per test after a 3 second warm-up, 3 rounds, averaged. Your numbers will differ: a real server has a network in between, and Cloudflare's cache answers most requests before they reach it.

---

## License

MIT — see [LICENSE](LICENSE).
