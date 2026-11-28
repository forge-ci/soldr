//! Neutral window-icon mechanics and host result codes.

use std::path::PathBuf;

pub mod ico;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconScope {
    Host,
    Child { pid: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StockIcon {
    Application,
    Warning,
    Error,
    Information,
    Shield,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSource {
    Path(PathBuf),
    Bytes(Vec<u8>),
    Stock(StockIcon),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconDegradedReason {
    WindowsTerminal,
    NonClassicWindowsHost,
    LinuxNameOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconUnsupportedReason {
    ChildHasNoConsole,
    NoConsole,
    MacTerminalOwnsWindow,
    Wayland,
    NoBackend,
    LinuxChildScope,
    LinuxNoDisplay,
    TargetDisappeared,
    UnknownImageFormat,
    StockNeedsPixels,
    OversizedIcon,
    UnsupportedPngColorType,
    UnsupportedPngBitDepth,
    UnsupportedX11VisualDepth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconSupport {
    Available,
    Degraded(IconDegradedReason),
    Unsupported(IconUnsupportedReason),
}

#[derive(Debug, thiserror::Error)]
pub enum IconError {
    #[error("window-icon operation is unsupported: {0:?}")]
    Unsupported(IconUnsupportedReason),
    #[error("cannot load icon from {path}: {source}")]
    Load {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the host refused the icon data: {0}")]
    Apply(#[source] std::io::Error),
    #[error("supplied icon data is unusable: {0}")]
    Decode(ico::IcoError),
}

pub fn icon_support(scope: IconScope) -> IconSupport {
    crate::window_icon_support_impl(scope)
}

pub fn set_icon(scope: IconScope, source: &IconSource) -> Result<(), IconError> {
    crate::set_window_icon_impl(scope, source)
}
