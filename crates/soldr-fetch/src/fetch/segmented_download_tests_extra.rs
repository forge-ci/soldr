use super::test_client;
use super::{
    clear_segmented_env, runtime, serve_range_tracking_concurrency, set_segmented_env, ENV_LOCK,
};
use crate::fetch::segmented_download::{
    SocketPool, SEGMENTED_DOWNLOAD_ENV_VAR, SEGMENTED_DOWNLOAD_N_ENV_VAR,
};
use crate::fetch::stream_download::get_request;
use crate::fetch::stream_download::{
    send_control_request_with_pool, stream_response_to_temp_file_with_pool, ASSET_SAFETY_TIMEOUT,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve_redirect_chain(redirects: u32) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect chain");
    let address = listener.local_addr().expect("redirect chain address");
    let url = format!("http://{address}/asset");
    let location = url.clone();
    tokio::spawn(async move {
        for hop in 0..=redirects {
            let (mut socket, _) = listener.accept().await.expect("accept redirect chain hop");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let response = if hop < redirects {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            };
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            let _ = socket.shutdown().await;
        }
    });
    url
}

#[test]
fn manual_redirect_limit_accepts_five_then_final_response() {
    runtime().block_on(async {
        let url = serve_redirect_chain(super::MAX_REDIRECT_HOPS).await;
        let client = test_client();
        let response =
            super::send_with_hop_timeout(&client, &url, get_request, Duration::from_secs(2))
                .await
                .expect("five redirects followed by a final response must succeed");
        assert!((200..300).contains(&response.status()));
    });
}

#[test]
fn automatic_redirects_follow_five_hops() {
    runtime().block_on(async {
        for (hops, expected_success) in [(5, true), (6, false)] {
            let url = serve_redirect_chain(hops).await;
            let client = kernal_api::http::Client::new(kernal_api::http::Limits {
                max_redirects: 5,
                ..kernal_api::http::Limits::default()
            })
            .unwrap();
            assert_eq!(client.get(&url).await.is_ok(), expected_success);
        }
    });
}

#[test]
fn manual_redirect_limit_rejects_sixth_and_redacts_unsafe_target() {
    runtime().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let location = format!("http://{address}/asset?secret=redacted");
        tokio::spawn(async move {
            for _ in 0..=super::MAX_REDIRECT_HOPS {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                let _ = socket.shutdown().await;
            }
        });
        let url = format!("http://{address}/asset?token=initial");
        let client = test_client();
        let error = super::send_with_hop_timeout(
            &client,
            &url,
            get_request,
            Duration::from_secs(2),
        )
        .await
        .expect_err("a sixth redirect must fail");
        assert!(error.contains("exceeded 5 redirect hops"));
        assert!(!error.contains("token=initial"));
        assert!(!error.contains("secret=redacted"));

        let unsafe_error = super::resolve_redirect_url(
            "https://safe.invalid/asset?token=initial",
            "http://user:password@other.invalid/asset?secret=redacted",
        )
        .expect_err("HTTPS downgrade with credentials must fail");
        assert!(!unsafe_error.contains("token=initial"));
        assert!(!unsafe_error.contains("user:password"));
        assert!(!unsafe_error.contains("secret=redacted"));
    });
}

#[test]
fn mixed_workload_never_exceeds_bulk_plus_quick_total_capacity() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    set_segmented_env(&[
        (SEGMENTED_DOWNLOAD_ENV_VAR, "1"),
        (SEGMENTED_DOWNLOAD_N_ENV_VAR, "3"),
    ]);

    runtime().block_on(async {
        // One large (segmentable) download against Bulk, plus
        // several small control-style requests against Quick, all
        // concurrent, hitting servers that each track their own
        // concurrency high-water-mark. The combined observed
        // concurrency on each side must never exceed that pool's
        // own size.
        let bulk = SocketPool::new(2);
        let quick = SocketPool::new(2);

        let big_body: Vec<u8> = (0..3072u32).map(|i| (i % 241) as u8).collect();
        let (big_url, big_hw) = serve_range_tracking_concurrency(big_body.clone()).await;
        let big_resp = test_client().get(&big_url).await.expect("GET big");
        let big_fut = stream_response_to_temp_file_with_pool(
            big_resp,
            &big_url,
            Duration::from_secs(5),
            ASSET_SAFETY_TIMEOUT,
            Arc::clone(&bulk),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind small");
        let address = listener.local_addr().expect("addr small");
        let small_hw = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let small_current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hw_task = Arc::clone(&small_hw);
        let cur_task = Arc::clone(&small_current);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let hw = Arc::clone(&hw_task);
                let cur = Arc::clone(&cur_task);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    hw.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = socket.shutdown().await;
                    cur.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        let small_url = format!("http://{address}/manifest.json");

        let small_handles: Vec<_> = (0..3)
            .map(|_| {
                let quick = Arc::clone(&quick);
                let url = small_url.clone();
                tokio::spawn(async move {
                    let client = test_client();
                    send_control_request_with_pool(
                        get_request(&client, &url),
                        &url,
                        Duration::from_secs(5),
                        quick,
                    )
                    .await
                })
            })
            .collect();

        let big_result = big_fut.await;
        big_result.expect("big download must complete");
        for handle in small_handles {
            handle
                .await
                .expect("small task must not panic")
                .expect("small control request must complete");
        }

        assert!(
            big_hw.load(Ordering::SeqCst) <= 2,
            "bulk-side concurrency must stay <= bulk pool size"
        );
        assert!(
            small_hw.load(Ordering::SeqCst) <= 2,
            "quick-side concurrency must stay <= quick pool size"
        );
    });

    clear_segmented_env();
}
