#![cfg(feature = "json")]

use kernal_api::json::{encode, parse, Error, Layout, Value};

#[test]
fn member_parsing_retains_nested_duplicates_and_order() {
    use kernal_api::json::parse_members;
    let source = br#"{"a":null,"a":1,"nested":[{"x":false,"x":true}]}"#;
    let value = parse_members(source).unwrap();
    assert_eq!(
        value,
        Value::ObjectMembers(vec![
            ("a".into(), Value::Null),
            ("a".into(), Value::Signed(1)),
            (
                "nested".into(),
                Value::Array(vec![Value::ObjectMembers(vec![
                    ("x".into(), Value::Bool(false)),
                    ("x".into(), Value::Bool(true)),
                ])])
            ),
        ])
    );
    assert_eq!(encode(&value, Layout::Compact).unwrap(), source);
    assert_ne!(parse(source).unwrap(), value);
}

#[test]
fn member_parser_bounds_count_repeated_values_and_depth() {
    use kernal_api::json::{parse_members, MAX_DEPTH, MAX_INPUT_BYTES, MAX_NODES};
    let source = format!("{{{}\"x\":null}}", "\"x\":null,".repeat(MAX_NODES - 2));
    let Value::ObjectMembers(members) = parse_members(source.as_bytes()).unwrap() else {
        panic!("member object")
    };
    assert_eq!(members.len(), MAX_NODES - 1);
    let source = format!("{{{}\"x\":null}}", "\"x\":null,".repeat(MAX_NODES - 1));
    assert_eq!(parse_members(source.as_bytes()), Err(Error::TooManyNodes));
    for depth in [MAX_DEPTH, MAX_DEPTH + 1] {
        let source = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        let result = parse_members(source.as_bytes());
        if depth == MAX_DEPTH {
            assert!(result.is_ok());
        } else {
            assert_eq!(result, Err(Error::TooDeep));
        }
    }
    let mut source = vec![b' '; MAX_INPUT_BYTES];
    source[0] = b'0';
    assert_eq!(parse_members(&source), Ok(Value::Signed(0)));
    source.push(b' ');
    assert_eq!(parse_members(&source), Err(Error::InputTooLarge));
    for source in [b"[".as_slice(), b"{} {}", b"\xff", b"{\"a\":1,}"] {
        assert_eq!(parse_members(source), Err(Error::InvalidSyntax));
    }
}

#[test]
fn member_parser_keeps_scalar_contract_and_decoded_key_spelling() {
    use kernal_api::json::parse_members;
    for source in [
        "null",
        "true",
        "-9223372036854775808",
        "18446744073709551615",
        "1.5",
        "-0.0",
        "\"日本\\ntext\"",
    ] {
        assert_eq!(
            parse_members(source.as_bytes()).unwrap(),
            parse(source.as_bytes()).unwrap()
        );
    }
    assert_eq!(
        parse_members(br#"{"a":null,"\u0061":true}"#).unwrap(),
        Value::ObjectMembers(vec![
            ("a".into(), Value::Null),
            ("a".into(), Value::Bool(true))
        ])
    );
}

#[test]
fn member_parser_does_not_confuse_user_keys_with_private_number_markers() {
    let key = "$serde_json::private::Number";
    let source = format!("{{\"{key}\":\"1.5\",\"{key}\":2.5}}");
    assert_eq!(
        kernal_api::json::parse_members(source.as_bytes()).unwrap(),
        Value::ObjectMembers(vec![
            (key.into(), Value::String("1.5".into())),
            (key.into(), Value::Float(2.5)),
        ])
    );
}

#[test]
fn json_values_preserve_integer_extrema_and_unicode() {
    let value =
        parse(br#"[-9223372036854775808,18446744073709551615,"\u65e5\u672c",null,true,1.5]"#)
            .unwrap();
    assert_eq!(
        value,
        Value::Array(vec![
            Value::Signed(i64::MIN),
            Value::Unsigned(u64::MAX),
            Value::String("日本".into()),
            Value::Null,
            Value::Bool(true),
            Value::Float(1.5)
        ])
    );
    assert_eq!(
        parse(&encode(&value, Layout::Compact).unwrap()).unwrap(),
        value
    );
}

#[test]
fn objects_have_last_key_wins_and_deterministic_layout() {
    let value = parse(br#"{"z":1,"a":"x","z":2}"#).unwrap();
    assert_eq!(
        encode(&value, Layout::Compact).unwrap(),
        br#"{"a":"x","z":2}"#
    );
    assert_eq!(
        encode(&value, Layout::Pretty).unwrap(),
        b"{\n  \"a\": \"x\",\n  \"z\": 2\n}"
    );
}

#[test]
fn malformed_input_and_nonfinite_values_are_errors() {
    for input in [b"[".as_slice(), b"{} trailing", b"\xff", b"[1,]"] {
        assert_eq!(parse(input), Err(Error::InvalidSyntax));
    }
    assert_eq!(
        encode(&Value::Float(f64::NAN), Layout::Compact),
        Err(Error::NonFiniteNumber)
    );
}

#[test]
fn source_and_output_byte_limits_are_exact() {
    use kernal_api::json::{MAX_INPUT_BYTES, MAX_OUTPUT_BYTES};
    let mut source = vec![b' '; MAX_INPUT_BYTES];
    source[0] = b'0';
    assert_eq!(parse(&source), Ok(Value::Signed(0)));
    source.push(b' ');
    assert_eq!(parse(&source), Err(Error::InputTooLarge));
    let value = Value::String("a".repeat(MAX_OUTPUT_BYTES - 2));
    assert_eq!(
        encode(&value, Layout::Compact).unwrap().len(),
        MAX_OUTPUT_BYTES
    );
    assert_eq!(
        encode(
            &Value::String("a".repeat(MAX_OUTPUT_BYTES - 1)),
            Layout::Compact
        ),
        Err(Error::OutputTooLarge)
    );
    // Escaping expands bytes, and must be included in the output bound.
    assert_eq!(
        encode(
            &Value::String("\n".repeat(MAX_OUTPUT_BYTES / 2)),
            Layout::Compact
        ),
        Err(Error::OutputTooLarge)
    );
}

#[test]
fn decoded_and_constructed_tree_limits_are_exact() {
    use kernal_api::json::{MAX_DEPTH, MAX_NODES};
    let value = Value::Array(vec![Value::Null; MAX_NODES - 1]);
    let encoded = encode(&value, Layout::Compact).unwrap();
    assert_eq!(parse(&encoded).unwrap(), value);
    let oversized = Value::Array(vec![Value::Null; MAX_NODES]);
    assert_eq!(
        encode(&oversized, Layout::Compact),
        Err(Error::TooManyNodes)
    );
    let source = format!("[{}null]", "null,".repeat(MAX_NODES - 1));
    assert_eq!(parse(source.as_bytes()), Err(Error::TooManyNodes));
    let mut value = Value::Null;
    for _ in 0..MAX_DEPTH {
        value = Value::Array(vec![value]);
    }
    assert_eq!(
        parse(&encode(&value, Layout::Compact).unwrap()).unwrap(),
        value
    );
    value = Value::Array(vec![value]);
    assert_eq!(encode(&value, Layout::Compact), Err(Error::TooDeep));
    let source = format!(
        "{}null{}",
        "[".repeat(MAX_DEPTH + 1),
        "]".repeat(MAX_DEPTH + 1)
    );
    assert_eq!(parse(source.as_bytes()), Err(Error::TooDeep));
}
