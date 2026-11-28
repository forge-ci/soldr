//! Owned application error context with bounded display text.

use std::error::Error as StdError;
use std::fmt::{self, Display, Formatter, Write as _};

/// Maximum UTF-8 bytes retained for one error context message.
pub const MAX_CONTEXT_BYTES: usize = 8 * 1024;

/// Context wrapper used by [`Context`] to retain an underlying error.
#[derive(Debug)]
struct ContextError {
    message: String,
    source: Error,
}

impl ContextError {
    fn new<E>(message: impl Display, source: E) -> Self
    where
        E: Into<Error>,
    {
        Self {
            message: bounded(message),
            source: source.into(),
        }
    }
}

impl Display for ContextError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)?;
        if formatter.alternate() {
            let mut source: Option<&(dyn StdError + 'static)> = Some(self.source.as_ref());
            while let Some(error) = source {
                write!(formatter, ": {error}")?;
                source = error.source();
            }
        }
        Ok(())
    }
}

impl StdError for ContextError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Facade-owned application error type. Its standard conversion support lets
/// callers use `?` with any thread-safe standard error.
pub type Error = Box<dyn StdError + Send + Sync + 'static>;

/// Construct a message-only error, truncating excessively long text.
pub fn message(message: impl Display) -> Error {
    Box::new(MessageError(bounded(message)))
}

#[derive(Debug)]
struct MessageError(String);

impl Display for MessageError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for MessageError {}

/// Facade-owned application result alias. The optional second parameter keeps
/// ordinary `Result<T, E>` signatures source-compatible during migration.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Add bounded context to a standard fallible result.
pub trait Context<T> {
    /// Attach a context message only if the result is an error.
    fn context(self, message: impl Display) -> Result<T>;
    /// Lazily attach a context message only if the result is an error.
    fn with_context<F, M>(self, message: F) -> Result<T>
    where
        F: FnOnce() -> M,
        M: Display;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: Into<Error>,
{
    fn context(self, message: impl Display) -> Result<T> {
        self.map_err(|source| Box::new(ContextError::new(message, source)) as Error)
    }

    fn with_context<F, M>(self, message: F) -> Result<T>
    where
        F: FnOnce() -> M,
        M: Display,
    {
        self.map_err(|source| Box::new(ContextError::new(message(), source)) as Error)
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl Display) -> Result<T> {
        self.ok_or_else(|| crate::error::message(message))
    }

    fn with_context<F, M>(self, message: F) -> Result<T>
    where
        F: FnOnce() -> M,
        M: Display,
    {
        self.ok_or_else(|| crate::error::message(message()))
    }
}

fn bounded(message: impl Display) -> String {
    let mut output = BoundedText(String::with_capacity(MAX_CONTEXT_BYTES));
    let _ = write!(&mut output, "{message}");
    output.0
}

struct BoundedText(String);

impl fmt::Write for BoundedText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let remaining = MAX_CONTEXT_BYTES.saturating_sub(self.0.len());
        let mut end = remaining.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&text[..end]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_retains_source_and_bounds_display() {
        let error = Err::<(), _>(std::io::Error::other("source detail"))
            .context("opening test file")
            .unwrap_err();
        assert_eq!(error.to_string(), "opening test file");
        assert!(error.source().is_some());
        assert_eq!(
            message("x".repeat(MAX_CONTEXT_BYTES + 1)).to_string().len(),
            MAX_CONTEXT_BYTES
        );
        assert!(format!("{error:#}").contains("source detail"));
    }
}
