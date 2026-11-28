//! macOS terminal window-icon mechanics.

use crate::platform::window_icon::{
    IconError, IconScope, IconSource, IconSupport, IconUnsupportedReason,
};

pub fn icon_support(_scope: IconScope) -> IconSupport {
    IconSupport::Unsupported(IconUnsupportedReason::MacTerminalOwnsWindow)
}

pub fn set_icon(_scope: IconScope, _source: &IconSource) -> Result<(), IconError> {
    Err(IconError::Unsupported(IconUnsupportedReason::NoBackend))
}
