//! A count-tile server that accepts and never answers is a
//! `GridError::Timeout`, in bounded time -- not one of the dot layer's
//! four request slots held forever (Task 7 review, m2). A ClickHouse
//! exception response is a `GridError::Server`, never data (Task 11). A
//! range response is 206 and at most the requested length, or an error
//! (final review I4).
//!
//! An integration test because it binds a listener: the library's own
//! source must have no inbound network surface (`agent`'s
//! `the_library_binds_no_socket`).

#![cfg(feature = "sprite_http_fetch")]

use std::time::{Duration, Instant};

use gpui_maps::overlay::dotgrid::{fetch_native, GridError, GridRequest};

#[test]
fn a_server_that_never_answers_times_out() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/grid.pmtiles", listener.local_addr().unwrap());
    // Accept and hold the connection, answering nothing.
    let holder = std::thread::spawn(move || {
        let held = listener.accept();
        std::thread::sleep(Duration::from_secs(1));
        drop(held);
    });
    let started = Instant::now();
    let result = fetch_native::execute_within(
        &GridRequest::Range { location: url, offset: 0, length: 16 },
        Duration::from_millis(300),
    );
    assert!(matches!(result, Err(GridError::Timeout(_))), "expected Timeout, got {result:?}");
    assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
    holder.join().unwrap();
}

/// A one-shot HTTP server on a free local port answering `response`.
fn serve_once(response: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}/")
}

/// ClickHouse's failed query (`wait_end_of_query=1`) is the exception
/// header plus a `Code: …` body -- a server error, even on a 200.
#[test]
fn a_clickhouse_exception_header_is_a_server_error() {
    for status in ["500 Internal Server Error", "200 OK"] {
        let body = "Code: 60. DB::Exception: Table gpui_map_dots.x does not exist. (UNKNOWN_TABLE)";
        let url = serve_once(format!(
            "HTTP/1.1 {status}\r\nX-ClickHouse-Exception-Code: 60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        let req = GridRequest::Post { url, body: "SELECT 1".into(), headers: Vec::new() };
        match fetch_native::execute_within(&req, Duration::from_secs(5)) {
            Err(GridError::Server(text)) => assert!(text.contains("Code: 60."), "{text}"),
            other => panic!("{status}: expected GridError::Server, got {other:?}"),
        }
    }
}

/// A canned `status` response carrying `body`.
fn with_body(status: &str, body: &[u8]) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        String::from_utf8_lossy(body)
    )
}

/// Final review I4: a server that ignores `Range` and answers 200 with the
/// whole archive is an error, not a whole archive in memory.
#[test]
fn a_server_ignoring_range_is_an_error() {
    let url = serve_once(with_body("200 OK", &[b'7'; 4096]));
    let result = fetch_native::execute_within(&GridRequest::Range { location: url, offset: 100, length: 16 }, Duration::from_secs(5));
    assert!(matches!(result, Err(GridError::Server(_))), "a 200 for a range request was accepted: {:?}", result.map(|b| b.len()));
}

/// Final review I4: a 206 carrying more than the requested length is an
/// error too.
#[test]
fn a_range_response_longer_than_requested_is_an_error() {
    let url = serve_once(with_body("206 Partial Content", &[b'7'; 4096]));
    let result = fetch_native::execute_within(&GridRequest::Range { location: url, offset: 0, length: 16 }, Duration::from_secs(5));
    assert!(matches!(result, Err(GridError::Server(_))), "4096 bytes for a 16-byte range were accepted: {:?}", result.map(|b| b.len()));
}

/// And a well-behaved 206 still reads.
#[test]
fn a_range_response_of_the_requested_length_reads() {
    let url = serve_once(with_body("206 Partial Content", b"0123456789abcdef"));
    let body = fetch_native::execute_within(&GridRequest::Range { location: url, offset: 32, length: 16 }, Duration::from_secs(5)).unwrap();
    assert_eq!(body, b"0123456789abcdef");
}

/// Final review: an HTTP error keeps a bounded prefix of a huge body.
#[test]
fn a_huge_error_body_is_bounded() {
    let url = serve_once(with_body("500 Internal Server Error", &[b'x'; 100_000]));
    match fetch_native::execute_within(&GridRequest::Range { location: url, offset: 0, length: 16 }, Duration::from_secs(5)) {
        Err(GridError::Http { status: 500, body }) => assert!(body.len() <= gpui_maps::overlay::dotgrid::MAX_ERROR_TEXT + 3, "{} bytes", body.len()),
        other => panic!("expected Http 500, got {other:?}"),
    }
}

/// Final review minor: a ClickHouse response is untrusted too. One that
/// says it is longer than `fetch_native::MAX_POST_BODY_BYTES` is refused
/// before its body is read, and one that streams past it (no length
/// given, the connection closing at the end) is refused once it does.
#[test]
fn a_post_response_over_the_cap_is_an_error() {
    let cap = fetch_native::MAX_POST_BODY_BYTES;
    let claimed = serve_once(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n0123", cap + 1));
    let req = GridRequest::Post { url: claimed, body: "SELECT 1".into(), headers: Vec::new() };
    match fetch_native::execute_within(&req, Duration::from_secs(10)) {
        Err(GridError::Server(text)) => assert!(text.contains("limit"), "{text}"),
        other => panic!("a response claiming {} bytes: {:?}", cap + 1, other.map(|b| b.len())),
    }
    let mut streamed = String::from("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
    streamed.push_str(&"7".repeat(usize::try_from(cap).unwrap() + 1));
    let req = GridRequest::Post { url: serve_once(streamed), body: "SELECT 1".into(), headers: Vec::new() };
    match fetch_native::execute_within(&req, Duration::from_secs(20)) {
        Err(GridError::Server(text)) => assert!(text.contains("limit"), "{text}"),
        other => panic!("a response streaming {} bytes: {:?}", cap + 1, other.map(|b| b.len())),
    }
    // Under the cap still reads.
    let req = GridRequest::Post { url: serve_once(with_body("200 OK", b"rows")), body: "SELECT 1".into(), headers: Vec::new() };
    assert_eq!(fetch_native::execute_within(&req, Duration::from_secs(5)).unwrap(), b"rows");
}

/// Task 16: the proxy's whole-body GET (`GridRequest::Get`) reads under
/// the same rules as a POST -- a ClickHouse exception header is an error,
/// never rows, and the body is bounded.
#[test]
fn a_proxy_get_reads_rows_and_refuses_an_exception() {
    let req = GridRequest::Get { url: serve_once(with_body("200 OK", b"rows")) };
    assert_eq!(fetch_native::execute_within(&req, Duration::from_secs(5)).unwrap(), b"rows");
    let exception = serve_once(
        "HTTP/1.1 502 Bad Gateway\r\nX-ClickHouse-Exception-Code: 164\r\nContent-Length: 9\r\nConnection: close\r\n\r\nCode: 164".to_string(),
    );
    match fetch_native::execute_within(&GridRequest::Get { url: exception }, Duration::from_secs(5)) {
        Err(GridError::Server(text)) => assert!(text.contains("164"), "{text}"),
        other => panic!("an exception reply: {:?}", other.map(|b| b.len())),
    }
}
