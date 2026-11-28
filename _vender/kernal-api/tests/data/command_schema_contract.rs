#![cfg(feature = "command-schema")]

use kernal_api::command::{
    Command, CommandError, OptionSpec, ValueKind, MAX_ARGUMENTS, MAX_ARGUMENT_BYTES,
};

#[test]
fn parses_nested_commands_defaults_repeated_values_and_constraints() {
    let schema = Command::new("fastled")
        .option(OptionSpec::flag("quick"))
        .option(
            OptionSpec::value("link", ValueKind::enumeration(["static", "dynamic"]))
                .default("static"),
        )
        .subcommand(
            Command::new("source").subcommand(
                Command::new("update")
                    .option(OptionSpec::value("ref", ValueKind::string()).default("master")),
            ),
        );
    let parsed = schema
        .parse(["fastled", "--quick", "source", "update", "--ref", "main"])
        .unwrap();
    assert_eq!(parsed.command_path(), ["fastled", "source", "update"]);
    assert_eq!(parsed.flag("quick"), Some(true));
    assert_eq!(parsed.value("link"), Some("static"));
    assert_eq!(parsed.value("ref"), Some("main"));
}

#[test]
fn rejects_invalid_enumerations_and_bounds_before_backend_parsing() {
    let schema = Command::new("fastled").option(OptionSpec::value(
        "link",
        ValueKind::enumeration(["static", "dynamic"]),
    ));
    assert_eq!(
        schema.parse(["fastled", "--link", "unsupported"]),
        Err(CommandError::InvalidArguments)
    );
    assert_eq!(
        schema.parse(["fastled", "--link", &"x".repeat(MAX_ARGUMENT_BYTES + 1)]),
        Err(CommandError::ArgumentTooLarge)
    );
    let too_many = std::iter::once("fastled").chain(std::iter::repeat_n("arg", MAX_ARGUMENTS));
    assert_eq!(schema.parse(too_many), Err(CommandError::TooManyArguments));
    let too_large = std::iter::once("fastled".to_owned())
        .chain(std::iter::repeat_n("x".repeat(MAX_ARGUMENT_BYTES), 17))
        .collect::<Vec<_>>();
    assert_eq!(schema.parse(&too_large), Err(CommandError::InputTooLarge));
}

#[test]
fn invalid_schema_never_exposes_a_backend_error() {
    let invalid = Command::new("fastled").option(OptionSpec::value(
        "mode",
        ValueKind::enumeration(Vec::<String>::new()),
    ));
    assert_eq!(invalid.parse(["fastled"]), Err(CommandError::InvalidSchema));
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::flag("bad\0name"))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::flag("help"))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::flag("-bad"))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::flag("flag").default("true"))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::flag("flag").optional_value("true"))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::value("timeout", ValueKind::f64()).default("never"))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
    assert_eq!(
        Command::new("fastled")
            .option(OptionSpec::flag("same"))
            .subcommand(Command::new("child").option(OptionSpec::flag("same")))
            .parse(["fastled"]),
        Err(CommandError::InvalidSchema)
    );
}

#[test]
fn root_and_sibling_subcommands_only_collect_selected_schema_values() {
    let schema = Command::new("fastled")
        .option(OptionSpec::flag("quick"))
        .subcommand(Command::new("one").option(OptionSpec::value("first", ValueKind::string())))
        .subcommand(Command::new("two").option(OptionSpec::value("second", ValueKind::string())));
    let root = schema.parse(["fastled", "--quick"]).unwrap();
    assert_eq!(root.flag("quick"), Some(true));
    assert_eq!(root.value("first"), None);
    let selected = schema
        .parse(["fastled", "two", "--second", "value"])
        .unwrap();
    assert_eq!(selected.command_path(), ["fastled", "two"]);
    assert_eq!(selected.value("second"), Some("value"));
    assert_eq!(selected.value("first"), None);
}

#[test]
fn optional_and_repeated_values_and_option_relations_are_enforced() {
    let schema = Command::new("fastled")
        .option(
            OptionSpec::value("init", ValueKind::string())
                .optional_value("__init__")
                .conflicts("purge"),
        )
        .option(OptionSpec::flag("purge").conflicts("init"))
        .option(OptionSpec::flag("test"))
        .option(OptionSpec::flag("check"))
        .option(
            OptionSpec::value("test-cmd", ValueKind::string())
                .repeated()
                .requires_any(["test", "check"]),
        )
        .exclusive_group("production-test", ["test", "check"]);

    let parsed = schema
        .parse([
            "fastled",
            "--init",
            "--test",
            "--test-cmd=first",
            "--test-cmd",
            "second",
        ])
        .unwrap();
    assert_eq!(parsed.value("init"), Some("__init__"));
    assert_eq!(
        parsed.values("test-cmd").unwrap(),
        &["first".to_owned(), "second".to_owned()]
    );
    assert_eq!(
        schema.parse(["fastled", "--test-cmd=first"]),
        Err(CommandError::InvalidArguments)
    );
    assert_eq!(
        schema.parse(["fastled", "--test", "--check"]),
        Err(CommandError::InvalidArguments)
    );
    assert_eq!(
        schema.parse(["fastled", "--init", "--purge"]),
        Err(CommandError::InvalidArguments)
    );
}

#[test]
fn defaulted_options_do_not_count_as_explicit_relation_inputs() {
    let schema = Command::new("fastled")
        .option(OptionSpec::flag("test"))
        .option(
            OptionSpec::value("timeout", ValueKind::f64())
                .default("120")
                .requires_any(["test"]),
        );
    assert!(schema.parse(["fastled"]).is_ok());
    assert_eq!(
        schema.parse(["fastled", "--timeout", "10"]),
        Err(CommandError::InvalidArguments)
    );
}

#[test]
fn optional_positionals_do_not_hide_subcommands() {
    let schema = Command::new("fastled")
        .optional_positional("directory", ValueKind::string())
        .subcommand(
            Command::new("toolchain")
                .subcommand(Command::new("activate").positional("package-id", ValueKind::string())),
        );

    let directory = schema.parse(["fastled", "sketch"]).unwrap();
    assert_eq!(directory.value("directory"), Some("sketch"));
    assert_eq!(directory.command_path(), ["fastled"]);

    let nested = schema
        .parse(["fastled", "toolchain", "activate", "wasm-3.1"])
        .unwrap();
    assert_eq!(nested.command_path(), ["fastled", "toolchain", "activate"]);
    assert_eq!(nested.value("directory"), None);
    assert_eq!(nested.value("package-id"), Some("wasm-3.1"));
    assert_eq!(
        schema.parse(["fastled", "toolchain", "activate"]),
        Err(CommandError::InvalidArguments)
    );
}

#[test]
fn typed_scalars_keep_their_types_and_reject_invalid_input() {
    let schema = Command::new("fastled")
        .option(OptionSpec::value("timeout", ValueKind::f64()).default("120"))
        .option(OptionSpec::value("count", ValueKind::u32()));

    let parsed = schema
        .parse(["fastled", "--timeout", "1.5", "--count", "10"])
        .unwrap();
    assert_eq!(parsed.f64("timeout"), Some(1.5));
    assert_eq!(parsed.u32("count"), Some(10));
    assert_eq!(
        schema.parse(["fastled", "--timeout", "not-a-float"]),
        Err(CommandError::InvalidArguments)
    );
    assert_eq!(
        schema.parse(["fastled", "--timeout", "NaN"]),
        Err(CommandError::InvalidArguments)
    );
    assert_eq!(
        schema.parse(["fastled", "--timeout", "inf"]),
        Err(CommandError::InvalidArguments)
    );
    assert_eq!(
        schema.parse(["fastled", "--count", "-1"]),
        Err(CommandError::InvalidArguments)
    );
}

#[test]
fn help_is_rendered_without_exposing_the_parser_backend() {
    let schema = Command::new("fastled")
        .about("FastLED WASM compilation CLI")
        .option(OptionSpec::flag("quick").help("Build quickly."))
        .option(
            OptionSpec::value("link", ValueKind::enumeration(["static", "dynamic"]))
                .help("Select static or dynamic linking."),
        )
        .subcommand(Command::new("source").about("Manage cached source."));

    let help = schema.render_help();
    assert!(help.contains("Usage: fastled [OPTIONS] [COMMAND]"));
    assert!(help.contains("FastLED WASM compilation CLI"));
    assert!(help.contains("--quick"));
    assert!(help.contains("Select static or dynamic linking."));
    assert!(help.contains("source"));
    assert!(!help.contains("clap"));
}

#[test]
fn hidden_options_parse_but_are_absent_from_help_and_double_dash_is_literal() {
    let schema = Command::new("fastled")
        .option(OptionSpec::flag("internal").hidden())
        .optional_positional("directory", ValueKind::string());

    let hidden = schema.parse(["fastled", "--internal"]).unwrap();
    assert_eq!(hidden.flag("internal"), Some(true));
    assert!(!schema.render_help().contains("internal"));

    let literal = schema.parse(["fastled", "--", "--not-an-option"]).unwrap();
    assert_eq!(literal.value("directory"), Some("--not-an-option"));
}

#[test]
fn version_is_rendered_from_facade_owned_metadata() {
    let schema = Command::new("fastled").version("2.0.20");
    assert_eq!(schema.render_version(), "fastled 2.0.20\n");
}

#[cfg(unix)]
#[test]
fn non_utf8_native_arguments_require_an_os_string_schema_value() {
    use std::os::unix::ffi::OsStringExt;

    let string_schema =
        Command::new("fastled").optional_positional("directory", ValueKind::string());
    assert_eq!(
        string_schema.parse([
            std::ffi::OsString::from("fastled"),
            std::ffi::OsString::from_vec(vec![0xff])
        ]),
        Err(CommandError::InvalidArguments)
    );

    let os_schema =
        Command::new("fastled").optional_positional("directory", ValueKind::os_string());
    let parsed = os_schema
        .parse([
            std::ffi::OsString::from("fastled"),
            std::ffi::OsString::from_vec(vec![0xff]),
        ])
        .unwrap();
    assert_eq!(
        parsed.os_value("directory").unwrap().as_encoded_bytes(),
        [0xff]
    );

    let guarded = Command::new("fastled")
        .option(OptionSpec::flag("enabled"))
        .option(
            OptionSpec::value("path", ValueKind::os_string())
                .repeated()
                .requires_any(["enabled"]),
        );
    let inline = std::ffi::OsString::from_vec(b"--path=\xff".to_vec());
    assert_eq!(
        guarded.parse([std::ffi::OsString::from("fastled"), inline.clone()]),
        Err(CommandError::InvalidArguments)
    );
    let parsed = guarded
        .parse([
            std::ffi::OsString::from("fastled"),
            std::ffi::OsString::from("--enabled"),
            inline,
        ])
        .unwrap();
    assert_eq!(
        parsed.os_values("path").unwrap()[0].as_encoded_bytes(),
        b"\xff"
    );
}
