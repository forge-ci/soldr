#![cfg(feature = "config-toml")]

use kernal_api::config::{Document, ParseError, Value, MAX_INPUT_BYTES, MAX_NODES};

#[test]
fn preserves_toml_values_without_exposing_parser_types() {
    let document = Document::parse_toml("title = '日本語'\nn = 42\nok = true\nf = 1.5\nwhen = 1979-05-27\n[flags]\nitems = ['-O0', '-pthread']\n").unwrap();
    let Value::Table(root) = document.root() else {
        panic!("root table")
    };
    assert_eq!(root["title"], Value::String("日本語".into()));
    assert_eq!(root["n"], Value::Integer(42));
    assert_eq!(root["ok"], Value::Boolean(true));
    assert_eq!(root["f"], Value::Float(1.5));
    assert_eq!(root["when"], Value::DateTime("1979-05-27".into()));
    let Value::Table(flags) = &root["flags"] else {
        panic!("flags table")
    };
    assert_eq!(
        flags["items"],
        Value::Array(vec![
            Value::String("-O0".into()),
            Value::String("-pthread".into())
        ])
    );
}

#[test]
fn rejects_invalid_documents_and_honors_input_bound() {
    for source in ["x =", "x=1\nx=2", "x='unterminated"] {
        assert!(matches!(
            Document::parse_toml(source),
            Err(ParseError::InvalidSyntax)
        ));
    }
    let source = format!("#{}", " ".repeat(MAX_INPUT_BYTES - 1));
    assert!(Document::parse_toml(&source).is_ok());
    assert!(matches!(
        Document::parse_toml(&(source + " ")),
        Err(ParseError::InputTooLarge)
    ));
}

#[test]
fn decoded_node_and_depth_limits_are_exact() {
    let source = format!("x=[{}]", vec!["0"; MAX_NODES - 2].join(","));
    assert!(Document::parse_toml(&source).is_ok());
    let source = format!("x=[{}]", vec!["0"; MAX_NODES - 1].join(","));
    assert!(matches!(
        Document::parse_toml(&source),
        Err(ParseError::TooManyNodes)
    ));
    let source = format!("x={}0{}", "[".repeat(31), "]".repeat(31));
    assert!(Document::parse_toml(&source).is_ok());
    let source = format!("x={}0{}", "[".repeat(32), "]".repeat(32));
    assert!(matches!(
        Document::parse_toml(&source),
        Err(ParseError::TooDeep)
    ));
}
