//! Explicit unsupported status area; clients keep a visible fallback.
use crate::system_tray::{TrayError, TrayEvent, TrayOptions};
pub(crate) struct Handle;
pub(crate) async fn register(options: TrayOptions) -> Result<Handle, TrayError> {
    let _ = (options.id, options.title, options.size, options.argb, options.labels);
    Err(TrayError::Unsupported)
}
impl Handle {
    pub(crate) fn is_online(&self) -> bool {
        false
    }
    pub(crate) fn try_event(&self) -> Option<TrayEvent> {
        None
    }
    pub(crate) async fn set_title(&self, _title: &str) -> Result<(), TrayError> {
        Err(TrayError::Unsupported)
    }
}
