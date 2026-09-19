use std::{collections::HashMap, env, io::Write, path::Path, process, time::Duration};

use actix_web::{
    http::{
        header::{
            HeaderValue, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_SECURITY_POLICY,
            CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION, PERMISSIONS_POLICY, REFERRER_POLICY,
            STRICT_TRANSPORT_SECURITY, VARY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
        },
        KeepAlive, StatusCode,
    },
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use bytes::Bytes;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const HTML_CACHE_CONTROL: &str = "public, max-age=60, no-transform";
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable, no-transform";

// Same levels as the Go version (the libraries' defaults).
const ZSTD_LEVEL: i32 = 3; // == zstd.SpeedDefault
const BROTLI_QUALITY: u32 = 6; // == brotli.DefaultCompression
const BROTLI_LGWIN: u32 = 24;
const GZIP_LEVEL: u32 = 6; // == gzip.DefaultCompression

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

const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'self'; \
    base-uri 'self'; \
    form-action 'self'; \
    frame-ancestors 'none'; \
    img-src 'self' data:; \
    style-src 'self' 'unsafe-inline'; \
    script-src 'self' 'unsafe-inline'; \
    font-src 'self'; \
    connect-src 'self'; \
    object-src 'none'; \
    upgrade-insecure-requests";

const PERMISSIONS_POLICY_VALUE: &str = "accelerometer=(), autoplay=(), camera=(), \
    display-capture=(), encrypted-media=(), fullscreen=(), \
    geolocation=(), gyroscope=(), magnetometer=(), microphone=(), \
    midi=(), payment=(), picture-in-picture=(), publickey-credentials-get=(), \
    screen-wake-lock=(), sync-xhr=(), usb=(), xr-spatial-tracking=()";

/// One cached file. `Bytes` is refcounted, so handing a body to a response
/// is a pointer copy, never a memcpy.
struct File {
    ctype: HeaderValue,
    cc: HeaderValue,
    etag: HeaderValue,
    body: Bytes,
    br: Option<Bytes>,
    gz: Option<Bytes>,
    zs: Option<Bytes>,
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
    .backlog(BACKLOG);

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
    // fasthttp hands Fiber a percent-decoded path; do the same here.
    let p = percent_encoding::percent_decode_str(req.path()).decode_utf8_lossy();

    if let Some(dir) = p.strip_suffix("index.html").filter(|d| d.ends_with('/')) {
        let mut res = status(StatusCode::MOVED_PERMANENTLY, ASSET_CACHE_CONTROL);
        if let Ok(loc) = HeaderValue::from_str(dir) {
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
        return status(StatusCode::NOT_FOUND, "no-store");
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

    let ae = req
        .headers()
        .get(ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (body, enc) = match (&f.zs, &f.br, &f.gz) {
        (Some(zs), _, _) if ae.contains("zstd") => (zs, Some("zstd")),
        (_, Some(br), _) if ae.contains("br") => (br, Some("br")),
        (_, _, Some(gz)) if ae.contains("gzip") => (gz, Some("gzip")),
        _ => (&f.body, None),
    };

    let mut res = HttpResponse::Ok().body(body.clone());
    let h = res.headers_mut();
    h.insert(ETAG, f.etag.clone());
    h.insert(CACHE_CONTROL, f.cc.clone());
    h.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
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
    h.insert(CONTENT_TYPE, f.ctype.clone());
    if let Some(enc) = enc {
        h.insert(CONTENT_ENCODING, HeaderValue::from_static(enc));
    }
    res
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

    let (mut zs, mut br, mut gz) = (None, None, None);
    if compressible(&ctype) {
        // keep a variant only if it is actually smaller than the original
        let smaller = |c: Vec<u8>| (!c.is_empty() && c.len() < body.len()).then(|| Bytes::from(c));
        zs = zstd::bulk::compress(&body, ZSTD_LEVEL)
            .ok()
            .and_then(smaller);
        br = brotli_of(&body).and_then(smaller);
        gz = gzip_of(&body).and_then(smaller);
    }

    files.insert(
        rel,
        File {
            ctype: HeaderValue::from_str(&ctype)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            cc: HeaderValue::from_static(cc),
            etag: HeaderValue::from_str(&etag).expect("etag is ascii"),
            body: Bytes::from(body),
            br,
            gz,
            zs,
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
        "json" | "webmanifest" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "wasm" => "application/wasm",
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
            "font/woff",
        ]
        .iter()
        .any(|s| ctype.contains(s))
}

fn brotli_of(b: &[u8]) -> Option<Vec<u8>> {
    let mut w = brotli::CompressorWriter::new(
        Vec::with_capacity(b.len() / 2),
        4096,
        BROTLI_QUALITY,
        BROTLI_LGWIN,
    );
    w.write_all(b).ok()?;
    Some(w.into_inner()) // into_inner() finishes the stream
}

fn gzip_of(b: &[u8]) -> Option<Vec<u8>> {
    let mut w = flate2::write::GzEncoder::new(
        Vec::with_capacity(b.len() / 2),
        flate2::Compression::new(GZIP_LEVEL),
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
