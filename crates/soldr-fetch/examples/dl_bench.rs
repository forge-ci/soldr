//! Segmented-download experiment harness (setup-soldr feat/segmented-download-experiment).
//!
//! Standalone measurement tool -- deliberately NOT wired into the
//! production `syslib_common.rs` / `xwin_cache.rs` paths. It answers one
//! empirical question: does N-way HTTP Range segmentation beat soldr's
//! current single-stream download against the CDNs soldr actually talks
//! to (media.githubusercontent.com LFS-style assets, GitHub release
//! signed-URL redirects)?
//!
//! Run with:
//!   CMAKE_GENERATOR=Ninja soldr --no-cache cargo run -p soldr-fetch \
//!     --example dl_bench -- [--repeats N] [--urls all|xwin|musl|mingw] \
//!     [--n-list 2,4,8,16] [--skip-aria2] [--skip-segmented]
//!
//! Methodology notes (see module docs in the PR description for the full
//! writeup):
//! - Every request (probe + segment + single-stream) goes through the
//!   ORIGINAL url with reqwest's default redirect-following. This means
//!   a per-request signed URL (GitHub release assets redirect to
//!   objects.githubusercontent.com with a short-lived SAS token) is
//!   re-resolved independently for every single HTTP request, which is
//!   the safe thing to do -- no stale-token risk from caching a resolved
//!   URL across segments or across repeats.
//! - Configs are interleaved per repeat (all configs run once, then the
//!   cycle repeats) rather than run back-to-back, to spread CDN warm/cold
//!   effects evenly across methods instead of concentrating them on
//!   whichever config runs last.
//! - The timed window covers the transfer AND a `sync_all()` durability
//!   flush. sha256 verification happens strictly after the clock stops,
//!   identically for every method, so hashing cost never enters the
//!   measurement and every method's output is checked against the same
//!   reference digest.
//! - The one-time Range/Accept-Ranges/Content-Length preflight probe for
//!   segmented configs IS counted in that config's total (it's a real
//!   cost a production caller would pay every invocation in this
//!   prototype -- no cross-run caching).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernal_api::hash::Sha256Hasher;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, BoxError>;

const AUTH_TOKEN_ENV: &str = "SOLDR_TOOLCHAIN_AUTH_TOKEN";

struct BenchUrl {
    label: &'static str,
    url: &'static str,
    note: &'static str,
}

const URLS: &[BenchUrl] = &[
    BenchUrl {
        label: "xwin",
        url: "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-x86_64-msvc/xwin-cache.tar.zst",
        note: "media.githubusercontent LFS-style asset (~85 MB) -- the named xwin MSVCRT pain point",
    },
    BenchUrl {
        label: "musl",
        url: "https://github.com/zackees/soldr-toolchain/releases/download/musl-cross-v1/aarch64-linux-musl-cross.tgz",
        note: "classic GitHub release asset, 302s to a signed objects.githubusercontent.com URL (~108 MB)",
    },
    BenchUrl {
        label: "mingw",
        url: "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/mingw-w64-gcc/15.3.0posix-14.0.0-msvcrt-r1/windows-x64-gnu/bundle.tar.zst",
        note: "media.githubusercontent LFS-style asset, largest tractable catalogue entry (~192 MB)",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Method {
    Single,
    Segmented(u32),
    Aria2(u32),
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Method::Single => write!(f, "single-stream"),
            Method::Segmented(n) => write!(f, "segmented-N{n}"),
            Method::Aria2(n) => write!(f, "aria2c-x{n}"),
        }
    }
}

struct RunOutcome {
    elapsed: Duration,
    bytes: u64,
    sha256: String,
}

struct Args {
    repeats: u32,
    urls: Vec<&'static BenchUrl>,
    n_list: Vec<u32>,
    skip_aria2: bool,
    skip_segmented: bool,
}

fn parse_args() -> Args {
    let mut repeats = 3u32;
    let mut which = "all".to_string();
    let mut n_list = vec![2u32, 4, 8, 16];
    let mut skip_aria2 = false;
    let mut skip_segmented = false;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--repeats" => {
                i += 1;
                repeats = argv[i].parse().expect("--repeats takes an integer");
            }
            "--urls" => {
                i += 1;
                which = argv[i].clone();
            }
            "--n-list" => {
                i += 1;
                n_list = argv[i]
                    .split(',')
                    .map(|s| {
                        s.trim()
                            .parse()
                            .expect("--n-list takes comma-separated integers")
                    })
                    .collect();
            }
            "--skip-aria2" => skip_aria2 = true,
            "--skip-segmented" => skip_segmented = true,
            other => panic!("unrecognized arg: {other}"),
        }
        i += 1;
    }

    let urls: Vec<&'static BenchUrl> = if which == "all" {
        URLS.iter().collect()
    } else {
        which
            .split(',')
            .map(|label| {
                URLS.iter()
                    .find(|u| u.label == label.trim())
                    .unwrap_or_else(|| panic!("unknown url label: {label}"))
            })
            .collect()
    };

    Args {
        repeats,
        urls,
        n_list,
        skip_aria2,
        skip_segmented,
    }
}

fn aria2c_available() -> bool {
    Command::new("aria2c")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256Hasher::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex())
}

fn auth_token() -> Option<String> {
    std::env::var(AUTH_TOKEN_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent("soldr-dl-bench/0.1 (segmented-download-experiment)")
        .build()
        .expect("build reqwest client")
}

// ---- positional (pwrite/seek_write) file I/O so N segment tasks can
// ---- share one file handle without fighting over a shared cursor. ----

fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    soldr_platform::fs::positioned_io::write_at(file, buf, offset)
}

fn write_at_all(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        let n = write_at(file, buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write_at wrote 0 bytes",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

async fn run_single_stream(client: &reqwest::Client, url: &str, out: &Path) -> Result<RunOutcome> {
    use std::io::Write;
    let started = Instant::now();

    let mut req = client.get(url);
    if let Some(tok) = auth_token() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let mut resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(format!("single-stream GET failed: HTTP {}", resp.status()).into());
    }

    let mut file = std::fs::File::create(out)?;
    let mut bytes = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
        bytes += chunk.len() as u64;
    }
    file.sync_all()?;
    let elapsed = started.elapsed();

    let sha256 = sha256_file(out)?;
    Ok(RunOutcome {
        elapsed,
        bytes,
        sha256,
    })
}

struct ProbeResult {
    total_len: u64,
    range_supported: bool,
}

/// Pre-flight probe: request a 1-byte range and inspect the response.
/// A 206 with `Content-Range: bytes 0-0/<total>` confirms Range support
/// and gives us the full resource length without a separate HEAD (some
/// CDNs mishandle HEAD on redirected assets; a tiny ranged GET is a more
/// reliable probe across both media.githubusercontent and GitHub release
/// signed URLs).
async fn probe_range_support(client: &reqwest::Client, url: &str) -> Result<ProbeResult> {
    let mut req = client.get(url).header(reqwest::header::RANGE, "bytes=0-0");
    if let Some(tok) = auth_token() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let resp = req.send().await?;
    let status = resp.status();

    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .ok_or("206 response missing Content-Range")?;
        // Format: "bytes 0-0/12345"
        let total: u64 = content_range
            .rsplit('/')
            .next()
            .ok_or("malformed Content-Range")?
            .parse()
            .map_err(|_| "non-numeric Content-Range total")?;
        Ok(ProbeResult {
            total_len: total,
            range_supported: true,
        })
    } else if status.is_success() {
        // Server ignored Range and sent the whole thing (or the whole 1
        // byte, coincidentally) -- treat as unsupported and let the
        // caller fall back to single-stream. Content-Length here would
        // be 1, not the real total, so we cannot recover a usable total
        // from this branch.
        Ok(ProbeResult {
            total_len: 0,
            range_supported: false,
        })
    } else {
        Err(format!("probe request failed: HTTP {status}").into())
    }
}

async fn run_segmented(
    client: &reqwest::Client,
    url: &str,
    out: &Path,
    n: u32,
) -> Result<RunOutcome> {
    let started = Instant::now();

    let probe = probe_range_support(client, url).await?;
    if !probe.range_supported || probe.total_len == 0 {
        return Err(
            "server does not honor Range requests (or reported zero length); \
                     a real implementation would fall back to single-stream here"
                .into(),
        );
    }
    let total = probe.total_len;

    let file = std::fs::File::create(out)?;
    file.set_len(total)?;
    let file = Arc::new(file);

    // Even split of [0, total) into n non-overlapping segments; any
    // remainder bytes go to the first `remainder` segments so every byte
    // is covered exactly once.
    let base = total / n as u64;
    let remainder = total % n as u64;
    let mut segments = Vec::with_capacity(n as usize);
    let mut cursor = 0u64;
    for i in 0..n {
        let len = base + if (i as u64) < remainder { 1 } else { 0 };
        if len == 0 {
            continue;
        }
        let start = cursor;
        let end_inclusive = start + len - 1;
        segments.push((start, end_inclusive));
        cursor += len;
    }
    debug_assert_eq!(cursor, total);

    let mut tasks = Vec::with_capacity(segments.len());
    for (start, end_inclusive) in segments {
        let client = client.clone();
        let url = url.to_string();
        let file = Arc::clone(&file);
        let token = auth_token();
        tasks.push(tokio::spawn(async move {
            let mut req = client.get(&url).header(
                reqwest::header::RANGE,
                format!("bytes={start}-{end_inclusive}"),
            );
            if let Some(tok) = &token {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {tok}"));
            }
            let mut resp = req.send().await?;
            if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && !resp.status().is_success()
            {
                return Err::<u64, BoxError>(
                    format!(
                        "segment [{start},{end_inclusive}] failed: HTTP {}",
                        resp.status()
                    )
                    .into(),
                );
            }
            let mut offset = start;
            let mut written = 0u64;
            while let Some(chunk) = resp.chunk().await? {
                write_at_all(&file, &chunk, offset)?;
                offset += chunk.len() as u64;
                written += chunk.len() as u64;
            }
            let expected = end_inclusive - start + 1;
            if written != expected {
                return Err(format!(
                    "segment [{start},{end_inclusive}] wrote {written} bytes, expected {expected}"
                )
                .into());
            }
            Ok(written)
        }));
    }

    let mut bytes = 0u64;
    for t in tasks {
        bytes += t.await??;
    }
    file.sync_all()?;
    let elapsed = started.elapsed();

    let sha256 = sha256_file(out)?;
    Ok(RunOutcome {
        elapsed,
        bytes,
        sha256,
    })
}

fn run_aria2c(url: &str, out: &Path, n: u32) -> Result<RunOutcome> {
    let dir = out.parent().ok_or("out path has no parent dir")?;
    let filename = out
        .file_name()
        .ok_or("out path has no file name")?
        .to_string_lossy()
        .to_string();

    let started = Instant::now();
    let status = Command::new("aria2c")
        .arg(format!("-x{n}"))
        .arg(format!("-s{n}"))
        .arg("-k")
        .arg("1M")
        .arg("--min-split-size=1M")
        .arg("--allow-overwrite=true")
        .arg("--auto-file-renaming=false")
        // This runner's IPv6 route to GitHub's CDN edges is flaky
        // (`AbstractCommand.cc:312 ... unreachable network`) even though
        // IPv4 works every time; force IPv4 so aria2c is a fair
        // reference point instead of an unrelated network-stack failure.
        .arg("--disable-ipv6=true")
        .arg("--summary-interval=0")
        .arg("--console-log-level=warn")
        .arg("-d")
        .arg(dir)
        .arg("-o")
        .arg(&filename)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("aria2c exited with {status}").into());
    }
    // aria2c writes the file itself; there is no separate fsync hook we
    // control, but the process has already exited by the time .status()
    // returns, which on every supported OS means its writes are visible
    // to us (the OS does not report process exit before close()/fclose()
    // complete). We do an explicit read-open + sync of our own handle
    // for symmetry with the other two methods before stopping the clock.
    let f = std::fs::File::open(out)?;
    f.sync_all().ok(); // best-effort; read-only handles may not need this
    let elapsed = started.elapsed();

    let bytes = std::fs::metadata(out)?.len();
    let sha256 = sha256_file(out)?;
    Ok(RunOutcome {
        elapsed,
        bytes,
        sha256,
    })
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

/// A plain `#[tokio::main]` polls the top-level future on the process's
/// original OS thread, which on Windows defaults to a 1 MiB stack. This
/// benchmark's `main` body (many nested loops, `BTreeMap`s, per-repeat
/// vectors) produces a large generated future state machine in debug
/// builds and reliably blew that default stack (`STATUS_STACK_OVERFLOW`)
/// during real runs. Running the async body on a dedicated thread with
/// an explicit larger stack sidesteps it without needing release-mode
/// optimization to shrink the state machine.
fn main() -> Result<()> {
    std::thread::Builder::new()
        .name("dl-bench-main".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(async_main())
        })
        .expect("spawn dl-bench-main thread")
        .join()
        .expect("dl-bench-main thread panicked")
}

#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
async fn async_main() -> Result<()> {
    let args = parse_args();
    let http = client();
    let has_aria2 = !args.skip_aria2 && aria2c_available();

    if let Some(tok) = auth_token() {
        eprintln!(
            "note: {AUTH_TOKEN_ENV} is set ({} chars) -- attaching Authorization: Bearer to every request",
            tok.len()
        );
    }
    eprintln!(
        "aria2c: {}",
        if args.skip_aria2 {
            "skipped (--skip-aria2)".to_string()
        } else if has_aria2 {
            "found on PATH".to_string()
        } else {
            "NOT found on PATH -- external reference points will be omitted".to_string()
        }
    );

    let workdir = tempfile::tempdir()?;

    // results[(url_label, method)] = Vec<RunOutcome>
    let mut results: BTreeMap<(&'static str, Method), Vec<RunOutcome>> = BTreeMap::new();
    let mut reference_sha: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();

    for bench_url in &args.urls {
        eprintln!(
            "\n=== {} ({}) ===\n{}",
            bench_url.label, bench_url.url, bench_url.note
        );

        let mut methods = vec![Method::Single];
        if !args.skip_segmented {
            methods.extend(args.n_list.iter().map(|&n| Method::Segmented(n)));
        }
        if has_aria2 {
            methods.extend(
                args.n_list
                    .iter()
                    .filter(|&&n| n >= 4)
                    .map(|&n| Method::Aria2(n)),
            );
        }

        for repeat in 0..args.repeats {
            for &method in &methods {
                let out_path: PathBuf = workdir
                    .path()
                    .join(format!("{}-{:?}-{}.bin", bench_url.label, method, repeat));
                let attempt = match method {
                    Method::Single => run_single_stream(&http, bench_url.url, &out_path).await,
                    Method::Segmented(n) => run_segmented(&http, bench_url.url, &out_path, n).await,
                    Method::Aria2(n) => run_aria2c(bench_url.url, &out_path, n),
                };
                let _ = std::fs::remove_file(&out_path);

                match attempt {
                    Ok(outcome) => {
                        eprintln!(
                            "  [{repeat}] {:<16} {:>7.2}s  {:>7.2} MB/s  sha256={}",
                            method.to_string(),
                            outcome.elapsed.as_secs_f64(),
                            (outcome.bytes as f64 / 1_000_000.0) / outcome.elapsed.as_secs_f64(),
                            &outcome.sha256[..12]
                        );
                        match reference_sha.get(bench_url.label) {
                            Some(expected) if expected != &outcome.sha256 => {
                                errors.push(format!(
                                    "{} {method}: sha256 MISMATCH got={} expected={}",
                                    bench_url.label, outcome.sha256, expected
                                ));
                            }
                            None => {
                                reference_sha.insert(bench_url.label, outcome.sha256.clone());
                            }
                            _ => {}
                        }
                        results
                            .entry((bench_url.label, method))
                            .or_default()
                            .push(outcome);
                    }
                    Err(e) => {
                        eprintln!("  [{repeat}] {:<16} FAILED: {e}", method.to_string());
                        errors.push(format!("{} {method} repeat {repeat}: {e}", bench_url.label));
                    }
                }
            }
        }
    }

    println!("\n\n## Results (median of successful runs)\n");
    println!("| url | method | n_ok | median s | min s | max s | median MB/s |");
    println!("|---|---|---|---|---|---|---|");
    for ((label, method), runs) in &results {
        let secs: Vec<f64> = runs.iter().map(|r| r.elapsed.as_secs_f64()).collect();
        let bytes = runs.first().map(|r| r.bytes).unwrap_or(0);
        let med = median(secs.clone());
        let min = secs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = secs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mb_s = (bytes as f64 / 1_000_000.0) / med;
        println!(
            "| {label} | {method} | {}/{} | {med:.2} | {min:.2} | {max:.2} | {mb_s:.1} |",
            runs.len(),
            args.repeats
        );
    }

    if !errors.is_empty() {
        println!("\n## Errors / mismatches ({})\n", errors.len());
        for e in &errors {
            println!("- {e}");
        }
    }

    Ok(())
}
