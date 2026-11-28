//! C++ syntax mechanics adapted from FastLED/fbuild's source scanner at
//! 1e75ccf5a4ca922b4d922a6da286b965fac8832d via fastled-wasm. No Arduino policy.

use std::ops::{ControlFlow, Range};
use std::time::{Duration, Instant};
use tree_sitter::{Node, ParseOptions, Parser};

pub const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FUNCTIONS: usize = 16_384;
pub const MAX_VISITED_NODES: usize = 131_072;
pub const MAX_DEPTH: usize = 256;

/// Enclosing syntax contexts, not compiler-resolved semantic scopes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FunctionContext {
    pub namespace: bool,
    pub aggregate: bool,
    pub explicit_linkage: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionDefinition {
    /// Definition header without its body or function parameter defaults.
    /// Formatting and comments are retained. This is not a semantic C++ compiler
    /// and does not promise that every header can become a standalone declaration.
    pub signature: String,
    pub source_range: Range<usize>,
    pub context: FunctionContext,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AnalysisError {
    #[error("C++ source exceeds the byte limit")]
    InputTooLarge,
    #[error("C++ syntax is incomplete or invalid")]
    InvalidSyntax,
    #[error("C++ parsing exceeded its cooperative deadline")]
    TimedOut,
    #[error("C++ analysis exceeded its traversal or output limit")]
    LimitExceeded,
    #[error("C++ definition form is unsupported")]
    UnsupportedDefinition,
    #[error("C++ parser initialization failed")]
    ParserUnavailable,
}

/// Analyze definitions in source order, without deduplication or name filtering.
/// Definitions nested inside a function body are not inventoried.
///
/// The byte limit is checked before parsing. A two-second cooperative parser
/// deadline is not a hard CPU deadline. Traversal depth/node and record limits
/// apply afterward; they are not independent parser-allocation quotas. Failure
/// returns no partial records. Source is never included in errors.
pub fn analyze_cpp(source: &str) -> Result<Vec<FunctionDefinition>, AnalysisError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(AnalysisError::InputTooLarge);
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|_| AnalysisError::ParserUnavailable)?;
    let started = Instant::now();
    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= Duration::from_secs(2) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = source.as_bytes();
    let tree = parser
        .parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .ok_or(AnalysisError::TimedOut)?;
    if tree.root_node().has_error() {
        return Err(AnalysisError::InvalidSyntax);
    }
    let mut remaining = MAX_VISITED_NODES;
    let mut pending = vec![(tree.root_node(), 0, FunctionContext::default())];
    let mut output = Vec::new();
    while let Some((node, depth, mut context)) = pending.pop() {
        charge(&mut remaining, depth)?;
        match node.kind() {
            "namespace_definition" => context.namespace = true,
            "class_specifier" | "struct_specifier" | "union_specifier" => context.aggregate = true,
            "linkage_specification" => context.explicit_linkage = true,
            "function_definition" => {
                if output.len() == MAX_FUNCTIONS {
                    return Err(AnalysisError::LimitExceeded);
                }
                output.push(definition(node, source, context, depth, &mut remaining)?);
                continue;
            }
            _ => {}
        }
        if pending.len().saturating_add(node.child_count()) > remaining {
            return Err(AnalysisError::LimitExceeded);
        }
        for index in (0..node.child_count()).rev() {
            let index = u32::try_from(index).map_err(|_| AnalysisError::LimitExceeded)?;
            if let Some(child) = node.child(index) {
                pending.push((child, depth + 1, context));
            }
        }
    }
    Ok(output)
}

fn charge(remaining: &mut usize, depth: usize) -> Result<(), AnalysisError> {
    if depth > MAX_DEPTH {
        return Err(AnalysisError::LimitExceeded);
    }
    *remaining = remaining
        .checked_sub(1)
        .ok_or(AnalysisError::LimitExceeded)?;
    Ok(())
}

fn definition(
    node: Node<'_>,
    source: &str,
    context: FunctionContext,
    root_depth: usize,
    remaining: &mut usize,
) -> Result<FunctionDefinition, AnalysisError> {
    let header = node
        .parent()
        .filter(|parent| parent.kind() == "template_declaration")
        .unwrap_or(node);
    let body = node
        .child_by_field_name("body")
        .ok_or(AnalysisError::UnsupportedDefinition)?;
    let range = header.start_byte()..body.start_byte();
    let mut removals = Vec::new();
    let mut pending = vec![(node, root_depth)];
    while let Some((part, depth)) = pending.pop() {
        charge(remaining, depth)?;
        if part.start_byte() >= range.end {
            continue;
        }
        if part.kind() == "optional_parameter_declaration" {
            let mut cursor = part.walk();
            let equals = part
                .children(&mut cursor)
                .find(|child| child.kind() == "=")
                .ok_or(AnalysisError::UnsupportedDefinition)?;
            let mut start = equals.start_byte();
            // A newline can terminate a preceding // comment. Removing it
            // would comment out the following comma or closing parenthesis.
            while start > part.start_byte() && matches!(source.as_bytes()[start - 1], b' ' | b'\t')
            {
                start -= 1;
            }
            removals.push(start..part.end_byte());
            continue;
        }
        if pending.len().saturating_add(part.child_count()) > *remaining {
            return Err(AnalysisError::LimitExceeded);
        }
        for index in (0..part.child_count()).rev() {
            let index = u32::try_from(index).map_err(|_| AnalysisError::LimitExceeded)?;
            if let Some(child) = part.child(index) {
                pending.push((child, depth + 1));
            }
        }
    }
    let original = source
        .get(range.clone())
        .ok_or(AnalysisError::UnsupportedDefinition)?;
    // Copy retained spans once. Repeated in-place deletion would make a wide
    // parameter list quadratic even though its source and node count are bounded.
    let mut signature = String::with_capacity(original.len());
    let mut retained_start = range.start;
    for remove in removals {
        if remove.start < retained_start || remove.end > range.end {
            return Err(AnalysisError::UnsupportedDefinition);
        }
        signature.push_str(&source[retained_start..remove.start]);
        retained_start = remove.end;
    }
    signature.push_str(&source[retained_start..range.end]);
    Ok(FunctionDefinition {
        // Likewise keep a trailing newline before the removed function body:
        // the caller may append a semicolon after a line-commented header.
        signature: signature
            .trim_start()
            .trim_end_matches([' ', '\t'])
            .to_owned(),
        source_range: range,
        context,
    })
}
