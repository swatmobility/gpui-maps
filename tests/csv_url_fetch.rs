//! `CsvSource::from_url` against a local server: bounded, timed out, and
//! a non-2xx status is an error. Here rather than in `src/` because the
//! library is held to zero listening sockets (`agent::tests`), and a test
//! server is one.
#![cfg(feature = "sprite_http_fetch")]

use gpui_maps::overlay::csv::{CsvError, CsvOptions, CsvSource};

/// A tiny HTTP server answering one request with `reply`, for the URL door.
fn serve_once(reply: Vec<u8>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(&reply);
        }
    });
    format!("http://{addr}/data.csv")
}

#[test]
fn a_url_is_fetched_bounded_and_parsed() {
    let body = "lat,lon,name\n1,2,a\n";
    let ok = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let csv = CsvSource::from_url(&serve_once(ok.into_bytes()), &CsvOptions::default()).unwrap();
    assert_eq!(csv.points().unwrap(), [[2.0, 1.0]]);

    let declared = "HTTP/1.1 200 OK\r\nContent-Length: 999999\r\nConnection: close\r\n\r\nlat,lon\n".to_string();
    let small = CsvOptions { max_bytes: 100, ..CsvOptions::default() };
    assert!(matches!(
        CsvSource::from_url(&serve_once(declared.into_bytes()), &small),
        Err(CsvError::TooLarge { limit: 100, got: 999_999 })
    ));

    // No Content-Length: refused at the chunk that crosses the bound.
    let mut streamed = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
    streamed.extend(std::iter::repeat_n(b'x', 1000));
    assert!(matches!(CsvSource::from_url(&serve_once(streamed), &small), Err(CsvError::TooLarge { limit: 100, .. })));

    let missing = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
    assert_eq!(CsvSource::from_url(&serve_once(missing), &CsvOptions::default()).unwrap_err(), CsvError::Http("HTTP 404".into()));
}

#[test]
fn a_url_that_never_answers_times_out() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/slow.csv", listener.local_addr().unwrap());
    let hold = std::thread::spawn(move || {
        let held = listener.accept();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        drop(held);
    });
    let options = CsvOptions { timeout: std::time::Duration::from_millis(300), ..CsvOptions::default() };
    assert_eq!(CsvSource::from_url(&url, &options).unwrap_err(), CsvError::Timeout);
    hold.join().unwrap();
}

