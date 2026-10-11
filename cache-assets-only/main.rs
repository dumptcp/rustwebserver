use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    io::Write,
    path::Path,
    pin::Pin,
    process,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    thread,
    time::{Duration, Instant},
};

use actix_web::{
    body::{BodySize, MessageBody},
    http::{
        header::{
            HeaderMap, HeaderValue, ACCEPT_ENCODING, ACCEPT_RANGES, CACHE_CONTROL,
            CONTENT_ENCODING, CONTENT_RANGE, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG,
            IF_NONE_MATCH, IF_RANGE, LOCATION, PERMISSIONS_POLICY, RANGE, REFERRER_POLICY,
            STRICT_TRANSPORT_SECURITY, VARY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
        KeepAlive, Method, StatusCode,
    },
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use arc_swap::ArcSwap;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

const HTML_CACHE_CONTROL: &str = "no-cache, must-revalidate";
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable, no-transform";

// Files are compressed ahead of time, never per request, and in two steps so
// that a restart never keeps the site offline. Before the server starts
// listening, every file is compressed at the fast levels. Then, while the site
// is already online, every file is compressed again at the strongest levels in
// the background and swapped in. Visitors always get a compressed file;
// shortly after startup they get the smallest one possible.
const ZSTD_FAST: i32 = 3;
const ZSTD_BEST: i32 = 19;
const BROTLI_FAST: u32 = 6;
const BROTLI_BEST: u32 = 11;
const GZIP_FAST: u32 = 6;
const GZIP_BEST: u32 = 9;

// Brotli looks for repeats inside a window, given as a power of two. The fast
// level uses 4 MB for every file. The strongest level needs memory in
// proportion to its window, so there each file gets the smallest window that
// covers it (64 KB to 4 MB); see brotli_window.
const BROTLI_MIN_WINDOW: u32 = 16;
const BROTLI_MAX_WINDOW: u32 = 22;

// While the background step runs, the versions made at startup are freed file
// by file. Each time this many bytes of them have been replaced, the freed
// memory is handed back to the operating system, so that it does not pile up
// until the step is done.
const RETURN_MEMORY_EVERY: usize = 16 * 1024 * 1024;

// Bits of the index into a pick table: which encodings the client accepts.
const ACCEPT_ZSTD: usize = 1;
const ACCEPT_BR: usize = 2;
const ACCEPT_GZIP: usize = 4;

// ---- Cloudflare-facing connection policy: never be the side that says no ----
//
// Cloudflare pools connections to the origin and reuses them for up to 900 s
// after the last request. If we close an idle connection first, Cloudflare can
// fire a request into a dead socket and the visitor gets an intermittent 520.
// So our idle timeout must outlast theirs. (Go version: IdleTimeout 960 s.)
const KEEP_ALIVE: Duration = Duration::from_secs(960);
// Only reaps sockets that connect and then never finish sending a request
// head. Cloudflare always sends the full head at once, so real traffic never
// hits this; it just stops dead sockets from leaking forever.
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(60);
// Per-worker cap. Actix defaults to 25k and stops accepting above that;
// this makes it effectively unlimited.
const MAX_CONNECTIONS: usize = 10_000_000;
// Pending-accept queue size requested from the kernel.
const BACKLOG: u32 = 65_535;
// A file larger than this is handed to a connection in pieces of this size
// instead of all at once. Actix copies what it is handed into the connection's
// own buffer, and keeps that buffer for as long as the connection stays open.
// Handed over in one piece, a 50 MB video would cost 50 MB of memory for every
// connection that ever asked for it.
const BODY_PIECE: usize = 64 * 1024;

// This policy only allows resources from your own site. If your pages load
// fonts, scripts, images, video or embeds from another site, add that site to
// the matching line (see the README), or they will be blocked.
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
    upgrade-insecure-requests";

const PERMISSIONS_POLICY_VALUE: &str = "accelerometer=(), autoplay=(), camera=(), \
    display-capture=(), encrypted-media=(), fullscreen=(self), \
    geolocation=(), gyroscope=(), magnetometer=(), microphone=(), \
    midi=(), payment=(), picture-in-picture=(self), publickey-credentials-get=(), \
    screen-wake-lock=(), sync-xhr=(), usb=(), xr-spatial-tracking=()";

// Everything except these characters is percent-encoded in a redirect target.
// It is the same set the Go version leaves alone.
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

/// One way of sending a file: the bytes, and the Content-Encoding they need
/// (`None` for the original bytes). `Bytes` is refcounted, so handing a body
/// to a response is a pointer copy, never a memcpy.
#[derive(Clone)]
struct Variant {
    body: Bytes,
    enc: Option<&'static str>,
}

/// For every combination of encodings a client can accept (the index is a mix
/// of the ACCEPT_* bits), the smallest version of the file that client can
/// read. Entry 0 is always the original bytes.
type PickTable = [Variant; 8];

struct File {
    ctype: HeaderValue,
    cc: HeaderValue,
    etag: HeaderValue,
    /// The original bytes.
    body: Bytes,
    /// Replaced as a whole when the background step finishes this file, so a
    /// request sees either the old table or the new one.
    pick: ArcSwap<PickTable>,
    /// True when the file has no compressed versions (video, audio, images,
    /// PDFs, ...). Only those are served in byte ranges, which is what video
    /// and audio players need for seeking, and what Safari needs to play at
    /// all.
    ranges: bool,
}

type Files = HashMap<String, File>;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let site_root = env_str("ROOT", "/root/www");
    let port: u16 = env_str("PORT", "80").parse().unwrap_or_else(|e| {
        eprintln!("bad PORT: {e}");
        process::exit(1);
    });

    // Go raises the open-file soft limit to the hard limit at startup; Rust
    // doesn't. Without this the server stalls at ~1000 connections.
    match rlimit::increase_nofile_limit(u64::MAX) {
        Ok(n) => println!("open file limit: {n}"),
        Err(e) => eprintln!("could not raise open file limit: {e}"),
    }

    let files = load(Path::new(&site_root));
    if files.is_empty() {
        eprintln!("no files found under {site_root}");
        process::exit(1);
    }
    println!("loaded {} files from {site_root}", files.len());
    let files = web::Data::new(files);

    // The site goes online now; the strongest compression follows in the
    // background. Memory that loading no longer needs goes back to the
    // operating system first, and so does what the background step frees.
    return_large_blocks_when_freed();
    return_freed_memory_to_the_system();
    let background = files.clone();
    thread::spawn(move || {
        recompress(&background);
        return_freed_memory_to_the_system();
    });

    let server = HttpServer::new(move || {
        App::new()
            .app_data(files.clone())
            .default_service(web::to(serve))
    })
    .keep_alive(KeepAlive::Timeout(KEEP_ALIVE))
    .client_request_timeout(REQUEST_HEAD_TIMEOUT)
    .max_connections(MAX_CONNECTIONS)
    .backlog(BACKLOG)
    // Send what is written right away instead of letting the kernel hold the
    // last bytes back while it waits for more (Nagle's algorithm). Go does
    // this for every connection by default.
    .tcp_nodelay(true);

    // Dual-stack like Go's ":80"; use IPv4 only if the host has IPv6 disabled.
    let host = if std::net::TcpListener::bind(("::", 0)).is_ok() {
        "::"
    } else {
        "0.0.0.0"
    };
    let server = server.bind((host, port)).unwrap_or_else(|e| {
        eprintln!("listen on :{port}: {e}");
        process::exit(1);
    });
    println!("listening on :{port}");
    server.run().await
}

async fn serve(files: web::Data<Files>, req: HttpRequest) -> HttpResponse {
    // The path arrives percent-encoded, so "/my%20file.html" would never match
    // a file named "my file.html". Decode it first, like the Go version does.
    let p = percent_encoding::percent_decode_str(req.path()).decode_utf8_lossy();

    if let Some(dir) = p.strip_suffix("index.html").filter(|d| d.ends_with('/')) {
        let mut res = status(StatusCode::MOVED_PERMANENTLY, ASSET_CACHE_CONTROL);
        let location = redirect_target(dir) + &query(&req);
        if let Ok(loc) = HeaderValue::from_str(&location) {
            res.headers_mut().insert(LOCATION, loc);
        }
        return res;
    }

    let key = p.strip_prefix('/').unwrap_or(&p);
    let f = if key.is_empty() || key.ends_with('/') {
        files.get(&format!("{key}index.html"))
    } else {
        files.get(key)
    };
    let Some(f) = f else {
        // "/about" when the site has an "about" folder: send the browser to
        // "/about/", which is where that folder's index.html is served.
        if !p.ends_with('/') && files.contains_key(&format!("{key}/index.html")) {
            let mut res = status(StatusCode::MOVED_PERMANENTLY, ASSET_CACHE_CONTROL);
            let location = redirect_target(&format!("{p}/")) + &query(&req);
            if let Ok(loc) = HeaderValue::from_str(&location) {
                res.headers_mut().insert(LOCATION, loc);
            }
            return res;
        }
        return not_found(&files, &req);
    };

    // Conditional request: the browser (or Cloudflare) already holds this exact
    // version, so answer 304 with no body instead of resending the file.
    // Substring match like the Go version, which also accepts the W/"..." form
    // proxies use and comma-separated lists.
    let not_modified = req
        .headers()
        .get(IF_NONE_MATCH)
        .is_some_and(|inm| contains(inm.as_bytes(), f.etag.as_bytes()));
    if not_modified {
        let mut res = HttpResponse::new(StatusCode::NOT_MODIFIED);
        let h = res.headers_mut();
        h.insert(ETAG, f.etag.clone());
        h.insert(CACHE_CONTROL, f.cc.clone());
        h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        return res;
    }

    // Every client gets the smallest version it said it can read. A client
    // that sends no Accept-Encoding at all gets the original bytes.
    let Variant { mut body, enc } = f.pick.load()[accept_mask(&req)].clone();

    // Byte ranges, for the files that have no compressed versions.
    let range = req
        .headers()
        .get(RANGE)
        .filter(|_| f.ranges && req.method() == Method::GET)
        .and_then(|v| v.to_str().ok());
    let mut code = StatusCode::OK;
    let mut content_range = None;
    if let Some(range) = range {
        // If-Range means "only send the part if the file is still this
        // version"; when it is not, the whole file is sent instead.
        let current = req
            .headers()
            .get(IF_RANGE)
            .is_none_or(|ir| ir.is_empty() || ir.as_bytes() == f.etag.as_bytes());
        if current {
            match parse_range(range, body.len()) {
                ByteRange::Part(start, end) => {
                    content_range = Some(format!("bytes {start}-{end}/{}", body.len()));
                    body = body.slice(start..=end);
                    code = StatusCode::PARTIAL_CONTENT;
                }
                ByteRange::Unsatisfiable => {
                    let mut res = status(StatusCode::RANGE_NOT_SATISFIABLE, "no-store");
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
    h.insert(ETAG, f.etag.clone());
    h.insert(CACHE_CONTROL, f.cc.clone());
    h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    if let Some(enc) = enc {
        h.insert(CONTENT_ENCODING, HeaderValue::from_static(enc));
    }
    res
}

/// Reads which encodings the client accepts, as the index into a file's pick
/// table.
fn accept_mask(req: &HttpRequest) -> usize {
    let ae = req
        .headers()
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut mask = 0;
    if ae.contains("zstd") {
        mask |= ACCEPT_ZSTD;
    }
    if ae.contains("br") {
        mask |= ACCEPT_BR;
    }
    if ae.contains("gzip") {
        mask |= ACCEPT_GZIP;
    }
    mask
}

/// Adds the headers that tell the browser how to protect the visitor. They go
/// on every page, including the 404 page.
fn security_headers(h: &mut HeaderMap) {
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    h.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    h.insert(
        PERMISSIONS_POLICY,
        HeaderValue::from_static(PERMISSIONS_POLICY_VALUE),
    );
}

/// Answers 404. If the site has a 404.html in its top folder, that page is
/// shown; otherwise the visitor gets a plain "Not Found".
fn not_found(files: &Files, req: &HttpRequest) -> HttpResponse {
    let Some(f) = files.get("404.html") else {
        return status(StatusCode::NOT_FOUND, "no-store");
    };
    let Variant { body, enc } = f.pick.load()[accept_mask(req)].clone();
    let mut res = response(StatusCode::NOT_FOUND, body);
    let h = res.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    if let Some(enc) = enc {
        h.insert(CONTENT_ENCODING, HeaderValue::from_static(enc));
    }
    res
}

/// A response with this status and these bytes as its body. A large body goes
/// out piece by piece; see BODY_PIECE.
fn response(code: StatusCode, body: Bytes) -> HttpResponse {
    if body.len() > BODY_PIECE {
        HttpResponse::build(code).body(Pieces(body))
    } else {
        HttpResponse::build(code).body(body)
    }
}

/// A response body that gives out a file's bytes one piece at a time. Each
/// piece is a view into the bytes already in memory, so nothing is copied
/// here.
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

/// Same as Fiber's SendStatus: the reason phrase as a small text body.
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

/// Turns a decoded directory path into a Location that can only ever point
/// back at this site. Every unusual character is percent-encoded again, so a
/// browser cannot reinterpret a `\` or a tab as part of the address, and the
/// leading slashes are collapsed to one, so `//other.example/` stays a path on
/// this site instead of becoming another host.
fn redirect_target(dir: &str) -> String {
    let encoded = utf8_percent_encode(dir, REDIRECT_KEEP).to_string();
    format!("/{}", encoded.trim_start_matches('/'))
}

/// The request's query string with its `?`, so that a redirect keeps it:
/// `/about?ref=x` goes to `/about/?ref=x`. It is passed on only when it is
/// made of plain visible characters; anything else is left out.
fn query(req: &HttpRequest) -> String {
    let q = req.query_string();
    if q.is_empty() || !q.bytes().all(|b| b.is_ascii_graphic()) {
        return String::new();
    }
    format!("?{q}")
}

enum ByteRange {
    /// Not a single valid byte range: send the whole file.
    Ignore,
    /// Send bytes start..=end.
    Part(usize, usize),
    /// Starts past the end of the file: answer 416.
    Unsatisfiable,
}

/// Reads a Range header holding one byte range: `bytes=0-99`, `bytes=100-` or
/// `bytes=-500` (the last 500 bytes). Anything else, including a list of
/// several ranges, is ignored, which HTTP allows: the client then simply
/// receives the whole file.
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
        // "-N": the last N bytes
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

/// Parses a plain run of digits. Numbers too large to be a position in any
/// file are capped rather than rejected.
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

/// FNV-1a 64: the same hash the Go version uses (hash/fnv), so both servers
/// produce identical ETags for identical files. Stable across restarts; it
/// only changes when the file's contents change.
fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn load(root: &Path) -> Files {
    let mut files = HashMap::new();
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        // skip dotfiles and whole dot-directories (but never the root itself)
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));

    for entry in walker.flatten() {
        // symlinks are not followed, so this is "regular files only" like Go
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(body) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        add(&mut files, rel, body);
    }
    files
}

fn add(files: &mut Files, rel: String, body: Vec<u8>) {
    let ctype = content_type(&rel);
    let lower = rel.to_ascii_lowercase();
    let cc = if lower.ends_with(".html") || lower.ends_with(".htm") {
        HTML_CACHE_CONTROL
    } else {
        ASSET_CACHE_CONTROL
    };
    let etag = format!("\"{:016x}\"", fnv1a(&body));
    let body = Bytes::from(body);

    // Startup compression at the fast levels. A version is kept only if it is
    // actually smaller than the original.
    let (mut zs, mut br, mut gz) = (None, None, None);
    if compressible(&ctype) {
        zs = smaller(zstd::bulk::compress(&body, ZSTD_FAST).ok(), body.len());
        br = smaller(brotli_of(&body, BROTLI_FAST, BROTLI_MAX_WINDOW), body.len());
        gz = smaller(gzip_of(&body, GZIP_FAST), body.len());
    }
    let ranges = zs.is_none() && br.is_none() && gz.is_none();

    files.insert(
        rel,
        File {
            ctype: HeaderValue::from_str(&ctype)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            cc: HeaderValue::from_static(cc),
            etag: HeaderValue::from_str(&etag).expect("etag is ascii"),
            pick: ArcSwap::from_pointee(pick_table(&body, zs, br, gz)),
            ranges,
            body,
        },
    );
}

/// The background step: compresses every compressible file again at the
/// strongest levels and swaps the result in. It uses half of the CPU cores so
/// the server stays responsive while it runs.
fn recompress(files: &Files) {
    let start = Instant::now();
    let todo: Vec<&File> = files.values().filter(|f| !f.ranges).collect();
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));

    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                // Bytes of startup versions this worker has replaced since it
                // last handed freed memory back to the operating system.
                let mut replaced = 0;
                while let Some(f) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let old = f.pick.load_full();
                    // Per encoding, keep whichever is smaller: the new result
                    // or the one from startup.
                    let keep = |new: Option<Vec<u8>>, current: &Variant| {
                        smaller(new, current.body.len())
                            .or_else(|| current.enc.map(|_| current.body.clone()))
                    };
                    let zs = keep(
                        zstd::bulk::compress(&f.body, ZSTD_BEST).ok(),
                        &old[ACCEPT_ZSTD],
                    );
                    let br = keep(
                        brotli_of(&f.body, BROTLI_BEST, brotli_window(f.body.len())),
                        &old[ACCEPT_BR],
                    );
                    let gz = keep(gzip_of(&f.body, GZIP_BEST), &old[ACCEPT_GZIP]);
                    f.pick.store(Arc::new(pick_table(&f.body, zs, br, gz)));

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
    // Not println!: that stops the whole server when the line cannot be
    // written, and by now the terminal the server was started from may be
    // closed, or its output piped into a program that has ended.
    let _ = writeln!(
        std::io::stdout(),
        "recompressed {} files at the strongest levels in {:.1?}",
        todo.len(),
        start.elapsed()
    );
}

/// Returns the compressed bytes only if they are non-empty and smaller than
/// `than` bytes. They are first cut down to exactly their own size: the buffer
/// a compressor writes into is much larger than its result, and would
/// otherwise stay in memory for as long as the server runs.
fn smaller(compressed: Option<Vec<u8>>, than: usize) -> Option<Bytes> {
    compressed
        .filter(|c| !c.is_empty() && c.len() < than)
        .map(|c| Bytes::from(c.into_boxed_slice()))
}

/// How many bytes the compressed versions in a pick table take up.
fn compressed_bytes(table: &PickTable) -> usize {
    [ACCEPT_ZSTD, ACCEPT_BR, ACCEPT_GZIP]
        .iter()
        .filter(|&&accept| table[accept].enc.is_some())
        .map(|&accept| table[accept].body.len())
        .sum()
}

/// Works out, for every combination of encodings a client can accept, which
/// version of the file is smallest. A `None` version does not exist.
fn pick_table(body: &Bytes, zs: Option<Bytes>, br: Option<Bytes>, gz: Option<Bytes>) -> PickTable {
    std::array::from_fn(|mask| {
        let mut best = Variant {
            body: body.clone(),
            enc: None,
        };
        // Ties go to the first one listed: br, then zstd, then gzip.
        for (bit, candidate, enc) in [
            (ACCEPT_BR, &br, "br"),
            (ACCEPT_ZSTD, &zs, "zstd"),
            (ACCEPT_GZIP, &gz, "gzip"),
        ] {
            if let Some(c) = candidate.as_ref().filter(|_| mask & bit != 0) {
                if c.len() < best.body.len() {
                    best = Variant {
                        body: c.clone(),
                        enc: Some(enc),
                    };
                }
            }
        }
        best
    })
}

/// Content types for the extensions a website normally uses, the same list as
/// the Go version. Anything not listed falls back to a built-in MIME table.
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
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "zip" => "application/zip",

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
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_owned();
    }

    // Stand-in for Go's mime.TypeByExtension, including its habit of
    // tagging text/* types with a utf-8 charset.
    match mime_guess::from_ext(&ext).first_raw() {
        Some(m) if m.starts_with("text/") => format!("{m}; charset=utf-8"),
        Some(m) => m.to_owned(),
        None => "application/octet-stream".to_owned(),
    }
}

/// Text-like formats, plus WebAssembly and uncompressed fonts. Images, video,
/// audio and woff/woff2 fonts are already compressed, so compressing them
/// again would only waste time.
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
    Some(w.into_inner()) // into_inner() finishes the stream
}

/// The window for a file of the given size at the strongest level: just large
/// enough to reach back over the whole file. A larger window cannot find
/// anything more and only costs memory, a lot of it at that level.
///
/// The fast level does not use this. It needs little memory whatever the
/// window, and with a window of 64 KB this encoder would switch to a search
/// table of 32 MB for every file, which makes the startup far slower.
fn brotli_window(size: usize) -> u32 {
    let mut lgwin = BROTLI_MIN_WINDOW;
    while lgwin < BROTLI_MAX_WINDOW && (1usize << lgwin) - 16 < size {
        lgwin += 1;
    }
    lgwin
}

/// Makes the C library's allocator give every large block of memory (128 KB
/// and up) back to the operating system the moment it is freed. Left alone, it
/// keeps freed blocks for itself, and compressing every file frees a great
/// many of them: the server would sit on several times the memory it needs for
/// as long as it runs.
///
/// This is called once, when the files are loaded and before any other thread
/// is started. Not earlier: while the files load, reusing freed blocks is what
/// keeps the startup fast.
fn return_large_blocks_when_freed() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_TRIM_THRESHOLD: i32 = -1;
        const M_MMAP_THRESHOLD: i32 = -3;
        // SAFETY: mallopt takes no pointers and only changes two settings of
        // the allocator. No other thread exists yet, so nothing can be using
        // the allocator while they change.
        unsafe {
            mallopt(M_TRIM_THRESHOLD, 128 * 1024);
            mallopt(M_MMAP_THRESHOLD, 128 * 1024);
        }
    }
}

/// Gives the memory that has been freed so far back to the operating system.
/// This is called once the files are loaded, at intervals while the
/// background compression runs, and when it is done.
fn return_freed_memory_to_the_system() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim takes no pointers, only releases memory that
        // is not in use, and may be called from any thread.
        unsafe {
            malloc_trim(0);
        }
    }
}

fn gzip_of(b: &[u8], level: u32) -> Option<Vec<u8>> {
    let mut w = flate2::write::GzEncoder::new(
        Vec::with_capacity(b.len() / 2),
        flate2::Compression::new(level),
    );
    w.write_all(b).ok()?;
    w.finish().ok()
}

fn env_str(key: &str, fallback: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}
