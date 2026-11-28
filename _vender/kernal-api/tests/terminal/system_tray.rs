#![cfg(feature = "system-tray")]
use kernal_api::system_tray::{TrayError, TrayOptions};
#[test]
fn invalid_icon_is_rejected_before_native_registration() {
    let result = TrayOptions::new("dev.test.tray", "Test", 16, vec![0; 3]);
    assert!(matches!(result, Err(TrayError::InvalidOptions)));
}
#[test]
fn valid_icon_is_bounded_and_typed() {
    assert!(TrayOptions::new("dev.test.tray", "Test", 16, vec![255; 1024]).is_ok());
    assert!(TrayOptions::new("dev.test.tray", "Test", 1024, vec![]).is_err());
}
