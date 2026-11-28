#![cfg(feature = "source-cpp")]

use kernal_api::source::{analyze_cpp, AnalysisError};

#[test]
fn extracts_definitions_without_arduino_policy() {
    let functions =
        analyze_cpp("void setup() {}\nint helper(int value = 1) { return value; }").unwrap();
    assert_eq!(functions.len(), 2);
    assert_eq!(functions[0].signature, "void setup()");
    assert_eq!(functions[1].signature, "int helper(int value)");
    assert!(!functions[1].context.namespace);
}

#[test]
fn preserves_scope_and_linkage_for_consumer_selection() {
    let functions = analyze_cpp(
        "namespace n { void f() {} }\nstruct S { void m() {} };\nextern \"C\" { void hook() {} }",
    )
    .unwrap();
    assert_eq!(functions.len(), 3);
    assert!(functions[0].context.namespace);
    assert!(functions[1].context.aggregate);
    assert!(functions[2].context.explicit_linkage);
}

#[test]
fn rejects_incomplete_syntax_without_partial_results() {
    assert_eq!(
        analyze_cpp("void good() {}\nvoid bad("),
        Err(AnalysisError::InvalidSyntax)
    );
}

#[test]
fn strips_syntax_defaults_not_commas_inside_expressions() {
    let functions = analyze_cpp(
        r#"int helper(int x = (1 < 2 ? 3 : 4), const char* text = "a,b") { return x; }"#,
    )
    .unwrap();
    assert_eq!(
        functions[0].signature,
        "int helper(int x, const char* text)"
    );
    let functions =
        analyze_cpp("template <typename T>\nT f(T value = T{}) { return value; }").unwrap();
    assert_eq!(
        functions[0].signature,
        "template <typename T>\nT f(T value)"
    );
}

#[test]
fn enforces_source_and_traversal_bounds() {
    use kernal_api::source::{MAX_DEPTH, MAX_SOURCE_BYTES};
    assert_eq!(
        analyze_cpp(&" ".repeat(MAX_SOURCE_BYTES + 1)),
        Err(AnalysisError::InputTooLarge)
    );
    let source = format!(
        "{}void f() {{}}{}",
        "namespace n {".repeat(MAX_DEPTH + 1),
        "}".repeat(MAX_DEPTH + 1)
    );
    assert_eq!(analyze_cpp(&source), Err(AnalysisError::LimitExceeded));
}

#[test]
fn preserves_line_comment_terminators_when_removing_defaults() {
    let functions = analyze_cpp(
        "int f(int x // keep parameter comment\n = 1) // keep header comment\n { return x; }",
    )
    .unwrap();
    assert_eq!(
        functions[0].signature,
        "int f(int x // keep parameter comment\n) // keep header comment\n"
    );
    // Appending a declaration terminator must not put it inside a comment.
    let declaration = format!("{};", functions[0].signature);
    assert_eq!(analyze_cpp(&declaration).unwrap(), vec![]);
}

#[test]
fn retains_attributes_function_pointer_parameters_and_source_ranges() {
    let source =
        "// prefix\n[[nodiscard]] int f(int (*callback)(int) = nullptr) noexcept { return 1; }";
    let functions = analyze_cpp(source).unwrap();
    assert_eq!(
        functions[0].signature,
        "[[nodiscard]] int f(int (*callback)(int)) noexcept"
    );
    assert_eq!(
        &source[functions[0].source_range.clone()],
        "[[nodiscard]] int f(int (*callback)(int) = nullptr) noexcept "
    );
}

#[test]
fn multiple_defaults_with_lambdas_and_braced_values_keep_parameter_order() {
    let functions = analyze_cpp(
        "void f(int x = [] { return 1; }(), Pair y = Pair{1, 2}, const char* z = \"é,終\") {}",
    )
    .unwrap();
    assert_eq!(
        functions[0].signature,
        "void f(int x, Pair y, const char* z)"
    );
}
