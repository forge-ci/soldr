//! Native WinHTTP download for the ConPTY sidecar archive.
//!
//! This replaced the `ureq` client so that `conpty-sidecar` no longer
//! compiles an HTTP/TLS stack on Linux and macOS, where the sidecar is never
//! fetched. It keeps the contract the `ureq` call had:
//!
//! - HTTPS only. The request URL must be `https://`, the request handle is
//!   opened with `WINHTTP_FLAG_SECURE`, the redirect policy is pinned to
//!   `WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP` (WinHTTP's
//!   default, set explicitly here), and the final URL after redirects is
//!   checked to still be `https://`.
//! - Redirects are followed automatically (the GitHub release asset answers
//!   with a 302 to its CDN), capped at [`MAX_REDIRECTS`], the `ureq` agent
//!   default.
//! - One wall-clock deadline bounds the whole call, body read included.
//!   `ureq`'s request-level `.timeout()` did the same; its 30 s connect
//!   default was always clamped to that deadline. Before every blocking
//!   WinHTTP call the resolve, connect, send and receive timeouts are set to
//!   the time remaining, and the deadline is checked between reads.
//! - The body is capped at [`MAX_BODY_BYTES`]. A larger body is an error
//!   rather than a silent truncation; either way nothing past the SHA-256
//!   check is touched.
//! - Proxy: none, exactly as before. `ureq` was built without
//!   `proxy-from-env`, so it always connected directly; WinHTTP is opened
//!   with `WINHTTP_ACCESS_TYPE_NO_PROXY`. Automatic (WPAD) proxy discovery is
//!   deliberately not used: it runs inside `WinHttpSendRequest` and is not
//!   bounded by `WinHttpSetTimeouts`, which would break the single-deadline
//!   guarantee above (#433). Hosts that need a proxy use
//!   `KERNAL_API_CONPTY_OFFLINE` plus a pre-staged sidecar, as before.
//! - TLS roots come from the Windows certificate store instead of bundled
//!   webpki roots. The compile-time SHA-256 pin remains the trust root for
//!   the bytes.
//!
//! Every error is a `String`, mapped to `io::Error::other` by the caller
//! exactly as the `ureq` error was.

#![cfg(windows)]

use std::ffi::c_void;
use std::ptr::{null, null_mut, NonNull};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER};
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpQueryOption, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts, ERROR_WINHTTP_REDIRECT_FAILED, ERROR_WINHTTP_TIMEOUT,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_MAX_HTTP_AUTOMATIC_REDIRECTS, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP, WINHTTP_OPTION_URL,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

/// Hard cap on the downloaded archive: 64 MiB, about ten times the asset.
pub(super) const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Initial body allocation, matching the previous client.
const INITIAL_BODY_CAPACITY: usize = 8 * 1024 * 1024;
/// Automatic redirects followed before WinHTTP fails the request.
pub(super) const MAX_REDIRECTS: u32 = 5;
const READ_CHUNK: usize = 64 * 1024;

/// An `https://host[:port]/path` URL split into WinHTTP's inputs.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct HttpsTarget {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) path: String,
}

/// Accept only absolute `https://` URLs with a plain host (no userinfo).
pub(super) fn parse_https_url(url: &str) -> Result<HttpsTarget, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("refusing non-HTTPS URL {url}"))?;
    // A fragment is client-side only and must never reach the request line.
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(format!("refusing URL with credentials {url}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .ok()
                .filter(|&p| p != 0)
                .ok_or_else(|| format!("invalid port in URL {url}"))?,
        ),
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(format!("missing host in URL {url}"));
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(HttpsTarget {
        host: host.to_string(),
        port,
        path,
    })
}

/// Reject a post-redirect URL that left HTTPS. WinHTTP's redirect policy
/// already refuses the downgrade; this is the independent check.
pub(super) fn check_final_url(final_url: &str) -> Result<(), String> {
    if final_url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        Ok(())
    } else {
        Err(format!("redirect left HTTPS: {final_url}"))
    }
}

/// Interpret the size probe for `WINHTTP_OPTION_URL`. It must fail with
/// `ERROR_INSUFFICIENT_BUFFER`; a success means there is no fresh error to
/// report, so it gets a fixed message rather than a stale last error.
pub(super) fn check_url_size_probe(succeeded: bool, last_error: u32) -> Result<(), String> {
    const OP: &str = "WinHttpQueryOption(URL)";
    if succeeded {
        return Err(format!("{OP}: size probe unexpectedly succeeded"));
    }
    if last_error == ERROR_INSUFFICIENT_BUFFER {
        Ok(())
    } else {
        Err(describe_error(OP, last_error))
    }
}

/// Only `200 OK` carries the whole asset: the request is a plain GET, so a
/// 206 or any other 2xx is not the archive. `ureq` failed on 4xx/5xx and on
/// a 3xx it could not follow; WinHTTP hands back an unfollowed 3xx as-is.
pub(super) fn check_status(status: u32) -> Result<(), String> {
    if status == 200 {
        Ok(())
    } else {
        Err(format!("status code {status}"))
    }
}

/// Map a WinHTTP / Win32 error code to the message carried by the error.
pub(super) fn describe_error(op: &str, code: u32) -> String {
    match code {
        ERROR_WINHTTP_TIMEOUT => format!("{op}: timed out"),
        ERROR_WINHTTP_REDIRECT_FAILED => format!(
            "{op}: redirect refused (HTTPS to HTTP downgrade or more than {MAX_REDIRECTS} redirects)"
        ),
        _ => format!("{op}: WinHTTP error {code}"),
    }
}

/// One overall deadline for the whole download.
#[derive(Clone, Copy, Debug)]
pub(super) struct Deadline {
    at: Instant,
}

impl Deadline {
    pub(super) fn after(timeout: Duration, now: Instant) -> Self {
        Self { at: now + timeout }
    }

    /// Milliseconds left as a WinHTTP timeout (at least 1, at most
    /// `i32::MAX`), or an error once the deadline has passed.
    pub(super) fn remaining_ms(&self, now: Instant) -> Result<i32, String> {
        let left = self.at.saturating_duration_since(now);
        if left.is_zero() {
            return Err("timed out".to_string());
        }
        Ok(i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1))
    }
}

/// Append a chunk, failing once the body would exceed `limit`.
pub(super) fn append_limited(body: &mut Vec<u8>, chunk: &[u8], limit: usize) -> Result<(), String> {
    if body.len().saturating_add(chunk.len()) > limit {
        return Err(format!("response body exceeds the {limit}-byte limit"));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(op: &str) -> String {
    // SAFETY: `GetLastError` only reads the calling thread's error slot.
    describe_error(op, unsafe { GetLastError() })
}

/// An open WinHTTP handle (session, connection or request), closed on drop.
struct WinHttpHandle(NonNull<c_void>);

impl WinHttpHandle {
    fn new(raw: *mut c_void, op: &str) -> Result<Self, String> {
        NonNull::new(raw).map(Self).ok_or_else(|| last_error(op))
    }

    fn raw(&self) -> *mut c_void {
        self.0.as_ptr()
    }

    fn set_u32_option(&self, option: u32, value: u32, op: &str) -> Result<(), String> {
        // SAFETY: the handle is open, and the buffer is a live `u32` whose
        // size is passed alongside it, as the DWORD options require.
        let ok = unsafe {
            WinHttpSetOption(
                self.raw(),
                option,
                (&raw const value).cast(),
                size_of::<u32>() as u32,
            )
        };
        if ok == 0 {
            Err(last_error(op))
        } else {
            Ok(())
        }
    }

    /// Bound every phase of the next blocking call by the time remaining.
    fn arm(&self, deadline: &Deadline) -> Result<(), String> {
        let ms = deadline.remaining_ms(Instant::now())?;
        // SAFETY: the handle is open; the timeouts are plain integers.
        if unsafe { WinHttpSetTimeouts(self.raw(), ms, ms, ms, ms) } == 0 {
            return Err(last_error("WinHttpSetTimeouts"));
        }
        Ok(())
    }

    fn status_code(&self) -> Result<u32, String> {
        let mut status: u32 = 0;
        let mut len = size_of::<u32>() as u32;
        // SAFETY: the request handle has a received response; the buffer is
        // a live `u32` and `len` is its size in bytes, as
        // `WINHTTP_QUERY_FLAG_NUMBER` requires. Name and index are unused.
        let ok = unsafe {
            WinHttpQueryHeaders(
                self.raw(),
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&raw mut status).cast(),
                &mut len,
                null_mut(),
            )
        };
        if ok == 0 {
            Err(last_error("WinHttpQueryHeaders"))
        } else {
            Ok(status)
        }
    }

    fn final_url(&self) -> Result<String, String> {
        let mut len: u32 = 0;
        // SAFETY: a null buffer with zero length asks only for the required
        // size in bytes; the call is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER.
        let probe = unsafe { WinHttpQueryOption(self.raw(), WINHTTP_OPTION_URL, null_mut(), &mut len) };
        // SAFETY: reads the calling thread's error slot.
        check_url_size_probe(probe != 0, unsafe { GetLastError() })?;
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        let mut len_bytes = u32::try_from(buf.len() * 2).map_err(|_| "URL too long".to_string())?;
        // SAFETY: `buf` is live and `len_bytes` is its size in bytes.
        let ok = unsafe {
            WinHttpQueryOption(
                self.raw(),
                WINHTTP_OPTION_URL,
                buf.as_mut_ptr().cast(),
                &mut len_bytes,
            )
        };
        if ok == 0 {
            return Err(last_error("WinHttpQueryOption(URL)"));
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..end]))
    }

    /// Read one chunk; `Ok(0)` is end of body.
    fn read_chunk(&self, buf: &mut [u8]) -> Result<usize, String> {
        let want = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let mut got: u32 = 0;
        // SAFETY: `buf` is live for `want` bytes and `got` receives the count.
        if unsafe { WinHttpReadData(self.raw(), buf.as_mut_ptr().cast(), want, &mut got) } == 0 {
            return Err(last_error("WinHttpReadData"));
        }
        Ok(got as usize)
    }
}

impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from WinHTTP, is non-null and is closed
        // exactly once, here. Children are declared after their parent, so
        // they drop (close) first.
        unsafe {
            WinHttpCloseHandle(self.raw());
        }
    }
}

/// GET `url` over HTTPS within `timeout`, returning at most
/// [`MAX_BODY_BYTES`] of body.
pub(super) fn https_get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let deadline = Deadline::after(timeout, Instant::now());
    let target = parse_https_url(url)?;
    let agent = wide(concat!("kernal-api/", env!("CARGO_PKG_VERSION")));
    let host = wide(&target.host);
    let path = wide(&target.path);
    let verb = wide("GET");

    // SAFETY: `agent` is a live NUL-terminated UTF-16 string; null proxy and
    // bypass lists are what the no-proxy access type requires.
    let session = WinHttpHandle::new(
        unsafe { WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_NO_PROXY, null(), null(), 0) },
        "WinHttpOpen",
    )?;
    session.arm(&deadline)?;
    // SAFETY: `session` is open and `host` is NUL-terminated UTF-16.
    let connect = WinHttpHandle::new(
        unsafe { WinHttpConnect(session.raw(), host.as_ptr(), target.port, 0) },
        "WinHttpConnect",
    )?;
    // SAFETY: `connect` is open; `verb` and `path` are NUL-terminated;
    // null version, referrer and accept types select the defaults.
    let request = WinHttpHandle::new(
        unsafe {
            WinHttpOpenRequest(
                connect.raw(),
                verb.as_ptr(),
                path.as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;
    request.set_u32_option(
        WINHTTP_OPTION_REDIRECT_POLICY,
        WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP,
        "WinHttpSetOption(REDIRECT_POLICY)",
    )?;
    request.set_u32_option(
        WINHTTP_OPTION_MAX_HTTP_AUTOMATIC_REDIRECTS,
        MAX_REDIRECTS,
        "WinHttpSetOption(MAX_HTTP_AUTOMATIC_REDIRECTS)",
    )?;

    request.arm(&deadline)?;
    // SAFETY: `request` is open; no extra headers and no request body.
    if unsafe { WinHttpSendRequest(request.raw(), null(), 0, null(), 0, 0, 0) } == 0 {
        return Err(last_error("WinHttpSendRequest"));
    }
    request.arm(&deadline)?;
    // SAFETY: `request` was sent; the reserved argument must be null.
    if unsafe { WinHttpReceiveResponse(request.raw(), null_mut()) } == 0 {
        return Err(last_error("WinHttpReceiveResponse"));
    }
    check_final_url(&request.final_url()?)?;
    check_status(request.status_code()?)?;

    let mut body = Vec::with_capacity(INITIAL_BODY_CAPACITY);
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        request.arm(&deadline)?;
        let n = request.read_chunk(&mut chunk)?;
        if n == 0 {
            return Ok(body);
        }
        append_limited(&mut body, &chunk[..n], MAX_BODY_BYTES)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_asset_url() {
        let t = parse_https_url(
            "https://github.com/zackees/kernal-api/releases/download/v1.2.3/conpty-sidecar-x64.tar.zst",
        )
        .unwrap();
        assert_eq!(t.host, "github.com");
        assert_eq!(t.port, 443);
        assert_eq!(
            t.path,
            "/zackees/kernal-api/releases/download/v1.2.3/conpty-sidecar-x64.tar.zst"
        );
    }

    #[test]
    fn parses_port_and_bare_host() {
        let t = parse_https_url("https://example.com:8443").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.path.as_str()), ("example.com", 8443, "/"));
        let t = parse_https_url("https://example.com?x=1").unwrap();
        assert_eq!(t.path, "/?x=1");
    }

    #[test]
    fn strips_fragment_from_request_path() {
        let t = parse_https_url("https://example.com/a/b?x=1#frag").unwrap();
        assert_eq!(t.path, "/a/b?x=1");
        let t = parse_https_url("https://example.com#frag").unwrap();
        assert_eq!((t.host.as_str(), t.path.as_str()), ("example.com", "/"));
        let t = parse_https_url("https://example.com:8443/#").unwrap();
        assert_eq!((t.port, t.path.as_str()), (8443, "/"));
    }

    #[test]
    fn url_size_probe_requires_insufficient_buffer() {
        check_url_size_probe(false, ERROR_INSUFFICIENT_BUFFER).unwrap();
        let err = check_url_size_probe(true, ERROR_INSUFFICIENT_BUFFER).unwrap_err();
        assert!(err.contains("unexpectedly succeeded"), "{err}");
        // A success must not surface whatever stale error is in the slot.
        let err = check_url_size_probe(true, 12029).unwrap_err();
        assert!(!err.contains("12029"), "{err}");
        let err = check_url_size_probe(false, 12029).unwrap_err();
        assert!(err.contains("WinHTTP error 12029"), "{err}");
    }

    #[test]
    fn rejects_non_https_and_malformed_urls() {
        for url in [
            "http://github.com/a",
            "ftp://github.com/a",
            "github.com/a",
            "HTTPS:/github.com",
            "https:///path",
            "https://user:pw@github.com/a",
            "https://github.com:0/a",
            "https://github.com:99999/a",
            "https://github.com:x/a",
        ] {
            assert!(parse_https_url(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn final_url_must_stay_https() {
        check_final_url("https://objects.githubusercontent.com/x").unwrap();
        check_final_url("HTTPS://objects.githubusercontent.com/x").unwrap();
        let err = check_final_url("http://objects.githubusercontent.com/x").unwrap_err();
        assert!(err.contains("left HTTPS"), "{err}");
        assert!(check_final_url("").is_err());
    }

    #[test]
    fn only_status_200_is_accepted() {
        check_status(200).unwrap();
        for status in [0, 199, 201, 204, 206, 299, 301, 302, 304, 404, 500] {
            let err = check_status(status).unwrap_err();
            assert_eq!(err, format!("status code {status}"));
        }
    }

    #[test]
    fn errors_map_to_readable_messages() {
        assert_eq!(describe_error("op", ERROR_WINHTTP_TIMEOUT), "op: timed out");
        let redirect = describe_error("op", ERROR_WINHTTP_REDIRECT_FAILED);
        assert!(redirect.contains("redirect refused"), "{redirect}");
        assert!(redirect.contains(&MAX_REDIRECTS.to_string()), "{redirect}");
        assert_eq!(describe_error("op", 12029), "op: WinHTTP error 12029");
    }

    #[test]
    fn deadline_reports_remaining_and_expiry() {
        let start = Instant::now();
        let d = Deadline::after(Duration::from_secs(20), start);
        assert_eq!(d.remaining_ms(start).unwrap(), 20_000);
        assert_eq!(
            d.remaining_ms(start + Duration::from_millis(19_999)).unwrap(),
            1
        );
        // Sub-millisecond remainder still yields a positive timeout.
        assert_eq!(
            d.remaining_ms(start + Duration::from_micros(19_999_500)).unwrap(),
            1
        );
        assert!(d.remaining_ms(start + Duration::from_secs(20)).is_err());
        assert!(d.remaining_ms(start + Duration::from_secs(21)).is_err());
        let huge = Deadline::after(Duration::from_secs(u64::from(u32::MAX)), start);
        assert_eq!(huge.remaining_ms(start).unwrap(), i32::MAX);
    }

    #[test]
    fn body_limit_is_inclusive_and_enforced() {
        let mut body = Vec::new();
        append_limited(&mut body, &[1; 4], 8).unwrap();
        append_limited(&mut body, &[2; 4], 8).unwrap();
        assert_eq!(body.len(), 8);
        let err = append_limited(&mut body, &[3], 8).unwrap_err();
        assert!(err.contains("8-byte limit"), "{err}");
        assert_eq!(body.len(), 8, "rejected chunk must not be appended");
        assert_eq!(MAX_BODY_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_REDIRECTS, 5);
    }
}
