use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    path::Path,
    pin::Pin,
    process,
    task::{Context, Poll},
    time::Duration,
};

use actix_web::{
    body::{BodySize, MessageBody},
    http::{
        header::{
            HeaderMap, HeaderValue, ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE,
            CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, LOCATION,
            PERMISSIONS_POLICY, RANGE, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY,
            X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
        KeepAlive, Method, StatusCode,
    },
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

const HTML_CACHE_CONTROL: &str = "no-cache, must-revalidate";
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable, no-transform";

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

/// One cached file. `Bytes` is refcounted, so handing a body to a response
/// is a pointer copy, never a memcpy.
struct File {
    ctype: HeaderValue,
    cc: HeaderValue,
    etag: HeaderValue,
    body: Bytes,
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
        return not_found(&files);
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
        return res;
    }

    let mut body = f.body.clone();

    // Byte ranges: what video and audio players use for seeking, and what
    // Safari needs to play media at all.
    let range = req
        .headers()
        .get(RANGE)
        .filter(|_| req.method() == Method::GET)
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
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(v) = content_range.and_then(|v| HeaderValue::from_str(&v).ok()) {
        h.insert(CONTENT_RANGE, v);
    }
    h.insert(ETAG, f.etag.clone());
    h.insert(CACHE_CONTROL, f.cc.clone());
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
    res
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
fn not_found(files: &Files) -> HttpResponse {
    let Some(f) = files.get("404.html") else {
        return status(StatusCode::NOT_FOUND, "no-store");
    };
    let mut res = response(StatusCode::NOT_FOUND, f.body.clone());
    let h = res.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    security_headers(h);
    h.insert(CONTENT_TYPE, f.ctype.clone());
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

    files.insert(
        rel,
        File {
            ctype: HeaderValue::from_str(&ctype)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            cc: HeaderValue::from_static(cc),
            etag: HeaderValue::from_str(&etag).expect("etag is ascii"),
            body,
        },
    );
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

fn env_str(key: &str, fallback: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}
