//! Desktop status-area presence without exposing toolkit or D-Bus types.

use crate::native_system_tray;

/// A bounded square ARGB32 icon and application identity.
#[derive(Clone, Debug)]
pub struct TrayOptions {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) size: u16,
    pub(crate) argb: Vec<u8>,
    pub(crate) labels: TrayLabels,
}

/// The words the desktop shows for the item: its menu entries and tooltip line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayLabels {
    pub(crate) activate: String,
    pub(crate) open_dashboard: String,
    pub(crate) quit: String,
    pub(crate) description: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self {
            activate: "Show activity".into(),
            open_dashboard: "Open dashboard".into(),
            quit: "Quit".into(),
            description: "Click to show CI activity".into(),
        }
    }
}

fn valid_label(text: &str) -> bool {
    !text.is_empty() && text.len() <= 255 && !text.contains('\0')
}

impl TrayOptions {
    /// Validate all data before contacting the desktop. Icons are at most 128².
    pub fn new(id: &str, title: &str, size: u16, argb: Vec<u8>) -> Result<Self, TrayError> {
        if id.is_empty()
            || id.len() > 255
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || title.is_empty()
            || title.len() > 255
            || title.contains('\0')
            || !(1..=128).contains(&size)
            || argb.len() != usize::from(size).pow(2) * 4
        {
            return Err(TrayError::InvalidOptions);
        }
        Ok(Self {
            id: id.into(),
            title: title.into(),
            size,
            argb,
            labels: TrayLabels::default(),
        })
    }

    /// Name the three menu entries after what the caller does with
    /// [`TrayEvent::Activate`], [`TrayEvent::OpenDashboard`] and
    /// [`TrayEvent::Quit`], and set the tooltip's description line. The events
    /// stay the same; only the words change. Each is 1-255 bytes without NUL.
    pub fn with_labels(
        mut self,
        activate: &str,
        open_dashboard: &str,
        quit: &str,
        description: &str,
    ) -> Result<Self, TrayError> {
        if ![activate, open_dashboard, quit, description]
            .into_iter()
            .all(valid_label)
        {
            return Err(TrayError::InvalidOptions);
        }
        self.labels = TrayLabels {
            activate: activate.into(),
            open_dashboard: open_dashboard.into(),
            quit: quit.into(),
            description: description.into(),
        };
        Ok(self)
    }
}

/// Registration is optional; callers retain their existing visible fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayError {
    InvalidOptions,
    Unsupported,
    Unavailable,
}

/// A deliberate user action. Native callbacks never open windows themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Activate,
    OpenDashboard,
    Quit,
}

/// Owned tray registration. Dropping it unregisters the item.
pub struct TrayHandle {
    native: native_system_tray::Handle,
}
impl TrayHandle {
    /// Register on the existing facade async runtime without blocking the UI thread.
    /// Fails within two seconds; unsupported desktops retain a caller-owned fallback.
    pub async fn register(options: TrayOptions) -> Result<Self, TrayError> {
        native_system_tray::register(options)
            .await
            .map(|native| Self { native })
    }
    /// Whether a desktop host can currently display the item.
    pub fn is_online(&self) -> bool {
        self.native.is_online()
    }
    /// Take one buffered action; the callback queue is bounded to 16 actions.
    pub fn try_event(&self) -> Option<TrayEvent> {
        self.native.try_event()
    }
    /// Update the displayed status; icon dimensions and identity stay unchanged.
    pub async fn set_title(&self, title: &str) -> Result<(), TrayError> {
        if !valid_label(title) {
            return Err(TrayError::InvalidOptions);
        }
        self.native.set_title(title).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_default_to_the_original_words_and_are_validated() {
        let options = TrayOptions::new("a.b", "T", 1, vec![0; 4]).unwrap();
        assert_eq!(options.labels, TrayLabels::default());
        let named = options
            .clone()
            .with_labels("Search", "Tasks", "Quit bar", "Talos")
            .unwrap();
        assert_eq!(named.labels.activate, "Search");
        assert_eq!(named.labels.open_dashboard, "Tasks");
        assert_eq!(named.labels.quit, "Quit bar");
        assert_eq!(named.labels.description, "Talos");
        assert_eq!(
            options.clone().with_labels("", "b", "c", "d").err(),
            Some(TrayError::InvalidOptions)
        );
        assert_eq!(
            options.with_labels("a", "b\0", "c", "d").err(),
            Some(TrayError::InvalidOptions)
        );
    }
}
