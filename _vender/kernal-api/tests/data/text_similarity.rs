#![cfg(feature = "text-similarity")]

use kernal_api::text::{name_similarity, SimilarityError};

#[test]
fn known_scores_and_unicode_semantics() {
    assert!((name_similarity("MARTHA", "MARHTA").unwrap() - 0.9611111111111111).abs() < 1e-12);
    assert_eq!(name_similarity("", "").unwrap(), 1.0);
    assert_eq!(name_similarity("", "x").unwrap(), 0.0);
    assert_eq!(name_similarity("é🦀", "é🦀").unwrap(), 1.0);
    assert_eq!(name_similarity("abc", "XYZ").unwrap(), 0.0);
    assert!(name_similarity("é", "e\u{301}").unwrap() < 1.0);
}

#[test]
fn rejects_storage_and_comparison_work_before_scoring() {
    assert_eq!(
        name_similarity(&"x".repeat(4097), "x"),
        Err(SimilarityError::InputTooLarge)
    );
    assert_eq!(
        name_similarity(&"x".repeat(1025), &"y".repeat(1024)),
        Err(SimilarityError::WorkLimitExceeded)
    );
    assert!(name_similarity(&"🦀".repeat(1024), "🦀").unwrap() > 0.0);
    assert_eq!(
        name_similarity(&"x".repeat(1024), &"x".repeat(1024)).unwrap(),
        1.0
    );
    assert_eq!(name_similarity(&"x".repeat(4096), "").unwrap(), 0.0);
}

#[test]
fn scores_are_finite_normalized_and_symmetric_for_representative_names() {
    for left in ["", "Blink", "blnk", "Fire2012", "é🦀", "e\u{301}"] {
        for right in ["", "Blink", "blnk", "Fire2012", "é🦀", "e\u{301}"] {
            let score = name_similarity(left, right).unwrap();
            assert!(score.is_finite() && (0.0..=1.0).contains(&score));
            assert!((score - name_similarity(right, left).unwrap()).abs() < 1e-12);
        }
    }
}
