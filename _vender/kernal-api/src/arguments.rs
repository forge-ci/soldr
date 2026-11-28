//! Bounded argument decoding for tool output, not shell execution.

/// Maximum UTF-8 input size, checked before parsing or allocation.
pub const MAX_POSIX_INPUT_BYTES: usize = 1024 * 1024;
/// Maximum number of decoded arguments returned to a caller.
pub const MAX_POSIX_ARGUMENTS: usize = 16_384;

/// Semantic failures independent of the private parsing implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArgumentParseError {
    #[error("argument source exceeds 1048576 UTF-8 bytes")]
    InputTooLarge,
    #[error("argument source contains a NUL character")]
    ContainsNul,
    #[error("missing closing quote")]
    UnterminatedQuote,
    #[error("argument source exceeds 16384 decoded arguments")]
    TooManyArguments,
}

/// Decode POSIX-style quoting, escapes, continuations and comments into words.
///
/// No shell is started and no variable, tilde, glob, arithmetic or command
/// expansion occurs. Operators are literal text rather than shell grammar.
/// This is not Windows command-line decoding. Empty quoted words are retained;
/// empty or comment-only input returns an empty vector for caller policy.
///
/// Input is limited to [`MAX_POSIX_INPUT_BYTES`] before backend allocation.
/// This also bounds intermediate storage when [`MAX_POSIX_ARGUMENTS`] is
/// exceeded: the word-count check occurs after parsing. Errors never return a
/// truncated list. NUL is rejected because process arguments cannot contain it.
pub fn parse_posix(source: &str) -> Result<Vec<String>, ArgumentParseError> {
    if source.len() > MAX_POSIX_INPUT_BYTES {
        return Err(ArgumentParseError::InputTooLarge);
    }
    if source.contains('\0') {
        return Err(ArgumentParseError::ContainsNul);
    }
    let words = shell_words::split(source).map_err(|_| ArgumentParseError::UnterminatedQuote)?;
    if words.len() > MAX_POSIX_ARGUMENTS {
        return Err(ArgumentParseError::TooManyArguments);
    }
    Ok(words)
}
