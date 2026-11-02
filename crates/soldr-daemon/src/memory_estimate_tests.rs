//! Unit coverage for the soldr#3152 shadow-mode memory estimator.

use super::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn sizes(entries: &[(&str, u64)]) -> impl Fn(&Path) -> Option<u64> {
    let table: HashMap<PathBuf, u64> = entries
        .iter()
        .map(|(path, bytes)| (PathBuf::from(path), *bytes))
        .collect();
    move |path: &Path| table.get(path).copied()
}

fn no_sizes(_: &Path) -> Option<u64> {
    None
}

#[test]
fn lto_mode_is_read_from_both_codegen_flag_spellings() {
    let cases: &[(&[&str], LtoMode)] = &[
        (&[], LtoMode::Off),
        (&["-C", "lto=fat"], LtoMode::Fat),
        (&["-Clto=fat"], LtoMode::Fat),
        (&["-C", "lto"], LtoMode::Fat),
        (&["-C", "lto=yes"], LtoMode::Fat),
        (&["-C", "lto=true"], LtoMode::Fat),
        (&["-C", "lto=thin"], LtoMode::Thin),
        (&["-Clto=thin"], LtoMode::Thin),
        (&["-C", "lto=off"], LtoMode::Off),
        (&["-C", "lto=no"], LtoMode::Off),
        (&["-C", "lto=false"], LtoMode::Off),
        // Linker-plugin LTO defers the work to the linker; it is not an
        // in-rustc whole-graph mode.
        (&["-C", "linker-plugin-lto"], LtoMode::Off),
    ];
    for (flags, expected) in cases {
        let features = features_with(&args(flags), no_sizes);
        assert_eq!(features.lto, *expected, "flags {flags:?}");
    }
}

#[test]
fn codegen_units_and_metadata_only_emit_are_recognised() {
    let features = features_with(&args(&["-C", "codegen-units=1"]), no_sizes);
    assert_eq!(features.codegen_units, Some(1));
    let features = features_with(&args(&["-Ccodegen-units=16"]), no_sizes);
    assert_eq!(features.codegen_units, Some(16));
    assert_eq!(features_with(&args(&[]), no_sizes).codegen_units, None);

    assert!(features_with(&args(&["--emit=dep-info,metadata"]), no_sizes).emit_metadata_only);
    assert!(features_with(&args(&["--emit", "metadata"]), no_sizes).emit_metadata_only);
    assert!(!features_with(&args(&["--emit=dep-info,metadata,link"]), no_sizes).emit_metadata_only);
    assert!(!features_with(&args(&[]), no_sizes).emit_metadata_only);
}

#[test]
fn extern_rlibs_are_counted_summed_and_maxed_from_the_command_line() {
    let command = args(&[
        "--crate-name",
        "broker",
        "--test",
        "--extern",
        "serde=/t/deps/libserde.rlib",
        "--extern=soldr_cli=/t/deps/libsoldr_cli.rlib",
        "--extern",
        "proc_macro",
        "--extern",
        "gone=/t/deps/libgone.rlib",
    ]);
    let features = features_with(
        &command,
        sizes(&[
            ("/t/deps/libserde.rlib", 3_000_000),
            ("/t/deps/libsoldr_cli.rlib", 88_000_000),
        ]),
    );
    assert_eq!(features.crate_name.as_deref(), Some("broker"));
    assert!(features.is_test);
    // Every `--extern` is an input; only measurable paths contribute bytes.
    assert_eq!(features.extern_count, 4);
    assert_eq!(features.extern_bytes, 91_000_000);
    assert_eq!(features.max_extern_bytes, 88_000_000);
}

#[test]
fn the_estimate_is_monotone_in_every_feature() {
    let base = features_with(&args(&["--crate-name", "unit"]), no_sizes);
    let small = estimate_peak_bytes(&base);
    assert!(small > 0, "every compile has a floor");

    let mut heavier = base.clone();
    heavier.extern_bytes = 200_000_000;
    heavier.max_extern_bytes = 80_000_000;
    heavier.extern_count = 43;
    assert!(estimate_peak_bytes(&heavier) > small);

    let mut thin = heavier.clone();
    thin.lto = LtoMode::Thin;
    let mut fat = heavier.clone();
    fat.lto = LtoMode::Fat;
    assert!(estimate_peak_bytes(&thin) >= estimate_peak_bytes(&heavier));
    assert!(estimate_peak_bytes(&fat) >= estimate_peak_bytes(&thin));

    // soldr#4014: more concurrent codegen threads never predict less.
    let mut previous = 0;
    for units in [1, 2, 4, 8, 16] {
        let mut unit = heavier.clone();
        unit.codegen_units = Some(units);
        let estimate = estimate_peak_bytes_with(&unit, 16);
        assert!(estimate >= previous, "{units} codegen units predicted less");
        previous = estimate;
    }

    let mut metadata = heavier.clone();
    metadata.emit_metadata_only = true;
    assert!(estimate_peak_bytes(&metadata) <= estimate_peak_bytes(&heavier));
}

#[test]
fn the_args_digest_is_a_stable_join_key_for_the_compile_journal() {
    let a = args(&["--crate-name", "unit", "src/lib.rs"]);
    let b = args(&["--crate-name", "unit", "src/main.rs"]);
    assert_eq!(args_digest(&a), args_digest(&a.clone()));
    assert_ne!(args_digest(&a), args_digest(&b));
    // Joining on NUL keeps ["ab","c"] distinct from ["a","bc"].
    assert_ne!(
        args_digest(&args(&["ab", "c"])),
        args_digest(&args(&["a", "bc"]))
    );
    let digest = args_digest(&a);
    assert_eq!(digest.len(), 64);
    assert!(digest
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
}

#[test]
fn record_appends_one_parseable_row_per_admission_decision() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("logs").join("admission-estimate.jsonl");
    let command = args(&["--crate-name", "broker", "--test", "-C", "lto=thin"]);
    let features = features_with(&command, no_sizes);
    let estimate = estimate_peak_bytes(&features);

    record(&log, &command, &features, estimate, true);
    record(&log, &command, &features, estimate, false);

    let text = std::fs::read_to_string(&log).expect("log written");
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("row parses as json"))
        .collect();
    assert_eq!(rows.len(), 2);
    let row = &rows[0];
    assert_eq!(row["schema_version"], 4);
    assert_eq!(row["crate_name"], "broker");
    assert_eq!(row["args_digest"], args_digest(&command));
    assert_eq!(row["is_test"], true);
    assert_eq!(row["lto"], "thin");
    assert_eq!(row["estimate_bytes"], estimate);
    assert_eq!(row["exclusive"], true);
    assert_eq!(rows[1]["exclusive"], false);
}

#[test]
fn the_log_lives_beside_the_other_daemon_jsonl_logs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = crate::core::SoldrPaths::with_root(temp.path().to_path_buf());
    let path = estimate_log_path(&paths);
    assert!(path.ends_with("logs/admission-estimate.jsonl"), "{path:?}");
    assert_eq!(
        path.parent(),
        Some(
            crate::cache_lib::soldr_daemon_dir(&paths)
                .join("logs")
                .as_path()
        )
    );
}

#[test]
fn shadow_logs_rust_compiles_and_skips_other_compilers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("admission-estimate.jsonl");
    let rust = args(&["--crate-name", "unit", "--emit=dep-info,metadata"]);

    shadow(&log, false, &rust, false);
    assert!(!log.exists(), "a C/C++ compile writes no estimate row");

    shadow(&log, true, &rust, false);
    let text = std::fs::read_to_string(&log).expect("rust compile logged");
    let row: serde_json::Value = serde_json::from_str(text.trim()).expect("one row");
    assert_eq!(row["crate_name"], "unit");
    assert_eq!(row["emit_metadata_only"], true);
    assert_eq!(row["lto"], "off");
}

/// Cross-language pin: `.github/scripts/test_fit_memory_estimate.py` asserts
/// the same value, so the offline join key cannot silently drift from the
/// daemon's.
#[test]
fn the_args_digest_matches_the_offline_fitter() {
    assert_eq!(
        args_digest(&args(&["--crate-name", "unit"])),
        "24bd102264f931a80336bf1378360965954b9c77d2ab1576c44cebda09e61176"
    );
}

/// soldr#3152: the first CI data joined only 92 of 1,235 estimates by
/// `args_digest`, because zccache hands admission a different argument vector
/// than the one it journals. Cargo's `-C metadata=` is unique per unit and
/// survives in both, so it keys the join.
#[test]
fn the_unit_key_is_crate_name_and_cargo_metadata() {
    let both_spellings = [
        args(&[
            "--crate-name",
            "thiserror",
            "-C",
            "metadata=a811629650414cac",
        ]),
        args(&["--crate-name=thiserror", "-Cmetadata=a811629650414cac"]),
    ];
    for command in &both_spellings {
        assert_eq!(
            unit_key(command).as_deref(),
            Some("thiserror/a811629650414cac")
        );
    }
    // Without cargo's metadata there is no stable unit identity to join on.
    assert_eq!(unit_key(&args(&["--crate-name", "thiserror"])), None);
    assert_eq!(unit_key(&args(&["-C", "metadata=a811629650414cac"])), None);
}

#[test]
fn schema_two_rows_carry_the_unit_key() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("admission-estimate.jsonl");
    let command = args(&["--crate-name", "anyhow", "-C", "metadata=0123456789abcdef"]);
    let features = features_with(&command, no_sizes);
    record(
        &log,
        &command,
        &features,
        estimate_peak_bytes(&features),
        false,
    );
    let row: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&log).expect("row").trim()).expect("json");
    assert_eq!(row["schema_version"], 4);
    assert_eq!(row["unit_key"], "anyhow/0123456789abcdef");
}

/// soldr#3152 step 4, shadow mode: rows carry the unit's remembered peak so
/// the history estimator can be compared against measured peaks before
/// admission spends it.
#[test]
fn schema_three_rows_carry_the_units_remembered_peak() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("admission-estimate.jsonl");
    let command = args(&["--crate-name", "zccache", "-C", "metadata=0123456789abcdef"]);
    let features = features_with(&command, no_sizes);
    let remembered = crate::daemon::unit_memory_history::UnitMemory {
        peak_rss_bytes: 90,
        tree_peak_rss_bytes: 1_356,
        updated_ms: 7,
        samples: 3,
    };

    record_with_history(
        &log,
        &command,
        &features,
        estimate_peak_bytes(&features),
        true,
        Some(remembered),
    );
    record_with_history(
        &log,
        &command,
        &features,
        estimate_peak_bytes(&features),
        false,
        None,
    );

    let text = std::fs::read_to_string(&log).expect("rows");
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    assert_eq!(rows[0]["schema_version"], 4);
    assert_eq!(rows[0]["history_tree_peak_bytes"], 1_356);
    assert_eq!(rows[0]["history_samples"], 3);
    assert!(
        rows[1]["history_tree_peak_bytes"].is_null(),
        "an unseen unit has no history"
    );
}

#[tokio::test]
async fn shadow_looks_up_the_units_history_by_unit_key() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("admission-estimate.jsonl");
    let history = crate::daemon::unit_memory_history::UnitMemoryHistory::start(
        temp.path().join("state.sqlite3"),
    );
    // Realistic peaks: readings under the history's 8 MiB trust floor are
    // spawn-instant samples and are deliberately not remembered.
    history.record(
        "anyhow/0123456789abcdef",
        10 * 1024 * 1024,
        42 * 1024 * 1024,
    );
    let command = args(&["--crate-name", "anyhow", "-C", "metadata=0123456789abcdef"]);

    shadow_with_history(&log, true, &command, false, Some(&history));

    let row: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&log).expect("row").trim()).expect("json");
    assert_eq!(row["unit_key"], "anyhow/0123456789abcdef");
    assert_eq!(row["history_tree_peak_bytes"], 42 * 1024 * 1024);
}

#[test]
fn codegen_units_are_priced_as_llvm_fanout_capped_by_cores() {
    // soldr#4014: the amalgamated zccache unit at Cargo's default fans out
    // into one LLVM thread per core; admission must see that, not one compile.
    let base = features_with(&args(&["--crate-name", "zccache"]), no_sizes);
    assert_eq!(codegen_threads(&base, 32), 16, "default is 16 units");
    assert_eq!(codegen_threads(&base, 4), 4, "threads are capped by cores");

    let mut one = base.clone();
    one.codegen_units = Some(1);
    assert_eq!(codegen_threads(&one, 32), 1);

    let mut metadata = base.clone();
    metadata.emit_metadata_only = true;
    assert_eq!(codegen_threads(&metadata, 32), 0);

    let single = estimate_peak_bytes_with(&one, 16);
    let fanned = estimate_peak_bytes_with(&base, 16);
    assert!(
        fanned > single,
        "16 concurrent LLVM threads must cost more than one: {fanned} vs {single}"
    );
    assert!(estimate_peak_bytes_with(&base, 1) < fanned);
}
