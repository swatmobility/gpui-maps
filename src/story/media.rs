//! Story images (spec §4.3): a PNG, JPEG or WebP from a path inside the
//! story's folder or an `https://` URL, fetched once with the project
//! User-Agent and nothing personal, cached, size-guarded from the header
//! alone, turned upright by its EXIF orientation, and downscaled on load
//! to the frame's physical long edge.
//!
//! - **Fetch.** [`ImageFetch`] is one `reqwest` client: [`IMAGE_USER_AGENT`],
//!   connect 10 s, total 30 s, no cookie store, no referer, no auth header
//!   (a URL with `user:pass@` is refused rather than turned into one), at
//!   most 3 redirects and never one to a non-https URL. No request goes to
//!   a loopback, private, link-local, unique-local or unspecified address:
//!   a literal IP or `localhost` is refused before sending and on every
//!   redirect, and a name is resolved through a resolver that drops such
//!   addresses (so DNS rebinding cannot reach them either). The refusal
//!   says only that the address is not public. Every URL in a message is
//!   passed through [`crate::story::redact_url_secrets`].
//! - **Cache.** `cache_dir/<sha256(url)>` ([`crate::image_cache`]): a
//!   uniquely named temp file persisted in one rename, kept under 512 MiB
//!   least-recently-used. A cached copy that no longer decodes is evicted
//!   and fetched once more (never `offline`, which reads the cache only).
//! - **Guard.** More than [`MAX_IMAGE_BYTES`] of file (a local file is
//!   opened once, must be a regular file, and is read through
//!   `take(MAX + 1)`), or more than [`MAX_IMAGE_PIXELS`] declared in the
//!   header, is `image_too_large`, decided before any pixel is decoded
//!   (pre-flight ruling F4: a 12000 x 12000 image is refused, not
//!   downscaled).
//! - **Pixels.** The EXIF orientation is applied (a phone portrait stored
//!   landscape draws upright; [`LoadedImage::original`] is the upright
//!   size). A resize premultiplies alpha first, so a transparent edge
//!   gets no dark fringe. ICC profiles are NOT applied (deferred): a
//!   Display-P3 photo draws as if it were sRGB, slightly desaturated.
//!   An animated WebP or APNG draws its first frame.
//! - **Async** (ruling F5): the fetch is async; the decode runs under
//!   `spawn_blocking`. The player drives it on its own runtime.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use image::ImageDecoder;
use sha2::Digest;

use crate::story::script::{EventAction, ImageSpec, Script, Severity};
use crate::story::Diagnostic;

/// The User-Agent of every image fetch: the project and its repository,
/// nothing about the user.
pub const IMAGE_USER_AGENT: &str = concat!("gpui-maps/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)");
/// The largest image file accepted (40 MiB).
pub const MAX_IMAGE_BYTES: u64 = 40 * 1024 * 1024;
/// The most pixels an image's header may declare (64 MP).
pub const MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// How long a fetch may take to connect, and in total.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Redirects followed at most.
const MAX_REDIRECTS: usize = 3;
/// What a refused address is called, with nothing about the network.
const NOT_PUBLIC: &str = "images are only fetched from public internet addresses";

/// One loaded image: decoded, upright, downscaled, and named for the bundle.
#[derive(Clone, Debug)]
pub struct LoadedImage {
    /// The script's `src`, as written.
    pub src: String,
    pub image: Arc<image::RgbaImage>,
    /// The file's original bytes, for the bundle's `media/` copy (Task 16).
    pub source: Arc<[u8]>,
    /// SHA-256 of [`Self::source`], lowercase hex.
    pub sha256: String,
    /// The file name it is copied to in the bundle's media/ folder: `<sha256[..16]>.<ext>`.
    pub bundle_name: String,
    /// The upright size before any downscale (the header's, swapped by a
    /// 90° EXIF orientation).
    pub original: (u32, u32),
    /// An animated WebP or APNG: only its first frame is drawn.
    pub animated: bool,
}

/// The fetcher [`load_image`] uses (`ImageFetch::new()`); tests pass
/// `ImageFetch::for_tests()`, the one constructor that allows plain http
/// and loopback (a local test server; private and link-local addresses
/// stay refused).
pub struct ImageFetch {
    client: reqwest::Client,
    allow_http: bool,
    allow_loopback: bool,
}

impl ImageFetch {
    /// The production fetcher: https only, public addresses only.
    ///
    /// # Errors
    /// The TLS backend does not initialise (`image_fetch_failed`).
    pub fn new() -> Result<Self, Diagnostic> {
        Self::build(false, false)
    }

    /// A fetcher that also follows plain `http://` to loopback.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::build(true, true).expect("a reqwest client")
    }

    fn build(allow_http: bool, allow_loopback: bool) -> Result<Self, Diagnostic> {
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            // `previous()` holds the first URL too: the 3rd redirect sees 3.
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error(format!("more than {MAX_REDIRECTS} redirects"))
            } else if !allow_http && attempt.url().scheme() != "https" {
                attempt.error("a redirect to a non-https URL is refused")
            } else if host_refused(attempt.url(), allow_loopback) {
                attempt.error(NOT_PUBLIC)
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(IMAGE_USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .redirect(policy)
            .referer(false)
            // A proxy would resolve the host itself: `PublicOnly` would
            // never see the target, so no proxy is ever used.
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnly { allow_loopback }))
            .build()
            .map_err(|e| error("image_fetch_failed", format!("cannot build the image fetcher: {}", e.without_url())))?;
        Ok(Self { client, allow_http, allow_loopback })
    }
}

/// Whether `ip` is one no image is fetched from: loopback (unless
/// `allow_loopback`), private, shared (CGNAT 100.64/10), benchmarking
/// (198.18/15), reserved (240/4, broadcast included), link-local,
/// unspecified, `0/8`, multicast or documentation space; in IPv6 also
/// unique-local, link-local, site-local (`fec0::/10`) and multicast. An
/// IPv6 address carrying an IPv4 one -- mapped `::ffff:a.b.c.d`,
/// compatible `::a.b.c.d`, NAT64 `64:ff9b::/96`, 6to4 `2002::/16` -- is
/// judged by that IPv4 address.
fn ip_refused(ip: IpAddr, allow_loopback: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return !allow_loopback;
            }
            let [a, b, c, _] = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_documentation()
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                // IETF protocol assignments (final review F6).
                || (a == 192 && b == 0 && c == 0)
                || a >= 240
                || a == 0
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let [s0, s1, s2, s3, s4, s5, s6, s7] = seg;
            let v4_of = |hi: u16, lo: u16| std::net::Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_refused(IpAddr::V4(v4), allow_loopback);
            }
            if v6.is_loopback() {
                return !allow_loopback;
            }
            if v6.is_unspecified() {
                return true;
            }
            // IPv4-compatible `::a.b.c.d` (deprecated, still routable by some stacks).
            if [s0, s1, s2, s3, s4, s5] == [0; 6] {
                return ip_refused(IpAddr::V4(v4_of(s6, s7)), allow_loopback);
            }
            // NAT64 well-known prefix.
            if [s0, s1, s2, s3, s4, s5] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return ip_refused(IpAddr::V4(v4_of(s6, s7)), allow_loopback);
            }
            // 6to4: the IPv4 address is the next 32 bits.
            if s0 == 0x2002 {
                return ip_refused(IpAddr::V4(v4_of(s1, s2)), allow_loopback);
            }
            // Teredo `2001::/32` (final review F6): the server's IPv4 in the
            // next 32 bits, the client's in the last 32, obfuscated (every
            // bit flipped). Judged by both.
            if [s0, s1] == [0x2001, 0] {
                return ip_refused(IpAddr::V4(v4_of(s2, s3)), allow_loopback) || ip_refused(IpAddr::V4(v4_of(!s6, !s7)), allow_loopback);
            }
            // The discard prefix `100::/64` (final review F6).
            if [s0, s1, s2, s3] == [0x100, 0, 0, 0] {
                return true;
            }
            v6.is_multicast() || (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80 || (s0 & 0xffc0) == 0xfec0
        }
    }
}

/// Whether `url`'s host is refused before any connection: `localhost` (or
/// a `.localhost` name) unless `allow_loopback`, or a refused literal IP.
fn host_refused(url: &reqwest::Url, allow_loopback: bool) -> bool {
    let Some(host) = url.host_str() else { return true };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip_refused(ip, allow_loopback),
        Err(_) => {
            let d = bare.trim_end_matches('.').to_ascii_lowercase();
            !allow_loopback && (d == "localhost" || d.ends_with(".localhost"))
        }
    }
}

/// A resolver that hands `reqwest` only the public addresses of a name.
struct PublicOnly {
    allow_loopback: bool,
}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_loopback = self.allow_loopback;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
                std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), 0)).map(Iterator::collect::<Vec<_>>)
            })
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })??;
            let public = public_addrs(found, allow_loopback);
            if public.is_empty() {
                return Err(NOT_PUBLIC.into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether `e` is [`PublicOnly`] refusing the name (its message, somewhere
/// down the source chain), as opposed to a public host that is down.
fn refused_by_resolver(e: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = source {
        if s.to_string() == NOT_PUBLIC {
            return true;
        }
        source = s.source();
    }
    false
}

/// `addrs` without the refused ones ([`ip_refused`]).
fn public_addrs(addrs: Vec<SocketAddr>, allow_loopback: bool) -> Vec<SocketAddr> {
    addrs.into_iter().filter(|a| !ip_refused(a.ip(), allow_loopback)).collect()
}

fn error(code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic { severity: Severity::Error, path: String::new(), code, message: message.into(), hint: None }
}

/// `~/.cache/gpui-map/story-images`: where fetched images are kept unless
/// the caller names a folder (`RenderOptions::cache_dir`/`story-images`,
/// `ValidateOptions::image_cache_dir`).
#[must_use]
pub fn default_image_cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    home.join(".cache").join("gpui-map").join("story-images")
}

fn hex_sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Load `src`: a path inside `base_dir` (via `resolve_in_story_dir`) or an https URL fetched once with
/// IMAGE_USER_AGENT and cached at `cache_dir/<sha256(url)>`. PNG, JPEG or WebP only (by magic bytes). The header's
/// dimensions are read before decoding and more than MAX_IMAGE_PIXELS is `image_too_large`; more than
/// MAX_IMAGE_BYTES is `image_too_large`. Turned upright by its EXIF orientation, then downscaled (Lanczos3,
/// premultiplied) so its long edge is at most `max_long_edge` (the frame's physical long edge).
///
/// # Errors
/// `file_not_found`, `path_escapes`, `image_fetch_failed`, `image_format` or `image_too_large`; the diagnostic's
/// `path` is empty (the caller sets the script's pointer).
pub async fn load_image(src: &str, base_dir: &Path, cache_dir: &Path, max_long_edge: u32, offline: bool) -> Result<LoadedImage, Diagnostic> {
    let fetch = ImageFetch::new()?;
    load_image_with(&fetch, src, base_dir, cache_dir, max_long_edge, offline).await
}

/// [`load_image`] with a given fetcher.
///
/// # Errors
/// See [`load_image`].
pub async fn load_image_with(
    fetch: &ImageFetch,
    src: &str,
    base_dir: &Path,
    cache_dir: &Path,
    max_long_edge: u32,
    offline: bool,
) -> Result<LoadedImage, Diagnostic> {
    let remote = src.starts_with("https://") || (fetch.allow_http && src.starts_with("http://"));
    if !remote && src.contains("://") {
        return Err(error("image_fetch_failed", format!("`{}`: only https:// images are fetched", crate::story::redact_url_secrets(src))));
    }
    if !remote {
        return decode_bytes(src, local_bytes(base_dir, src)?, max_long_edge).await;
    }
    let key = hex_sha256(src.as_bytes());
    let (bytes, cached) = remote_bytes(fetch, src, cache_dir, offline, true).await?;
    let loaded = match decode_bytes(src, bytes, max_long_edge).await {
        // A cached copy that does not decode (a torn or stale write) is
        // dropped and fetched once more. Only corruption: a refusal the
        // bytes decide (`image_too_large`) would come back the same.
        Err(e) if cached && !offline && e.code == "image_format" => {
            crate::image_cache::evict(cache_dir, &key);
            let (bytes, _) = remote_bytes(fetch, src, cache_dir, offline, false).await?;
            (decode_bytes(src, bytes, max_long_edge).await?, false)
        }
        other => (other?, cached),
    };
    let (image, from_cache) = loaded;
    // Cached only once it decoded within every limit: an oversized or
    // undecodable answer never enters the cache. A failed write costs a
    // refetch next time, never this load.
    if !from_cache {
        let _ = crate::image_cache::write(cache_dir, &key, &image.source, crate::image_cache::MAX_IMAGE_CACHE_BYTES);
    }
    Ok(image)
}

/// Sniff, hash and decode `bytes` (on a blocking thread).
async fn decode_bytes(src: &str, bytes: Vec<u8>, max_long_edge: u32) -> Result<LoadedImage, Diagnostic> {
    let name = crate::story::redact_url_secrets(src);
    let format = sniff(&bytes).ok_or_else(|| error("image_format", format!("`{name}` is not a PNG, JPEG or WebP image")))?;
    let sha256 = hex_sha256(&bytes);
    let source: Arc<[u8]> = bytes.into();
    let input = Arc::clone(&source);
    let decode = move || decode_guarded(Cursor::new(&*input), format, max_long_edge);
    // On a blocking thread under tokio; inline under any other executor
    // (`validate_scene` may be driven by one).
    let decoded = if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(decode).await.map_err(|_| error("image_format", format!("decoding `{name}` panicked")))?
    } else {
        decode()
    };
    let decoded = decoded.map_err(|mut d| {
            d.message = format!("`{name}`: {}", d.message);
            d
        })?;
    let ext = match format {
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::WebP => "webp",
        _ => "png",
    };
    let bundle_name = format!("{}.{ext}", sha256.get(..16).unwrap_or(&sha256));
    let animated = is_animated(&source, format);
    Ok(LoadedImage { src: src.to_string(), image: Arc::new(decoded.0), source, sha256, bundle_name, original: decoded.1, animated })
}

/// A local image's bytes: opened once, refused unless a regular file of
/// at most [`MAX_IMAGE_BYTES`], read through `take(MAX + 1)` so a file
/// that grows after the check is still bounded.
fn local_bytes(base_dir: &Path, src: &str) -> Result<Vec<u8>, Diagnostic> {
    let path = crate::story::validate::resolve_in_story_dir(base_dir, src)?;
    let unreadable = |e: std::io::Error| error("file_not_found", format!("cannot read {}: {e}", path.display()));
    let not_a_file = || error("file_not_found", format!("{} is not a regular file", path.display()));
    // Checked by path first: opening a FIFO would block until a writer came.
    if !std::fs::metadata(&path).map_err(unreadable)?.is_file() {
        return Err(not_a_file());
    }
    let file = std::fs::File::open(&path).map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(not_a_file());
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(too_many_bytes(src, meta.len()));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes).map_err(unreadable)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(too_many_bytes(src, bytes.len() as u64));
    }
    Ok(bytes)
}

fn too_many_bytes(src: &str, len: u64) -> Diagnostic {
    error(
        "image_too_large",
        format!(
            "`{}` is {len} bytes; images are limited to {MAX_IMAGE_BYTES} bytes (40 MiB) and {MAX_IMAGE_PIXELS} pixels (64 MP)",
            crate::story::redact_url_secrets(src)
        ),
    )
}

/// A remote image's bytes and whether they came from the cache: the
/// cached copy if `use_cache` and there is one, else one fetch (none
/// `offline`). The caller caches a fetch once it decodes.
async fn remote_bytes(fetch: &ImageFetch, src: &str, cache_dir: &Path, offline: bool, use_cache: bool) -> Result<(Vec<u8>, bool), Diagnostic> {
    let shown = crate::story::redact_url_secrets(src);
    let key = hex_sha256(src.as_bytes());
    if use_cache {
        if let Some(bytes) = crate::image_cache::read(cache_dir, &key) {
            if bytes.len() as u64 <= MAX_IMAGE_BYTES && sniff(&bytes).is_some() {
                return Ok((bytes, true));
            }
        }
    }
    if offline {
        return Err(error("image_fetch_failed", format!("`{shown}` is not cached and the render is offline")));
    }
    let url = reqwest::Url::parse(src).map_err(|e| error("image_fetch_failed", format!("`{shown}` is not a URL: {e}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(error("image_fetch_failed", format!("`{shown}` carries credentials; images are fetched with no credentials")));
    }
    if host_refused(&url, fetch.allow_loopback) {
        return Err(error("image_fetch_failed", format!("`{shown}` is refused: {NOT_PUBLIC}")));
    }
    // The cause is named only by kind: a connect error's details (which
    // host refused, which port is open) would make this a network probe.
    let failed = |e: reqwest::Error| {
        let why = if e.is_redirect() {
            format!("a redirect was refused (more than {MAX_REDIRECTS}, to a non-https URL, or to an address that is not public)")
        } else if e.is_timeout() {
            "it timed out".to_string()
        } else if refused_by_resolver(&e) {
            format!("refused: not a public address ({NOT_PUBLIC})")
        } else if e.is_connect() {
            "could not connect".to_string()
        } else {
            "the request failed".to_string()
        };
        error("image_fetch_failed", format!("fetching `{shown}` failed: {why}"))
    };
    let mut response = fetch.client.get(url).send().await.map_err(failed)?;
    let status = response.status();
    if !status.is_success() {
        return Err(error("image_fetch_failed", format!("fetching `{shown}` failed: HTTP {status}")));
    }
    if let Some(len) = response.content_length().filter(|l| *l > MAX_IMAGE_BYTES) {
        return Err(too_many_bytes(src, len));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(too_many_bytes(src, bytes.len() as u64));
        }
    }
    Ok((bytes, false))
}

/// The format named by `bytes`' magic number: PNG, JPEG or WebP only.
fn sniff(bytes: &[u8]) -> Option<image::ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(image::ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(image::ImageFormat::Jpeg)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(image::ImageFormat::WebP)
    } else {
        None
    }
}

/// Whether `bytes` (of `format`) is animated: an APNG has an `acTL` chunk
/// before its first `IDAT`; an animated WebP is a `VP8X` file with the
/// animation flag (bit 1 of the flags byte) set.
fn is_animated(bytes: &[u8], format: image::ImageFormat) -> bool {
    match format {
        image::ImageFormat::Png => {
            // Walk the chunks: length (4), type (4), data, CRC (4).
            let mut at = 8usize;
            while let (Some(len), Some(kind)) = (bytes.get(at..at + 4), bytes.get(at + 4..at + 8)) {
                match kind {
                    b"acTL" => return true,
                    b"IDAT" | b"IEND" => return false,
                    _ => {}
                }
                let Ok(len) = <[u8; 4]>::try_from(len) else { return false };
                let len = u32::from_be_bytes(len) as usize;
                at = match at.checked_add(12 + len) {
                    Some(next) => next,
                    None => return false,
                };
            }
            false
        }
        image::ImageFormat::WebP => bytes.get(12..16) == Some(b"VP8X") && bytes.get(20).is_some_and(|f| f & 0x02 != 0),
        _ => false,
    }
}

/// Whether `o` turns the image a quarter (width and height swap).
fn quarter_turn(o: image::metadata::Orientation) -> bool {
    use image::metadata::Orientation as O;
    matches!(o, O::Rotate90 | O::Rotate270 | O::Rotate90FlipH | O::Rotate270FlipH)
}

/// Decode `reader` as `format`: the header's dimensions first (refused past
/// [`MAX_IMAGE_PIXELS`] with no pixel decoded), then the pixels under
/// `image::Limits`, its EXIF orientation applied, then a premultiplied
/// Lanczos3 downscale to `max_long_edge`. Returns the image and its
/// upright original size.
fn decode_guarded<R: BufRead + Seek>(
    mut reader: R,
    format: image::ImageFormat,
    max_long_edge: u32,
) -> Result<(image::RgbaImage, (u32, u32)), Diagnostic> {
    let bad = |e: image::ImageError| error("image_format", format!("does not decode: {e}"));
    let (w, h) = image::ImageReader::with_format(&mut reader, format).into_dimensions().map_err(bad)?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > MAX_IMAGE_PIXELS {
        return Err(error(
            "image_too_large",
            format!(
                "is {w} x {h} ({pixels} pixels); images are limited to {MAX_IMAGE_PIXELS} pixels (64 MP) and {MAX_IMAGE_BYTES} bytes (40 MiB)"
            ),
        ));
    }
    if w == 0 || h == 0 {
        return Err(error("image_format", format!("is {w} x {h}: empty")));
    }
    reader.seek(SeekFrom::Start(0)).map_err(|e| error("image_format", format!("cannot re-read: {e}")))?;
    let mut opener = image::ImageReader::with_format(&mut reader, format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(w);
    limits.max_image_height = Some(h);
    // The RGBA8 buffer plus the decoder's own working set.
    limits.max_alloc = Some(MAX_IMAGE_PIXELS * 4 * 2);
    opener.limits(limits);
    let mut decoder = opener.into_decoder().map_err(bad)?;
    // An unreadable EXIF block is no orientation, not a failed image.
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut dynamic = image::DynamicImage::from_decoder(decoder).map_err(bad)?;
    dynamic.apply_orientation(orientation);
    let decoded = dynamic.to_rgba8();
    let original = if quarter_turn(orientation) { (h, w) } else { (w, h) };
    let (ow, oh) = decoded.dimensions();
    let long = ow.max(oh);
    let max = max_long_edge.max(1);
    let image = if long > max {
        let scale = f64::from(max) / f64::from(long);
        let nw = ((f64::from(ow) * scale).round() as u32).clamp(1, max);
        let nh = ((f64::from(oh) * scale).round() as u32).clamp(1, max);
        resize_rgba(&decoded, nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        decoded
    };
    Ok((image, original))
}

/// `img` resized to `w x h` with alpha premultiplied during the filter (no
/// dark fringe where a transparent pixel's colour would bleed in). An
/// opaque image skips the round trip: premultiplying it is the identity.
pub(crate) fn resize_rgba(img: &image::RgbaImage, w: u32, h: u32, filter: image::imageops::FilterType) -> image::RgbaImage {
    if img.pixels().all(|p| p.0[3] == 255) {
        return image::imageops::resize(img, w, h, filter);
    }
    let mut linear = image::DynamicImage::ImageRgba8(img.clone()).to_rgba32f();
    for p in linear.pixels_mut() {
        let [r, g, b, a] = p.0;
        p.0 = [r * a, g * a, b * a, a];
    }
    let mut out = image::imageops::resize(&linear, w, h, filter);
    for p in out.pixels_mut() {
        let [r, g, b, a] = p.0;
        let a = a.clamp(0.0, 1.0);
        p.0 = if a > 0.0 { [(r / a).clamp(0.0, 1.0), (g / a).clamp(0.0, 1.0), (b / a).clamp(0.0, 1.0), a] } else { [0.0; 4] };
    }
    image::DynamicImage::ImageRgba32F(out).to_rgba8()
}

/// `img` and each half-size level below it down to 1 x 1 (a box-like
/// Triangle filter, premultiplied): the mip chain a story image is
/// uploaded with, so a 4x minification samples an average, not every
/// fourth pixel.
#[must_use]
pub(crate) fn mip_chain(img: &image::RgbaImage) -> Vec<image::RgbaImage> {
    let mut levels = vec![img.clone()];
    while let Some(last) = levels.last() {
        let (w, h) = last.dimensions();
        if w <= 1 && h <= 1 {
            break;
        }
        let next = resize_rgba(last, (w / 2).max(1), (h / 2).max(1), image::imageops::FilterType::Triangle);
        levels.push(next);
    }
    levels
}

/// Every image a script names, with the JSON pointer of the image object:
/// the `image` events, the `image` of each panel and callout and the
/// `photo` of each event card, in event order, then each chapter card's
/// `photo` (ruling R2-a: a card photo loads like any story image and its
/// `alt` and `credit` are checked the same way).
#[must_use]
pub fn script_images(script: &Script) -> Vec<(String, std::borrow::Cow<'_, ImageSpec>)> {
    use std::borrow::Cow;
    let photo = |p: &crate::story::script::PhotoSpec| {
        Cow::Owned(ImageSpec {
            // Trimmed, as the card names it (`card_layout::Photo`).
            src: p.src.trim().to_string(),
            alt: Some(p.alt.clone()),
            credit: p.credit.clone(),
            placement: None,
            fit: Some(crate::story::script::Fit::Cover),
            fade_s: None,
        })
    };
    let mut out = Vec::new();
    for (i, e) in script.events.iter().enumerate() {
        match &e.action {
            EventAction::Image(spec) => out.push((format!("/events/{i}/image"), Cow::Borrowed(spec))),
            EventAction::Panel(p) => {
                if let Some(spec) = &p.image {
                    out.push((format!("/events/{i}/panel/image"), Cow::Borrowed(spec)));
                }
            }
            EventAction::Callout(c) => {
                if let Some(spec) = &c.image {
                    out.push((format!("/events/{i}/callout/image"), Cow::Borrowed(spec)));
                }
            }
            EventAction::Card(c) => {
                if let Some(p) = c.photo.as_ref().filter(|p| !p.src.trim().is_empty()) {
                    out.push((format!("/events/{i}/card/photo"), photo(p)));
                }
            }
            _ => {}
        }
    }
    for (i, c) in script.chapters.iter().enumerate() {
        if let Some(p) = c.card.as_ref().and_then(|c| c.photo.as_ref()).filter(|p| !p.src.trim().is_empty()) {
            out.push((format!("/chapters/{i}/card/photo"), photo(p)));
        }
    }
    out
}

/// What [`load_script_images`] found.
#[derive(Clone, Debug, Default)]
pub struct ScriptImages {
    /// Each distinct `src` that loaded, in first-use order.
    pub loaded: Vec<LoadedImage>,
    /// Each distinct `src` that did not, with why (path empty).
    pub failed: Vec<(String, Diagnostic)>,
    /// Every failure at its `src` pointer (errors), plus the `missing_alt`
    /// and `missing_credit` warnings.
    pub diagnostics: Vec<Diagnostic>,
}

impl ScriptImages {
    /// The image loaded for `src`.
    #[must_use]
    pub fn get(&self, src: &str) -> Option<&LoadedImage> {
        self.loaded.iter().find(|l| l.src == src)
    }
}

/// Load every image `script` names (each distinct `src` once), at
/// `max_long_edge`: every failure as an error at its `src` pointer, plus a
/// `missing_alt` warning per image without `alt` and a `missing_credit`
/// warning (the render is not publishable, spec R7; Task 17's publish
/// table blocks it) per image without `credit`.
pub async fn load_script_images(script: &Script, base_dir: &Path, cache_dir: &Path, max_long_edge: u32, offline: bool) -> ScriptImages {
    let mut out = ScriptImages::default();
    let base_dir = if base_dir.as_os_str().is_empty() { Path::new(".") } else { base_dir };
    let fetch = match ImageFetch::new() {
        Ok(f) => Some(f),
        Err(d) => {
            out.diagnostics.push(d);
            None
        }
    };
    for (at, spec) in script_images(script) {
        let warn = |code: &'static str, message: String, hint: &str| Diagnostic {
            severity: Severity::Warning,
            path: at.clone(),
            code,
            message,
            hint: Some(hint.to_string()),
        };
        let shown = crate::story::redact_url_secrets(&spec.src);
        if spec.alt.as_deref().is_none_or(|a| a.trim().is_empty()) {
            out.diagnostics.push(warn(
                "missing_alt",
                format!("image `{shown}` has no `alt` text"),
                "describe the image in `alt` for screen readers and the transcript",
            ));
        }
        if spec.credit.as_deref().is_none_or(|c| c.trim().is_empty()) {
            out.diagnostics.push(warn(
                "missing_credit",
                format!("image `{shown}` has no `credit`: the render is not publishable"),
                "name the photographer or source in `credit`",
            ));
        }
        if out.get(&spec.src).is_some() {
            continue;
        }
        if let Some((_, d)) = out.failed.iter().find(|(src, _)| *src == spec.src) {
            let d = Diagnostic { path: format!("{at}/src"), ..d.clone() };
            out.diagnostics.push(d);
            continue;
        }
        let Some(fetch) = fetch.as_ref() else { continue };
        match load_image_with(fetch, &spec.src, base_dir, cache_dir, max_long_edge, offline).await {
            Ok(image) => {
                if image.animated {
                    out.diagnostics.push(warn(
                        "animated_image_first_frame",
                        format!("image `{shown}` is animated; only its first frame is drawn"),
                        "use a still image, or export the frame you want",
                    ));
                }
                out.loaded.push(image);
            }
            Err(d) => {
                out.diagnostics.push(Diagnostic { path: format!("{at}/src"), ..d.clone() });
                out.failed.push((spec.src.clone(), d));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap().block_on(f)
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(w, h, image::Rgba([200, 100, 50, 255])).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// The 404 half of Review Focus 4's test (the render half is in
    /// `player::tests`): a fetch answered 404 is `image_fetch_failed` with
    /// the status in the message.
    #[test]
    fn an_image_that_404s_is_an_error_in_validation_and_a_gap_in_a_render() {
        let dir = tempfile::tempdir().unwrap();
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/a.png")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
            let url = format!("{}/a.png?token=sekrit", server.uri());
            let e = load_image_with(&ImageFetch::for_tests(), &url, dir.path(), dir.path(), 1920, false).await.unwrap_err();
            assert_eq!(e.code, "image_fetch_failed");
            assert!(e.message.contains("404"), "{}", e.message);
            assert!(!e.message.contains("sekrit"), "the secret is redacted: {}", e.message);
            // Production refuses plain http before connecting.
            let e = load_image(&url, dir.path(), dir.path(), 1920, false).await.unwrap_err();
            assert_eq!(e.code, "image_fetch_failed");
            assert!(server.received_requests().await.unwrap().len() == 1, "the https-only fetcher never connected");
        });
    }

    #[test]
    fn a_huge_image_is_downscaled_on_load() {
        let dir = tempfile::tempdir().unwrap();
        image::RgbaImage::from_pixel(6000, 3000, image::Rgba([10, 20, 30, 255])).save(dir.path().join("big.png")).unwrap();
        let img = block_on(load_image("big.png", dir.path(), dir.path(), 1920, true)).unwrap();
        assert_eq!((img.image.width(), img.image.height()), (1920, 960));
        assert_eq!(img.original, (6000, 3000));
        assert_eq!(img.bundle_name, format!("{}.png", &img.sha256[..16]));
    }

    /// A reader that counts the bytes taken out of it.
    struct Counting<R> {
        inner: R,
        read: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<R: std::io::Read> std::io::Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    /// A 1x1 PNG whose IHDR is patched to `w x h` (CRC recomputed), with a
    /// 64 KiB IDAT behind it: a full decode would have to read it.
    fn bomb(w: u32, h: u32) -> Vec<u8> {
        let mut png = png_bytes(1, 1);
        // Signature (8), length (4), "IHDR" (4), then width and height.
        png[16..20].copy_from_slice(&w.to_be_bytes());
        png[20..24].copy_from_slice(&h.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let iend = png.split_off(png.len() - 12);
        let body = vec![0u8; 64 * 1024];
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut chunk = b"IDAT".to_vec();
        chunk.extend_from_slice(&body);
        png.extend_from_slice(&chunk);
        png.extend_from_slice(&crc32(&chunk).to_be_bytes());
        png.extend_from_slice(&iend);
        png
    }

    /// Ruling F21: decided from the header, under 1 KiB read, before any
    /// pixel is decoded -- and through `load_image` too.
    #[test]
    fn a_decompression_bomb_is_refused() {
        let bytes = bomb(20_000, 20_000);
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = std::io::BufReader::with_capacity(64, Counting { inner: Cursor::new(bytes.clone()), read: Arc::clone(&read) });
        let e = decode_guarded(reader, image::ImageFormat::Png, 1920).unwrap_err();
        assert_eq!(e.code, "image_too_large");
        assert!(e.message.contains("64 MP") && e.message.contains("40 MiB"), "the limits are named: {}", e.message);
        let consumed = read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(consumed < 1024, "header only: {consumed} bytes read of {}", bytes.len());
        // The same file through the public door.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bomb.png"), &bytes).unwrap();
        let e = block_on(load_image("bomb.png", dir.path(), dir.path(), 1920, true)).unwrap_err();
        assert_eq!(e.code, "image_too_large");
        // Ruling F4: 12000 x 12000 (144 MP) is refused, not downscaled.
        let e = decode_guarded(Cursor::new(bomb(12_000, 12_000)), image::ImageFormat::Png, 1920).unwrap_err();
        assert_eq!(e.code, "image_too_large");
    }

    #[test]
    fn the_request_carries_no_personal_data() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = png_bytes(4, 2);
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/p.png"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .mount(&server)
                .await;
            let url = format!("{}/p.png", server.uri());
            let img = load_image_with(&ImageFetch::for_tests(), &url, dir.path(), dir.path(), 1920, false).await.unwrap();
            assert_eq!(img.original, (4, 2));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let mut names: Vec<String> = requests[0].headers.keys().map(|k| k.as_str().to_ascii_lowercase()).collect();
            names.sort();
            assert_eq!(names, ["accept", "accept-encoding", "host", "user-agent"], "{:?}", requests[0].headers);
            assert_eq!(requests[0].headers.get("user-agent").unwrap().to_str().unwrap(), IMAGE_USER_AGENT);
            // Cached: a second load (even offline) does not fetch again.
            let again = load_image_with(&ImageFetch::for_tests(), &url, dir.path(), dir.path(), 1920, true).await.unwrap();
            assert_eq!(again.sha256, img.sha256);
            assert_eq!(server.received_requests().await.unwrap().len(), 1, "served from the cache");
            let cached: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert!(cached.iter().all(|n| !n.to_string_lossy().contains("partial")), "{cached:?}");
        });
    }

    #[test]
    fn only_png_jpeg_and_webp_load_and_paths_stay_in_the_story() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.gif"), b"GIF89a\x01\x00\x01\x00").unwrap();
        let e = block_on(load_image("a.gif", dir.path(), dir.path(), 1920, true)).unwrap_err();
        assert_eq!(e.code, "image_format");
        let e = block_on(load_image("../a.png", dir.path(), dir.path(), 1920, true)).unwrap_err();
        assert_eq!(e.code, "path_escapes");
        let e = block_on(load_image("img/none.png", dir.path(), dir.path(), 1920, true)).unwrap_err();
        assert_eq!(e.code, "file_not_found");
        // The same bytes give the same pixels.
        std::fs::write(dir.path().join("p.png"), png_bytes(30, 20)).unwrap();
        let a = block_on(load_image("p.png", dir.path(), dir.path(), 16, true)).unwrap();
        let b = block_on(load_image("p.png", dir.path(), dir.path(), 16, true)).unwrap();
        assert_eq!((a.image.width(), a.image.height()), (16, 11));
        assert_eq!(a.image.as_raw(), b.image.as_raw());
    }

    /// A PNG with an `eXIf` chunk (before IDAT) carrying `orientation`.
    fn png_with_orientation(w: u32, h: u32, orientation: u16) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, _| if x == 0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        let mut png = out.into_inner();
        // Big-endian TIFF: header, one IFD entry (0x0112 Orientation, SHORT, 1), no next IFD.
        let mut tiff = b"MM\x00\x2a\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01".to_vec();
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut chunk = b"eXIf".to_vec();
        chunk.extend_from_slice(&tiff);
        let mut bytes = (tiff.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&chunk);
        bytes.extend_from_slice(&crc32(&chunk).to_be_bytes());
        // After the IHDR chunk (8 + 25 bytes).
        let tail = png.split_off(33);
        png.extend_from_slice(&bytes);
        png.extend_from_slice(&tail);
        png
    }

    /// Review F2: a phone portrait stored landscape (EXIF orientation 6,
    /// rotate 90° clockwise) loads upright, and `original` is upright.
    #[test]
    fn an_exif_rotated_portrait_loads_upright() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.png"), png_with_orientation(40, 20, 6)).unwrap();
        let img = block_on(load_image("p.png", dir.path(), dir.path(), 1920, true)).unwrap();
        assert_eq!((img.image.width(), img.image.height()), (20, 40), "upright portrait");
        assert_eq!(img.original, (20, 40));
        // The stored left column (red) is the top row after a clockwise quarter turn.
        assert_eq!(img.image.get_pixel(10, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.image.get_pixel(10, 39).0, [0, 0, 255, 255]);
        // Downscaled after the turn: the long edge is the height.
        let small = block_on(load_image("p.png", dir.path(), dir.path(), 20, true)).unwrap();
        assert_eq!((small.image.width(), small.image.height()), (10, 20));
        assert_eq!(small.original, (20, 40));
        // No orientation: as stored.
        std::fs::write(dir.path().join("q.png"), png_with_orientation(40, 20, 1)).unwrap();
        let img = block_on(load_image("q.png", dir.path(), dir.path(), 1920, true)).unwrap();
        assert_eq!((img.image.width(), img.image.height(), img.original), (40, 20, (40, 20)));
    }

    /// Review F13: a resize premultiplies, so the edge of a white shape on
    /// transparent black stays white (no dark fringe).
    #[test]
    fn a_downscale_leaves_no_dark_fringe_on_a_transparent_edge() {
        let img = image::RgbaImage::from_fn(64, 64, |x, _| if x < 30 { image::Rgba([0, 0, 0, 0]) } else { image::Rgba([255, 255, 255, 255]) });
        let small = resize_rgba(&img, 16, 16, image::imageops::FilterType::Lanczos3);
        let partial: Vec<_> = small.pixels().filter(|p| p.0[3] > 8 && p.0[3] < 250).collect();
        assert!(!partial.is_empty(), "the edge is partly transparent");
        for p in small.pixels().filter(|p| p.0[3] > 8) {
            assert!(p.0[0] >= 240 && p.0[1] >= 240 && p.0[2] >= 240, "a fringe pixel {:?}", p.0);
        }
        // An opaque image is resized exactly as before.
        let opaque = image::RgbaImage::from_fn(30, 20, |x, y| image::Rgba([(x * 8) as u8, (y * 12) as u8, 7, 255]));
        assert_eq!(resize_rgba(&opaque, 15, 10, image::imageops::FilterType::Lanczos3), image::imageops::resize(&opaque, 15, 10, image::imageops::FilterType::Lanczos3));
    }

    #[test]
    fn a_mip_chain_halves_to_one_pixel_and_averages() {
        let checker = image::RgbaImage::from_fn(16, 8, |x, y| if (x + y) % 2 == 0 { image::Rgba([255; 4]) } else { image::Rgba([0, 0, 0, 255]) });
        let levels = mip_chain(&checker);
        let sizes: Vec<_> = levels.iter().map(|l| l.dimensions()).collect();
        assert_eq!(sizes, [(16, 8), (8, 4), (4, 2), (2, 1), (1, 1)]);
        for p in levels[2].pixels() {
            assert!((100..=155).contains(&p.0[0]), "a quarter-size level is grey, not checker: {:?}", p.0);
        }
    }

    /// Review F4: the original bytes travel with the image, for the bundle.
    #[test]
    fn the_original_bytes_travel_with_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = png_bytes(8, 4);
        std::fs::write(dir.path().join("p.png"), &bytes).unwrap();
        let img = block_on(load_image("p.png", dir.path(), dir.path(), 4, true)).unwrap();
        assert_eq!(&img.source[..], &bytes[..]);
        assert_eq!(img.sha256, hex_sha256(&bytes));
    }

    /// Review F14: a FIFO in the story folder is refused, never read.
    #[test]
    fn a_fifo_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.png");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let e = block_on(load_image("pipe.png", dir.path(), dir.path(), 1920, true)).unwrap_err();
        assert_eq!(e.code, "file_not_found");
        assert!(e.message.contains("not a regular file"), "{}", e.message);
    }

    /// Review F6: loopback, private, link-local and unique-local hosts are
    /// refused before any request, with nothing about the network said.
    #[test]
    fn local_and_private_hosts_are_refused_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        for url in [
            "https://127.0.0.1/a.png",
            "https://10.1.2.3/a.png",
            "https://172.16.0.9/a.png",
            "https://192.168.1.1/a.png",
            "https://169.254.169.254/a.png",
            "https://100.64.0.1/a.png",
            "https://[::1]/a.png",
            "https://[fd00::1]/a.png",
            "https://[fe80::1]/a.png",
            "https://[::ffff:10.0.0.1]/a.png",
            "https://localhost/a.png",
            "https://x.localhost/a.png",
            "https://0.0.0.0/a.png",
            // Residuals r2: decimal IPv4, site-local, NAT64, 6to4, IPv4-compatible, benchmarking, reserved.
            "https://2130706433/a.png",
            "https://[fec0::1]/a.png",
            "https://[64:ff9b::a00:1]/a.png",
            "https://[2002:7f00:1::]/a.png",
            "https://[::a00:1]/a.png",
            "https://198.18.0.1/a.png",
            "https://240.0.0.1/a.png",
            "https://255.255.255.255/a.png",
            // Final review F6: IETF protocol assignments, Teredo with a
            // private client (10.0.0.1 obfuscated) or a private server, discard.
            "https://192.0.0.8/a.png",
            "https://[2001:0:4136:e378:8000:63bf:f5ff:fffe]/a.png",
            "https://[2001:0:a00:1:8000:63bf:f7f7:f7f7]/a.png",
            "https://[100::1]/a.png",
        ] {
            let e = block_on(load_image(url, dir.path(), dir.path(), 1920, false)).unwrap_err();
            assert_eq!(e.code, "image_fetch_failed", "{url}");
            assert!(e.message.contains(NOT_PUBLIC), "{url}: {}", e.message);
        }
        // The resolver drops non-public answers, and a name with only those fails.
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            public_addrs(vec![addr("127.0.0.1:0"), addr("10.0.0.1:0"), addr("93.184.216.34:0"), addr("[fd12::1]:0")], false),
            vec![addr("93.184.216.34:0")]
        );
        assert!(!ip_refused("2606:4700::1111".parse().unwrap(), false), "public IPv6 is fetched");
        assert!(!ip_refused("64:ff9b::5db8:d822".parse().unwrap(), false), "NAT64 of a public IPv4 is fetched");
        assert!(!ip_refused("2002:5db8:d822::".parse().unwrap(), false), "6to4 of a public IPv4 is fetched");
        assert!(
            !ip_refused("2001:0:4136:e378:8000:63bf:f7f7:f7f7".parse().unwrap(), false),
            "Teredo of a public server and a public client (8.8.8.8) is fetched"
        );
        assert!(ip_refused("2001:0:4136:e378:8000:63bf:3fff:fdd2".parse().unwrap(), false), "Teredo of 192.0.2.45 is not");
        assert!(!ip_refused("192.0.1.1".parse().unwrap(), false) && !ip_refused("100:0:0:1::1".parse().unwrap(), false), "the ranges' neighbours");
        assert!(!ip_refused("198.20.0.1".parse().unwrap(), false) && !ip_refused("93.184.216.34".parse().unwrap(), false));
        let resolved = block_on(async {
            let name: reqwest::dns::Name = "localhost".parse().unwrap();
            reqwest::dns::Resolve::resolve(&PublicOnly { allow_loopback: false }, name).await.map(|a| a.count())
        });
        assert!(resolved.is_err(), "localhost resolves to loopback only");
    }

    /// Review F6 + F7: three redirects are followed, a fourth is refused,
    /// and a redirect to a private address is refused.
    #[test]
    fn three_redirects_are_followed_and_a_fourth_or_a_private_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = png_bytes(2, 2);
        block_on(async {
            let server = MockServer::start().await;
            let hop = |from: &str, to: String| Mock::given(method("GET")).and(path(from.to_string())).respond_with(ResponseTemplate::new(302).insert_header("location", to.as_str()));
            for (from, to) in [("/a0", "/a1"), ("/a1", "/a2"), ("/a2", "/a3"), ("/b0", "/b1"), ("/b1", "/b2"), ("/b2", "/b3"), ("/b3", "/b4")] {
                hop(from, format!("{}{to}", server.uri())).mount(&server).await;
            }
            hop("/p0", "http://10.0.0.1/a.png".to_string()).mount(&server).await;
            for end in ["/a3", "/b4"] {
                Mock::given(method("GET")).and(path(end)).respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone())).mount(&server).await;
            }
            let fetch = ImageFetch::for_tests();
            let ok = load_image_with(&fetch, &format!("{}/a0", server.uri()), dir.path(), dir.path(), 64, false).await;
            assert_eq!(ok.unwrap().original, (2, 2), "3 redirects are followed");
            let e = load_image_with(&fetch, &format!("{}/b0", server.uri()), dir.path(), dir.path(), 64, false).await.unwrap_err();
            assert!(e.message.contains("redirect"), "a 4th is refused: {}", e.message);
            let e = load_image_with(&fetch, &format!("{}/p0", server.uri()), dir.path(), dir.path(), 64, false).await.unwrap_err();
            assert!(e.message.contains("redirect"), "a private target is refused: {}", e.message);
            assert!(!e.message.contains("10.0.0.1"), "{}", e.message);
        });
    }

    /// Review F8: a cached copy that does not decode is evicted and fetched
    /// once more online; offline it is an error, not a fetch.
    #[test]
    fn a_cached_copy_that_does_not_decode_is_refetched_once() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = png_bytes(3, 3);
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/c.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone())).mount(&server).await;
            let url = format!("{}/c.png", server.uri());
            let key = hex_sha256(url.as_bytes());
            // A torn entry that still sniffs as PNG.
            std::fs::write(dir.path().join(&key), &bytes[..20]).unwrap();
            let fetch = ImageFetch::for_tests();
            let e = load_image_with(&fetch, &url, dir.path(), dir.path(), 64, true).await.unwrap_err();
            assert_eq!(e.code, "image_format", "offline: the bad copy is all there is");
            assert_eq!(server.received_requests().await.unwrap().len(), 0);
            let img = load_image_with(&fetch, &url, dir.path(), dir.path(), 64, false).await.unwrap();
            assert_eq!(img.original, (3, 3));
            assert_eq!(server.received_requests().await.unwrap().len(), 1, "fetched once more");
            assert_eq!(std::fs::read(dir.path().join(&key)).unwrap(), bytes, "the good copy replaced it");
            let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).filter(|n| n.to_string_lossy().starts_with(".partial")).collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        });
    }

    /// Review F10: a presigned URL's signatures never reach a diagnostic.
    #[test]
    fn a_presigned_url_that_404s_keeps_its_signatures_secret() {
        let dir = tempfile::tempdir().unwrap();
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
            let url = format!(
                "{}/a.png?X-Amz-Algorithm=AWS4&X-Amz-Credential=AKIASECRET1&X-Amz-Security-Token=SECRET2&X-Amz-Signature=SECRET3\
                 &Signature=SECRET4&sig=SECRET5&X-Goog-Signature=SECRET6&X-Goog-Credential=SECRET7",
                server.uri()
            );
            let e = load_image_with(&ImageFetch::for_tests(), &url, dir.path(), dir.path(), 64, false).await.unwrap_err();
            assert!(e.message.contains("404"), "{}", e.message);
            assert!(!e.message.contains("SECRET"), "{}", e.message);
        });
    }

    /// Residuals r3: a host that is down is "could not connect", never
    /// blamed on an address rule.
    #[test]
    fn a_host_that_is_down_is_could_not_connect() {
        let dir = tempfile::tempdir().unwrap();
        // Port 9 (discard) on loopback: nothing listens.
        let e = block_on(load_image_with(&ImageFetch::for_tests(), "http://127.0.0.1:9/a.png", dir.path(), dir.path(), 64, false)).unwrap_err();
        assert!(e.message.ends_with("failed: could not connect"), "{}", e.message);
        assert!(!e.message.contains("public"), "{}", e.message);
    }

    /// Residuals r1: a proxy in the environment is never used (through
    /// one, `PublicOnly` would never see the target). Run in a child
    /// process, as the proxy variables are process-wide.
    #[test]
    fn a_proxy_in_the_environment_is_ignored() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "story::media::tests::proxy_child", "--test-threads=1"])
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("http_proxy", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && text.contains("1 passed"), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    }

    /// The child half of [`a_proxy_in_the_environment_is_ignored`].
    #[test]
    #[ignore = "run by a_proxy_in_the_environment_is_ignored with proxy variables set"]
    fn proxy_child() {
        assert!(std::env::var("HTTP_PROXY").is_ok(), "run by the parent test");
        let dir = tempfile::tempdir().unwrap();
        let bytes = png_bytes(2, 2);
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/p.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone())).mount(&server).await;
            let img = load_image_with(&ImageFetch::for_tests(), &format!("{}/p.png", server.uri()), dir.path(), dir.path(), 64, false).await;
            assert_eq!(img.expect("fetched directly, not through the dead proxy").original, (2, 2));
        });
    }

    /// Residuals NEW-2: an image refused for its size is never cached, and
    /// a cached copy refused for its size is not refetched (the answer
    /// would be the same).
    #[test]
    fn an_oversized_remote_image_is_not_cached_or_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let big = bomb(20_000, 20_000);
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/big.png")).respond_with(ResponseTemplate::new(200).set_body_bytes(big.clone())).mount(&server).await;
            let url = format!("{}/big.png", server.uri());
            let fetch = ImageFetch::for_tests();
            let e = load_image_with(&fetch, &url, dir.path(), dir.path(), 64, false).await.unwrap_err();
            assert_eq!(e.code, "image_too_large");
            let key = hex_sha256(url.as_bytes());
            assert!(!dir.path().join(&key).exists(), "never cached");
            // A copy already in the cache: refused from it, not fetched again.
            std::fs::write(dir.path().join(&key), &big).unwrap();
            let e = load_image_with(&fetch, &url, dir.path(), dir.path(), 64, false).await.unwrap_err();
            assert_eq!(e.code, "image_too_large");
            assert_eq!(server.received_requests().await.unwrap().len(), 1, "no refetch for a size refusal");
        });
    }

    /// Residuals F13: an APNG or an animated WebP is drawn by its first
    /// frame, with a warning.
    #[test]
    fn an_animated_image_warns_that_only_its_first_frame_draws() {
        // APNG: an `acTL` chunk (2 frames, loop forever) after IHDR.
        let mut png = png_bytes(4, 4);
        let mut chunk = b"acTL".to_vec();
        chunk.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 0]);
        let mut actl = 8u32.to_be_bytes().to_vec();
        actl.extend_from_slice(&chunk);
        actl.extend_from_slice(&crc32(&chunk).to_be_bytes());
        let tail = png.split_off(33);
        png.extend_from_slice(&actl);
        png.extend_from_slice(&tail);
        assert!(is_animated(&png, image::ImageFormat::Png));
        assert!(!is_animated(&png_bytes(4, 4), image::ImageFormat::Png));
        // WebP: VP8X with the animation flag, and without.
        let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00".to_vec();
        webp.extend_from_slice(&[0x02, 0, 0, 0]);
        assert!(is_animated(&webp, image::ImageFormat::WebP));
        webp[20] = 0x10;
        assert!(!is_animated(&webp, image::ImageFormat::WebP));
        // Through the loader: the warning at the image.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), &png).unwrap();
        let json = serde_json::json!({"story": 1, "scene": {"style": "s.json", "basemap": {"source": "fixture"}},
            "camera": [{"type": "jump", "to": {"lon": 0.0, "lat": 0.0, "zoom": 1.0}}],
            "events": [{"at": 0.0, "until": 1.0, "image": {"src": "a.png", "alt": "a", "credit": "c"}}]});
        let script = crate::story::parse(&serde_json::to_vec(&json).unwrap()).unwrap();
        let found = block_on(load_script_images(&script, dir.path(), dir.path(), 64, true));
        assert_eq!(found.loaded.len(), 1, "{:?}", found.diagnostics);
        assert!(found.loaded[0].animated);
        assert!(found.diagnostics.iter().any(|d| d.code == "animated_image_first_frame" && d.path == "/events/0/image"), "{:?}", found.diagnostics);
    }
}
