use super::*;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve_chunks(chunks: Vec<(Vec<u8>, Duration)>, content_length: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("server address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept client");
        let mut request = [0_u8; 1024];
        let _ = socket.read(&mut request).await;
        socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write headers");
        for (chunk, pause_after) in chunks {
            socket.write_all(&chunk).await.expect("write chunk");
            tokio::time::sleep(pause_after).await;
        }
        let _ = socket.shutdown().await;
    });
    format!("http://{address}/asset")
}

pub(super) fn test_client() -> kernal_api::http::Client {
    kernal_api::http::Client::new(kernal_api::http::Limits {
        max_body_bytes: u64::MAX,
        ..kernal_api::http::Limits::default()
    })
    .expect("create test HTTP client")
}

pub(super) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
}

/// Serialised because these tests mutate process env (segmented-
/// download knobs).
pub(super) static ENV_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn clear_segmented_env() {
    for var in [
        SEGMENTED_DOWNLOAD_ENV_VAR,
        SEGMENTED_DOWNLOAD_N_ENV_VAR,
        CONNECT_TIMEOUT_ENV_VAR,
        STALL_TIMEOUT_ENV_VAR,
        SEGMENT_RETRIES_ENV_VAR,
        GLOBAL_TIMEOUT_ENV_VAR,
        MAX_SOCKETS_ENV_VAR,
        QUICK_POOL_ENV_VAR,
        QUICK_THRESHOLD_ENV_VAR,
        AUTH_TOKEN_ENV_VAR,
    ] {
        std::env::remove_var(var);
    }
}

pub(super) fn set_segmented_env(pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        std::env::set_var(key, value);
    }
}

#[test]
fn healthy_chunks_reset_the_idle_watchdog() {
    runtime().block_on(async {
        // Idle is 5x the chunk gap: a loaded runner stretching one 100ms
        // gap must not fire the watchdog this test asserts is RESET by
        // healthy chunks (darwin lane flake, 2026-08-16). Total transfer
        // (8 x 100ms) still outlives the idle interval, preserving intent.
        let idle = Duration::from_millis(500);
        let url = serve_chunks(
            (b'a'..=b'h')
                .map(|byte| (vec![byte], Duration::from_millis(100)))
                .collect(),
            8,
        )
        .await;
        let started = Instant::now();
        let response = test_client().get(&url).await.expect("GET");
        let asset = stream_response_to_temp_file(response, &url, idle)
            .await
            .expect("progressing transfer succeeds");
        assert!(
            started.elapsed() > idle,
            "transfer must outlive one idle interval"
        );
        assert_eq!(asset.bytes(), 8);
        assert_eq!(asset.sha256(), super::super::trust::sha256_of(b"abcdefgh"));
    });
}

#[test]
fn idle_pause_reports_bytes_and_is_transient() {
    runtime().block_on(async {
        // Scheduler-proof margins (the soldr#2592 doctrine; this test was
        // missed by that rescale): the first chunk must arrive well inside
        // one idle interval on a loaded Windows runner — at 40 ms the idle
        // timer could fire before ANY bytes landed, and the error then
        // said "0 bytes", failing the assert twice in a row on the
        // target-run lane. 400 ms idle vs near-instant delivery, and a
        // 1200 ms pause (3x idle) to trigger the timeout deterministically.
        let idle = Duration::from_millis(400);
        let url = serve_chunks(vec![(b"partial".to_vec(), Duration::from_millis(1200))], 12).await;
        let response = test_client().get(&url).await.expect("GET");
        let error = stream_response_to_temp_file(response, &url, idle)
            .await
            .expect_err("paused body must fail");
        assert!(super::super::retry::is_transient(&error));
        assert!(error.to_string().contains("7 bytes"), "{error}");
        assert!(error.to_string().contains("no progress"), "{error}");
    });
}

#[test]
fn truncated_body_is_transient() {
    runtime().block_on(async {
        let url = serve_chunks(vec![(b"short".to_vec(), Duration::ZERO)], 12).await;
        let response = test_client().get(&url).await.expect("GET");
        let error = stream_response_to_temp_file(response, &url, Duration::from_secs(1))
            .await
            .expect_err("truncated body must fail");
        assert!(super::super::retry::is_transient(&error));
        assert!(error.to_string().contains("5 bytes"), "{error}");
    });
}

#[test]
fn global_safety_ceiling_stops_a_slow_but_progressing_transfer() {
    runtime().block_on(async {
        // The asserted error must be the CEILING, so every competing
        // timeout needs slack (windows arm64 flake, 2026-08-16): idle (2s)
        // is 20x the 100ms chunk gap, and the 300ms ceiling fires mid-way
        // through the ~600ms transfer with 3x the gap in margin.
        let url = serve_chunks(
            (b'a'..=b'f')
                .map(|byte| (vec![byte], Duration::from_millis(100)))
                .collect(),
            6,
        )
        .await;
        let response = test_client().get(&url).await.expect("GET");
        let error = stream_response_to_temp_file_with_safety_timeout(
            response,
            &url,
            Duration::from_millis(2000),
            Duration::from_millis(300),
        )
        .await
        .expect_err("global ceiling must stop the transfer");
        assert!(super::super::retry::is_transient(&error));
        assert!(
            error.to_string().contains("global safety ceiling"),
            "{error}"
        );
    });
}

#[test]
fn header_timeout_is_separate_from_body_idle_timeout() {
    runtime().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let address = listener.local_addr().expect("server address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        let url = format!("http://{address}/slow-headers");
        let error = send_asset_request(
            super::super::stream_download::get_request(&test_client(), &url),
            &url,
            Duration::from_millis(20),
        )
        .await
        .expect_err("slow headers must fail before the body starts");
        assert!(super::super::retry::is_transient(&error));
        assert!(error.to_string().contains("waiting for headers"), "{error}");
    });
}

// ---- segment-plan math ----

fn assert_exact_coverage(total: u64, segments: &[(u64, u64)]) {
    assert!(!segments.is_empty(), "must produce at least one segment");
    let mut expected_start = 0u64;
    for &(start, end_inclusive) in segments {
        assert_eq!(
            start, expected_start,
            "segment must start where the previous ended"
        );
        assert!(end_inclusive >= start, "segment must be non-empty");
        expected_start = end_inclusive + 1;
    }
    assert_eq!(
        expected_start, total,
        "segments must cover exactly [0, total) with no gap and no overrun"
    );
}

#[test]
fn segments_cover_exact_range_evenly_divisible() {
    let segments = compute_segments(1000, 4);
    assert_eq!(segments.len(), 4);
    assert_exact_coverage(1000, &segments);
    for &(start, end_inclusive) in &segments {
        assert_eq!(end_inclusive - start + 1, 250);
    }
}

#[test]
fn segments_cover_exact_range_with_remainder() {
    let segments = compute_segments(1000, 3);
    assert_eq!(segments.len(), 3);
    assert_exact_coverage(1000, &segments);
    let lens: Vec<u64> = segments.iter().map(|&(s, e)| e - s + 1).collect();
    assert_eq!(lens, vec![334, 333, 333]);
}

#[test]
fn segments_never_overlap_across_many_n() {
    for total in [1u64, 2, 7, 4096, 84_664_072, 108_209_048, 192_470_485] {
        for n in [2u32, 3, 4, 8, 16] {
            let segments = compute_segments(total, n);
            assert_exact_coverage(total, &segments);
            assert!(segments.len() as u32 <= n, "total={total} n={n}");
        }
    }
}

#[test]
fn zero_total_or_zero_n_produces_no_segments() {
    assert!(compute_segments(0, 4).is_empty());
    assert!(compute_segments(1000, 0).is_empty());
}

#[test]
fn more_segments_than_bytes_collapses_without_empty_segments() {
    let segments = compute_segments(3, 8);
    assert_exact_coverage(3, &segments);
    assert!(segments.len() <= 3);
    for &(start, end_inclusive) in &segments {
        assert_eq!(end_inclusive - start + 1, 1);
    }
}

// ---- config parsing: defaults, overrides, junk-fails-safe ----

#[test]
fn opt_out_recognizes_common_spellings_default_is_enabled() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();

    assert!(
        !opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
        "unset must leave segmentation enabled (default-on posture)"
    );
    for falsy in ["off", "0", "false", "no", "OFF", "False", " off "] {
        std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, falsy);
        assert!(
            opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
            "{falsy:?} must opt out"
        );
    }
    for other in ["1", "true", "yes", "on", "garbage"] {
        std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, other);
        assert!(
            !opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
            "{other:?} must NOT opt out -- only the documented falsy spellings do"
        );
    }
    clear_segmented_env();
}

#[test]
fn default_segment_count_is_sixteen() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(
        parse_segment_count(),
        16,
        "default N must be 16 per the maintainer's plateau-not-found decision"
    );
    assert_eq!(DEFAULT_SEGMENT_COUNT, 16);
    assert_eq!(MAX_SEGMENTS, 16);
    clear_segmented_env();
}

#[test]
fn segment_count_env_override_is_clamped_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();

    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "1");
    assert_eq!(
        parse_segment_count(),
        DEFAULT_SEGMENT_COUNT,
        "below-minimum falls back to default"
    );
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "4");
    assert_eq!(parse_segment_count(), 4);
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "9999");
    assert_eq!(
        parse_segment_count(),
        MAX_SEGMENTS,
        "above-maximum clamps to MAX_SEGMENTS"
    );
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "not-a-number");
    assert_eq!(
        parse_segment_count(),
        DEFAULT_SEGMENT_COUNT,
        "junk falls back to default"
    );
    clear_segmented_env();
    assert_eq!(
        parse_segment_count(),
        DEFAULT_SEGMENT_COUNT,
        "unset falls back to default"
    );
}

#[test]
fn connect_timeout_defaults_to_ten_seconds_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(parse_connect_timeout(), Duration::from_secs(10));

    std::env::set_var(CONNECT_TIMEOUT_ENV_VAR, "3");
    assert_eq!(parse_connect_timeout(), Duration::from_secs(3));
    std::env::set_var(CONNECT_TIMEOUT_ENV_VAR, "0");
    assert_eq!(
        parse_connect_timeout(),
        Duration::from_secs(10),
        "0 is not meaningful -- fails safe to default"
    );
    std::env::set_var(CONNECT_TIMEOUT_ENV_VAR, "nope");
    assert_eq!(parse_connect_timeout(), Duration::from_secs(10));
    clear_segmented_env();
}

#[test]
fn stall_timeout_defaults_to_thirty_seconds_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(parse_stall_timeout(), Duration::from_secs(30));

    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "5");
    assert_eq!(parse_stall_timeout(), Duration::from_secs(5));
    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "0");
    assert_eq!(
        parse_stall_timeout(),
        Duration::from_secs(30),
        "an explicit 0 is not a meaningful watchdog -- fails safe to default"
    );
    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "banana");
    assert_eq!(parse_stall_timeout(), Duration::from_secs(30));
    clear_segmented_env();
}

#[test]
fn segment_retries_defaults_to_three_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(parse_segment_retries(), 3);

    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "0");
    assert_eq!(
        parse_segment_retries(),
        0,
        "0 is a legitimate 'no retries' value, not junk"
    );
    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "9999");
    assert_eq!(parse_segment_retries(), MAX_SEGMENT_RETRIES);
    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "nope");
    assert_eq!(parse_segment_retries(), 3);
    clear_segmented_env();
}

#[test]
fn global_timeout_disabled_by_default_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(parse_global_timeout(), None, "disabled by default");

    std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "0");
    assert_eq!(parse_global_timeout(), None, "0 fails safe to disabled");
    std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "junk");
    assert_eq!(parse_global_timeout(), None, "junk fails safe to disabled");
    std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "45");
    assert_eq!(parse_global_timeout(), Some(Duration::from_secs(45)));
    clear_segmented_env();
}

#[test]
fn max_sockets_defaults_to_sixteen_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(parse_max_sockets(), Some(16));

    std::env::set_var(MAX_SOCKETS_ENV_VAR, "4");
    assert_eq!(parse_max_sockets(), Some(4));
    std::env::set_var(MAX_SOCKETS_ENV_VAR, "0");
    assert_eq!(parse_max_sockets(), None, "0 disables the Bulk pool cap");
    std::env::set_var(MAX_SOCKETS_ENV_VAR, "banana");
    assert_eq!(parse_max_sockets(), Some(16));
    clear_segmented_env();
}

#[test]
fn quick_pool_size_defaults_to_four_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    assert_eq!(parse_quick_pool_size(), Some(4));

    std::env::set_var(QUICK_POOL_ENV_VAR, "8");
    assert_eq!(parse_quick_pool_size(), Some(8));
    std::env::set_var(QUICK_POOL_ENV_VAR, "0");
    assert_eq!(
        parse_quick_pool_size(),
        None,
        "0 disables the Quick pool cap"
    );
    std::env::set_var(QUICK_POOL_ENV_VAR, "nope");
    assert_eq!(parse_quick_pool_size(), Some(4));
    clear_segmented_env();
}

#[test]
fn quick_threshold_defaults_and_fails_safe() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    // cfg(test) default is intentionally small -- see the constant's docs.
    assert_eq!(parse_quick_threshold(), DEFAULT_QUICK_THRESHOLD_BYTES);

    std::env::set_var(QUICK_THRESHOLD_ENV_VAR, "1024");
    assert_eq!(parse_quick_threshold(), 1024);
    std::env::set_var(QUICK_THRESHOLD_ENV_VAR, "0");
    assert_eq!(
        parse_quick_threshold(),
        DEFAULT_QUICK_THRESHOLD_BYTES,
        "0 fails safe to default"
    );
    std::env::set_var(QUICK_THRESHOLD_ENV_VAR, "not-a-number");
    assert_eq!(parse_quick_threshold(), DEFAULT_QUICK_THRESHOLD_BYTES);
    clear_segmented_env();
}

// ---- end-to-end segmented behavior against a local mock server ----

fn parse_range_header(request: &str) -> Option<(u64, u64)> {
    for line in request.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range: bytes=") {
            let mut parts = rest.trim().splitn(2, '-');
            let start: u64 = parts.next()?.parse().ok()?;
            let end: u64 = parts.next()?.parse().ok()?;
            return Some((start, end));
        }
    }
    None
}

/// Serves `body` for segmented-download tests:
/// - a plain (no-Range) GET gets `Accept-Ranges: bytes` + the full
///   correct body, so the caller's `response` both triggers
///   segmentation AND remains a valid single-stream fallback source.
/// - a Range GET for the FIRST attempt at `bytes=0-*` (i.e. the very
///   start of segment 0) delivers 2 bytes then hangs forever without
///   closing -- this must trip the stall watchdog. Any Range GET NOT
///   starting at byte 0 (i.e. a resume after that stall) is served
///   normally, proving retries resume from the correct offset instead
///   of re-requesting the whole segment.
/// - every other Range GET is served normally and immediately.
async fn serve_stalling_then_recovering(body: Vec<u8>) -> (String, Arc<Mutex<Vec<(u64, u64)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let seen_ranges: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let body = Arc::new(body);
    let stalled_zero_start_once = Arc::new(Mutex::new(false));

    let seen_for_task = Arc::clone(&seen_ranges);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = Arc::clone(&body);
            let seen = Arc::clone(&seen_for_task);
            let stalled_zero_start_once = Arc::clone(&stalled_zero_start_once);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let range = parse_range_header(&request);

                let Some((s, e)) = range else {
                    // Plain GET: the caller's initial request.
                    let header = format!(
                            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                    return;
                };

                seen.lock().unwrap().push((s, e));
                let total = body.len();
                let slice = &body[s as usize..=(e as usize).min(total - 1)];

                let should_stall = {
                    let mut stalled_guard = stalled_zero_start_once.lock().unwrap();
                    let should_stall = s == 0 && !*stalled_guard;
                    if should_stall {
                        *stalled_guard = true;
                    }
                    should_stall
                };
                if should_stall {
                    let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&slice[..2.min(slice.len())]).await;
                    // Hang well past the test's stall timeout without
                    // closing -- this is what must trip the watchdog.
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return;
                }

                let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(slice).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    (format!("http://{address}/asset"), seen_ranges)
}

#[test]
fn stalling_segment_trips_watchdog_recovers_and_resumes_from_offset() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");
    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "1");
    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "2");

    runtime().block_on(async {
        let body: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
        let (url, seen_ranges) = serve_stalling_then_recovering(body.clone()).await;

        let response = test_client().get(&url).await.expect("GET");
        let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
            .await
            .expect("stalled segment must recover via retry, not fail the download");

        let expected_sha = super::super::trust::sha256_of(&body);
        assert_eq!(
            asset.sha256(),
            expected_sha,
            "assembled file must match the source exactly"
        );
        assert_eq!(asset.bytes(), body.len() as u64);

        let ranges = seen_ranges.lock().unwrap().clone();
        assert!(
            ranges.iter().any(|&(s, _)| s == 0),
            "the initial (stalling) request for segment 0 must have been observed: {ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, _)| s == 2),
            "the retry must resume at byte 2 (only the missing tail), not restart at 0: {ranges:?}"
        );
        assert_eq!(
            ranges.iter().filter(|&&(s, _)| s == 0).count(),
            1,
            "segment 0 must be requested from byte 0 exactly once (the stalling attempt); \
                     every subsequent request must resume, never restart from 0: {ranges:?}"
        );
    });

    clear_segmented_env();
}

/// A server whose plain GET advertises Range support (and serves the
/// correct full body, for the fallback path) but whose EVERY Range GET
/// fails outright -- this must exhaust the per-segment retry budget
/// and fall all the way through to draining the original response.
async fn serve_range_always_failing(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let body = Arc::new(body);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                if parse_range_header(&request).is_some() {
                    let _ = socket
                            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    let _ = socket.shutdown().await;
                    return;
                }
                let header = format!(
                        "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{address}/asset")
}

#[test]
fn segment_retry_exhaustion_falls_back_to_single_stream() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");
    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "1");
    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "2");

    runtime().block_on(async {
        let body: Vec<u8> = (0..1024u32).map(|i| (i % 191) as u8).collect();
        let url = serve_range_always_failing(body.clone()).await;

        let response = test_client().get(&url).await.expect("GET");
        let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
            .await
            .expect("every Range request failing must still resolve via single-stream fallback");

        assert_eq!(asset.bytes(), body.len() as u64);
        assert_eq!(asset.sha256(), super::super::trust::sha256_of(&body));
    });

    clear_segmented_env();
}

/// A server that advertises Range support but IGNORES the `Range` header,
/// answering every request -- ranged or not -- with `200 OK` and the FULL
/// body. A naive segmented client that accepts a 200 for a ranged GET
/// would write the whole body at each segment's own offset, corrupting the
/// shared file (N overlapping full-length writes, size >> total). The
/// download must instead reject the non-206 segments and fall through to a
/// correct single stream.
async fn serve_range_ignored_returns_200_full_body(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let body = Arc::new(body);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                // Deliberately ignore any Range header: always 200 + full body.
                let header = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{address}/asset")
}

#[test]
fn range_ignoring_server_returning_200_never_corrupts_the_file() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "4");
    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "1");
    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "2");

    runtime().block_on(async {
        let body: Vec<u8> = (0..2048u32).map(|i| (i % 193) as u8).collect();
        let url = serve_range_ignored_returns_200_full_body(body.clone()).await;

        let response = test_client().get(&url).await.expect("GET");
        let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
            .await
            .expect("a 200-to-a-ranged-GET server must resolve via single-stream fallback");

        // The whole point: exactly the source bytes, never an
        // N-times-overwritten oversized file.
        assert_eq!(
            asset.bytes(),
            body.len() as u64,
            "assembled asset must be exactly the source length, not a corrupted overlay"
        );
        assert_eq!(
            asset.sha256(),
            super::super::trust::sha256_of(&body),
            "assembled file must match the source exactly"
        );
    });

    clear_segmented_env();
}

#[test]
fn global_timeout_expiry_with_no_budget_surfaces_a_clear_error() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");
    std::env::set_var(STALL_TIMEOUT_ENV_VAR, "30");
    std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "0");
    // Smaller than MEANINGFUL_FALLBACK_MIN (5s), so expiry must
    // surface the hard timeout error, not attempt a fallback.
    std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "1");

    runtime().block_on(async {
                // Every segment stalls forever (server never responds to
                // Range requests at all -- just accepts and hangs), so the
                // 1s global timeout is what ends the attempt.
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("addr");
                let body = vec![7u8; 1024];
                let body_for_task = body.clone();
                tokio::spawn(async move {
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            return;
                        };
                        let body = body_for_task.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 4096];
                            let n = socket.read(&mut buf).await.unwrap_or(0);
                            let request = String::from_utf8_lossy(&buf[..n]).to_string();
                            if parse_range_header(&request).is_some() {
                                // Accept the connection but never respond --
                                // simulates a fully stalled segment.
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                return;
                            }
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                            let _ = socket.shutdown().await;
                        });
                    }
                });
                let url = format!("http://{address}/asset");

                let response = test_client().get(&url).await.expect("GET");
                let error = stream_response_to_temp_file(response, &url, Duration::from_secs(30))
                    .await
                    .expect_err("global timeout with no remaining budget must be a hard error");
                let message = error.to_string();
                assert!(
                    message.contains(GLOBAL_TIMEOUT_ENV_VAR),
                    "error must name the env var that controls this deadline: {message}"
                );
            });

    clear_segmented_env();
}

#[test]
fn segmentation_never_attempted_when_response_lacks_accept_ranges() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");

    runtime().block_on(async {
        // Reuses the plain `serve_chunks` helper, whose response has
        // no Accept-Ranges header -- segmentation must be skipped
        // entirely and the existing single-stream path must run.
        let url = serve_chunks(vec![(b"abcd".to_vec(), Duration::ZERO)], 4).await;
        let response = test_client().get(&url).await.expect("GET");
        let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
            .await
            .expect("must succeed via the untouched single-stream path");
        assert_eq!(asset.bytes(), 4);
    });

    clear_segmented_env();
}

// ---- threshold routing: quick vs bulk, quick == never segmented ----

fn response_headers(accept_ranges: bool, content_length: Option<u64>) -> String {
    let mut headers = String::from("HTTP/1.1 200 OK\r\n");
    if accept_ranges {
        headers.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(len) = content_length {
        headers.push_str(&format!("Content-Length: {len}\r\n"));
    }
    headers.push_str("Connection: close\r\n\r\n");
    headers
}

async fn serve_fixed_response(status_and_headers: String, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 1024];
        let _ = socket.read(&mut buf).await;
        let _ = socket.write_all(status_and_headers.as_bytes()).await;
        let _ = socket.write_all(&body).await;
        let _ = socket.shutdown().await;
    });
    format!("http://{address}/asset")
}

#[test]
fn response_at_or_below_threshold_is_never_segmentable() {
    runtime().block_on(async {
        let threshold = DEFAULT_QUICK_THRESHOLD_BYTES;
        let body = vec![1u8; threshold as usize];
        let url = serve_fixed_response(response_headers(true, Some(threshold)), body).await;
        let response = test_client().get(&url).await.expect("GET");
        assert!(
            segmentable_total_len(&response, threshold).is_none(),
            "Content-Length exactly at the threshold must NOT be segmented"
        );
    });
}

#[test]
fn response_above_threshold_is_segmentable() {
    runtime().block_on(async {
        let threshold = DEFAULT_QUICK_THRESHOLD_BYTES;
        let total = threshold + 1;
        let body = vec![1u8; total as usize];
        let url = serve_fixed_response(response_headers(true, Some(total)), body).await;
        let response = test_client().get(&url).await.expect("GET");
        assert_eq!(
            segmentable_total_len(&response, threshold),
            Some(total),
            "Content-Length just above the threshold must be segmentable"
        );
    });
}

#[test]
fn unknown_size_response_is_never_segmentable() {
    runtime().block_on(async {
        // No Content-Length header at all (server signals end via
        // connection close instead).
        let url = serve_fixed_response(
            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n".to_string(),
            vec![1u8; 64],
        )
        .await;
        let response = test_client().get(&url).await.expect("GET");
        assert!(
            segmentable_total_len(&response, DEFAULT_QUICK_THRESHOLD_BYTES).is_none(),
            "unknown size must never be treated as segmentable"
        );
    });
}

// ---- socket pools: bounded concurrency, sharing, permit-leak safety ----

/// A server that answers any Range GET after `body` is served for a
/// plain GET, tracking concurrent-connection high-water-mark via an
/// atomic counter incremented on accept and decremented once the
/// response is fully written.
pub(super) async fn serve_range_tracking_concurrency(
    body: Vec<u8>,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let body = Arc::new(body);
    let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let high_water = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hw_for_task = Arc::clone(&high_water);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = Arc::clone(&body);
            let current = Arc::clone(&current);
            let high_water = Arc::clone(&hw_for_task);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();

                let Some((s, e)) = parse_range_header(&request) else {
                    let header = format!(
                            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                    return;
                };

                let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                high_water.fetch_max(now, Ordering::SeqCst);

                // Hold the connection open briefly so overlapping
                // segment requests actually overlap in wall-clock time.
                tokio::time::sleep(Duration::from_millis(80)).await;

                let total = body.len();
                let slice = &body[s as usize..=(e as usize).min(total - 1)];
                let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(slice).await;
                let _ = socket.shutdown().await;
                current.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (format!("http://{address}/asset"), high_water)
}

#[test]
fn pool_bounds_concurrent_segment_connections() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "4");

    runtime().block_on(async {
        let body: Vec<u8> = (0..4096u32).map(|i| (i % 233) as u8).collect();
        let (url, high_water) = serve_range_tracking_concurrency(body.clone()).await;
        let pool = SocketPool::new(2);

        let response = test_client().get(&url).await.expect("GET");
        let asset = stream_response_to_temp_file_with_pool(
            response,
            &url,
            Duration::from_secs(5),
            ASSET_SAFETY_TIMEOUT,
            pool,
        )
        .await
        .expect("4-segment plan against a size-2 pool must still complete");

        assert_eq!(asset.sha256(), super::super::trust::sha256_of(&body));
        assert!(
            high_water.load(Ordering::SeqCst) <= 2,
            "observed concurrency {} must never exceed the pool size 2",
            high_water.load(Ordering::SeqCst)
        );
    });

    clear_segmented_env();
}

#[test]
fn two_concurrent_downloads_share_one_pool() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
    std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");

    runtime().block_on(async {
        let body_a: Vec<u8> = (0..2048u32).map(|i| (i % 197) as u8).collect();
        let body_b: Vec<u8> = (0..2048u32).map(|i| (i % 199) as u8).collect();
        let (url_a, hw_a) = serve_range_tracking_concurrency(body_a.clone()).await;
        let (url_b, hw_b) = serve_range_tracking_concurrency(body_b.clone()).await;
        let pool = SocketPool::new(2);

        let resp_a = test_client().get(&url_a).await.expect("GET a");
        let resp_b = test_client().get(&url_b).await.expect("GET b");

        let fut_a = stream_response_to_temp_file_with_pool(
            resp_a,
            &url_a,
            Duration::from_secs(5),
            ASSET_SAFETY_TIMEOUT,
            Arc::clone(&pool),
        );
        let fut_b = stream_response_to_temp_file_with_pool(
            resp_b,
            &url_b,
            Duration::from_secs(5),
            ASSET_SAFETY_TIMEOUT,
            Arc::clone(&pool),
        );
        let (asset_a, asset_b) = tokio::join!(fut_a, fut_b);
        let asset_a = asset_a.expect("download a must complete");
        let asset_b = asset_b.expect("download b must complete");

        assert_eq!(asset_a.sha256(), super::super::trust::sha256_of(&body_a));
        assert_eq!(asset_b.sha256(), super::super::trust::sha256_of(&body_b));
        assert!(
            hw_a.load(Ordering::SeqCst) <= 2 && hw_b.load(Ordering::SeqCst) <= 2,
            "neither server should ever see more than the pool's total capacity in flight"
        );
    });

    clear_segmented_env();
}

/// A server that never responds to a Range GET at all (accepts, then
/// hangs), so `fetch_segment_once` must trip the stall watchdog and
/// return a failure -- exercising the RAII permit-drop path.
async fn serve_range_hangs_forever() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            });
        }
    });
    format!("http://{address}/asset")
}

#[test]
fn stalled_segment_retry_does_not_leak_pool_permits() {
    runtime().block_on(async {
                let url = serve_range_hangs_forever().await;
                let pool = SocketPool::new(1);
                let client = test_client();

                assert_eq!(pool.available(), 1);
                let result = fetch_segment_with_retries(
                    &client,
                    &url,
                    0,
                    9,
                    &tempfile::tempfile().expect("tempfile"),
                    SegmentedDownloadConfig {
                        enabled: true,
                        segment_count: 1,
                        connect_timeout: Duration::from_millis(300),
                        stall_timeout: Duration::from_millis(300),
                        segment_retries: 2,
                        global_timeout: None,
                    },
                    Arc::clone(&pool),
                )
                .await;

                assert!(result.is_err(), "a segment that always stalls must exhaust its retries");
                assert_eq!(
                    pool.available(),
                    1,
                    "every stall+retry cycle must release its permit -- the pool must not shrink over retries"
                );
            });
}

// ---- three clocks: connect/TTFB, redirect-hop reset, slow-header/fast-body ----

#[test]
fn hung_connect_trips_connect_timeout_and_releases_permit() {
    runtime().block_on(async {
        let url = serve_range_hangs_forever().await;
        let pool = SocketPool::new(1);
        let client = test_client();

        assert_eq!(pool.available(), 1);
        let outcome = fetch_segment_once(
            &client,
            &url,
            0,
            9,
            &tempfile::tempfile().expect("tempfile"),
            Duration::from_secs(30),
            Duration::from_millis(300),
            &pool,
        )
        .await;

        match outcome {
            SegmentAttemptOutcome::Failed(0, reason) => {
                assert!(
                    reason.contains("TTFB") || reason.contains("connect"),
                    "reason should name the connect/TTFB phase: {reason}"
                );
            }
            _ => panic!("a server that never responds must trip the connect/TTFB clock"),
        }
        assert_eq!(
            pool.available(),
            1,
            "the permit must be released after the connect timeout"
        );
    });
}

#[test]
fn slow_header_fast_body_succeeds() {
    runtime().block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("addr");
            let body = vec![9u8; 64];
            let body_for_task = body.clone();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/64\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_for_task.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body_for_task).await;
                let _ = socket.shutdown().await;
            });
            let url = format!("http://{address}/asset");
            let pool = SocketPool::new(1);
            let client = test_client();

            let outcome = fetch_segment_once(
                &client,
                &url,
                0,
                63,
                &tempfile::tempfile().expect("tempfile"),
                Duration::from_secs(5),
                Duration::from_secs(2),
                &pool,
            )
            .await;
            match outcome {
                SegmentAttemptOutcome::Completed(n) => assert_eq!(n, 64),
                _ => panic!("slow headers within budget followed by a fast body must succeed"),
            }
        });
}

#[test]
fn redirect_hop_resets_connect_clock() {
    runtime().block_on(async {
            let listener2 = TcpListener::bind("127.0.0.1:0").await.expect("bind hop2");
            let address2 = listener2.local_addr().expect("addr2");
            let body = vec![5u8; 32];
            let body_for_task = body.clone();
            tokio::spawn(async move {
                let (mut socket, _) = listener2.accept().await.expect("accept hop2");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-31/32\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_for_task.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body_for_task).await;
                let _ = socket.shutdown().await;
            });
            let hop2_url = format!("http://{address2}/asset");

            let listener1 = TcpListener::bind("127.0.0.1:0").await.expect("bind hop1");
            let address1 = listener1.local_addr().expect("addr1");
            tokio::spawn(async move {
                let (mut socket, _) = listener1.accept().await.expect("accept hop1");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let header = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {hop2_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
            let url = format!("http://{address1}/asset");
            let pool = SocketPool::new(1);
            // Follow each hop in the segment loop, as in production.
            let client = test_client();

            let outcome = fetch_segment_once(
                &client,
                &url,
                0,
                31,
                &tempfile::tempfile().expect("tempfile"),
                Duration::from_secs(5),
                Duration::from_secs(2),
                &pool,
            )
            .await;
            match outcome {
                SegmentAttemptOutcome::Completed(n) => assert_eq!(n, 32),
                SegmentAttemptOutcome::Failed(_, reason) => {
                    panic!("each redirect hop must get its own fresh connect clock: {reason}")
                }
                SegmentAttemptOutcome::Preempted => panic!("no contention expected in this test"),
            }
        });
}

#[test]
fn cross_origin_redirect_does_not_forward_bearer_token() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_segmented_env();
    std::env::set_var(AUTH_TOKEN_ENV_VAR, "secret-token");
    runtime().block_on(async {
            let listener_b = TcpListener::bind("127.0.0.1:0").await.expect("bind b");
            let address_b = listener_b.local_addr().expect("addr b");
            let request_at_b = Arc::new(std::sync::Mutex::new(String::new()));
            let request_at_b_task = Arc::clone(&request_at_b);
            tokio::spawn(async move {
                let (mut socket, _) = listener_b.accept().await.expect("accept b");
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.expect("read b");
                *request_at_b_task.lock().expect("request lock") =
                    String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nConnection: close\r\n\r\nsafe",
                    )
                    .await;
            });
            let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("bind a");
            let address_a = listener_a.local_addr().expect("addr a");
            let redirect = format!("http://{address_b}/asset");
            tokio::spawn(async move {
                let (mut socket, _) = listener_a.accept().await.expect("accept a");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = format!("HTTP/1.1 302 Found\r\nLocation: {redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
            });
            let url = format!("http://{address_a}/asset");
            let client = test_client();
            let outcome = fetch_segment_once(
                &client,
                &url,
                0,
                3,
                &tempfile::tempfile().expect("tempfile"),
                Duration::from_secs(5),
                Duration::from_secs(5),
                &SocketPool::new(1),
            )
            .await;
            assert!(matches!(outcome, SegmentAttemptOutcome::Completed(4)));
            assert!(!request_at_b
                .lock()
                .expect("request lock")
                .to_ascii_lowercase()
                .contains("authorization:"));
        });
    clear_segmented_env();
}

// ---- permit preemption ----

#[test]
fn preempted_segment_requeues_without_spending_retry_budget() {
    runtime().block_on(async {
        let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("bind a");
        let address_a = listener_a.local_addr().expect("addr a");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener_a.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
            }
        });

        let body_b = vec![3u8; 16];
        let (url_b, _hw_b) = serve_range_tracking_concurrency(body_b.clone()).await;

        let pool = SocketPool::new(1);
        let client = test_client();
        let url_a = format!("http://{address_a}/asset");

        let pool_a = Arc::clone(&pool);
        let client_a = client.clone();
        let file_a = tempfile::tempfile().expect("tempfile a");
        let seg_a = tokio::spawn(async move {
            fetch_segment_once(
                &client_a,
                &url_a,
                0,
                9,
                &file_a,
                Duration::from_secs(30),
                Duration::from_secs(30),
                &pool_a,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let config = SegmentedDownloadConfig {
            enabled: true,
            segment_count: 1,
            connect_timeout: Duration::from_secs(5),
            stall_timeout: Duration::from_secs(5),
            segment_retries: 0,
            global_timeout: None,
        };
        let file_b = tempfile::tempfile().expect("tempfile b");
        let result_b =
            fetch_segment_with_retries(&client, &url_b, 0, 15, &file_b, config, Arc::clone(&pool))
                .await;

        assert!(
            result_b.is_ok(),
            "B must complete via preemption of A despite zero configured retries: {result_b:?}"
        );

        seg_a.abort();
    });
}

#[test]
fn uniformly_slow_contention_still_completes_not_spins() {
    // Three equally-slow-connecting segments contend for a pool of
    // 1. The anti-livelock guard (max 2 preemptions per PENDING
    // holder) must let the whole batch converge within a bounded
    // time rather than cycling forever. This is a coarse
    // "completes at all" check, not a precise preemption-count
    // assertion -- see the report for why.
    runtime().block_on(async {
        let body: Vec<u8> = (0..600u32).map(|i| (i % 173) as u8).collect();
        let (url, _hw) = serve_range_tracking_concurrency(body.clone()).await;

        let pool = SocketPool::new(1);
        let config = SegmentedDownloadConfig {
            enabled: true,
            segment_count: 3,
            connect_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_secs(10),
            segment_retries: 5,
            global_timeout: None,
        };
        let client = test_client();
        let file = Arc::new(tempfile::tempfile().expect("tempfile"));

        let plan = compute_segments(body.len() as u64, 3);
        let started = Instant::now();
        let result = run_all_segments(client, url, plan, file, config, pool).await;
        assert!(
            result.is_ok(),
            "contended-but-uniformly-slow segments must still converge: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "must complete well within the test's own budget, not hang: {:?}",
            started.elapsed()
        );
    });
}

// ---- two pools: the motivating scenario + mixed workload ----

#[test]
fn quick_pool_serves_control_requests_while_bulk_pool_is_saturated() {
    runtime().block_on(async {
        let bulk = SocketPool::new(1);
        let quick = SocketPool::new(4);

        // Saturate Bulk with a long-lived STREAMING holder,
        // simulating an in-flight segment that will not finish
        // for the duration of this test.
        let mut bulk_permit = bulk.acquire().await;
        bulk_permit.mark_streaming();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
            let _ = socket.shutdown().await;
        });
        let url = format!("http://{address}/manifest.json");

        let started = Instant::now();
        let resp = send_control_request_with_pool(
            super::super::stream_download::get_request(&test_client(), &url),
            &url,
            Duration::from_secs(5),
            quick,
        )
        .await
        .expect("control request must complete despite bulk saturation");
        assert!((200..300).contains(&resp.status()));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must complete promptly, not queue behind a fully-saturated bulk pool: {:?}",
            started.elapsed()
        );

        drop(bulk_permit);
        assert_eq!(bulk.available(), 1);
    });
}

#[path = "segmented_download_tests_extra.rs"]
mod extra;
