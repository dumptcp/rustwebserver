//! rawstatic: dumptcp/rustwebserver's `cache-1-minute-no-compression` behaviour on a purpose-built
//! HTTP/1.1 engine instead of a framework.
//!
//! * one current-thread Tokio runtime per core, each with its own SO_REUSEPORT
//!   listener (the layout of HttpArena's fastest Rust engine entry);
//! * requests parsed with `httparse`, the fuzzed parser hyper itself uses;
//! * every 200/304/404 response head is built once per file and per encoding, and
//!   rebuilt when the background recompression swaps in smaller bodies, so a request
//!   only appends the Date line;
//! * large bodies go out with writev straight from the shared `Bytes`, no copy.
//!
//! Anything with a request body (Content-Length > 0 or Transfer-Encoding) is answered
//! and the connection closed, so a body can never be misread as the next request.

// ================================================================ settings
// Version: cache-1-minute-no-compression (HTML cached for 1 minute, other files for a year, no compression).
// This block is the only difference between the eight versions of this server; it
// matches what the actix version in the same folder of dumptcp/rustwebserver sends.

/// Keep a Brotli copy (strongest level) of every text file and send it to requests that
/// accept Brotli. Cloudflare always asks for Brotli and decompresses it for visitors
/// that cannot read it.
const COMPRESSION: bool = false;
/// Send an ETag with every file and answer 304 Not Modified while it still matches.
const ETAGS: bool = true;
/// Cache-Control for .html / .htm files, and for every other file.
const HTML_CACHE_CONTROL: &str = "public, max-age=60";
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Add `no-transform` only to file types that are already compressed (images, fonts,
/// video, ...), so Cloudflare can still compress text files itself.
const NO_TRANSFORM_ONLY_FOR_PRECOMPRESSED_TYPES: bool = true;
/// Cache-Control for redirects, and for 404 / 405 / 416 responses.
const REDIRECT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const ERROR_CACHE_CONTROL: &str = "no-store";


use std::{
    cell::RefCell,
    io::Write,
    collections::HashMap,
    io::{self, IoSlice},
    net::SocketAddr,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use http::StatusCode;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};

const KEEP_ALIVE: Duration = Duration::from_secs(960);
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// An unfinished request head this long gets 431 (actix's limit, MAX_BUFFER_SIZE).
const MAX_HEAD: usize = 128 * 1024;
const MAX_HEADERS: usize = 96; // actix's limit; more gets 431
/// Bodies up to this size are copied next to their head (one send for pipelined
/// batches); larger ones are written from shared memory with writev.
const INLINE_BODY: usize = 16 * 1024;
const FLUSH_AT: usize = 64 * 1024;
/// How long a shutdown waits for in-flight responses (actix's default shutdown_timeout).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Freed compression memory is handed back to the OS after this much is replaced.
const RETURN_MEMORY_EVERY: usize = 16 * 1024 * 1024;

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static OPEN_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

struct Counted;
impl Counted {
    fn new() -> Self {
        OPEN_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
        Counted
    }
}
impl Drop for Counted {
    fn drop(&mut self) {
        OPEN_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

const SECURITY: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    ("x-frame-options", "DENY"),
    ("strict-transport-security", "max-age=31536000; includeSubDomains"),
    ("content-security-policy", CONTENT_SECURITY_POLICY_VALUE),
    ("permissions-policy", PERMISSIONS_POLICY_VALUE),
];

// ---------------------------------------------------------------- from rustwebserver main.rs

const BROTLI_FAST: u32 = 6;
const BROTLI_BEST: u32 = 11;
const BROTLI_MIN_WINDOW: u32 = 16;
const BROTLI_MAX_WINDOW: u32 = 22;
const ACCEPT_BR: usize = 1;
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

enum ByteRange {
    Ignore,
    Part(usize, usize),
    Unsatisfiable,
}

fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn smaller(compressed: Option<Vec<u8>>, than: usize) -> Option<Bytes> {
    compressed
        .filter(|c| !c.is_empty() && c.len() < than)
        .map(|c| Bytes::from(c.into_boxed_slice()))
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

fn redirect_target(dir: &str) -> String {
    let encoded = utf8_percent_encode(dir, REDIRECT_KEEP).to_string();
    format!("/{}", encoded.trim_start_matches('/'))
}

fn query_suffix(q: &str) -> String {
    if q.is_empty() || !q.bytes().all(|b| b.is_ascii_graphic()) {
        return String::new();
    }
    format!("?{q}")
}

fn env_str(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

// ---------------------------------------------------------------- prebuilt files

#[derive(Clone)]
struct Variant {
    body: Bytes,
    enc: Option<&'static str>,
    /// Everything up to and including "date: " for a 200 (or 404 for 404.html).
    head: Bytes,
}
/// [original, Brotli]: Cloudflare always asks for `br, gzip` and decompresses Brotli
/// itself for clients without it, so no gzip copy is kept.
type PickTable = [Variant; 2];

struct File {
    ctype: String,
    cc: String,
    etag: String,
    body: Bytes,
    pick: ArcSwap<PickTable>,
    ranges: bool,
    /// Prebuilt 304 head up to "date: ".
    not_modified: Bytes,
    is_404_page: bool,
}
type Files = HashMap<String, File>;

fn push_header(h: &mut Vec<u8>, name: &str, value: &str) {
    h.extend_from_slice(name.as_bytes());
    h.extend_from_slice(b": ");
    h.extend_from_slice(value.as_bytes());
    h.extend_from_slice(b"\r\n");
}

fn status_line(h: &mut Vec<u8>, code: StatusCode) {
    h.extend_from_slice(b"HTTP/1.1 ");
    h.extend_from_slice(code.as_str().as_bytes());
    h.push(b' ');
    h.extend_from_slice(code.canonical_reason().unwrap_or("").as_bytes());
    h.extend_from_slice(b"\r\n");
}

/// Headers of a full-file response, in the original's order, without the status line,
/// Content-Length or Date. `is_404_page` uses the original's not_found() header set.
fn file_headers(h: &mut Vec<u8>, f: &File, enc: Option<&str>, content_range: Option<&str>) {
    if f.is_404_page {
        push_header(h, "cache-control", ERROR_CACHE_CONTROL);
        if COMPRESSION {
            push_header(h, "vary", "Accept-Encoding");
        }
    } else {
        if f.ranges {
            push_header(h, "accept-ranges", "bytes");
        }
        if let Some(cr) = content_range {
            push_header(h, "content-range", cr);
        }
        if ETAGS {
            push_header(h, "etag", &f.etag);
        }
        push_header(h, "cache-control", &f.cc);
        if COMPRESSION {
            push_header(h, "vary", "Accept-Encoding");
        }
    }
    for (n, v) in SECURITY {
        push_header(h, n, v);
    }
    push_header(h, "content-type", &f.ctype);
    if let Some(enc) = enc {
        push_header(h, "content-encoding", enc);
    }
}

fn build_variant(f: &File, body: Bytes, enc: Option<&'static str>) -> Variant {
    let mut h = Vec::with_capacity(1024);
    let code = if f.is_404_page { StatusCode::NOT_FOUND } else { StatusCode::OK };
    status_line(&mut h, code);
    push_header(&mut h, "content-length", &body.len().to_string());
    file_headers(&mut h, f, enc, None);
    h.extend_from_slice(b"date: ");
    Variant { body, enc, head: Bytes::from(h) }
}

fn pick_table(f: &File, br: Option<Bytes>) -> PickTable {
    let original = build_variant(f, f.body.clone(), None);
    let brotli = match br {
        Some(c) if c.len() < f.body.len() => build_variant(f, c, Some("br")),
        _ => original.clone(),
    };
    [original, brotli]
}

fn placeholder() -> PickTable {
    std::array::from_fn(|_| Variant { body: Bytes::new(), enc: None, head: Bytes::new() })
}

fn load(root: &Path) -> Files {
    let mut files = HashMap::new();
    let walker = walkdir_files(root);
    for (rel, body) in walker {
        let ctype = content_type(&rel);
        let lower = rel.to_ascii_lowercase();
        let base = if lower.ends_with(".html") || lower.ends_with(".htm") {
            HTML_CACHE_CONTROL
        } else {
            ASSET_CACHE_CONTROL
        };
        let cc = if NO_TRANSFORM_ONLY_FOR_PRECOMPRESSED_TYPES && !compressible(&ctype) {
            format!("{base}, no-transform")
        } else {
            base.to_owned()
        };
        let etag = format!("\"{:016x}\"", fnv1a(&body));
        let body = Bytes::from(body);
        let br = (COMPRESSION && compressible(&ctype))
            .then(|| smaller(brotli_of(&body, BROTLI_FAST, BROTLI_MAX_WINDOW), body.len()))
            .flatten();
        let ranges = br.is_none();
        let mut nm = Vec::with_capacity(256);
        status_line(&mut nm, StatusCode::NOT_MODIFIED);
        push_header(&mut nm, "etag", &etag);
        push_header(&mut nm, "cache-control", &cc);
        if COMPRESSION {
            push_header(&mut nm, "vary", "Accept-Encoding");
        }
        nm.extend_from_slice(b"date: ");
        let mut f = File {
            ctype,
            cc,
            etag,
            body,
            pick: ArcSwap::from_pointee(placeholder()),
            ranges,
            not_modified: Bytes::from(nm),
            is_404_page: false,
        };
        // The 404 page is served under every missing path; its own URL still gets a 200.
        f.pick = ArcSwap::from_pointee(pick_table(&f, br));
        files.insert(rel, f);
    }
    if let Some(f) = files.get("404.html") {
        let mut page = File {
            ctype: f.ctype.clone(),
            cc: f.cc.clone(),
            etag: f.etag.clone(),
            body: f.body.clone(),
            pick: ArcSwap::from_pointee(placeholder()),
            ranges: f.ranges,
            not_modified: f.not_modified.clone(),
            is_404_page: true,
        };
        let cur = f.pick.load();
        let br = cur[ACCEPT_BR].enc.map(|_| cur[ACCEPT_BR].body.clone());
        page.pick = ArcSwap::from_pointee(pick_table(&page, br));
        files.insert(NOT_FOUND_KEY.to_owned(), page);
    }
    files
}

/// Key no request path can produce (requests always decode from a path starting '/').
const NOT_FOUND_KEY: &str = "\0404";

fn walkdir_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                let Ok(body) = std::fs::read(&p) else { continue };
                let Ok(rel) = p.strip_prefix(root) else { continue };
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, body));
            }
        }
    }
    out
}

fn recompress(files: &Files) {
    let todo: Vec<&File> = files.values().filter(|f| !f.ranges).collect();
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut replaced = 0;
                while let Some(f) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let old = f.pick.load_full();
                    let keep = |new: Option<Vec<u8>>, current: &Variant| {
                        smaller(new, current.body.len())
                            .or_else(|| current.enc.map(|_| current.body.clone()))
                    };
                    let br = keep(
                        brotli_of(&f.body, BROTLI_BEST, brotli_window(f.body.len())),
                        &old[ACCEPT_BR],
                    );
                    f.pick.store(Arc::new(pick_table(f, br)));
                    replaced += old[ACCEPT_BR].enc.map_or(0, |_| old[ACCEPT_BR].body.len());
                    drop(old);
                    if replaced >= RETURN_MEMORY_EVERY {
                        replaced = 0;
                        return_freed_memory_to_the_system();
                    }
                }
            });
        }
    });
    println!("recompressed {} files at the strongest levels", todo.len());
}

// ---------------------------------------------------------------- per-thread Date

thread_local! {
    static DATE: RefCell<(u64, [u8; 29])> = const { RefCell::new((0, [0; 29])) };
}

fn push_date_end(out: &mut Vec<u8>) {
    let now = SystemTime::now();
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    DATE.with(|d| {
        let mut d = d.borrow_mut();
        if d.0 != secs {
            let s = httpdate::fmt_http_date(now);
            d.1.copy_from_slice(&s.as_bytes()[..29]);
            d.0 = secs;
        }
        out.extend_from_slice(&d.1);
    });
    out.extend_from_slice(b"\r\n\r\n");
}

// ---------------------------------------------------------------- request handling

struct Reply {
    keep_alive: bool,
    /// A body too large to copy into `out`; sent with writev right after it.
    big_body: Option<Bytes>,
}

struct Req<'a> {
    method: &'a str,
    target: &'a str,
    http11: bool,
    accept_encoding: &'a [u8],
    if_none_match: Option<&'a [u8]>,
    range: Option<&'a [u8]>,
    if_range: Option<&'a [u8]>,
    close: bool,
    has_body: bool,
    bad: bool,
}

fn read_req<'b>(r: &httparse::Request<'_, 'b>) -> Req<'b> {
    let mut q = Req {
        method: r.method.unwrap_or(""),
        target: r.path.unwrap_or(""),
        http11: r.version == Some(1),
        accept_encoding: b"",
        if_none_match: None,
        range: None,
        if_range: None,
        close: false,
        has_body: false,
        bad: false,
    };
    let (mut hosts, mut lengths, mut keep_alive_token) = (0, 0, false);
    for h in r.headers.iter() {
        let n = h.name;
        if n.eq_ignore_ascii_case("host") {
            hosts += 1;
        } else if n.eq_ignore_ascii_case("accept-encoding") {
            q.accept_encoding = h.value;
        } else if n.eq_ignore_ascii_case("if-none-match") {
            q.if_none_match = Some(h.value);
        } else if n.eq_ignore_ascii_case("range") {
            q.range = Some(h.value);
        } else if n.eq_ignore_ascii_case("if-range") {
            q.if_range = Some(h.value);
        } else if n.eq_ignore_ascii_case("connection") {
            let v = h.value.to_ascii_lowercase();
            if contains(&v, b"close") {
                q.close = true;
            }
            if contains(&v, b"keep-alive") {
                keep_alive_token = true;
            }
        } else if n.eq_ignore_ascii_case("content-length") {
            lengths += 1;
            match std::str::from_utf8(h.value).ok().and_then(|s| s.trim().parse::<u64>().ok()) {
                Some(0) => {}
                Some(_) => q.has_body = true,
                None => q.bad = true,
            }
        } else if n.eq_ignore_ascii_case("transfer-encoding") {
            q.has_body = true;
        }
    }
    // RFC 9112: an HTTP/1.1 request carries exactly one Host; never two in any version.
    // Two Content-Length headers are refused outright, as actix does.
    if (q.http11 && hosts != 1) || hosts > 1 || lengths > 1 {
        q.bad = true;
    }
    // HTTP/1.0 keeps the connection only when it asks for keep-alive.
    if !q.http11 && !keep_alive_token {
        q.close = true;
    }
    q
}

fn accept_mask(ae: &[u8]) -> usize {
    accept_mask_of(std::str::from_utf8(ae).unwrap_or(""))
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
                && p.get(2..).and_then(|v| v.trim().parse::<f32>().ok()).is_some_and(|q| q <= 0.0)
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

/// Writes a response that was not prebuilt: status, Content-Length, given headers, Date.
fn dynamic(out: &mut Vec<u8>, code: StatusCode, headers: &[(&str, &str)], body: &[u8], head: bool) {
    status_line(out, code);
    push_header(out, "content-length", &body.len().to_string());
    for (n, v) in headers {
        push_header(out, n, v);
    }
    out.extend_from_slice(b"date: ");
    push_date_end(out);
    if !head {
        out.extend_from_slice(body);
    }
}

/// Error responses in actix's format: empty body and `connection: close`.
fn bare_error(out: &mut Vec<u8>, code: StatusCode) {
    status_line(out, code);
    out.extend_from_slice(b"content-length: 0\r\nconnection: close\r\ndate: ");
    push_date_end(out);
}

/// HTTP/1.0 requests get an HTTP/1.0 status line, as actix sends; a response that ends
/// the connection says `connection: close`, and an HTTP/1.0 keep-alive one says so.
/// Only these rare responses are touched; keep-alive HTTP/1.1 replies are left as built.
fn finalize(out: &mut Vec<u8>, start: usize, http11: bool, keep_alive: bool) {
    if !out[start..].starts_with(b"HTTP/1.1 ") {
        return;
    }
    if !http11 {
        out[start + 5..start + 8].copy_from_slice(b"1.0");
    }
    let header: &[u8] = if !keep_alive && !http11 {
        return; // closing is HTTP/1.0's default; actix adds no header either
    } else if !keep_alive {
        b"connection: close\r\n"
    } else if !http11 {
        b"connection: keep-alive\r\n"
    } else {
        return;
    };
    let Some(end) = out[start..].windows(4).position(|w| w == b"\r\n\r\n") else { return };
    let head = &out[start..start + end];
    if head.windows(13).any(|w| w.eq_ignore_ascii_case(b"\r\nconnection:")) {
        return;
    }
    let at = start + end + 2;
    out.splice(at..at, header.iter().copied());
}

fn plain(out: &mut Vec<u8>, code: StatusCode, cc: &str, extra: &[(&str, &str)], head: bool) {
    let mut h = vec![("content-type", "text/plain; charset=utf-8"), ("cache-control", cc)];
    h.extend_from_slice(extra);
    dynamic(out, code, &h, code.canonical_reason().unwrap_or("").as_bytes(), head);
}

fn send_variant(out: &mut Vec<u8>, v: &Variant, head: bool) -> Option<Bytes> {
    out.extend_from_slice(&v.head);
    push_date_end(out);
    if head || v.body.is_empty() {
        None
    } else if v.body.len() <= INLINE_BODY {
        out.extend_from_slice(&v.body);
        None
    } else {
        Some(v.body.clone())
    }
}

fn handle(files: &Files, q: &Req, out: &mut Vec<u8>) -> Reply {
    let head = q.method == "HEAD";
    let mut keep = !q.close;
    let done = |keep| Reply { keep_alive: keep, big_body: None };

    if q.bad {
        bare_error(out, StatusCode::BAD_REQUEST);
        return done(false);
    }
    if q.method != "GET" && !head {
        // A request body is never read, so the connection cannot be reused after one.
        if q.has_body {
            keep = false;
        }
        const OTHER: [&str; 7] = ["POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE", "CONNECT"];
        if OTHER.contains(&q.method) {
            plain(out, StatusCode::METHOD_NOT_ALLOWED, ERROR_CACHE_CONTROL, &[("allow", "GET, HEAD")], false);
        } else {
            dynamic(
                out,
                StatusCode::NOT_IMPLEMENTED,
                &[("content-type", "text/plain; charset=utf-8")],
                b"Not Implemented",
                false,
            );
        }
        return done(keep);
    }
    // RFC 9112 §3.2.2: accept absolute-form ("http://host/path") as well as origin-form.
    // A #fragment is never part of the resource (browsers do not send one; actix drops it).
    let raw_target = q.target.split('#').next().unwrap_or("");
    let target = match raw_target.split_once("://") {
        Some((scheme, rest))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            rest.find('/').map_or("/", |i| &rest[i..])
        }
        _ => raw_target,
    };
    if q.has_body || !target.starts_with('/') {
        bare_error(out, StatusCode::BAD_REQUEST);
        return done(false);
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let p = percent_encoding::percent_decode_str(path).decode_utf8_lossy();

    if let Some(dir) = p.strip_suffix("index.html").filter(|d| d.ends_with('/')) {
        let loc = redirect_target(dir) + &query_suffix(query);
        plain(out, StatusCode::MOVED_PERMANENTLY, REDIRECT_CACHE_CONTROL, &[("location", loc.as_str())], head);
        return done(keep);
    }

    let key = p.strip_prefix('/').unwrap_or(&p);
    let f = if key.is_empty() || key.ends_with('/') {
        files.get(&format!("{key}index.html"))
    } else {
        files.get(key)
    };
    let Some(f) = f else {
        if !p.ends_with('/') && files.contains_key(&format!("{key}/index.html")) {
            let loc = redirect_target(&format!("{p}/")) + &query_suffix(query);
            plain(out, StatusCode::MOVED_PERMANENTLY, REDIRECT_CACHE_CONTROL, &[("location", loc.as_str())], head);
            return done(keep);
        }
        return match files.get(NOT_FOUND_KEY) {
            Some(page) => {
                let t = page.pick.load();
                let big = send_variant(out, &t[accept_mask(q.accept_encoding)], head);
                Reply { keep_alive: keep, big_body: big }
            }
            None => {
                plain(out, StatusCode::NOT_FOUND, ERROR_CACHE_CONTROL, &[], head);
                done(keep)
            }
        };
    };

    if ETAGS && q.if_none_match.is_some_and(|inm| contains(inm, f.etag.as_bytes())) {
        out.extend_from_slice(&f.not_modified);
        push_date_end(out);
        return done(keep);
    }

    let t = f.pick.load();
    let v = &t[accept_mask(q.accept_encoding)];

    if let Some(range) = q.range.filter(|_| f.ranges && !head) {
        let current = q
            .if_range
            .is_none_or(|ir| ir.is_empty() || (ETAGS && ir == f.etag.as_bytes()));
        if let (true, Ok(range)) = (current, std::str::from_utf8(range)) {
            let len = v.body.len();
            match parse_range(range, len) {
                ByteRange::Part(start, end) => {
                    let cr = format!("bytes {start}-{end}/{len}");
                    let part = v.body.slice(start..=end);
                    status_line(out, StatusCode::PARTIAL_CONTENT);
                    push_header(out, "content-length", &part.len().to_string());
                    file_headers(out, f, v.enc, Some(&cr));
                    out.extend_from_slice(b"date: ");
                    push_date_end(out);
                    if part.len() <= INLINE_BODY {
                        out.extend_from_slice(&part);
                        return done(keep);
                    }
                    return Reply { keep_alive: keep, big_body: Some(part) };
                }
                ByteRange::Unsatisfiable => {
                    let cr = format!("bytes */{len}");
                    plain(
                        out,
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        ERROR_CACHE_CONTROL,
                        &[("accept-ranges", "bytes"), ("content-range", cr.as_str())],
                        false,
                    );
                    return done(keep);
                }
                ByteRange::Ignore => {}
            }
        }
    }

    let big = send_variant(out, v, head);
    Reply { keep_alive: keep, big_body: big }
}

// ---------------------------------------------------------------- connection loop

async fn write_two(s: &mut TcpStream, mut a: &[u8], mut b: &[u8]) -> io::Result<()> {
    while !a.is_empty() || !b.is_empty() {
        let n = if a.is_empty() {
            s.write(b).await?
        } else if b.is_empty() {
            s.write(a).await?
        } else {
            s.write_vectored(&[IoSlice::new(a), IoSlice::new(b)]).await?
        };
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        let k = n.min(a.len());
        a = &a[k..];
        b = &b[n - k..];
    }
    Ok(())
}

async fn flush(s: &mut TcpStream, out: &mut Vec<u8>, body: &[u8]) -> bool {
    let ok = matches!(timeout(WRITE_TIMEOUT, write_two(s, out, body)).await, Ok(Ok(())));
    out.clear();
    ok
}

async fn connection(mut s: TcpStream, files: &'static Files, mut stop: watch::Receiver<bool>) {
    let _counted = Counted::new();
    let _ = s.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut out: Vec<u8> = Vec::with_capacity(16 * 1024);
    // Until a first request is answered, a silent connection gets the request-head
    // timeout (as actix's client_request_timeout), not the long keep-alive one.
    let mut answered = false;
    loop {
        // Answer every complete request already buffered (pipelining keeps order).
        let mut pos = 0;
        loop {
            let mut hs = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut r = httparse::Request::new(&mut hs);
            let (consumed, reply) = match r.parse(&buf[pos..]) {
                Ok(httparse::Status::Complete(n)) => {
                    let q = read_req(&r);
                    let start = out.len();
                    let reply = handle(files, &q, &mut out);
                    finalize(&mut out, start, q.http11, reply.keep_alive);
                    (n, reply)
                }
                Ok(httparse::Status::Partial) => break,
                Err(e) => {
                    let code = if matches!(e, httparse::Error::TooManyHeaders) {
                        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    bare_error(&mut out, code);
                    let _ = flush(&mut s, &mut out, &[]).await;
                    return;
                }
            };
            pos += consumed;
            answered = true;
            if let Some(body) = reply.big_body {
                if !flush(&mut s, &mut out, &body).await {
                    return;
                }
            }
            if !reply.keep_alive {
                let _ = flush(&mut s, &mut out, &[]).await;
                return;
            }
            if out.len() >= FLUSH_AT && !flush(&mut s, &mut out, &[]).await {
                return;
            }
        }
        if pos > 0 {
            buf.drain(..pos);
        }
        if !out.is_empty() && !flush(&mut s, &mut out, &[]).await {
            return;
        }
        if buf.len() >= MAX_HEAD {
            bare_error(&mut out, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
            let _ = flush(&mut s, &mut out, &[]).await;
            return;
        }
        // Everything answered so far has been written; during shutdown, close here.
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            return;
        }
        let wait = if buf.is_empty() && answered { KEEP_ALIVE } else { REQUEST_HEAD_TIMEOUT };
        buf.reserve(8192);
        tokio::select! {
            r = timeout(wait, s.read_buf(&mut buf)) => match r {
                Ok(Ok(n)) if n > 0 => {}
                _ => return,
            },
            _ = stop.changed() => return,
        }
    }
}

fn listener(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        s.set_only_v6(false)?; // "::" also takes IPv4, like the original's default
    }
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.set_nodelay(true)?;
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    s.listen(65_535)?;
    Ok(s.into())
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

fn main() {
    let root = env_str("ROOT", "/root/www");
    let port: u16 = env_str("PORT", "80").parse().unwrap_or_else(|e| {
        eprintln!("bad PORT: {e}");
        std::process::exit(1);
    });
    match rlimit::increase_nofile_limit(u64::MAX) {
        Ok(n) => println!("open file limit: {n}"),
        Err(e) => eprintln!("could not raise open file limit: {e}"),
    }
    let files: &'static Files = Box::leak(Box::new(load(Path::new(&root))));
    if files.is_empty() {
        eprintln!("no files found under {root}");
        std::process::exit(1);
    }
    println!("loaded {} files from {root}", files.len());
    return_large_blocks_when_freed();
    return_freed_memory_to_the_system();
    if COMPRESSION {
        thread::spawn(move || {
            recompress(files);
            return_freed_memory_to_the_system();
        });
    }

    let host = std::env::var("HOST").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| {
        if std::net::TcpListener::bind(("::", 0)).is_ok() { "::".to_owned() } else { "0.0.0.0".to_owned() }
    });
    let addr: SocketAddr = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port),
        Err(_) => {
            eprintln!("bad HOST: {host}");
            std::process::exit(1);
        }
    };
    let n = thread::available_parallelism().map_or(1, |x| x.get());

    // SO_REUSEPORT would let this process share a port another server already holds,
    // splitting visitors between them. A plain bind fails if anything listens there.
    if let Err(e) = probe(addr) {
        eprintln!("listen on :{port}: {e}");
        std::process::exit(1);
    }
    let listeners: Vec<std::net::TcpListener> = (0..n)
        .map(|_| {
            listener(addr).unwrap_or_else(|e| {
                eprintln!("listen on :{port}: {e}");
                std::process::exit(1);
            })
        })
        .collect();
    println!("listening on {addr} with {n} per-core workers");

    let (stop_tx, stop_rx) = watch::channel(false);
    let threads: Vec<_> = listeners
        .into_iter()
        .map(|std_listener| {
            let mut stop = stop_rx.clone();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let l = TcpListener::from_std(std_listener).unwrap();
                    loop {
                        tokio::select! {
                            r = l.accept() => if let Ok((s, _)) = r {
                                tokio::spawn(connection(s, files, stop.clone()));
                            },
                            _ = stop.changed() => break,
                        }
                    }
                    drop(l); // stop accepting; in-flight responses keep going
                    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
                    while OPEN_CONNECTIONS.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
            })
        })
        .collect();

    // Wait for SIGTERM (systemctl stop, kill) or SIGINT (Ctrl+C), then shut down gracefully.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        });
    println!("shutting down: finishing in-flight requests (up to {} s)", SHUTDOWN_TIMEOUT.as_secs());
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    let _ = stop_tx.send(true);
    for t in threads {
        let _ = t.join();
    }
}

/// Bind without SO_REUSEPORT to find out whether another process holds the port.
fn probe(addr: SocketAddr) -> io::Result<()> {
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        s.set_only_v6(false)?;
    }
    s.set_reuse_address(true)?;
    s.bind(&addr.into())?;
    s.listen(1)?;
    Ok(())
}
