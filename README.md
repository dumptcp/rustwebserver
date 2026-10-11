# Rust Web Server

A fast static file server written in Rust ([Actix Web](https://actix.rs)). Run a few commands, and your site is live.

**Benchmark:** about 248,000 requests per second on 4 CPU cores, with no failed requests. See [Benchmarks](#benchmarks).

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
- **Compression ✅** = Files are pre-compressed into Zstd, Brotli, and Gzip, and the server answers each request with the smallest version that request can accept. The site is online right away; the strongest compression finishes in the background shortly after.
- **Compression ❌** = The server sends files as-is and does no compressing itself, so it uses less CPU at startup.
- **HTML Cache** = How long browsers keep your `index.html`. "None" means every visit gets the latest version.
- **Asset Cache** = How long browsers keep images, CSS, and JS. "1 year" is standard.
- **Video and audio** = Every version supports seeking (byte ranges), which Safari on iPhone, iPad and Mac needs to play media at all.
- **ETag** = The six `cache-*` folders send an `ETag` with every file and answer `304 Not Modified` (no body) when the browser or Cloudflare already has the current version. The two `no-cache` folders don't, on purpose.
- **Cloudflare** = The server never tells Cloudflare how to treat your files. Whether Cloudflare compresses them, compresses them again, or passes them on unchanged is up to your own Cloudflare settings.

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

`cache-assets-only` with the server limited to 4 CPU cores:

| Test | Requests per second | Half of responses within | 99% of responses within |
|---|---|---|---|
| Small file | 248,000 | 1.0 ms | 2.9 ms |
| HTML page, compressed (33 KB sent as 5 KB) | 238,000 | 1.0 ms | 3.0 ms |
| HTML page, uncompressed (33 KB) | 201,000 | 1.2 ms | 3.6 ms |
| JavaScript file, compressed (269 KB sent as 26 KB) | 210,000 | 1.2 ms | 3.4 ms |
| File the browser already has (`304 Not Modified`) | 252,000 | 1.0 ms | 2.9 ms |
| Page not found (`404`) | 246,000 | 1.0 ms | 3.0 ms |
| Image (120 KB) | 91,800 | 1.4 ms | 1.7 ms |
| Video, 1 MB piece | 12,900 | 4.9 ms | 5.6 ms |
| Video, whole 20 MB file | 620 | 51 ms | 69 ms |

Requests per second with 1, 2 and 4 CPU cores:

| CPU cores | Small file | HTML page, compressed |
|---|---|---|
| 1 | 86,600 | 82,800 |
| 2 | 152,000 | 145,000 |
| 4 | 248,000 | 238,000 |

- **No failed requests** in over 48 million.
- **Large files** went out at 11 to 13 GB per second.
- **1,000 connections open at once:** 229,000 requests per second, 99% of them answered within 5.0 ms.
- **Memory:** 28 MB with a 21 MB site loaded, and 30 MB with 1,000 connections open.
- **Startup:** online in 0.2 seconds.
- All eight versions are within 2% of each other.

**How it was measured:** on one machine (Intel Core Ultra 7 270K Plus, Windows 11, Rust 1.98), with the server on 4 cores running one worker per core, as it does on its own on a 4-core Linux server, and the load generator on the other cores. The two were connected directly, with no network in between. HTTP/1.1 with keep-alive over 256 connections (128 for the image, 64 and 32 for the two video tests), 5 seconds per test after a 1 second warm-up, averaged over two runs. The 1 and 2 core tests ran once, for 4 seconds, with 64 and 128 connections, and the other seven versions ran a shorter set of the same tests once. Your numbers will differ: a real server has a network in between, runs Linux, and often has fewer or slower cores.

---

## License

MIT — see [LICENSE](LICENSE).
