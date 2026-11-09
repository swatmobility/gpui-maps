//! The disk cache's stale-if-error on a body that fails to arrive after a
//! 200 (review): a raw socket server, which the library's own tests may not
//! open (`agent::tests::the_library_binds_no_socket`), hence here.

use gpui_maps::tile_source::disk_cache::DiskCache;
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

const TILE: gpui_maps::geo::TileId = gpui_maps::geo::TileId { z: 3, x: 1, y: 2 };

/// Answers the first connection with a whole 200 carrying
/// `cache_control`, every later one with a 200 that promises 100 bytes and
/// hangs up after 10.
async fn truncating_server(cache_control: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            if first {
                first = false;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: {cache_control}\r\nETag: \"t\"\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(b"tile").await;
            } else {
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n0123456789").await;
            }
            let _ = socket.shutdown().await;
        }
    });
    format!("http://{addr}/{{z}}/{{x}}/{{y}}.mvt")
}

#[tokio::test]
async fn a_body_cut_off_after_a_200_serves_the_stale_entry() {
    let dir = tempfile::tempdir().unwrap();
    let source = HttpTileSource::new(truncating_server("max-age=0").await, 20, "").with_disk_cache(DiskCache::new(dir.path(), 1 << 20));
    assert_eq!(source.fetch(TILE).await.unwrap().as_deref(), Some(&b"tile"[..]));
    assert_eq!(source.fetch(TILE).await.unwrap().as_deref(), Some(&b"tile"[..]), "the cut-off body fell back to the stale entry");
}

#[tokio::test]
async fn a_must_revalidate_entry_is_not_served_for_a_cut_off_body() {
    let dir = tempfile::tempdir().unwrap();
    let source =
        HttpTileSource::new(truncating_server("max-age=0, must-revalidate").await, 20, "").with_disk_cache(DiskCache::new(dir.path(), 1 << 20));
    assert_eq!(source.fetch(TILE).await.unwrap().as_deref(), Some(&b"tile"[..]));
    assert!(source.fetch(TILE).await.is_err());
}
