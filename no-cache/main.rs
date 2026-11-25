// ================================================================ settings
// Version: no-cache (nothing cached, no compression).
// This block is the only difference between the eight versions of this server; the same
// folder of dumptcp/rawrustwebserver and dumptcp/golangwebserver uses the same settings.

/// Keep a Brotli copy (strongest level) of every text file and send it to requests that
/// accept Brotli. Cloudflare always asks for Brotli and decompresses it for visitors
/// that cannot read it.
const COMPRESSION: bool = false;
/// Send an ETag with every file and answer 304 Not Modified while it still matches.
const ETAGS: bool = false;
/// Cache-Control for .html / .htm files, and for every other file.
const HTML_CACHE_CONTROL: &str = "no-store, no-cache, must-revalidate, max-age=0";
const ASSET_CACHE_CONTROL: &str = "no-store, no-cache, must-revalidate, max-age=0";
/// Add `no-transform` only to file types that are already compressed (images, fonts,
/// video, ...), so Cloudflare can still compress text files itself.
const NO_TRANSFORM_ONLY_FOR_PRECOMPRESSED_TYPES: bool = true;
/// Cache-Control for redirects, and for 404 / 405 / 416 responses.
const REDIRECT_CACHE_CONTROL: &str = "no-store, no-cache, must-revalidate, max-age=0";
const ERROR_CACHE_CONTROL: &str = "no-store, no-cache, must-revalidate, max-age=0";

/// Which Cache-Control string a file gets. This is part of the settings block:
/// the other seven folders differ only here and in the constants above.
fn cache_control_for(rel: &str) -> &'static str {
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        HTML_CACHE_CONTROL
    } else {
        ASSET_CACHE_CONTROL
    }
}

// ================================================================ server
use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    ffi::OsStr,
    hash::{BuildHasherDefault, Hasher},
    io::{Read, Write},
    net::ToSocketAddrs,
    path::{Path, PathBuf},
    pin::Pin,
    process,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    task::{Context, Poll},
    thread,
    time::{Duration, Instant},
};

use actix_web::{
    body::{BodySize, MessageBody, SizedStream},
    http::{
        header::{
            HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, ACCEPT_RANGES, ALLOW,
            CACHE_CONTROL, CONNECTION, CONTENT_ENCODING, CONTENT_RANGE, CONTENT_SECURITY_POLICY,
            CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, LOCATION, PERMISSIONS_POLICY, RANGE,
            REFERRER_POLICY, RETRY_AFTER, STRICT_TRANSPORT_SECURITY, VARY, X_CONTENT_TYPE_OPTIONS,
            X_FRAME_OPTIONS,
        },
        KeepAlive, Method, StatusCode,
    },
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use arc_swap::ArcSwap;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

const BROTLI_FAST: u32 = 6;
const BROTLI_BEST: u32 = 11;

const BROTLI_MIN_WINDOW: u32 = 16;
const BROTLI_MAX_WINDOW: u32 = 22;

const RETURN_MEMORY_EVERY: usize = 16 * 1024 * 1024;

const ACCEPT_BR: usize = 1;

/// Idle keep-alive, in seconds. A longer idle timeout lets a flood pin
/// accepted sockets until they close together. 120 s is the default.
/// `KEEP_ALIVE` overrides it and is at least 1.
const DEFAULT_KEEP_ALIVE_SECS: u64 = 120;
/// A request head that is still unfinished after this is closed (408).
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// After the server starts closing a connection, drop it if the client lingers.
const CLIENT_DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Sent bytes that stay unacknowledged this long abort the connection.
const STALLED_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Actix HTTP/1 closes an unfinished request head at 128 KiB
/// (`actix_http::h1::decoder::MAX_BUFFER_SIZE`). `HttpServer` cannot lower it:
/// `h1_write_buffer_size` sizes the response buffer, and payload limits apply
/// to the body. The cap stays 128 KiB. The head budget (usable memory / 16, or
/// `HEAD_MEMORY`) is still computed and printed, matching the no-framework
/// server. That budget would want `budget / max connections` (never above
/// 128 KiB) on a small machine; Actix has nowhere to put that number.
const ACTIX_HEADER_LIMIT: usize = 128 * 1024;
/// Unsent bytes one socket may queue. Without this, a stalled reader pins the
/// kernel's default TCP send buffer, about 184 KiB.
const NOTSENT_LOWAT: u32 = 16 * 1024;
/// Give freed heap back once open connections fall this far under the recent
/// peak. The check runs every `TRIM_EVERY`.
const TRIM_AFTER_DROP: usize = 512;
const TRIM_EVERY: Duration = Duration::from_secs(10);
const DEFAULT_MAX_IN_MEMORY: usize = 2 * 1024 * 1024;
/// Accept-queue length passed to `listen`. Actix's default is 2048, and a
/// 65,535 queue still filled up on a live flood (millions of ListenOverflows)
/// when the accept loop paused. `i32::MAX` is the largest backlog `listen`
/// takes; the kernel clamps it to somaxconn. Startup prints that clamped
/// value (`effective_backlog`), not `i32::MAX`.
const LISTEN_BACKLOG: i32 = i32::MAX;
/// Per-worker ceiling for `max_connections` and `max_connection_rate`.
/// Actix stops calling accept when a worker reaches `max_connections`.
/// `usize::MAX / 2` stays below the wrap in Actix's counter (`limit + 1`).
/// There is no process-wide cap unless `MAX_CONNECTIONS` is a positive
/// number; then the handler answers 503 and the accept loop keeps running.
const ACTIX_ACCEPT_LIMIT: usize = usize::MAX / 2;
/// Per-connection HTTP/1 write buffer. Small so a slow reader cannot pin a large one.
const H1_WRITE_BUFFER: usize = 8 * 1024;
/// Blocking-pool threads per worker. Disk reads use these; each one has a stack.
const BLOCKING_THREADS_PER_WORKER: usize = 32;
const BODY_PIECE: usize = 64 * 1024;

const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'self'; \
    base-uri 'self'; \
    form-action 'self'; \
    frame-ancestors 'none'; \
    img-src 'self' data: blob:; \
    media-src 'self' data: blob:; \
    style-src 'self' 'unsafe-inline'; \
    script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; \
    worker-src 'self' blob:; \
    font-src 'self'; \
    connect-src 'self'; \
    object-src 'none'; \
    upgrade-insecure-requests; \
    require-trusted-types-for 'script'; \
    trusted-types 'none'";

const PERMISSIONS_POLICY_VALUE: &str = "accelerometer=(), autoplay=(), camera=(), \
    display-capture=(), encrypted-media=(), fullscreen=(self), \
    geolocation=(), gyroscope=(), magnetometer=(), microphone=(), \
    midi=(), payment=(), picture-in-picture=(self), publickey-credentials-get=(), \
    screen-wake-lock=(), sync-xhr=(), usb=(), xr-spatial-tracking=()";

const REDIRECT_KEEP: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b',')
    .remove(b':')
    .remove(b';')
    .remove(b'=')
    .remove(b'@');

#[derive(Clone)]
struct Variant {
    body: Bytes,
    enc: Option<&'static str>,
}

/// [original, Brotli]: Cloudflare always asks for `br, gzip` and decompresses Brotli
/// itself for clients without it, so no gzip copy is kept.
type PickTable = [Variant; 2];

/// In-memory bodies use `Bytes` (shared, not leaked). A file is served from
/// disk when it is larger than `MAX_IN_MEMORY` or keeping it (and its Brotli
/// copy) would pass `CACHE_MEMORY`. Disk files keep only path, size and ETag
/// here and are streamed without precompression.
struct File {
    ctype: HeaderValue,
    cc: HeaderValue,
    etag: HeaderValue,
    body: Bytes,
    disk_path: Option<PathBuf>,
    size: usize,
    pick: ArcSwap<PickTable>,
    ranges: bool,
}

enum FileSource {
    Memory {
        body: Vec<u8>,
        br: Option<Bytes>,
    },
    Disk {
        path: PathBuf,
        size: usize,
        hash: u64,
    },
}

/// FxHash: a few multiplies per lookup instead of SipHash's rounds. Every key is a
/// path from `www`, chosen by the site owner, so there is no hash flooding to defend
/// against; requests only look keys up.
#[derive(Default)]
struct FxHasher(u64);

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last));
        }
    }
    #[inline]
    fn write_u8(&mut self, b: u8) {
        self.add(u64::from(b));
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Keyed by path inside `www`. Every folder with an index.html is also listed under
/// its own path ("" for the top, "docs/" for docs/index.html), sharing the same File,
/// so `/` and `/docs/` are found without building a key per request.
type Files = HashMap<String, Arc<File>, BuildHasherDefault<FxHasher>>;

/// The site, loaded once before the server starts. A plain static rather than
/// `web::Data`, whose extractor clones an `Arc` on every request: one counter that
/// every CPU core would update.
static FILES: OnceLock<Files> = OnceLock::new();

fn files() -> &'static Files {
    FILES
        .get()
        .expect("files are loaded before the server starts")
}

/// Whether `key` is a folder entry added by `load` rather than a file of its own.
fn is_folder_key(key: &str) -> bool {
    key.is_empty() || key.ends_with('/')
}

/// Connections doing real work. When `MAX_CONNECTIONS` is a positive cap and
/// this count is already there, the connection is not counted: it is answered
/// with 503 and closed, so a flood cannot pin the cap or stop the accept loop.
/// Unset or `0` stores `usize::MAX`, so that path does not run. Actix keeps
/// the `Extensions` from `on_connect`
/// until the connection dispatcher drops, so `CountedConnection` lives as long
/// as the connection. On glibc the same count tells `malloc_trim` when a burst
/// has drained.
static OPEN_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static CONNECTION_CAP: AtomicUsize = AtomicUsize::new(usize::MAX);

struct CountedConnection;

impl Drop for CountedConnection {
    fn drop(&mut self) {
        OPEN_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Set on a connection that arrived when `OPEN_CONNECTIONS` was already at the cap.
struct OverCapacity;

/// Runs once per accepted connection. `TCP_NODELAY`, and on Linux
/// `TCP_USER_TIMEOUT` (10 seconds) and `TCP_NOTSENT_LOWAT` (16 KiB), are set
/// on the listening socket. An accepted socket inherits them, so they are not
/// set again here. Actix times out request heads and keep-alive, not a body
/// write that has stalled; the inherited user-timeout covers that write.
fn tune_accepted_socket(_conn: &dyn std::any::Any, ext: &mut actix_web::dev::Extensions) {
    let cap = CONNECTION_CAP.load(Ordering::Relaxed);
    let prev = OPEN_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
    if prev >= cap {
        OPEN_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        ext.insert(OverCapacity);
    } else {
        ext.insert(CountedConnection);
    }
}

/// `TCP_NODELAY`, `TCP_USER_TIMEOUT` and `TCP_NOTSENT_LOWAT` on the listener.
/// A failed set is logged and ignored: Actix still sets `TCP_NODELAY` on each
/// accepted socket, and the request timeouts still close a stalled head.
fn inherit_tcp_options(socket: &socket2::Socket) {
    if let Err(e) = socket.set_tcp_nodelay(true) {
        eprintln!("TCP_NODELAY: {e}");
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = socket.set_tcp_user_timeout(Some(STALLED_WRITE_TIMEOUT)) {
            eprintln!("TCP_USER_TIMEOUT: {e}");
        }
        if let Err(e) = socket.set_tcp_notsent_lowat(NOTSENT_LOWAT) {
            eprintln!("TCP_NOTSENT_LOWAT: {e}");
        }
    }
}

/// One listening socket. Same flags Actix's `bind` uses (`SO_REUSEADDR`, and
/// on Windows `IPV6_V6ONLY` off so `::` also takes IPv4), plus the inherited
/// TCP options and `listen(i32::MAX)`.
fn listen_on(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    #[cfg(windows)]
    if addr.is_ipv6() {
        if let Err(e) = socket.set_only_v6(false) {
            eprintln!("IPV6_V6ONLY: {e}");
        }
    }
    inherit_tcp_options(&socket);
    socket.bind(&addr.into())?;
    socket.listen(LISTEN_BACKLOG)?;
    Ok(std::net::TcpListener::from(socket))
}

/// Bind every address `host:port` resolves to. One success is enough, matching
/// Actix `HttpServer::bind`: a failure is returned only when every address fails.
fn listen_all(host: &str, port: u16) -> std::io::Result<Vec<std::net::TcpListener>> {
    let mut listeners = Vec::new();
    let mut last_err = None;
    for addr in (host, port).to_socket_addrs()? {
        match listen_on(addr) {
            Ok(listener) => listeners.push(listener),
            Err(e) => last_err = Some(e),
        }
    }
    if listeners.is_empty() {
        Err(last_err.unwrap_or_else(|| std::io::Error::other("Could not bind to address.")))
    } else {
        Ok(listeners)
    }
}

/// `listen` is called with `i32::MAX`. Linux, macOS and the BSDs clamp that to
/// somaxconn, so the number printed at startup is the queue the kernel will
/// actually use. When the clamp cannot be read, the requested backlog is printed.
fn kernel_somaxconn() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        if let Some(n) = std::fs::read_to_string("/proc/sys/net/core/somaxconn")
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            return Some(n);
        }
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        let mut val: libc::c_int = 0;
        let mut len: libc::size_t = std::mem::size_of::<libc::c_int>();
        let Ok(name) = std::ffi::CString::new("kern.ipc.somaxconn") else {
            return None;
        };
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut val as *mut libc::c_int as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && val > 0 {
            return Some(val as u32);
        }
    }
    None
}

fn effective_backlog() -> u32 {
    let requested = u32::try_from(LISTEN_BACKLOG).unwrap_or(u32::MAX);
    match kernel_somaxconn() {
        Some(n) => requested.min(n),
        None => requested,
    }
}

fn fail_listen(port: u16, err: &std::io::Error) -> ! {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        eprintln!(
            "listen on :{port}: permission denied. Run as root, `sudo setcap cap_net_bind_service=+ep <binary>`, or set PORT=8080 behind a reverse proxy."
        );
    } else if err.kind() == std::io::ErrorKind::AddrInUse {
        eprintln!("listen on :{port}: the port is in use");
    } else {
        eprintln!("listen on :{port}: {err}");
    }
    process::exit(1);
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let site_root = env_str("ROOT", "/root/www");
    let port: u16 = env_str("PORT", "80").parse().unwrap_or_else(|e| {
        eprintln!("bad PORT: {e}");
        process::exit(1);
    });
    let max_in_memory = env_usize("MAX_IN_MEMORY", DEFAULT_MAX_IN_MEMORY);
    let keep_alive_secs = env_u64("KEEP_ALIVE", DEFAULT_KEEP_ALIVE_SECS).max(1);
    let usable = usable_memory();
    let cache_budget = env_byte_size("CACHE_MEMORY", usable / 4);
    let head_budget = head_budget(usable);
    let max_connections = configured_max_connections();
    let workers = thread::available_parallelism().map_or(1, |n| n.get());
    raise_nofile_to_hard();

    // Bind before reading the site. A missing permission or a busy port fails
    // here, instead of after the files have been loaded into RAM.
    let host = env::var("HOST")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            if std::net::TcpListener::bind(("::", 0)).is_ok() {
                "::".to_owned()
            } else {
                "0.0.0.0".to_owned()
            }
        });
    let listeners = listen_all(host.as_str(), port).unwrap_or_else(|e| fail_listen(port, &e));

    let loaded = load(Path::new(&site_root), max_in_memory, cache_budget);
    if loaded.is_empty() {
        eprintln!("no files found under {site_root}");
        process::exit(1);
    }
    let (count, cached_files, cached_bytes, disk_files, disk_bytes) = cache_totals(&loaded);
    println!(
        "loaded {count} files from {site_root} ({cached_files} in memory, {cached_bytes} bytes cached; {disk_files} served from disk, {disk_bytes} bytes; cache budget {cache_budget} bytes)"
    );
    let backlog = effective_backlog();
    let max_connections_label = if max_connections == usize::MAX {
        "unlimited".to_owned()
    } else {
        max_connections.to_string()
    };
    println!(
        "usable memory: {usable} bytes, max connections: {max_connections_label} ({workers} workers), listen backlog: {backlog}, head budget: {head_budget} bytes, keep-alive: {keep_alive_secs} seconds"
    );
    println!(
        "max in-memory file: {max_in_memory} bytes, request head cap: {ACTIX_HEADER_LIMIT} bytes (Actix HTTP/1 limit, not configurable)"
    );
    let _ = FILES.set(loaded);
    CONNECTION_CAP.store(max_connections, Ordering::Relaxed);

    return_large_blocks_when_freed();
    return_freed_memory_to_the_system();
    release_memory_after_bursts();
    if COMPRESSION {
        thread::spawn(|| {
            recompress(files());
            return_freed_memory_to_the_system();
        });
    }

    // `on_connect` is captured when the listener is handed over, so it is set
    // before `.listen`. The socket is already listening: Actix's backlog
    // (default 2048) is not used, and `listen(i32::MAX)` from above stays.
    // Per-worker `max_connections` and `max_connection_rate` are
    // `ACTIX_ACCEPT_LIMIT`, so a worker does not stop accepting.
    // `max_connection_rate` only applies to TLS, which this server does not use.
    let server = HttpServer::new(|| App::new().default_service(web::to(serve)))
        .workers(workers)
        .worker_max_blocking_threads(BLOCKING_THREADS_PER_WORKER)
        .keep_alive(KeepAlive::Timeout(Duration::from_secs(keep_alive_secs)))
        .client_request_timeout(REQUEST_HEAD_TIMEOUT)
        .client_disconnect_timeout(CLIENT_DISCONNECT_TIMEOUT)
        .max_connections(ACTIX_ACCEPT_LIMIT)
        .max_connection_rate(ACTIX_ACCEPT_LIMIT)
        .h1_write_buffer_size(H1_WRITE_BUFFER)
        .tcp_nodelay(true)
        .on_connect(tune_accepted_socket);

    let mut server = server;
    for listener in listeners {
        server = server
            .listen(listener)
            .unwrap_or_else(|e| fail_listen(port, &e));
    }
    println!("listening on :{port}");
    server.run().await
}

async fn serve(req: HttpRequest) -> HttpResponse {
    if req.conn_data::<OverCapacity>().is_some() {
        return overloaded();
    }
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return refuse(req.method());
    }
    let files = files();

    let p = percent_encoding::percent_decode_str(req.path()).decode_utf8_lossy();

    if let Some(dir) = p.strip_suffix("index.html").filter(|d| d.ends_with('/')) {
        let mut res = status(StatusCode::MOVED_PERMANENTLY, REDIRECT_CACHE_CONTROL);
        let location = redirect_target(dir) + &query(&req);
        if let Ok(loc) = HeaderValue::from_str(&location) {
            res.headers_mut().insert(LOCATION, loc);
        }
        return res;
    }

    let key = p.strip_prefix('/').unwrap_or(&p);
    let Some(f) = files.get(key) else {
        if !p.ends_with('/') && files.contains_key(&format!("{key}/")) {
            let mut res = status(StatusCode::MOVED_PERMANENTLY, REDIRECT_CACHE_CONTROL);
            let location = redirect_target(&format!("{p}/")) + &query(&req);
            if let Ok(loc) = HeaderValue::from_str(&location) {
                res.headers_mut().insert(LOCATION, loc);
            }
            return res;
        }
        return not_found(files, &req).await;
    };

    if f.disk_path.is_some() {
        return serve_disk(f, &req).await;
    }

    let not_modified = ETAGS
        && req
            .headers()
            .get(IF_NONE_MATCH)
            .is_some_and(|inm| contains(inm.as_bytes(), f.etag.as_bytes()));
    if not_modified {
        let mut res = HttpResponse::new(StatusCode::NOT_MODIFIED);
        let h = res.headers_mut();
        h.insert(ETAG, f.etag.clone());
        h.insert(CACHE_CONTROL, f.cc.clone());
        if COMPRESSION {
            h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        return res;
    }

    // Shared with the cached file (Bytes refcount), not a copy of the bytes.
    let Variant { mut body, enc } = f.pick.load()[accept_mask(&req)].clone();

    let range = req
        .headers()
        .get(RANGE)
        .filter(|_| f.ranges && req.method() == Method::GET)
        .and_then(|v| v.to_str().ok());
    let mut code = StatusCode::OK;
    let mut content_range = None;
    if let Some(range) = range {
        let current = req
            .headers()
            .get(IF_RANGE)
            .is_none_or(|ir| ir.is_empty() || (ETAGS && ir.as_bytes() == f.etag.as_bytes()));
        if current {
            match parse_range(range, body.len()) {
                ByteRange::Part(start, end) => {
                    content_range = Some(format!("bytes {start}-{end}/{}", body.len()));
                    body = body.slice(start..=end);
                    code = StatusCode::PARTIAL_CONTENT;
                }
                ByteRange::Unsatisfiable => {
                    let mut res = status(StatusCode::RANGE_NOT_SATISFIABLE, ERROR_CACHE_CONTROL);
                    let h = res.headers_mut();
                    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", body.len())) {
                        h.insert(CONTENT_RANGE, v);
                    }
                    return res;
                }
                ByteRange::Ignore => {}
            }
        }
    }

    let mut res = response(code, body);
    let h = res.headers_mut();
    if f.ranges {
        h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    if let Some(v) = content_range.and_then(|v| HeaderValue::from_str(&v).ok()) {
        h.insert(CONTENT_RANGE, v);
    }
    if ETAGS {
        h.insert(ETAG, f.etag.clone());
    }
    h.insert(CACHE_CONTROL, f.cc.clone());
    if COMPRESSION {
        h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    if let Some(enc) = enc {
        h.insert(CONTENT_ENCODING, HeaderValue::from_static(enc));
    }
    res
}

async fn serve_disk(f: &File, req: &HttpRequest) -> HttpResponse {
    let not_modified = ETAGS
        && req
            .headers()
            .get(IF_NONE_MATCH)
            .is_some_and(|inm| contains(inm.as_bytes(), f.etag.as_bytes()));
    if not_modified {
        let mut res = HttpResponse::new(StatusCode::NOT_MODIFIED);
        let h = res.headers_mut();
        h.insert(ETAG, f.etag.clone());
        h.insert(CACHE_CONTROL, f.cc.clone());
        return res;
    }

    let mut start = 0usize;
    let mut end = f.size.saturating_sub(1);
    let mut code = StatusCode::OK;
    let mut content_range = None;
    let range = req
        .headers()
        .get(RANGE)
        .filter(|_| f.ranges && req.method() == Method::GET)
        .and_then(|v| v.to_str().ok());
    if let Some(range) = range {
        let current = req
            .headers()
            .get(IF_RANGE)
            .is_none_or(|ir| ir.is_empty() || (ETAGS && ir.as_bytes() == f.etag.as_bytes()));
        if current {
            match parse_range(range, f.size) {
                ByteRange::Part(s, e) => {
                    content_range = Some(format!("bytes {s}-{e}/{}", f.size));
                    start = s;
                    end = e;
                    code = StatusCode::PARTIAL_CONTENT;
                }
                ByteRange::Unsatisfiable => {
                    let mut res = status(StatusCode::RANGE_NOT_SATISFIABLE, ERROR_CACHE_CONTROL);
                    let h = res.headers_mut();
                    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    if let Ok(v) = HeaderValue::from_str(&format!("bytes */{}", f.size)) {
                        h.insert(CONTENT_RANGE, v);
                    }
                    return res;
                }
                ByteRange::Ignore => {}
            }
        }
    }

    let len = if f.size == 0 { 0 } else { end - start + 1 };
    let path = f.disk_path.as_ref().expect("disk file");
    let mut res = if req.method() == Method::HEAD || len == 0 {
        // BodySize::Sized so Content-Length is the file (or range) length, not 0.
        HttpResponse::build(code).body(EmptySized(len as u64))
    } else {
        match open_range(path, start as u64, len as u64).await {
            Ok(stream) => HttpResponse::build(code).body(stream),
            Err(_) => return not_found(files(), req).await,
        }
    };
    let h = res.headers_mut();
    if f.ranges {
        h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    if let Some(v) = content_range.and_then(|v| HeaderValue::from_str(&v).ok()) {
        h.insert(CONTENT_RANGE, v);
    }
    if ETAGS {
        h.insert(ETAG, f.etag.clone());
    }
    h.insert(CACHE_CONTROL, f.cc.clone());
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    res
}

async fn open_range(
    path: &Path,
    start: u64,
    len: u64,
) -> std::io::Result<SizedStream<ReaderStream<tokio::io::Take<tokio::fs::File>>>> {
    let mut file = tokio::fs::File::open(path).await?;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    Ok(SizedStream::new(len, ReaderStream::new(file.take(len))))
}

fn accept_mask(req: &HttpRequest) -> usize {
    if !COMPRESSION {
        return 0;
    }
    let ae = req
        .headers()
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // What Cloudflare sends on every request; skip parsing it.
    if ae == "gzip, br" || ae == "br, gzip" {
        return ACCEPT_BR;
    }
    accept_mask_of(ae)
}

/// Whether the request accepts Brotli, per RFC 9110 §12.5.3: codings are
/// case-insensitive, `;q=0` refuses one, and `*` stands for every coding not named.
fn accept_mask_of(ae: &str) -> usize {
    let (mut br, mut any) = (None, None);
    for item in ae.split(',') {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or("").trim();
        let refused = parts.any(|p| {
            let p = p.trim();
            // get() rather than [..2]: a non-ASCII byte must not panic (panic = abort)
            p.get(..2).is_some_and(|k| k.eq_ignore_ascii_case("q="))
                && p.get(2..)
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
        });
        let slot = if coding.eq_ignore_ascii_case("br") {
            &mut br
        } else if coding == "*" {
            &mut any
        } else {
            continue;
        };
        *slot = Some(!refused);
    }
    if br.or(any).unwrap_or(false) {
        ACCEPT_BR
    } else {
        0
    }
}

fn security_headers(h: &mut HeaderMap) {
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000; includeSubDomains; preload"),
    );
    h.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    h.insert(
        PERMISSIONS_POLICY,
        HeaderValue::from_static(PERMISSIONS_POLICY_VALUE),
    );
    h.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
}

/// Past `MAX_CONNECTIONS`: accept already happened, answer and close.
/// No body, no cache headers, no file lookup.
fn overloaded() -> HttpResponse {
    let mut res = HttpResponse::build(StatusCode::SERVICE_UNAVAILABLE);
    res.force_close();
    res.insert_header((CONNECTION, "close"));
    res.insert_header((RETRY_AFTER, "1"));
    res.no_chunking(0);
    res.body(Bytes::new())
}

fn refuse(method: &Method) -> HttpResponse {
    const OTHER_HTTP_METHODS: [Method; 7] = [
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::OPTIONS,
        Method::TRACE,
        Method::CONNECT,
    ];
    if !OTHER_HTTP_METHODS.contains(method) {
        return HttpResponse::build(StatusCode::NOT_IMPLEMENTED)
            .insert_header((CONTENT_TYPE, "text/plain; charset=utf-8"))
            .body("Not Implemented");
    }
    let mut res = status(StatusCode::METHOD_NOT_ALLOWED, ERROR_CACHE_CONTROL);
    res.headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    res
}

fn plain_not_found() -> HttpResponse {
    status(StatusCode::NOT_FOUND, ERROR_CACHE_CONTROL)
}

async fn not_found(files: &Files, req: &HttpRequest) -> HttpResponse {
    let Some(f) = files.get("404.html") else {
        return plain_not_found();
    };
    if f.disk_path.is_some() {
        return disk_not_found(f, req).await;
    }
    let Variant { body, enc } = f.pick.load()[accept_mask(req)].clone();
    let mut res = response(StatusCode::NOT_FOUND, body);
    let h = res.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static(ERROR_CACHE_CONTROL));
    if COMPRESSION {
        h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    if let Some(enc) = enc {
        h.insert(CONTENT_ENCODING, HeaderValue::from_static(enc));
    }
    res
}

/// `404.html` past the cache budget is streamed from disk with status 404.
/// A missing file on disk falls back to the short plain 404, and does not call
/// `not_found` again.
async fn disk_not_found(f: &File, req: &HttpRequest) -> HttpResponse {
    let len = f.size as u64;
    let path = f.disk_path.as_ref().expect("disk 404");
    let mut res = if req.method() == Method::HEAD || len == 0 {
        HttpResponse::build(StatusCode::NOT_FOUND).body(EmptySized(len))
    } else {
        match open_range(path, 0, len).await {
            Ok(stream) => HttpResponse::build(StatusCode::NOT_FOUND).body(stream),
            Err(_) => return plain_not_found(),
        }
    };
    let h = res.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static(ERROR_CACHE_CONTROL));
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    res
}

/// `body` is a refcounted slice of the cached file.
fn response(code: StatusCode, body: Bytes) -> HttpResponse {
    if body.len() > BODY_PIECE {
        HttpResponse::build(code).body(Pieces(body))
    } else {
        HttpResponse::build(code).body(body)
    }
}

struct Pieces(Bytes);

impl MessageBody for Pieces {
    type Error = Infallible;

    fn size(&self) -> BodySize {
        BodySize::Sized(self.0.len() as u64)
    }

    fn poll_next(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Infallible>>> {
        if self.0.is_empty() {
            return Poll::Ready(None);
        }
        let n = self.0.len().min(BODY_PIECE);
        Poll::Ready(Some(Ok(self.0.split_to(n))))
    }
}

struct EmptySized(u64);

impl MessageBody for EmptySized {
    type Error = Infallible;

    fn size(&self) -> BodySize {
        BodySize::Sized(self.0)
    }

    fn poll_next(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Infallible>>> {
        Poll::Ready(None)
    }
}

fn status(code: StatusCode, cache_control: &'static str) -> HttpResponse {
    let mut res = HttpResponse::build(code).body(code.canonical_reason().unwrap_or(""));
    let h = res.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    res
}

fn redirect_target(dir: &str) -> String {
    let encoded = utf8_percent_encode(dir, REDIRECT_KEEP).to_string();
    format!("/{}", encoded.trim_start_matches('/'))
}

fn query(req: &HttpRequest) -> String {
    let q = req.query_string();
    if q.is_empty() || !q.bytes().all(|b| b.is_ascii_graphic()) {
        return String::new();
    }
    format!("?{q}")
}

enum ByteRange {
    Ignore,
    Part(usize, usize),
    Unsatisfiable,
}

fn parse_range(header: &str, size: usize) -> ByteRange {
    const UNIT: &str = "bytes=";
    let unit_ok = header
        .get(..UNIT.len())
        .is_some_and(|u| u.eq_ignore_ascii_case(UNIT));
    if size == 0 || !unit_ok {
        return ByteRange::Ignore;
    }
    let spec = header[UNIT.len()..].trim();
    if spec.contains(',') {
        return ByteRange::Ignore;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return ByteRange::Ignore;
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        return match parse_pos(last) {
            None => ByteRange::Ignore,
            Some(0) => ByteRange::Unsatisfiable,
            Some(n) => ByteRange::Part(size - n.min(size), size - 1),
        };
    }

    let Some(start) = parse_pos(first) else {
        return ByteRange::Ignore;
    };
    let mut end = size - 1;
    if !last.is_empty() {
        match parse_pos(last) {
            Some(e) if e >= start => end = end.min(e),
            _ => return ByteRange::Ignore,
        }
    }
    if start >= size {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Part(start, end)
}

fn parse_pos(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for d in s.bytes() {
        if !d.is_ascii_digit() {
            return None;
        }
        n = n.saturating_mul(10).saturating_add(usize::from(d - b'0'));
    }
    Some(n)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn fnv1a(data: &[u8]) -> u64 {
    fnv1a_acc(0xcbf2_9ce4_8422_2325, data)
}

fn fnv1a_acc(mut h: u64, data: &[u8]) -> u64 {
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hash_file(path: &Path) -> std::io::Result<(u64, usize)> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut size = 0usize;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size = size.saturating_add(n);
        h = fnv1a_acc(h, &buf[..n]);
    }
    Ok((h, size))
}

/// `/.well-known/` is served (ACME and similar). Every other dotfile and dotdir
/// is skipped. Symlinks are not followed, and a symlink is not a regular file,
/// so `is_file()` below skips it too.
fn include_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name();
    name == OsStr::new(".well-known") || !name.to_string_lossy().starts_with('.')
}

fn load(root: &Path, max_in_memory: usize, cache_budget: u64) -> Files {
    let mut files = Files::default();
    let mut cached_bytes: u64 = 0;
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(include_entry);

    for entry in walker.flatten() {
        // Symlinks are not files when follow_links is off, so they are skipped.
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let len = meta.len();
        // A file that cannot fit, even before its Brotli copy, is not read in.
        let source =
            if len > max_in_memory as u64 || cached_bytes.saturating_add(len) > cache_budget {
                let Ok((hash, size)) = hash_file(path) else {
                    continue;
                };
                FileSource::Disk {
                    path: path.to_path_buf(),
                    size,
                    hash,
                }
            } else {
                let Ok(body) = std::fs::read(path) else {
                    continue;
                };
                let ctype = content_type(&rel);
                let br = brotli_for(&ctype, &body);
                let cost = (body.len() as u64)
                    .saturating_add(br.as_ref().map(|c| c.len() as u64).unwrap_or(0));
                if cached_bytes.saturating_add(cost) > cache_budget {
                    let hash = fnv1a(&body);
                    let size = body.len();
                    FileSource::Disk {
                        path: path.to_path_buf(),
                        size,
                        hash,
                    }
                } else {
                    cached_bytes = cached_bytes.saturating_add(cost);
                    FileSource::Memory { body, br }
                }
            };
        add(&mut files, rel, source);
    }
    let folders: Vec<(String, Arc<File>)> = files
        .iter()
        .filter_map(|(k, f)| {
            let dir = k.strip_suffix("index.html")?;
            (dir.is_empty() || dir.ends_with('/')).then(|| (dir.to_owned(), Arc::clone(f)))
        })
        .collect();
    files.extend(folders);
    files
}

fn cache_totals(files: &Files) -> (usize, usize, u64, usize, u64) {
    let mut cached_files = 0usize;
    let mut cached_bytes = 0u64;
    let mut disk_files = 0usize;
    let mut disk_bytes = 0u64;
    for (key, f) in files {
        if is_folder_key(key) {
            continue;
        }
        if f.disk_path.is_some() {
            disk_files += 1;
            disk_bytes = disk_bytes.saturating_add(f.size as u64);
        } else {
            cached_files += 1;
            cached_bytes = cached_bytes.saturating_add(f.body.len() as u64);
            let table = f.pick.load();
            if table[ACCEPT_BR].enc.is_some() {
                cached_bytes = cached_bytes.saturating_add(table[ACCEPT_BR].body.len() as u64);
            }
        }
    }
    (
        cached_files + disk_files,
        cached_files,
        cached_bytes,
        disk_files,
        disk_bytes,
    )
}

fn add(files: &mut Files, rel: String, source: FileSource) {
    let ctype = content_type(&rel);
    let base = cache_control_for(&rel);
    let cc = if NO_TRANSFORM_ONLY_FOR_PRECOMPRESSED_TYPES && !compressible(&ctype) {
        format!("{base}, no-transform")
    } else {
        base.to_owned()
    };
    let (body, disk_path, size, etag_hash, br) = match source {
        FileSource::Memory { body, br } => {
            let hash = fnv1a(&body);
            let size = body.len();
            (Bytes::from(body), None, size, hash, br)
        }
        FileSource::Disk { path, size, hash } => (Bytes::new(), Some(path), size, hash, None),
    };
    let etag = format!("\"{:016x}\"", etag_hash);
    let ranges = br.is_none();

    files.insert(
        rel,
        Arc::new(File {
            ctype: static_value(ctype, "application/octet-stream"),
            cc: static_value(cc, ASSET_CACHE_CONTROL),
            etag: static_value(etag, "\"0\""),
            pick: ArcSwap::from_pointee(pick_table(&body, br)),
            ranges,
            body,
            disk_path,
            size,
        }),
    );
}

/// Fast Brotli of `body` when this version compresses that type and the copy is smaller.
fn brotli_for(ctype: &str, body: &[u8]) -> Option<Bytes> {
    if COMPRESSION && compressible(ctype) {
        smaller(brotli_of(body, BROTLI_FAST, BROTLI_MAX_WINDOW), body.len())
    } else {
        None
    }
}

/// Build a header value without leaking it for the process lifetime.
fn static_value(value: String, fallback: &'static str) -> HeaderValue {
    HeaderValue::try_from(value).unwrap_or_else(|_| HeaderValue::from_static(fallback))
}

fn recompress(files: &Files) {
    let start = Instant::now();
    let todo: Vec<&File> = files
        .iter()
        .filter(|(k, f)| !is_folder_key(k) && !f.ranges)
        .map(|(_, f)| &**f)
        .collect();
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));

    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut replaced = 0;
                while let Some(f) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let old = f.pick.load_full();
                    let current = &old[ACCEPT_BR];
                    // The strongest copy is kept until the server exits, so it is static.
                    let br = brotli_of(&f.body, BROTLI_BEST, brotli_window(f.body.len()))
                        .filter(|c| !c.is_empty() && c.len() < current.body.len())
                        .map(Bytes::from)
                        .or_else(|| current.enc.map(|_| current.body.clone()));
                    f.pick.store(Arc::new(pick_table(&f.body, br)));

                    replaced += compressed_bytes(&old);
                    drop(old);
                    if replaced >= RETURN_MEMORY_EVERY {
                        replaced = 0;
                        return_freed_memory_to_the_system();
                    }
                }
            });
        }
    });
    let _ = writeln!(
        std::io::stdout(),
        "recompressed {} files at the strongest levels in {:.1?}",
        todo.len(),
        start.elapsed()
    );
}

fn smaller(compressed: Option<Vec<u8>>, than: usize) -> Option<Bytes> {
    compressed
        .filter(|c| !c.is_empty() && c.len() < than)
        .map(|c| Bytes::from(c.into_boxed_slice()))
}

fn compressed_bytes(table: &PickTable) -> usize {
    table[ACCEPT_BR]
        .enc
        .map_or(0, |_| table[ACCEPT_BR].body.len())
}

fn pick_table(body: &Bytes, br: Option<Bytes>) -> PickTable {
    let original = Variant {
        body: body.clone(),
        enc: None,
    };
    let brotli = match br {
        Some(c) if c.len() < body.len() => Variant {
            body: c,
            enc: Some("br"),
        },
        _ => original.clone(),
    };
    [original, brotli]
}

fn content_type(rel: &str) -> String {
    let ext = Path::new(rel)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let fixed = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "webmanifest" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        "yaml" | "yml" => "text/x-yaml; charset=utf-8",
        "toml" => "text/x-toml; charset=utf-8",
        "ini" | "conf" | "cfg" | "log" => "text/plain; charset=utf-8",
        "xslt" | "dtd" => "text/xml; charset=utf-8",
        "vcf" => "text/x-vcard; charset=utf-8",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tgz" => "application/x-compressed",
        "rar" => "application/x-rar-compressed",

        "ico" => "image/x-icon",
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "jxl" => "image/jxl",
        "heic" => "image/heic",

        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",

        "mp4" => "video/mp4",
        "m4v" => "video/x-m4v",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "3gp" => "video/3gpp",
        "ts" => "video/mp2t",
        "m3u8" => "application/vnd.apple.mpegurl",
        "mpd" => "application/dash+xml",

        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        "mid" | "midi" => "audio/mid",
        "aif" | "aiff" => "audio/aiff",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_owned();
    }

    match mime_guess::from_ext(&ext).first_raw() {
        Some(m) if m.starts_with("text/") => format!("{m}; charset=utf-8"),
        Some(m) => m.to_owned(),
        None => "application/octet-stream".to_owned(),
    }
}

fn compressible(ctype: &str) -> bool {
    ctype.starts_with("text/")
        || [
            "javascript",
            "json",
            "svg",
            "xml",
            "x-icon",
            "font/ttf",
            "font/otf",
            "application/wasm",
        ]
        .iter()
        .any(|s| ctype.contains(s))
}

fn brotli_of(b: &[u8], quality: u32, window: u32) -> Option<Vec<u8>> {
    let mut w =
        brotli::CompressorWriter::new(Vec::with_capacity(b.len() / 2), 4096, quality, window);
    w.write_all(b).ok()?;
    Some(w.into_inner())
}

fn brotli_window(size: usize) -> u32 {
    let mut lgwin = BROTLI_MIN_WINDOW;
    while lgwin < BROTLI_MAX_WINDOW && (1usize << lgwin) - 16 < size {
        lgwin += 1;
    }
    lgwin
}

fn return_large_blocks_when_freed() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_TRIM_THRESHOLD: i32 = -1;
        const M_MMAP_THRESHOLD: i32 = -3;
        unsafe {
            mallopt(M_TRIM_THRESHOLD, 128 * 1024);
            mallopt(M_MMAP_THRESHOLD, 128 * 1024);
        }
    }
}

/// glibc keeps freed allocations in its arenas. Every 10 seconds, if open
/// connections have fallen by more than 512 from the recent peak, hand that
/// memory back. Skipped without glibc: `malloc_trim` is a glibc call, and musl
/// already returns free pages to the OS.
fn release_memory_after_bursts() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        thread::spawn(|| {
            let mut peak = 0usize;
            loop {
                thread::sleep(TRIM_EVERY);
                let open = OPEN_CONNECTIONS.load(Ordering::Relaxed);
                if open > peak {
                    peak = open;
                } else if peak - open > TRIM_AFTER_DROP {
                    return_freed_memory_to_the_system();
                    peak = open;
                }
            }
        });
    }
}

fn return_freed_memory_to_the_system() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}

fn env_str(key: &str, fallback: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn env_usize(key: &str, fallback: usize) -> usize {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v.parse().unwrap_or_else(|e| {
            eprintln!("bad {key}: {e}");
            process::exit(1);
        }),
        _ => fallback,
    }
}

fn env_u64(key: &str, fallback: u64) -> u64 {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v.parse().unwrap_or_else(|e| {
            eprintln!("bad {key}: {e}");
            process::exit(1);
        }),
        _ => fallback,
    }
}

/// `CACHE_MEMORY`: a byte count, or a count with a suffix (`512M`, `2G`).
/// The suffix is a power of 1024. `512M` is 512 MiB.
fn env_byte_size(key: &str, fallback: u64) -> u64 {
    match env::var(key) {
        Ok(v) if !v.is_empty() => parse_byte_size(&v).unwrap_or_else(|e| {
            eprintln!("bad {key}: {e}");
            process::exit(1);
        }),
        _ => fallback,
    }
}

fn parse_byte_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    let split = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if split == 0 {
        return Err(format!("expected a size like 512M or 2G, got {raw}"));
    }
    let (num, suffix) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("expected a size like 512M or 2G, got {raw}"))?;
    let mult: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "ki" | "kib" => 1024,
        "m" | "mb" | "mi" | "mib" => 1024 * 1024,
        "g" | "gb" | "gi" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "ti" | "tib" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("expected a size like 512M or 2G, got {raw}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("{raw} is too large"))
}

/// RAM this process may use: the smaller of `MemTotal` and the tightest cgroup
/// limit above it. cgroup v2 `memory.max` is tried first (`max` means unlimited
/// and is skipped). When that file is missing, cgroup v1
/// `memory.limit_in_bytes` is read, and the huge unlimited value is ignored.
/// If `/proc/self/cgroup` has no `0::/path` line, `/sys/fs/cgroup/memory.max`
/// is read on its own. Off Linux, or when none of those can be read, use the
/// host's physical RAM, then 1 GiB.
fn usable_memory() -> u64 {
    #[cfg(target_os = "linux")]
    {
        match (mem_total_bytes(), cgroup_memory_limit()) {
            (Some(total), Some(limit)) => total.min(limit),
            (Some(total), None) => total,
            (None, Some(limit)) => limit,
            (None, None) => fallback_usable_memory(),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        fallback_usable_memory()
    }
}

fn fallback_usable_memory() -> u64 {
    physical_memory().unwrap_or(1024 * 1024 * 1024)
}

fn physical_memory() -> Option<u64> {
    #[cfg(unix)]
    {
        // A negative result means this host does not report the value.
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if let (Ok(pages), Ok(page)) = (u64::try_from(pages), u64::try_from(page)) {
            if pages > 0 && page > 0 {
                return Some(pages.saturating_mul(page));
            }
        }
        None
    }
    #[cfg(windows)]
    {
        return windows_physical_memory();
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(windows)]
fn windows_physical_memory() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(lpbuffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 && status.total_phys > 0 {
        Some(status.total_phys)
    } else {
        None
    }
}

/// `MAX_CONNECTIONS` when it is a positive number. Unset or `0` is no cap.
fn configured_max_connections() -> usize {
    let n = env_usize("MAX_CONNECTIONS", 0);
    if n == 0 {
        usize::MAX
    } else {
        n
    }
}

/// Raise the soft open-file limit to the hard limit. `increase_nofile_limit`
/// will not pass the hard limit, and does nothing when the soft limit is
/// already there. Windows has no Unix rlimit.
fn raise_nofile_to_hard() {
    #[cfg(unix)]
    {
        let hard = match rlimit::Resource::NOFILE.get() {
            Ok((_, hard)) => hard,
            Err(e) => {
                eprintln!("could not read open file limit: {e}");
                return;
            }
        };
        match rlimit::increase_nofile_limit(hard) {
            Ok(n) => println!("open file limit: {n}"),
            Err(e) => eprintln!("could not raise open file limit: {e}"),
        }
    }
}

/// Bytes that may be held in request heads. `HEAD_MEMORY` overrides it.
/// Actix cannot apply this as a per-connection cap; see `ACTIX_HEADER_LIMIT`.
fn head_budget(usable: u64) -> u64 {
    env_u64("HEAD_MEMORY", usable / 16)
}

#[cfg(target_os = "linux")]
fn mem_total_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
        return Some(kb.saturating_mul(1024));
    }
    None
}

/// Smallest numeric cgroup memory limit from this process up to the root.
/// v2 `memory.max` wins when it names a number. Otherwise v1
/// `memory.limit_in_bytes` is used. `None` when every level is unlimited or
/// unreadable.
#[cfg(target_os = "linux")]
fn cgroup_memory_limit() -> Option<u64> {
    let v2 = match self_cgroup_rel() {
        Some(rel) => tightest_memory_max(&rel),
        None => read_memory_max(Path::new("/sys/fs/cgroup/memory.max")),
    };
    if v2.is_some() {
        return v2;
    }
    cgroup_v1_memory_limit()
}

/// cgroup v1 reports "unlimited" as a huge page-aligned integer (often
/// 9223372036854771712). Anything at or above 1 EiB is that sentinel, not a
/// real cap.
#[cfg(target_os = "linux")]
const CGROUP_V1_UNLIMITED: u64 = 1 << 60;

#[cfg(target_os = "linux")]
fn cgroup_v1_memory_limit() -> Option<u64> {
    let mut best = read_memory_limit_v1("/sys/fs/cgroup/memory/memory.limit_in_bytes");
    if let Some(rel) = self_cgroup_v1_rel() {
        let root = PathBuf::from("/sys/fs/cgroup/memory");
        let mut dir = root.clone();
        if !rel.is_empty() {
            dir.push(&rel);
        }
        loop {
            if let Some(limit) = read_memory_limit_v1(dir.join("memory.limit_in_bytes")) {
                best = Some(best.map_or(limit, |so_far| so_far.min(limit)));
            }
            if dir == root || !dir.starts_with(&root) {
                break;
            }
            if !dir.pop() {
                break;
            }
        }
    }
    best
}

/// `id:controllers:path` line whose controllers include `memory`.
#[cfg(target_os = "linux")]
fn self_cgroup_v1_rel() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let _id = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if controllers.split(',').any(|c| c == "memory") {
            return Some(path.trim().trim_start_matches('/').to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_memory_limit_v1(path: impl AsRef<Path>) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let n = text.trim().parse::<u64>().ok()?;
    if n == 0 || n >= CGROUP_V1_UNLIMITED {
        None
    } else {
        Some(n)
    }
}

/// Path inside the cgroup fs from a `0::/path` line, or `None` when that line
/// is missing. `/` and an empty path both mean the root cgroup.
#[cfg(target_os = "linux")]
fn self_cgroup_rel() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Some(path.trim().trim_start_matches('/').to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn tightest_memory_max(rel: &str) -> Option<u64> {
    let root = PathBuf::from("/sys/fs/cgroup");
    let mut dir = root.clone();
    if !rel.is_empty() {
        dir.push(rel);
    }
    let mut best: Option<u64> = None;
    loop {
        if let Some(limit) = read_memory_max(dir.join("memory.max")) {
            best = Some(best.map_or(limit, |so_far| so_far.min(limit)));
        }
        if dir == root || !dir.starts_with(&root) {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    best
}

/// `memory.max` is a byte count, or `max` when the controller sets no limit.
#[cfg(target_os = "linux")]
fn read_memory_max(path: impl AsRef<Path>) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() || text == "max" {
        return None;
    }
    text.parse().ok()
}
