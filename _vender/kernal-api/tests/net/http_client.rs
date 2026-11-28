#![cfg(any(feature = "http-client", feature = "http-client-rustls"))]

use kernal_api::http::{Client, Limits, Method, Request};
use std::io::{Read, Write};

#[tokio::test]
async fn private_parser_bounds_metadata_even_when_acceptance_limits_are_relaxed() {
    let client = Client::new(Limits {
        max_header_bytes: usize::MAX,
        max_header_count: 1024,
        total_timeout: std::time::Duration::from_secs(5),
        ..Limits::default()
    })
    .unwrap();
    let mut oversized_head = b"HTTP/1.1 200 OK\r\nX-Large: ".to_vec();
    oversized_head.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
    oversized_head.extend_from_slice(b"\r\nContent-Length: 0\r\n\r\n");
    let mut excessive_fields = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n".to_vec();
    for _ in 0..100 {
        excessive_fields.extend_from_slice(b"X-Field: x\r\n");
    }
    excessive_fields.extend_from_slice(b"\r\n");
    for wire in [oversized_head, excessive_fields] {
        let (url, worker) = fixture(&wire);
        let error = client
            .get(&url)
            .await
            .err()
            .expect("parser accepted excessive metadata");
        worker.join().unwrap();
        assert_ne!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    let mut trailers =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX-Large: ".to_vec();
    trailers.extend(std::iter::repeat_n(b'x', 32 * 1024));
    trailers.extend_from_slice(b"\r\n\r\n");
    let mut extensions = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1;name=".to_vec();
    extensions.extend(std::iter::repeat_n(b'x', 32 * 1024));
    extensions.extend_from_slice(b"\r\nx\r\n0\r\n\r\n");
    for wire in [trailers, extensions] {
        let (url, worker) = fixture(&wire);
        let response = client.get(&url).await.unwrap();
        let error = response
            .into_bytes()
            .await
            .expect_err("parser accepted excessive chunk metadata");
        worker.join().unwrap();
        assert_ne!(error.kind(), std::io::ErrorKind::TimedOut);
    }
}

#[tokio::test]
async fn encoded_responses_preserve_wire_bytes_and_headers() {
    // gzip -n of "hello". The owned client links no decompression layer, so
    // no downstream feature unification can enable one.
    let encoded = [
        31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 203, 72, 205, 201, 201, 7, 0, 134, 166, 16, 54, 5, 0, 0, 0,
    ];
    let mut wire = b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 25\r\nConnection: close\r\n\r\n".to_vec();
    wire.extend_from_slice(&encoded);
    let (url, worker) = fixture(&wire);
    let response = Client::new(Limits::default())
        .unwrap()
        .get(&url)
        .await
        .unwrap();
    worker.join().unwrap();
    assert_eq!(
        response.header("Content-Encoding"),
        Some(b"gzip".as_slice())
    );
    assert_eq!(response.header("Content-Length"), Some(b"25".as_slice()));
    assert_eq!(response.into_bytes().await.unwrap(), encoded);
}

#[tokio::test]
#[ignore = "requires KERNAL_HTTP_HTTPS_FIXTURE pointing to a trusted public HTTPS resource"]
async fn trusted_https_fixture_uses_verified_tls_and_bounded_body() {
    let url = std::env::var("KERNAL_HTTP_HTTPS_FIXTURE").expect("HTTPS fixture URL");
    assert!(url.starts_with("https://"));
    let response = Client::new(Limits {
        max_redirects: 5,
        max_body_bytes: 4 * 1024 * 1024,
        ..Limits::default()
    })
    .unwrap()
    .execute(Request {
        headers: &[("User-Agent", "kernal-api-https-acceptance")],
        ..Request::get(&url)
    })
    .await
    .unwrap();
    assert!((200..300).contains(&response.status()));
    assert!(!response.into_bytes().await.unwrap().is_empty());
}

#[test]
fn blocking_adapter_streams_on_the_callers_runtime_and_rejects_nested_use() {
    use kernal_api::async_engine::RuntimeBuilder;
    use kernal_api::http::BlockingClient;
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = BlockingClient::new(&runtime, Limits::default()).unwrap();
    let (url, worker) =
        fixture(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef");
    let mut response = client.get(&url).unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.header("Content-Length"), Some(b"6".as_slice()));
    let mut prefix = [0; 2];
    runtime
        .run_within(
            async {
                assert_eq!(
                    response.read(&mut prefix).unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    response.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"ab");
    assert_eq!(response.into_bytes().unwrap(), b"cdef");
    worker.join().unwrap();
    runtime
        .run_within(
            async {
                assert!(BlockingClient::new(&runtime, Limits::default()).is_err());
                assert!(client.get("http://127.0.0.1:1/").is_err());
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
}

#[test]
fn blocking_stream_preserves_overflow_and_timeout_errors() {
    use kernal_api::async_engine::RuntimeBuilder;
    use kernal_api::http::BlockingClient;
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = BlockingClient::new(
        &runtime,
        Limits {
            max_body_bytes: 3,
            ..Limits::default()
        },
    )
    .unwrap();
    let (url, worker) = fixture(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcdef");
    let mut response = client.get(&url).unwrap();
    assert_eq!(
        std::io::copy(&mut response, &mut std::io::sink())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    worker.join().unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (release, held) = std::sync::mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).unwrap();
        // Deliberately exceed the old 50 ms header budget.
        std::thread::sleep(std::time::Duration::from_millis(100));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
            .unwrap();
        // Keep the body pending until the client reports its timeout. A wall-clock
        // fallback could close the socket first on a descheduled runner.
        let _ = held.recv();
    });
    let limits = Limits::default();
    let client = BlockingClient::new(&runtime, limits).unwrap();
    let response = client.get(&url).unwrap();
    // Headers use the normal generous budget. Only the pending body uses a
    // paused clock: Tokio advances to its read deadline without a real-time race
    // against the fixture or a slow runner. No production timeout API is needed.
    let started = runtime
        .run_within(
            async {
                tokio::time::pause();
                tokio::time::Instant::now()
            },
            std::time::Duration::from_secs(300),
        )
        .unwrap();
    let result = response.into_bytes();
    let elapsed = runtime
        .run_within(
            async { started.elapsed() },
            std::time::Duration::from_secs(300),
        )
        .unwrap();
    let _ = release.send(());
    worker.join().unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!(elapsed >= limits.read_timeout);
    assert!(elapsed < limits.total_timeout);
}

fn fixture(response: &[u8]) -> (String, std::thread::JoinHandle<()>) {
    let response = response.to_vec();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).unwrap();
        let _ = socket.write_all(&response);
    });
    (url, worker)
}

fn read_request(socket: &mut std::net::TcpStream) -> Vec<u8> {
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let mut wire = Vec::new();
    while !wire.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        wire.push(byte[0]);
        assert!(wire.len() < 4096);
    }
    if wire.starts_with(b"POST ") {
        let mut body = [0; 4];
        socket.read_exact(&mut body).unwrap();
        wire.extend_from_slice(&body);
    }
    wire
}

#[tokio::test]
async fn dropping_pending_request_or_response_closes_transport() {
    for send_headers in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (ready, accepted) = tokio::sync::oneshot::channel();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            read_request(&mut socket);
            if send_headers {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                    .unwrap();
            }
            let _ = ready.send(());
            match socket.read(&mut [0]) {
                Ok(0) => (),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                result => panic!("dropping HTTP operation did not close transport: {result:?}"),
            }
        });
        let client = Client::new(Limits::default()).unwrap();
        if send_headers {
            let response = client.get(&url).await.unwrap();
            drop(response);
        } else {
            let mut request = Box::pin(client.get(&url));
            tokio::select! {
                result = &mut request => panic!("request completed without headers: {:?}", result.err()),
                result = accepted => result.unwrap(),
            }
            drop(request);
        }
        tokio::task::spawn_blocking(move || worker.join().unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn relative_redirects_preserve_head_and_replay_post_only_for_307_308() {
    for (status, method, expected) in [
        (301, Method::Post, "GET"),
        (302, Method::Post, "GET"),
        (303, Method::Post, "GET"),
        (307, Method::Post, "POST"),
        (308, Method::Post, "POST"),
        (303, Method::Head, "HEAD"),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/start", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            read_request(&mut socket);
            write!(socket, "HTTP/1.1 {status} Redirect\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            drop(socket);
            let (mut socket, _) = listener.accept().unwrap();
            let wire = read_request(&mut socket);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            wire
        });
        let response = Client::new(Limits {
            max_redirects: 1,
            ..Limits::default()
        })
        .unwrap()
        .execute(Request {
            method,
            headers: &[("X-Test", "retained")],
            body: if method == Method::Post { b"ping" } else { b"" },
            ..Request::get(&url)
        })
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        let wire = String::from_utf8(worker.join().unwrap()).unwrap();
        assert!(wire.starts_with(&format!("{expected} /next HTTP/1.1")));
        assert!(wire.to_ascii_lowercase().contains("x-test: retained"));
        assert_eq!(wire.ends_with("ping"), expected == "POST");
    }
}

#[tokio::test]
async fn opted_in_redirects_follow_and_enforce_hop_limit() {
    let (final_url, final_worker) =
        fixture(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let redirect = format!("HTTP/1.1 302 Found\r\nLocation: {final_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let (url, worker) = fixture(redirect.as_bytes());
    let response = Client::new(Limits {
        max_redirects: 1,
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.into_bytes().await.unwrap(), b"ok");
    worker.join().unwrap();
    final_worker.join().unwrap();

    let (middle, middle_worker) = fixture(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let redirect = format!("HTTP/1.1 302 Found\r\nLocation: {middle}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let (url, worker) = fixture(redirect.as_bytes());
    let error = Client::new(Limits {
        max_redirects: 1,
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    worker.join().unwrap();
    middle_worker.join().unwrap();
}

#[tokio::test]
async fn redirects_drop_cross_origin_headers_and_convert_post_to_get() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let target = format!("http://{}/next", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 4096);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        String::from_utf8(request).unwrap()
    });
    let (url, first) = fixture(format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
    let response = Client::new(Limits {
        max_redirects: 2,
        ..Limits::default()
    })
    .unwrap()
    .execute(Request {
        method: Method::Post,
        headers: &[("X-Secret", "private"), ("Content-Type", "text/plain")],
        body: b"ping",
        ..Request::get(&url)
    })
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    first.join().unwrap();
    let wire = worker.join().unwrap().to_ascii_lowercase();
    assert!(wire.starts_with("get /next http/1.1"));
    assert!(!wire.contains("x-secret"));
    assert!(!wire.contains("content-type"));
}

#[tokio::test]
async fn cross_origin_redirects_never_replay_post_payloads() {
    for status in [307, 308] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let target = format!("http://{}/next", listener.local_addr().unwrap());
        let (url, first) = fixture(format!("HTTP/1.1 {status} Redirect\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
        let result = Client::new(Limits {
            max_redirects: 1,
            total_timeout: std::time::Duration::from_millis(200),
            ..Limits::default()
        })
        .unwrap()
        .execute(Request {
            method: Method::Post,
            body: b"secret=private",
            ..Request::get(&url)
        })
        .await;
        first.join().unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "cross-origin POST replay must be rejected before connecting"
        );
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

#[tokio::test]
async fn redirect_hops_reject_unsafe_urls_and_oversized_metadata() {
    for target in ["file:///secret", "http://user:secret@127.0.0.1:1/"] {
        let (url, worker) = fixture(format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
        let error = Client::new(Limits {
            max_redirects: 1,
            ..Limits::default()
        })
        .unwrap()
        .get(&url)
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        worker.join().unwrap();
    }
    let (url, worker) = fixture(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nX-Large: abcdef\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let error = Client::new(Limits {
        max_redirects: 1,
        max_header_bytes: 8,
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    worker.join().unwrap();
}

#[tokio::test]
async fn total_deadline_applies_to_already_buffered_unread_body() {
    let (url, worker) =
        fixture(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef");
    let mut response = Client::new(Limits {
        total_timeout: std::time::Duration::from_millis(100),
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .unwrap();
    assert_eq!(response.read(&mut [0; 1]).await.unwrap(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        response.read(&mut [0; 1]).await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    worker.join().unwrap();
}

#[tokio::test]
async fn streaming_returns_before_body_completion_and_collection_keeps_only_unread_bytes() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (release, held) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
            .unwrap();
        held.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        socket.write_all(b"def").unwrap();
    });
    let mut response = Client::new(Limits {
        max_body_bytes: 6,
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .unwrap();
    let mut prefix = [0; 3];
    let mut offset = 0;
    while offset < prefix.len() {
        let n = response.read(&mut prefix[offset..]).await.unwrap();
        assert_ne!(n, 0);
        offset += n;
    }
    assert_eq!(&prefix, b"abc");
    release.send(()).unwrap();
    assert_eq!(response.into_bytes().await.unwrap(), b"def");
    worker.join().unwrap();
}

#[tokio::test]
async fn streaming_reads_respect_caller_buffers_and_stay_failed_after_overflow() {
    let (url, worker) =
        fixture(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef");
    let mut response = Client::new(Limits::default())
        .unwrap()
        .get(&url)
        .await
        .unwrap();
    assert_eq!(response.read(&mut []).await.unwrap(), 0);
    let mut buffer = [0; 2];
    let mut body = Vec::new();
    loop {
        let n = response.read(&mut buffer).await.unwrap();
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..n]);
    }
    assert_eq!(body, b"abcdef");
    assert_eq!(response.read(&mut buffer).await.unwrap(), 0);
    worker.join().unwrap();

    let (url, worker) = fixture(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabcdef");
    let mut response = Client::new(Limits {
        max_body_bytes: 4,
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .unwrap();
    loop {
        match response.read(&mut buffer).await {
            Ok(n) => assert_ne!(n, 0, "overflow must not become EOF"),
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                break;
            }
        }
    }
    assert!(response.read(&mut buffer).await.is_err());
    assert!(response.into_bytes().await.is_err());
    worker.join().unwrap();
}

#[tokio::test]
async fn head_preserves_headers_without_treating_entity_length_as_body() {
    let (url, worker) = fixture(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nX-Test: value\r\nConnection: close\r\n\r\n",
    );
    let client = Client::new(Limits {
        max_body_bytes: 0,
        ..Limits::default()
    })
    .unwrap();
    let response = client
        .execute(Request {
            method: Method::Head,
            ..Request::get(&url)
        })
        .await
        .unwrap();
    assert_eq!(response.header("X-Test"), Some(b"value".as_slice()));
    assert!(response.into_bytes().await.unwrap().is_empty());
    worker.join().unwrap();
}

#[tokio::test]
async fn post_sends_application_headers_and_body() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/debug", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut wire = Vec::new();
        while !wire.ends_with(b"ping") {
            let mut bytes = [0; 256];
            let n = socket.read(&mut bytes).unwrap();
            assert_ne!(n, 0);
            wire.extend_from_slice(&bytes[..n]);
            assert!(wire.len() < 4096);
        }
        socket
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        wire
    });
    let client = Client::new(Limits::default()).unwrap();
    let response = client
        .execute(Request {
            method: Method::Post,
            headers: &[
                ("Content-Type", "application/json"),
                ("User-Agent", "fixture"),
            ],
            body: b"ping",
            ..Request::get(&url)
        })
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let wire = String::from_utf8(worker.join().unwrap()).unwrap();
    assert!(wire.starts_with("POST /debug HTTP/1.1\r\n"));
    assert!(wire
        .to_lowercase()
        .contains("content-type: application/json\r\n"));
    assert!(wire.ends_with("\r\n\r\nping"));
}

#[tokio::test]
async fn request_limits_and_invalid_headers_fail_before_connecting() {
    let client = Client::new(Limits {
        max_request_bytes: 4,
        ..Limits::default()
    })
    .unwrap();
    let url = "http://127.0.0.1:1/";
    for headers in [
        &[("bad name", "value")][..],
        &[("X-Test", "value\r\nInjected: yes")][..],
        &[("Content-Length", "100")][..],
        &[("Host", "elsewhere")][..],
    ] {
        let result = client
            .execute(Request {
                headers,
                ..Request::get(url)
            })
            .await;
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
    let result = client
        .execute(Request {
            method: Method::Post,
            body: b"12345",
            ..Request::get(url)
        })
        .await;
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
    let headers = vec![("a", ""); 32769];
    let result = client
        .execute(Request {
            headers: &headers,
            ..Request::get(url)
        })
        .await;
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn bounded_get_preserves_status_and_body() {
    let (url, worker) =
        fixture(b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope");
    let response = Client::new(Limits::default())
        .unwrap()
        .get(&url)
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.into_bytes().await.unwrap(), b"nope");
    worker.join().unwrap();
}

#[tokio::test]
async fn declared_and_streamed_overflow_are_rejected() {
    for wire in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".as_slice(),
        b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n12345".as_slice(),
    ] {
        let (url, worker) = fixture(wire);
        let client = Client::new(Limits {
            max_body_bytes: 4,
            ..Limits::default()
        })
        .unwrap();
        let result = match client.get(&url).await {
            Ok(response) => response.into_bytes().await.map(|_| ()),
            Err(error) => Err(error),
        };
        assert!(result.is_err());
        worker.join().unwrap();
    }
}

#[tokio::test]
async fn redirects_are_returned_without_following_and_headers_are_bounded() {
    let (url, worker) = fixture(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let response = Client::new(Limits::default())
        .unwrap()
        .get(&url)
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(response.into_bytes().await.unwrap().is_empty());
    worker.join().unwrap();
    let (url, worker) = fixture(b"HTTP/1.1 200 OK\r\nX-Large: abcdef\r\nContent-Length: 0\r\n\r\n");
    assert!(Client::new(Limits {
        max_header_bytes: 4,
        ..Limits::default()
    })
    .unwrap()
    .get(&url)
    .await
    .is_err());
    worker.join().unwrap();
}

#[tokio::test]
async fn unsafe_urls_and_unbounded_timeout_configuration_fail() {
    let client = Client::new(Limits::default()).unwrap();
    for url in [
        "file:///etc/passwd",
        "ftp://localhost/file",
        "http://user:secret@localhost/",
        "not a URL",
    ] {
        assert!(client.get(url).await.is_err());
    }
    assert!(Client::new(Limits {
        total_timeout: std::time::Duration::ZERO,
        ..Limits::default()
    })
    .is_err());
}

#[test]
fn unrepresentable_deadlines_are_rejected_when_building_the_client() {
    for limits in [
        Limits {
            connect_timeout: std::time::Duration::MAX,
            ..Limits::default()
        },
        Limits {
            total_timeout: std::time::Duration::MAX,
            ..Limits::default()
        },
        Limits {
            read_timeout: std::time::Duration::MAX,
            ..Limits::default()
        },
    ] {
        assert!(
            Client::new(limits).is_err(),
            "unrepresentable deadline accepted"
        );
    }
}

#[tokio::test]
async fn stalled_headers_and_body_hit_read_deadline() {
    for headers in [
        b"".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".as_slice(),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (release, held) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).unwrap();
            socket.write_all(headers).unwrap();
            let _ = held.recv_timeout(std::time::Duration::from_secs(2));
        });
        let client = Client::new(Limits {
            read_timeout: std::time::Duration::from_millis(50),
            ..Limits::default()
        })
        .unwrap();
        let result = match client.get(&url).await {
            Ok(response) => response.into_bytes().await.map(|_| ()),
            Err(error) => Err(error),
        };
        let _ = release.send(());
        worker.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }
}

/// Accept one connection per canned response, in order, and return each
/// request head (plus a declared body) as received on the wire.
fn serve(
    listener: std::net::TcpListener,
    responses: Vec<Vec<u8>>,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut wire = Vec::new();
                while !wire.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    wire.push(byte[0]);
                    assert!(wire.len() < 8192);
                }
                let head = String::from_utf8(wire).unwrap();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                socket.write_all(&response).unwrap();
                head + &String::from_utf8(body).unwrap()
            })
            .collect()
    })
}

fn redirect_to(location: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

const EMPTY_OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

fn local_listener() -> (std::net::TcpListener, std::net::SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

#[tokio::test]
async fn relative_locations_resolve_against_the_current_request_url() {
    let client = Client::new(Limits {
        max_redirects: 1,
        ..Limits::default()
    })
    .unwrap();
    for (location, expected) in [
        ("next", "/a/b/next"),
        ("../up", "/a/up"),
        ("./same?y=2", "/a/b/same?y=2"),
        ("?only=q", "/a/b/start?only=q"),
        ("/abs/./p/../q", "/abs/q"),
        ("../../../../x", "/x"),
        ("next#fragment", "/a/b/next"),
        ("", "/a/b/start?x=1"),
        ("//{authority}/network", "/network"),
        ("http://{authority}/absolute", "/absolute"),
    ] {
        let (listener, address) = local_listener();
        let location = location.replace("{authority}", &address.to_string());
        let worker = serve(listener, vec![redirect_to(&location), EMPTY_OK.to_vec()]);
        let response = client
            .get(&format!("http://{address}/a/b/start?x=1"))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "Location {location:?}");
        let wire = worker.join().unwrap();
        assert!(
            wire[1].starts_with(&format!("GET {expected} HTTP/1.1\r\n")),
            "Location {location:?} sent {:?}",
            wire[1]
        );
    }
}

#[tokio::test]
async fn request_target_omits_fragments_and_host_names_the_authority() {
    let client = Client::new(Limits::default()).unwrap();
    for (url, target) in [
        ("http://{authority}/p?q=1#fragment", "/p?q=1"),
        ("http://{authority}", "/"),
        ("HTTP://{authority}/Case", "/Case"),
    ] {
        let (listener, address) = local_listener();
        let worker = serve(listener, vec![EMPTY_OK.to_vec()]);
        let url = url.replace("{authority}", &address.to_string());
        client.get(&url).await.unwrap();
        let wire = worker.join().unwrap().remove(0).to_ascii_lowercase();
        let target = target.to_ascii_lowercase();
        assert!(
            wire.starts_with(&format!("get {target} http/1.1\r\n")),
            "{wire:?}"
        );
        assert!(
            wire.contains(&format!("\r\nhost: {address}\r\n")),
            "{wire:?}"
        );
        assert!(wire.contains("\r\naccept: */*\r\n"), "{wire:?}");
        assert!(!wire.contains("proxy"), "{wire:?}");
    }
}

#[tokio::test]
async fn ipv6_literal_authority_is_bracketed_in_host() {
    let listener = match std::net::TcpListener::bind("[::1]:0") {
        Ok(listener) => listener,
        // Some containers have no IPv6 loopback; nothing to characterize there.
        Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => return,
        Err(error) => panic!("bind [::1]: {error}"),
    };
    let port = listener.local_addr().unwrap().port();
    let worker = serve(listener, vec![EMPTY_OK.to_vec()]);
    Client::new(Limits::default())
        .unwrap()
        .get(&format!("http://[::1]:{port}/v6"))
        .await
        .unwrap();
    let wire = worker.join().unwrap().remove(0).to_ascii_lowercase();
    assert!(wire.starts_with("get /v6 http/1.1\r\n"), "{wire:?}");
    assert!(
        wire.contains(&format!("\r\nhost: [::1]:{port}\r\n")),
        "{wire:?}"
    );
}

#[tokio::test]
async fn request_framing_declares_post_length_and_nothing_for_get() {
    let client = Client::new(Limits::default()).unwrap();
    for (method, body, framing) in [
        (Method::Post, b"ping".as_slice(), Some("content-length: 4")),
        // An empty POST is sent without any framing header.
        (Method::Post, b"".as_slice(), None),
        (Method::Get, b"".as_slice(), None),
        (Method::Head, b"".as_slice(), None),
    ] {
        let (listener, address) = local_listener();
        let worker = serve(listener, vec![EMPTY_OK.to_vec()]);
        client
            .execute(Request {
                method,
                body,
                ..Request::get(&format!("http://{address}/"))
            })
            .await
            .unwrap();
        let wire = worker.join().unwrap().remove(0).to_ascii_lowercase();
        assert!(!wire.contains("transfer-encoding"), "{wire:?}");
        match framing {
            Some(framing) => assert!(wire.contains(&format!("\r\n{framing}\r\n")), "{wire:?}"),
            None => assert!(!wire.contains("content-length"), "{wire:?}"),
        }
    }
}

#[tokio::test]
async fn malformed_or_credentialed_urls_are_rejected_as_invalid_data() {
    let client = Client::new(Limits::default()).unwrap();
    for url in [
        "http://",
        "https://:80/",
        "http://user@127.0.0.1:1/",
        "http://:secret@127.0.0.1:1/",
        "mailto:someone@example.com",
        "//127.0.0.1:1/relative",
        "/relative",
        "",
    ] {
        let error = client.get(url).await.err().expect(url);
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{url:?}");
    }
}

#[tokio::test]
async fn transport_errors_are_not_invalid_data_and_never_echo_the_url() {
    let (listener, address) = local_listener();
    drop(listener);
    let error = Client::new(Limits::default())
        .unwrap()
        .get(&format!("http://{address}/token-SECRET?key=SECRET"))
        .await
        .err()
        .unwrap();
    assert_ne!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_ne!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(!error.to_string().contains("SECRET"), "{error}");
    assert!(!format!("{error:?}").contains("SECRET"), "{error:?}");
}

#[tokio::test]
async fn connect_timeout_covers_the_tls_handshake() {
    // The kernel completes the TCP handshake from the backlog; nobody ever
    // answers the ClientHello.
    let (_listener, address) = local_listener();
    let started = std::time::Instant::now();
    let error = Client::new(Limits {
        connect_timeout: std::time::Duration::from_millis(200),
        read_timeout: std::time::Duration::from_secs(30),
        total_timeout: std::time::Duration::from_secs(30),
        ..Limits::default()
    })
    .unwrap()
    .get(&format!("https://{address}/"))
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn total_timeout_bounds_stalled_headers_and_bodies() {
    for headers in [
        b"".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n".as_slice(),
    ] {
        let (listener, address) = local_listener();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0; 4096]).unwrap();
            socket.write_all(headers).unwrap();
            let _ = held.recv_timeout(std::time::Duration::from_secs(10));
        });
        let started = std::time::Instant::now();
        let client = Client::new(Limits {
            read_timeout: std::time::Duration::from_secs(30),
            total_timeout: std::time::Duration::from_millis(300),
            ..Limits::default()
        })
        .unwrap();
        let result = match client.get(&format!("http://{address}/")).await {
            Ok(response) => response.into_bytes().await.map(|_| ()),
            Err(error) => Err(error),
        };
        let elapsed = started.elapsed();
        let _ = release.send(());
        worker.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(elapsed < std::time::Duration::from_secs(10));
    }
}

#[tokio::test]
async fn read_timeout_resets_on_body_progress() {
    let (listener, address) = local_listener();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let _ = socket.read(&mut [0; 4096]).unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n")
            .unwrap();
        for byte in b"abcde" {
            std::thread::sleep(std::time::Duration::from_millis(150));
            socket.write_all(&[*byte]).unwrap();
        }
    });
    // Each gap is under the read timeout; their sum (750 ms) exceeds it.
    let body = Client::new(Limits {
        read_timeout: std::time::Duration::from_millis(500),
        ..Limits::default()
    })
    .unwrap()
    .get(&format!("http://{address}/"))
    .await
    .unwrap()
    .into_bytes()
    .await
    .unwrap();
    worker.join().unwrap();
    assert_eq!(body, b"abcde");
}

#[tokio::test]
async fn unencoded_request_targets_are_sent_percent_encoded() {
    let (listener, address) = local_listener();
    let worker = serve(listener, vec![EMPTY_OK.to_vec()]);
    Client::new(Limits::default())
        .unwrap()
        .get(&format!("http://{address}/b\u{fc}cher/a b?q=c d&k='v'"))
        .await
        .unwrap();
    let wire = worker.join().unwrap().remove(0);
    assert!(
        wire.starts_with("GET /b%C3%BCcher/a%20b?q=c%20d&k=%27v%27 HTTP/1.1\r\n"),
        "{wire:?}"
    );
}

#[tokio::test]
async fn non_ascii_hosts_are_rejected_before_any_lookup() {
    let client = Client::new(Limits {
        max_redirects: 1,
        ..Limits::default()
    })
    .unwrap();
    for url in ["http://b\u{fc}cher.example/", "http://b%C3%BCcher.example/"] {
        let error = client.get(url).await.err().expect(url);
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{url:?}");
        assert!(error.to_string().contains("ASCII"), "{url:?}: {error}");
    }
    // A redirect hop is held to the same rule.
    let (listener, _) = local_listener();
    let address = listener.local_addr().unwrap();
    let worker = serve(listener, vec![redirect_to("http://b%C3%BCcher.example/")]);
    let error = client
        .get(&format!("http://{address}/"))
        .await
        .err()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("ASCII"), "{error}");
}

#[cfg(all(feature = "http-server", feature = "json"))]
#[tokio::test]
async fn http1_pool_reuses_connection_for_json_and_range_requests() {
    use kernal_api::{http::Version, http_server, json};

    let server = http_server::Server::bind("127.0.0.1:0".parse().unwrap(), Default::default())
        .await
        .unwrap();
    let diagnostics = server.diagnostics();
    let url = format!("http://{}/", server.local_addr().unwrap());
    let peer = tokio::spawn(server.serve(|request| async move {
        if request.method() == "POST" {
            assert_eq!(
                request.header("content-type"),
                Some(b"application/json".as_slice())
            );
            assert_eq!(request.header("x-test"), Some(b"application".as_slice()));
            http_server::Response::new(200, request.body().to_vec()).unwrap()
        } else {
            assert_eq!(request.header("range"), Some(b"bytes=2-4".as_slice()));
            http_server::Response::new(206, b"cde".to_vec())
                .unwrap()
                .with_header("content-range", "bytes 2-4/6")
                .unwrap()
        }
    }));

    let client = Client::new(Limits::default()).unwrap();
    let value = json::Value::String("hello".to_owned());
    let response = client
        .post_json(&url, &[("x-test", "application")], &value)
        .await
        .unwrap();
    assert_eq!(response.version(), Version::Http11);
    assert_eq!(response.into_json().await.unwrap(), value);
    let response = client
        .execute(Request {
            headers: &[("range", "bytes=2-4")],
            ..Request::get(&url)
        })
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.header("content-range"),
        Some(b"bytes 2-4/6".as_slice())
    );
    assert_eq!(response.into_bytes().await.unwrap(), b"cde");
    assert_eq!(diagnostics.snapshot().accepted_connections, 1);
    peer.abort();
}
