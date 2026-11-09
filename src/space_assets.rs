//! **The space look's textures**: fetched on first use, never committed.
//!
//! `render::space_look` draws from decoded RGBA and knows nothing about
//! where it came from; this module is where it comes from on the native
//! shell. Each asset is downloaded once into `~/.cache/gpui-map/space/`
//! (or `$GPUI_MAP_SPACE_DIR`), with the project User-Agent -- the repository
//! URL and nothing personal -- and accepted only if its sha256 matches the
//! digest pinned below. Anything else (offline, a changed upstream, a
//! corrupt file) leaves that texture `None`, and the look falls back to the
//! plain sky for that half. `scripts/fetch-space-assets.sh` does the same
//! download by hand.
//!
//! # What the digests pin: the UPSTREAM bytes
//!
//! Both files are used exactly as served -- no local re-encode -- so the
//! pin is the upstream file's sha256, reproducible by anyone with `curl`
//! and `shasum -a 256`. Downscaling for a device's texture limit happens at
//! upload (`EquirectImage::fit_to`), after verification. A re-encode by a
//! local tool (sips, ffmpeg) is not bit-reproducible across tool versions,
//! which is why the processed output is not what is pinned.
//!
//! # Licences (verified 2026-09-30)
//!
//!  - **Star map**: *Deep Star Maps 2020*, NASA/Goddard Space Flight Center
//!    Scientific Visualization Studio (Ernie Wright, Laurence Schuler, Ian
//!    Jones), <https://svs.gsfc.nasa.gov/4851>; Gaia DR2 data by
//!    ESA/Gaia/DPAC. Wikimedia Commons, which serves the 3840-px rendition
//!    fetched here, records the terms as use "for any purpose, provided that
//!    the copyright holder is properly attributed" -- attribution REQUIRED
//!    (the Gaia component is ESA's), so the credit joins the map's line
//!    whenever the texture is drawn.
//!  - **Clouds**: *Blue Marble: Clouds*, NASA Visible Earth
//!    <https://visibleearth.nasa.gov/images/57747>, "NASA Goddard Space
//!    Flight Center Image by Reto Stöckli". NASA imagery, not copyrighted
//!    in the US; credited anyway, as NASA's media guidelines ask.

use crate::render::space_look::{EquirectImage, SpaceTextures};
use sha2::Digest;

/// The User-Agent every space-asset request carries: the project and its
/// repository, never a user's email or a token.
pub const USER_AGENT: &str = concat!(
    "gpui-map-space-look/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/swatmobility/gpui-maps)"
);

/// One pinned download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// File name inside the cache directory.
    pub file: &'static str,
    /// Where it is fetched from.
    pub url: &'static str,
    /// Lower-case hex sha256 of the upstream bytes.
    pub sha256: &'static str,
}

/// Deep Star Maps 2020, `starmap_2020` in celestial coordinates, 3840x1920
/// JPEG (1,300,160 bytes), the Wikimedia Commons rendition of the SVS
/// original (`Deep_Star_Maps_2020_–_Starmap_2020_64k.jpg`).
///
/// **Known risk: this is a Commons THUMBNAIL**, which Wikimedia may
/// regenerate with different bytes; the pin then refuses it and the look
/// falls back to the plain sky (one stderr line). The stable sources were
/// checked on 2026-09-30 and neither is usable here: svs.gsfc.nasa.gov did
/// not answer from the build machine (and its 4k star maps are OpenEXR,
/// its JPEGs 1024 px previews), and the Commons ORIGINAL is a 541 MB
/// 32768x16384 JPEG (2 GB decoded) -- not a runtime download. The fix is a
/// project-hosted copy (the terms allow redistribution with attribution);
/// until then `$GPUI_MAP_SPACE_DIR` can point at a directory holding a file
/// with these exact bytes.
pub const STARMAP: Asset = Asset {
    file: "starmap_2020_3840.jpg",
    url: "https://upload.wikimedia.org/wikipedia/commons/thumb/a/a9/Deep_Star_Maps_2020_%E2%80%93_Starmap_2020_64k.jpg/3840px-Deep_Star_Maps_2020_%E2%80%93_Starmap_2020_64k.jpg",
    sha256: "1b5dd6f7e3dd959a742f35dde1d95966fbae14140ef29d1f648f1d0b6af0bfbc",
};

/// Blue Marble: Clouds, 2048x1024 JPEG (829,367 bytes), as NASA serves it.
pub const CLOUDS: Asset = Asset {
    file: "cloud_combined_2048.jpg",
    url: "https://eoimages.gsfc.nasa.gov/images/imagerecords/57000/57747/cloud_combined_2048.jpg",
    sha256: "daddaad84d7a33bbbc86cdda3f591099f57cee8607b7bcf3b67eb7e4f7a1c793",
};

/// Where the assets are cached: `$GPUI_MAP_SPACE_DIR`, else
/// `~/.cache/gpui-map/space`. `None` with neither variable set.
#[must_use]
pub fn cache_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("GPUI_MAP_SPACE_DIR") {
        return Some(dir.into());
    }
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".cache/gpui-map/space"))
}

/// Lower-case hex sha256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `bytes` if they are `asset`'s pinned bytes.
fn verified(asset: &Asset, bytes: Vec<u8>) -> Option<Vec<u8>> {
    (sha256_hex(&bytes) == asset.sha256).then_some(bytes)
}

/// Decodes a verified JPEG into RGBA.
fn decode(bytes: &[u8]) -> Option<EquirectImage> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some(EquirectImage {
        width,
        height,
        rgba: image.into_raw(),
    })
}

/// The asset from the cache, if present and verified. No network.
#[must_use]
pub fn load_cached(asset: &Asset) -> Option<EquirectImage> {
    let path = cache_dir()?.join(asset.file);
    decode(&verified(asset, std::fs::read(path).ok()?)?)
}

/// Both textures from the cache only -- what a headless capture reads.
#[must_use]
pub fn load_cached_textures() -> SpaceTextures {
    SpaceTextures {
        starmap: load_cached(&STARMAP),
        clouds: load_cached(&CLOUDS),
    }
}

/// A unique temporary name for one writer's download, so two maps (or two
/// processes) fetching at once never interleave writes into one `.part`.
fn temp_name(file: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{file}.{}.{}.part",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Runs `f` off the async runtime's workers (file reads, sha256 and a
/// 3840x1920 JPEG decode are ~100-300 ms of blocking work).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// The asset from the cache, else downloaded, verified and cached. `None`
/// on any failure; the reason goes to stderr once. Every read, digest and
/// decode runs under `spawn_blocking`.
pub async fn load_or_fetch(asset: &'static Asset) -> Option<EquirectImage> {
    if let Some(image) = blocking(move || load_cached(asset)).await.flatten() {
        return Some(image);
    }
    let fetched = async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;
        let response = client
            .get(asset.url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
        blocking(move || {
            let bytes = verified(asset, bytes).ok_or_else(|| "sha256 mismatch".to_string())?;
            if let Some(dir) = cache_dir() {
                // Written to a writer-unique temporary name and renamed, so a
                // half-written or interleaved file is never read back.
                let _ = std::fs::create_dir_all(&dir);
                let tmp = dir.join(temp_name(asset.file));
                if std::fs::write(&tmp, &bytes).is_ok()
                    && std::fs::rename(&tmp, dir.join(asset.file)).is_err()
                {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            decode(&bytes).ok_or_else(|| "undecodable".to_string())
        })
        .await
        .unwrap_or_else(|| Err("decode task failed".to_string()))
    }
    .await;
    match fetched {
        Ok(image) => Some(image),
        Err(reason) => {
            eprintln!(
                "[space look] {} unavailable ({reason}); drawing the plain sky for it",
                asset.file
            );
            None
        }
    }
}

/// Both textures, fetching whatever the cache lacks.
///
/// **One load per process**: concurrent callers (several maps turning the
/// look on) share the first caller's result instead of each downloading and
/// decoding. Held weakly, so the ~37 MB of decoded RGBA is freed once every
/// map has uploaded it and dropped its copy; a later caller loads again
/// (from the disk cache).
pub async fn load_or_fetch_textures() -> std::sync::Arc<SpaceTextures> {
    use std::sync::{Arc, Weak};
    static SHARED: tokio::sync::Mutex<Weak<SpaceTextures>> =
        tokio::sync::Mutex::const_new(Weak::new());
    let mut held = SHARED.lock().await;
    if let Some(textures) = held.upgrade() {
        return textures;
    }
    let (starmap, clouds) = tokio::join!(load_or_fetch(&STARMAP), load_or_fetch(&CLOUDS));
    let textures = Arc::new(SpaceTextures { starmap, clouds });
    *held = Arc::downgrade(&textures);
    textures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_names_the_project_and_nothing_personal() {
        assert!(USER_AGENT.contains("github.com/swatmobility/gpui-maps"));
        assert!(!USER_AGENT.contains('@'), "no email in the User-Agent");
    }

    #[test]
    fn every_writer_gets_its_own_temp_name() {
        let (a, b) = (temp_name(CLOUDS.file), temp_name(CLOUDS.file));
        assert_ne!(a, b);
        assert!(a.starts_with(CLOUDS.file) && a.ends_with(".part"));
    }

    #[test]
    fn the_digests_are_well_formed_and_a_wrong_file_is_refused() {
        for asset in [STARMAP, CLOUDS] {
            assert_eq!(asset.sha256.len(), 64);
            assert!(asset
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert!(verified(&asset, b"not the asset".to_vec()).is_none());
        }
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_missing_cache_is_no_texture_not_an_error() {
        let dir = std::env::temp_dir().join(format!("gpui-map-space-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // A file with the right name and the wrong bytes is refused too.
        std::fs::write(dir.join(CLOUDS.file), b"corrupt").unwrap();
        // Only this test reads GPUI_MAP_SPACE_DIR.
        let previous = std::env::var_os("GPUI_MAP_SPACE_DIR");
        std::env::set_var("GPUI_MAP_SPACE_DIR", &dir);
        let textures = load_cached_textures();
        match previous {
            Some(p) => std::env::set_var("GPUI_MAP_SPACE_DIR", p),
            None => std::env::remove_var("GPUI_MAP_SPACE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(textures, SpaceTextures::default());
    }
}
