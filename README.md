# Rust Web Server

A fast static file server written in Rust (Actix Web). Run a few commands, and your site is live.

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

When the server is back you can continue.

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

`build-essential` is required: the compression and allocator crates compile C code.

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
- **Compression ✅** = Files are pre-compressed into Zstd, Brotli, and Gzip at startup. Smaller downloads, faster page loads.
- **Compression ❌** = Files are served as-is. Larger downloads, but lower CPU usage at startup.
- **HTML Cache** = How long browsers keep your `index.html`. "None" means every visit gets the latest version.
- **Asset Cache** = How long browsers keep images, CSS, and JS. "1 year" is standard.
- **ETag** = The six `cache-*` folders send an `ETag` with every file and answer `304 Not Modified` (no body) when the browser or Cloudflare already has the current version. The two `no-cache` folders don't, on purpose.

---

## Step 3 — Add your website files

Put the two server files from the folder you picked in `/root`, and your website in `/root/www`:

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
cd /root
```

```bash
cargo build --release
```

The first build downloads and compiles everything the server needs and takes a few minutes. Later builds take seconds. It creates a `target/` folder and a `Cargo.lock` file next to your two files.

If the build is killed (`signal: 9`), the server ran out of memory. In `Cargo.toml` change `lto = "fat"` to `lto = "thin"` and `codegen-units = 1` to `codegen-units = 4`, then build again.

---

## Step 5 — Open the firewall for Cloudflare

Load the included `cloudflare.conf`:

```bash
sudo nft -f cloudflare.conf
```

Verify it's active:

```bash
sudo nft list ruleset
```

## Step 6 — Run the server

```bash
sudo ./target/release/rustwebserver
```

Press `Ctrl+C` to stop it

## Notes + Tips and Tricks

Can help with gain more performance (run it in the same terminal, before starting the server):

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
- Files are loaded into memory once at startup. After changing anything in `www`, restart the server. You only need to rebuild when `main.rs` or `Cargo.toml` changes.
- If you update a file while using `cache-all`, you may need to clear your browser cache or Cloudflare cache to see the change.
- `cache-assets-only` is the safest choice for most people.
- Built for running behind Cloudflare: idle connections are kept for 960 seconds (longer than Cloudflare's 900 second reuse window, which avoids intermittent 520 errors) and there is no connection cap.

## Differences from the Go version

- File names with spaces or other percent-encoded characters (`/my%20file.html`) are served. The Go version returns 404 for them.
- Compressed sizes for Zstd and Gzip differ by a few bytes because the libraries differ. Brotli output is byte-identical.

## License

MIT — see [LICENSE](LICENSE).
