//! A style's sprite sheet on disk: fetched once (with the project's
//! User-Agent), validated, cached under `~/.cache/gpui-map/sprites/`, and
//! refreshed online once older than [`SPRITE_CACHE_MAX_AGE`] -- the
//! story loader's (`story::validate::scene_inputs`) source of icons.
//!
//! Not under `src/story`: the cache's age is a wall-clock read, which the
//! story files may not make (`tests/story_clock_scan.rs`). It decides which
//! sheet a render LOADS, once, before any frame -- never a frame -- and the
//! sheet a render drew is recorded in its manifest ([`SpriteSheetInfo`]),
//! so a frame stays a function of its script, its tiles and that sheet.
//!
//! Nothing here holds or prints a credential: the cache folder and every
//! error go through `story::validate::redacted_url` (no query, no
//! userinfo) and `sprite::redact_url_credentials`.
use std::path::{Path, PathBuf};

use crate::story::validate::redacted_url;

/// The sprite sheet a render drew its icons from, in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SpriteSheetInfo {
    /// The sheet's base URL with no query and no userinfo
    /// (`story::validate::redacted_url`): never a credential.
    pub base: String,
    /// SHA-256 of the `@2x` JSON bytes followed by the `@2x` PNG bytes.
    pub sha256: String,
}

#[cfg(feature = "sprite_http_fetch")]
/// The project's User-Agent for the sprite fetch: the crate and its
/// repository, nothing about the person running it.
const SPRITE_USER_AGENT: &str = concat!("gpui-maps/", env!("CARGO_PKG_VERSION"), " (+https://github.com/swatmobility/gpui-maps)");

/// How old a cached sheet may get before an online load fetches it again.
/// `--offline` uses a cached sheet of any age.
pub const SPRITE_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// `~/.cache/gpui-map/sprites/<url>`: the sheet's folder, named after its
/// base URL through [`redacted_url`] (no query, no userinfo: no
/// `access_token`, `key` or `user:pass@` ever reaches a path), made one
/// safe path component. Outside every repository: a basemap's sprite art
/// is its licensor's.
pub(crate) fn sprite_cache_dir(base_url: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    sprite_cache_dir_under(&home.join(".cache/gpui-map/sprites"), base_url)
}

fn sprite_cache_dir_under(root: &Path, base_url: &str) -> PathBuf {
    let key = redacted_url(base_url);
    let safe: String = key
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    root.join(if safe.is_empty() { "unnamed".to_string() } else { safe })
}

/// A loaded sheet: the atlas and the manifest record of it.
pub(crate) struct LoadedSheet {
    pub(crate) atlas: crate::sprite::SpriteAtlas,
    pub(crate) info: SpriteSheetInfo,
}

/// Parse and slice a sheet's two files, with its manifest record.
fn sheet_from_bytes(base_url: &str, json: &[u8], png: &[u8]) -> anyhow::Result<LoadedSheet> {
    let manifest = crate::sprite::parse_sprite_manifest(json)?;
    let atlas = crate::sprite::slice_atlas(png, &manifest)?;
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update(json);
    hash.update(png);
    let sha256 = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(LoadedSheet { atlas, info: SpriteSheetInfo { base: redacted_url(base_url), sha256 } })
}

/// Write `bytes` to `path` through a temp file of its own (the file's
/// name and this process's id in it, so two files -- or two processes --
/// never share one) and an atomic rename.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = partial_path(path);
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&partial);
    })
}

/// `<file name>.<pid>.partial` beside `path`.
fn partial_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.{}.partial", std::process::id()))
}

/// The file naming the cache folder's current sheet: its SHA-256, which
/// is also the name of the folder holding the pair.
const CURRENT: &str = "current";

/// The pair in `dir/<sha>/`, if it is there, parses, and hashes to `sha`
/// (a pair from two different fetches cannot pass that check).
fn cached_pair(dir: &Path, base_url: &str, sha: &str) -> Option<LoadedSheet> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let pair = dir.join(sha);
    let json = std::fs::read(pair.join("sprite@2x.json")).ok()?;
    let png = std::fs::read(pair.join("sprite@2x.png")).ok()?;
    sheet_from_bytes(base_url, &json, &png).ok().filter(|sheet| sheet.info.sha256 == sha)
}

/// Stores a parsed pair as ONE unit: both files are written into a temp
/// folder of this process's, which is renamed to `dir/<sha>/` in one step
/// (an existing `dir/<sha>/` already holds the same bytes, and is kept),
/// and only then is `dir/current` pointed at it (atomically, last). Two
/// concurrent refreshes can each land a whole pair, but never a JSON from
/// one next to a PNG from the other.
fn store_pair(dir: &Path, sha: &str, json: &[u8], png: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let target = dir.join(sha);
    let whole = |t: &Path| -> bool {
        use sha2::Digest as _;
        let (Ok(j), Ok(p)) = (std::fs::read(t.join("sprite@2x.json")), std::fs::read(t.join("sprite@2x.png"))) else {
            return false;
        };
        let mut hash = sha2::Sha256::new();
        hash.update(&j);
        hash.update(&p);
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>() == sha
    };
    if target.exists() && !whole(&target) {
        // Damaged by hand or by a crash: replaced whole below.
        let _ = std::fs::remove_dir_all(&target);
    }
    if !whole(&target) {
        let temp = dir.join(format!(".{}-{}.partial", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let written = (|| {
            std::fs::create_dir_all(&temp)?;
            std::fs::write(temp.join("sprite@2x.json"), json)?;
            std::fs::write(temp.join("sprite@2x.png"), png)?;
            std::fs::rename(&temp, &target)
        })();
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&temp);
            // Another process renamed the same pair in first: that is the
            // pair wanted, so this is not a failure.
            if !whole(&target) {
                return written;
            }
        }
    }
    write_atomically(&dir.join(CURRENT), sha.as_bytes())
}

/// The `@2x` sheet at `base_url`, through the cache folder `dir`.
///
/// `pinned` is a sheet's SHA-256 from an earlier render's manifest
/// (`SpriteSheetInfo::sha256`): when that pair is still cached it is
/// used as it is, whatever its age, so a `--frames` range re-rendered to
/// splice into that render draws the icons it drew. When it is not, the
/// usual rules pick the sheet, and the caller compares the hash.
///
/// Otherwise a cached pair that parses and is younger than
/// [`SPRITE_CACHE_MAX_AGE`] wins. Otherwise, online, the sheet is fetched,
/// parsed and sliced, and only a sheet that parsed is written to the
/// cache (a captive portal's page is never cached); if that fetch fails,
/// a stale cached sheet is still used. `offline` uses a cached sheet of
/// any age and never the network. Every error is redacted
/// ([`redacted_url`] and `sprite::redact_url_credentials`).
///
/// On disk: `dir/<sha>/sprite@2x.{json,png}` per sheet and `dir/current`
/// naming the newest ([`store_pair`]).
pub(crate) fn sprite_sheet_in(
    dir: &Path,
    base_url: &str,
    offline: bool,
    pinned: Option<&str>,
    fetch: &dyn Fn(&str) -> anyhow::Result<(Vec<u8>, Vec<u8>)>,
) -> anyhow::Result<LoadedSheet> {
    if let Some(sheet) = pinned.and_then(|sha| cached_pair(dir, base_url, sha)) {
        return Ok(sheet);
    }
    let current = dir.join(CURRENT);
    let cached = std::fs::read_to_string(&current).ok().and_then(|sha| cached_pair(dir, base_url, sha.trim()));
    let fresh = std::fs::metadata(&current)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < SPRITE_CACHE_MAX_AGE);
    match cached {
        Some(sheet) if offline || fresh => return Ok(sheet),
        None if offline => anyhow::bail!("not cached at {} and --offline", dir.display()),
        _ => {}
    }
    let fetched = fetch(base_url).and_then(|(json, png)| {
        let sheet = sheet_from_bytes(base_url, &json, &png)
            .map_err(|e| anyhow::anyhow!("{} is not a sprite sheet: {e:#}", redacted_url(base_url)))?;
        store_pair(dir, &sheet.info.sha256, &json, &png)?;
        Ok(sheet)
    });
    match (fetched, cached) {
        (Ok(sheet), _) => Ok(sheet),
        (Err(e), Some(stale)) => {
            eprintln!(
                "story: the sprite sheet {} could not be refreshed ({}); using the cached copy",
                redacted_url(base_url),
                crate::sprite::redact_url_credentials(&format!("{e:#}"))
            );
            Ok(stale)
        }
        (Err(e), None) => Err(anyhow::anyhow!("{}", crate::sprite::redact_url_credentials(&format!("{e:#}")))),
    }
}

/// The sprite sheet at `base_url`, sliced, through the same disk cache the
/// story loader uses (`~/.cache/gpui-map/sprites/`): fetched once with the
/// project's User-Agent, reused for [`SPRITE_CACHE_MAX_AGE`], and a stale
/// copy still serves when a refresh fails. `offline` never touches the
/// network. For a shell that wants a public sheet it does not ship, such as
/// OpenFreeMap's, whose licence is unverified and so is not in the tree.
pub fn cached_sprite_atlas(base_url: &str, offline: bool) -> anyhow::Result<crate::sprite::SpriteAtlas> {
    sprite_sheet_in(&sprite_cache_dir(base_url), base_url, offline, None, &fetch_sprite_sheet).map(|sheet| sheet.atlas)
}

/// A `reqwest` failure as text with no URL in it but the redacted one.
#[cfg(feature = "sprite_http_fetch")]
fn fetch_error(url: &str, e: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!("{} ({})", e.without_url(), redacted_url(url))
}

/// The two `@2x` files of `base_url` (`sprite::sprite_urls`), on a thread
/// of its own with its own runtime so this works inside a runtime or not.
#[cfg(feature = "sprite_http_fetch")]
pub(crate) fn fetch_sprite_sheet(base_url: &str) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let (json_url, png_url) = crate::sprite::sprite_urls(base_url);
    std::thread::spawn(move || -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(async {
            let http = reqwest::Client::builder()
                .user_agent(SPRITE_USER_AGENT)
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
            let mut out = Vec::new();
            for url in [&json_url, &png_url] {
                let response = http
                    .get(url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(|e| fetch_error(url, e))?;
                out.push(response.bytes().await.map_err(|e| fetch_error(url, e))?.to_vec());
            }
            let png = out.pop().unwrap_or_default();
            let json = out.pop().unwrap_or_default();
            Ok::<_, anyhow::Error>((json, png))
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the sprite fetch panicked"))?
}

#[cfg(not(feature = "sprite_http_fetch"))]
pub(crate) fn fetch_sprite_sheet(_base_url: &str) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    anyhow::bail!("this build has no HTTP sprite fetch (feature `sprite_http_fetch`)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid one-icon `@2x` sheet: `(json, png)`.
    fn tiny_sheet() -> (Vec<u8>, Vec<u8>) {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 255, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        (br#"{"dot": {"x": 0, "y": 0, "width": 4, "height": 4, "pixelRatio": 2}}"#.to_vec(), png)
    }

    /// Review F1: no form of secret -- `?key=`, `?access_token=`,
    /// `user:pass@` -- reaches the cache folder or a fetch error.
    #[cfg(feature = "sprite_http_fetch")]
    #[test]
    fn no_sprite_secret_reaches_the_cache_path_or_an_error() {
        let dir = tempfile::tempdir().unwrap();
        for url in [
            "https://api.maptiler.com/maps/streets/sprite?key=SECRET",
            "https://api.mapbox.com/styles/v1/mapbox/streets-v12/sprite?access_token=SECRET",
            "https://user:SECRET@tiles.example/sprites/ofm",
            // Port 9 on loopback refuses at once: a real reqwest error.
            "http://user:SECRET@127.0.0.1:9/sprite?key=SECRET&access_token=SECRET",
        ] {
            let cache = sprite_cache_dir(url);
            assert!(!cache.to_string_lossy().contains("SECRET"), "{}", cache.display());
            let under = sprite_cache_dir_under(dir.path(), url);
            assert!(!under.to_string_lossy().contains("SECRET"), "{}", under.display());
        }
        let url = "http://user:SECRET@127.0.0.1:9/sprite?key=SECRET&access_token=SECRET";
        let direct = format!("{:#}", fetch_sprite_sheet(url).unwrap_err());
        assert!(!direct.contains("SECRET") && direct.contains("127.0.0.1:9/sprite"), "{direct}");
        let through = sprite_sheet_in(&sprite_cache_dir_under(dir.path(), url), url, false, None, &fetch_sprite_sheet);
        let text = format!("{:#}", through.err().unwrap());
        assert!(!text.contains("SECRET"), "{text}");
    }

    /// Review F2: bytes that are not a sheet are never cached, and the
    /// two files never share a temp name.
    #[test]
    fn a_corrupt_sheet_is_never_cached_and_temp_names_differ() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("sheet");
        let garbage = |_: &str| Ok((b"<html>captive portal</html>".to_vec(), b"not a png".to_vec()));
        assert!(sprite_sheet_in(&cache, "https://h.example/s", false, None, &garbage).is_err());
        assert!(!cache.join("sprite@2x.json").exists() && !cache.join("sprite@2x.png").exists());
        let (json, _) = tiny_sheet();
        let half = move |_: &str| Ok((json.clone(), b"not a png".to_vec()));
        assert!(sprite_sheet_in(&cache, "https://h.example/s", false, None, &half).is_err());
        assert!(!cache.join("sprite@2x.json").exists(), "a manifest with a broken image is not cached either");

        assert!(!cache.join(CURRENT).exists());
        let (j, p) = (partial_path(&cache.join("sprite@2x.json")), partial_path(&cache.join("sprite@2x.png")));
        assert_ne!(j, p);
        let pid = std::process::id().to_string();
        assert!(j.to_string_lossy().contains(&pid) && p.to_string_lossy().contains(&pid));

        // A good sheet is cached, and read back offline.
        let good = |_: &str| Ok(tiny_sheet());
        let fetched = sprite_sheet_in(&cache, "https://h.example/s", false, None, &good).unwrap();
        assert_eq!(fetched.atlas.pixel_ratio("dot"), Some(2.0));
        let never = |_: &str| -> anyhow::Result<(Vec<u8>, Vec<u8>)> { panic!("offline fetched") };
        let offline = sprite_sheet_in(&cache, "https://h.example/s", true, None, &never).unwrap();
        assert_eq!(offline.info, fetched.info);
        assert!(std::fs::read_dir(&cache).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".partial")));
    }

    /// Review F3: a fresh cached sheet is used as it is; a stale one is
    /// fetched again online (and still used if that fails), and used as it
    /// is offline. The manifest record names the redacted base and a hash.
    #[test]
    fn a_stale_sheet_is_refetched_online_and_kept_offline() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("sheet");
        let base = "https://user:SECRET@h.example/s?key=SECRET";
        let calls = std::cell::Cell::new(0);
        let counting = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(tiny_sheet())
        };
        let first = sprite_sheet_in(&cache, base, false, None, &counting).unwrap();
        assert_eq!(first.info.base, "https://h.example/s");
        assert_eq!(first.info.sha256.len(), 64);
        sprite_sheet_in(&cache, base, false, None, &counting).unwrap();
        assert_eq!(calls.get(), 1, "a fresh cached sheet is not fetched again");

        let old = std::time::SystemTime::now() - SPRITE_CACHE_MAX_AGE - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(cache.join(CURRENT)).unwrap().set_modified(old).unwrap();
        let never = |_: &str| -> anyhow::Result<(Vec<u8>, Vec<u8>)> { panic!("offline fetched") };
        sprite_sheet_in(&cache, base, true, None, &never).unwrap();
        let failing = |_: &str| -> anyhow::Result<(Vec<u8>, Vec<u8>)> { anyhow::bail!("down") };
        assert_eq!(sprite_sheet_in(&cache, base, false, None, &failing).unwrap().info, first.info, "a stale copy beats none");
        sprite_sheet_in(&cache, base, false, None, &counting).unwrap();
        assert_eq!(calls.get(), 2, "a stale cached sheet is fetched again online");
    }

    /// A 4x4 sheet of one colour: a different sheet per colour.
    fn sheet_of(rgba: [u8; 4]) -> (Vec<u8>, Vec<u8>) {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(4, 4, image::Rgba(rgba))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        (br#"{"dot": {"x": 0, "y": 0, "width": 4, "height": 4, "pixelRatio": 2}}"#.to_vec(), png)
    }

    /// Final review F12: the pair is stored as one unit -- a JSON and a
    /// PNG from different fetches are never read together (the pair must
    /// hash to the name `current` gives it) -- and a pinned sheet (the
    /// sha an earlier manifest recorded) is used after a refresh replaced
    /// it as current, so a re-rendered range draws the icons it drew.
    #[test]
    fn a_pair_is_stored_whole_and_a_pinned_sheet_survives_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("sheet");
        let base = "https://h.example/s";
        let first = sprite_sheet_in(&cache, base, false, None, &|_: &str| Ok(sheet_of([255, 0, 0, 255]))).unwrap();
        // A refresh (stale) brings a different sheet.
        let old = std::time::SystemTime::now() - SPRITE_CACHE_MAX_AGE - std::time::Duration::from_secs(60);
        std::fs::File::options().write(true).open(cache.join(CURRENT)).unwrap().set_modified(old).unwrap();
        let second = sprite_sheet_in(&cache, base, false, None, &|_: &str| Ok(sheet_of([0, 0, 255, 255]))).unwrap();
        assert_ne!(first.info.sha256, second.info.sha256);
        let never = |_: &str| -> anyhow::Result<(Vec<u8>, Vec<u8>)> { panic!("fetched") };
        assert_eq!(sprite_sheet_in(&cache, base, true, None, &never).unwrap().info, second.info, "current is the newest");
        // The pinned sheet wins, even online and whatever its age.
        let pinned = sprite_sheet_in(&cache, base, false, Some(&first.info.sha256), &never).unwrap();
        assert_eq!(pinned.info, first.info);
        // A pinned sha that is not cached falls back to the usual rules.
        let missing = "0".repeat(64);
        assert_eq!(sprite_sheet_in(&cache, base, true, Some(&missing), &never).unwrap().info, second.info);

        // A torn pair (the PNG of another fetch beside the JSON) is not
        // read as that sheet: offline, nothing cached is usable.
        std::fs::write(cache.join(&second.info.sha256).join("sprite@2x.png"), sheet_of([0, 255, 0, 255]).1).unwrap();
        assert!(sprite_sheet_in(&cache, base, true, None, &never).is_err());
        assert!(std::fs::read_dir(&cache).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".partial")));
    }
}
