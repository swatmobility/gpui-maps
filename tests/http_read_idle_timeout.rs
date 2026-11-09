//! F11 (story Task 12 fix round 2): the HTTP tile source has connect and
//! read-idle timeouts and NO total timeout, so a large tile arriving
//! slowly but steadily is not cut off (a total cap would fail it, and the
//! live map's backoff would retry it from scratch forever). These need a
//! raw socket server that drips a body, which the library's own tests may
//! not open (`agent::tests::the_library_binds_no_socket`), hence here.

use gpui_maps::tile_source::TileSource;

/// Serves one 200 response on a raw socket: headers at once, then
/// `chunks` bytes of body one at a time, `gap` apart.
async fn dripping_server(chunks: usize, gap: std::time::Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {chunks}\r\nConnection: close\r\n\r\n");
        socket.write_all(head.as_bytes()).await.unwrap();
        for _ in 0..chunks {
            tokio::time::sleep(gap).await;
            socket.write_all(&[7]).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    format!("http://{addr}/{{z}}/{{x}}/{{y}}.mvt")
}

/// F11: no total timeout. A body arriving steadily for longer than the
/// read-idle timeout (every gap shorter than it) succeeds: scaled
/// down, 12 bytes 100 ms apart (1.2 s) under a 300 ms idle timeout.
#[tokio::test]
async fn a_slow_but_steady_body_is_not_cut_off() {
    let url = dripping_server(12, std::time::Duration::from_millis(100)).await;
    let source = gpui_maps::tile_source::http::HttpTileSource::new(url, 20, "").with_read_idle_timeout(std::time::Duration::from_millis(300));
    let bytes = source.fetch(gpui_maps::geo::TileId { z: 3, x: 1, y: 2 }).await.unwrap();
    assert_eq!(bytes, Some(vec![7; 12]));
}

/// F11 at full scale, on the live client as `new` builds it: a body
/// dripping for 22 s (one byte a second) outlasts the 20 s read-idle
/// timeout in total and still succeeds.
#[tokio::test]
#[ignore = "takes 22 s of wall time; a_slow_but_steady_body_is_not_cut_off covers the same behaviour in 1.2 s"]
async fn the_live_client_takes_a_body_longer_than_twenty_seconds_in_total() {
    assert_eq!((gpui_maps::tile_source::http::HTTP_CONNECT_TIMEOUT.as_secs(), gpui_maps::tile_source::http::HTTP_READ_IDLE_TIMEOUT.as_secs()), (10, 20));
    let url = dripping_server(22, std::time::Duration::from_secs(1)).await;
    let source = gpui_maps::tile_source::http::HttpTileSource::new(url, 20, "");
    let started = std::time::Instant::now();
    let bytes = source.fetch(gpui_maps::geo::TileId { z: 3, x: 1, y: 2 }).await.unwrap();
    assert_eq!(bytes, Some(vec![7; 22]));
    assert!(started.elapsed() > gpui_maps::tile_source::http::HTTP_READ_IDLE_TIMEOUT, "{:?}", started.elapsed());
}
