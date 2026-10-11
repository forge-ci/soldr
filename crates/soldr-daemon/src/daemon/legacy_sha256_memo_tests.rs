use super::*;
use std::cell::Cell;

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "soldr-legacy-sha-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn second_lookup_for_same_content_skips_the_sha256_pass() {
    let memo = scratch("hit");
    let calls = Cell::new(0);
    let compute = || {
        calls.set(calls.get() + 1);
        Ok([7u8; 32])
    };
    let blake3 = [1u8; 32];
    assert_eq!(
        memoized_legacy_sha256(&memo, &blake3, compute).unwrap(),
        [7; 32]
    );
    assert_eq!(
        memoized_legacy_sha256(&memo, &blake3, compute).unwrap(),
        [7; 32]
    );
    assert_eq!(calls.get(), 1, "the second cold start must reuse the memo");
    let _ = std::fs::remove_dir_all(&memo);
}

#[test]
fn different_content_never_shares_a_memo_entry() {
    let memo = scratch("distinct");
    memoized_legacy_sha256(&memo, &[1; 32], || Ok([7; 32])).unwrap();
    let other = memoized_legacy_sha256(&memo, &[2; 32], || Ok([9; 32])).unwrap();
    assert_eq!(other, [9; 32]);
    let _ = std::fs::remove_dir_all(&memo);
}

#[test]
fn malformed_entry_is_recomputed() {
    let memo = scratch("malformed");
    let blake3 = [3u8; 32];
    std::fs::write(memo_entry_path(&memo, &blake3), "not-a-digest").unwrap();
    let got = memoized_legacy_sha256(&memo, &blake3, || Ok([5; 32])).unwrap();
    assert_eq!(got, [5; 32]);
    let _ = std::fs::remove_dir_all(&memo);
}

#[test]
fn memoized_identity_matches_the_legacy_compatible_identity() {
    let memo = scratch("identity");
    let endpoint = Endpoint::default;
    let reference = DaemonProcess::current_process(endpoint(), Some(9)).expect("reference");
    // Cold memo, then warm memo: both must carry the real SHA-256.
    for _ in 0..2 {
        let memoized = current_process_memoized(endpoint(), Some(9), &memo).expect("memoized");
        assert_eq!(memoized.exe_hash, reference.exe_hash);
        assert_eq!(memoized.legacy_exe_sha256, reference.legacy_exe_sha256);
        assert_ne!(memoized.legacy_exe_sha256, [0; 32]);
    }
    let _ = std::fs::remove_dir_all(&memo);
}
