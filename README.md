# Rust Web Server

A fast static file server written in Rust ([Actix Web](https://actix.rs)). Run a few commands, and your site is live.

**Benchmark:** about 98,000 requests per second from one CPU core of a 2-CPU DigitalOcean server, answering 22–24% more requests per CPU-second than the Golang Web Server ([dumptcp/golangwebserver](https://github.com/dumptcp/golangwebserver)). The no-framework Rust webserver ([dumptcp/rawrustwebserver](https://github.com/dumptcp/rawrustwebserver)) answered 6–10% more than this server. See [Benchmarks](#benchmarks).

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

Cloudflare caches images, CSS, JS, fonts, video and audio by default, but **not HTML or JSON** (it decides by file extension). With `cache-assets-only` that is exactly right: every page is checked with your server (a `304 Not Modified` when nothing changed) and everything else is answered from Cloudflare's cache. If you picked `cache-all` or `cache-1-minute` and want Cloudflare to cache your pages too, add a **Cache Rule** (Caching → Cache Rules) for your site with **Eligible for cache**, Edge TTL **Use cache-control header if present**, and Browser TTL **Respect origin**, so the times this server sends are the ones Cloudflare uses. To have Cloudflare cache `.json` files too, edit or add a Cache Rule for **File extension is in** `json` with **Eligible for cache** and Edge TTL **Use cache-control header if present**.

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
- Files are loaded into memory once at startup. After changing anything in `www`, restart the server (`Ctrl+C`, then start it again). You only need to rebuild when `main.rs` or `Cargo.toml` changes.
- `/about` and `/about/` both open `about/index.html`.
- To show your own "page not found" page, put a `404.html` in the top of `www`. Inside it, start links to your CSS and images with `/` (for example `/style.css`), because the page is shown at many different addresses.
- Pages can only load things from your own site unless you allow more. If you use Google Fonts, analytics, a CDN or embeds, see the next section.
- If you update a file while using `cache-all`, you may need to clear your browser cache or Cloudflare cache to see the change.
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

### No-framework Rust webserver versus Actix Web versus Golang Web Server

`cache-all` of the **no-framework Rust webserver** ([dumptcp/rawrustwebserver](https://github.com/dumptcp/rawrustwebserver)), **Actix Web** ([dumptcp/rustwebserver](https://github.com/dumptcp/rustwebserver)) and the **Golang Web Server** ([dumptcp/golangwebserver](https://github.com/dumptcp/golangwebserver), Fiber v3), on a DigitalOcean server with 2 CPUs (AMD EPYC 9555P, Ubuntu 26.04, Rust 1.99.1, Go 1.27.3). Measured in November 2026.

This was run the same way you would run it by hand. In the `screen` session that runs the live site, the running server was stopped with Ctrl+C. Then the `cache-all` folder of each webserver was cleaned and built (`cargo clean && cargo build --release --locked` for the two Rust ones, `go clean -cache` and the build command from the Go README for Go) and started on port 80, serving a real site: a 10 KB home page (2.0 KB with Brotli) and a 3.9 KB CSS file (1.2 KB with Brotli). Each server was tested and then stopped with Ctrl+C. Each one was started 3 times, in rotating order, and the results were averaged.

**Requests per CPU-second** (higher is better):

| Test | No-framework Rust webserver | Actix Web | Golang Web Server |
|---|---|---|---|
| Home page, Brotli | **111,467** | 101,100 | 83,171 |
| `304 Not Modified` | **121,222** | 114,778 | 92,786 |
| CSS file, Brotli | **112,995** | 104,504 | 84,965 |

**Requests per second:**

| Test | No-framework Rust webserver | Actix Web | Golang Web Server |
|---|---|---|---|
| Home page, Brotli | 100,756 | 98,217 | 83,290 |
| `304 Not Modified` | 112,173 | 111,659 | 92,873 |
| CSS file, Brotli | 101,048 | 101,560 | 85,047 |

- **Requests per CPU-second** is how many requests one CPU core can answer. It is the fair comparison here: with only 2 CPUs, the load generator had one of them and could not push the two Rust servers to their limit, which is why their requests per second are so close. The Golang Web Server used all of its CPU core, so its two numbers are the same.
- **Result:** the no-framework Rust webserver answered 6–10% more requests per CPU-second than Actix Web and 31–34% more than the Golang Web Server. Actix Web answered 22–24% more than the Golang Web Server.
- **Latency:** half of all responses came back in 1.1–1.3 ms with both Rust servers and 2.7–3.1 ms with Go. The slowest 1% took 2.1–2.2 ms (no-framework Rust), 2.3–2.6 ms (Actix Web) and 4.1–4.6 ms (Go).
- **Memory:** idle, 4.8 MB (no-framework Rust), 5.2 MB (Actix Web) and 12.1 MB (Go). With 1,000 connections open, 26 MB, 22 MB and 34 MB.
- **Clean build:** 21 seconds (no-framework Rust, 1.9 MB program), 47 seconds (Actix Web, 2.4 MB) and 28 seconds (Go, 10.8 MB).
- **No failed requests**, and every server stopped cleanly on Ctrl+C every time.

**How it was measured:** the server pinned to CPU 0 (`taskset -c 0`), the load generator (wrk 4.1.0, one thread, 256 connections) on CPU 1, over the loopback interface. HTTP/1.1 with keep-alive, 7 seconds per test after a 2 second warm-up, with `Accept-Encoding: gzip, br`. Your numbers will differ: a real server has a network in between, and Cloudflare's cache answers most requests before they reach it.

---

## License

MIT — see [LICENSE](LICENSE).
