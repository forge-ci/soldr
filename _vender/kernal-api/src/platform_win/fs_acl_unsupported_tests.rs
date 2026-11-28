//! Verify that an unimplemented held-file ACL query fails closed.

#[test]
fn unimplemented_acl_query_is_explicitly_unsupported() {
    let file = tempfile::tempfile().expect("temporary file");
    assert_eq!(
        super::ensure_no_extended_acl(&file)
            .expect_err("unimplemented host must not claim ACL absence")
            .kind(),
        std::io::ErrorKind::Unsupported
    );
}
