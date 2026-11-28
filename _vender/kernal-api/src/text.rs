//! Bounded text comparison without application-specific normalization or ranking.

/// Maximum UTF-8 bytes in either argument to [`name_similarity`].
pub const MAX_SIMILARITY_INPUT_BYTES: usize = 4096;
/// Maximum product of the two Unicode scalar counts.
pub const MAX_SIMILARITY_WORK: usize = 1_048_576;

/// Resource rejection before invoking the private scoring implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SimilarityError {
    #[error("similarity input exceeds the byte limit")]
    InputTooLarge,
    #[error("similarity comparison exceeds the work limit")]
    WorkLimitExceeded,
}

/// Score two names using Jaro-Winkler similarity, from 0 (unrelated) to 1 (equal).
///
/// Operates on Unicode scalar values, not bytes or grapheme clusters. It performs
/// no case folding or Unicode normalization. Both empty inputs score 1; exactly
/// one empty input scores 0. Callers own candidate selection and tie policy.
///
/// Both byte limits and the scalar-count product limit are checked before the
/// backend allocates matching flags or scores, including for equal inputs.
/// This bounds quadratic comparison work and linear scratch storage, not elapsed
/// time. This synchronous CPU operation has no cancellation or runtime dependency.
pub fn name_similarity(left: &str, right: &str) -> Result<f64, SimilarityError> {
    if left.len() > MAX_SIMILARITY_INPUT_BYTES || right.len() > MAX_SIMILARITY_INPUT_BYTES {
        return Err(SimilarityError::InputTooLarge);
    }
    // Byte limits also bound scalar counts, so this product cannot overflow.
    if left.chars().count() * right.chars().count() > MAX_SIMILARITY_WORK {
        return Err(SimilarityError::WorkLimitExceeded);
    }
    Ok(strsim::jaro_winkler(left, right))
}

#[cfg(test)]
mod error_message_tests {
    use super::SimilarityError;

    #[test]
    fn messages_remain_stable() {
        assert_eq!(
            SimilarityError::InputTooLarge.to_string(),
            "similarity input exceeds the byte limit"
        );
        assert_eq!(
            SimilarityError::WorkLimitExceeded.to_string(),
            "similarity comparison exceeds the work limit"
        );
    }
}
