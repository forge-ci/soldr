//! The capability document is a contract with a consumer (soldr#3924):
//! its keys are read by clud's `block_bad_cmd_soldr` classifier, so a rename
//! or a type change here breaks a shipped consumer. These tests pin the
//! document, not the implementation.

use super::*;

fn parsed(json: &str) -> serde_json::Value {
    serde_json::from_str(json).expect("capability document must be valid JSON")
}

#[test]
fn the_document_carries_the_keys_a_consumer_reads() {
    let line = Capabilities::current().to_json_line().expect("serialize");
    // Exactly one line: a consumer reads stdout line by line.
    assert!(!line.contains('\n'), "document must be one line: {line}");
    let value = parsed(&line);
    // clud's parser requires these three and this schema version.
    assert_eq!(value["schema_version"], CAPABILITIES_SCHEMA_VERSION);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["failure_first"], true);
    assert_eq!(value["artifact_continuation"], true);
    // Additive keys: present, but a consumer that ignores them still works.
    assert_eq!(
        value["porcelain_schema_version"],
        crate::cargo_front_door::porcelain::SCHEMA_VERSION
    );
    assert_eq!(value["product"], "soldr");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn an_unknown_or_missing_verb_is_usage_and_exit_two() {
    assert_eq!(run(&["capabilities".to_string()]).expect("run"), 0);
    assert_eq!(run(&[]).expect("run"), 2);
    assert_eq!(run(&["nonsense".to_string()]).expect("run"), 2);
    assert_eq!(
        run(&["capabilities".to_string(), "extra".to_string()]).expect("run"),
        2
    );
}

#[test]
fn the_human_form_repeats_every_advertised_fact() {
    let capabilities = Capabilities::current();
    let human = capabilities.to_human();
    for needle in [
        format!("soldr {}", capabilities.version),
        format!("schema_version={}", capabilities.schema_version),
        format!("failure_first: {}", capabilities.failure_first),
        format!(
            "artifact_continuation: {}",
            capabilities.artifact_continuation
        ),
        format!(
            "porcelain schema_version: {}",
            capabilities.porcelain_schema_version
        ),
    ] {
        assert!(human.contains(&needle), "missing {needle:?} in {human:?}");
    }
}

#[test]
fn the_advertised_modes_are_the_ones_the_build_implements() {
    // A document that advertises a mode the build does not implement would
    // make a consumer rewrite calls it cannot read. The booleans are literal
    // here because the modes are unconditional in this build: `--porcelain`
    // is always compiled in. If a feature gate ever removes one, this test
    // is the place to make the document follow the gate.
    let capabilities = Capabilities::current();
    assert!(capabilities.failure_first);
    assert!(capabilities.artifact_continuation);
    assert_eq!(
        crate::cargo_front_door::porcelain::PORCELAIN_ENV_VAR,
        "SOLDR_PORCELAIN"
    );
}
