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
            HeaderMap, HeaderValue, ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONTENT_RANGE,
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

const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

const KEEP_ALIVE: Duration = Duration::from_secs(960);
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CONNECTIONS: usize = 10_000_000;
const BACKLOG: u32 = 65_535;
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
    .tcp_nodelay(true);

    let host = env::var("HOST").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| {
        if std::net::TcpListener::bind(("::", 0)).is_ok() {
            "::".to_owned()
        } else {
            "0.0.0.0".to_owned()
        }
    });
    let server = server.bind((host.as_str(), port)).unwrap_or_else(|e| {
        eprintln!("listen on :{port}: {e}");
        process::exit(1);
    });
    println!("listening on :{port}");
    server.run().await
}

async fn serve(files: web::Data<Files>, req: HttpRequest) -> HttpResponse {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return refuse(req.method());
    }

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

    let range = req
        .headers()
        .get(RANGE)
        .filter(|_| req.method() == Method::GET)
        .and_then(|v| v.to_str().ok());
    let mut code = StatusCode::OK;
    let mut content_range = None;
    if let Some(range) = range {
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
    let mut res = status(StatusCode::METHOD_NOT_ALLOWED, "no-store");
    res.headers_mut().insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    res
}

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
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn load(root: &Path) -> Files {
    let mut files = HashMap::new();
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));

    for entry in walker.flatten() {
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
    let cc = if compressible(&ctype) {
        ASSET_CACHE_CONTROL.to_owned()
    } else {
        format!("{ASSET_CACHE_CONTROL}, no-transform")
    };
    let etag = format!("\"{:016x}\"", fnv1a(&body));
    let body = Bytes::from(body);

    files.insert(
        rel,
        File {
            ctype: HeaderValue::from_str(&ctype)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            cc: HeaderValue::from_str(&cc).expect("cache-control is ascii"),
            etag: HeaderValue::from_str(&etag).expect("etag is ascii"),
            body,
        },
    );
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

fn env_str(key: &str, fallback: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}
