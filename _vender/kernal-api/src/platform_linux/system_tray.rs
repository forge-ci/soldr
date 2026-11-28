//! KDE/freedesktop StatusNotifierItem backend on the caller's async runtime.
use crate::system_tray::{TrayError, TrayEvent, TrayOptions};
use ksni::TrayMethods as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
};

pub(crate) struct Item {
    options: TrayOptions,
    actions: SyncSender<TrayEvent>,
    online: Arc<AtomicBool>,
}
impl Item {
    fn action(&self, event: TrayEvent) {
        let _ = self.actions.try_send(event);
    }
}
impl ksni::Tray for Item {
    fn id(&self) -> String {
        self.options.id.clone()
    }
    fn title(&self) -> String {
        self.options.title.clone()
    }
    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon {
            width: i32::from(self.options.size),
            height: i32::from(self.options.size),
            data: self.options.argb.clone(),
        }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.options.title.clone(),
            description: self.options.labels.description.clone(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.action(TrayEvent::Activate);
    }
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.action(TrayEvent::OpenDashboard);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem {
                label: self.options.labels.activate.clone(),
                activate: Box::new(|item: &mut Self| item.action(TrayEvent::Activate)),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: self.options.labels.open_dashboard.clone(),
                activate: Box::new(|item: &mut Self| item.action(TrayEvent::OpenDashboard)),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: self.options.labels.quit.clone(),
                activate: Box::new(|item: &mut Self| item.action(TrayEvent::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
    fn watcher_online(&self) {
        self.online.store(true, Ordering::Release);
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.online.store(false, Ordering::Release);
        true // Keep the registration alive so a restarted shell can reclaim it.
    }
}
pub(crate) struct Handle {
    inner: ksni::Handle<Item>,
    actions: Mutex<Receiver<TrayEvent>>,
    online: Arc<AtomicBool>,
}
pub(crate) async fn register(options: TrayOptions) -> Result<Handle, TrayError> {
    crate::async_engine::RuntimeHandle::current().map_err(|_| TrayError::Unavailable)?;
    let (sender, actions) = mpsc::sync_channel(16);
    let online = Arc::new(AtomicBool::new(true));
    let item = Item {
        options,
        actions: sender,
        online: online.clone(),
    };
    let inner = crate::async_engine::timeout(std::time::Duration::from_secs(2), item.spawn())
        .await
        .map_err(|_| TrayError::Unavailable)?
        .map_err(|_| TrayError::Unavailable)?;
    Ok(Handle {
        inner,
        actions: Mutex::new(actions),
        online,
    })
}
impl Handle {
    pub(crate) fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire) && !self.inner.is_closed()
    }
    pub(crate) fn try_event(&self) -> Option<TrayEvent> {
        self.actions.lock().ok()?.try_recv().ok()
    }
    pub(crate) async fn set_title(&self, title: &str) -> Result<(), TrayError> {
        let title = title.to_owned();
        crate::async_engine::timeout(
            std::time::Duration::from_secs(2),
            self.inner.update(|item| item.options.title = title),
        )
        .await
        .map_err(|_| TrayError::Unavailable)?
        .ok_or(TrayError::Unavailable)
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        drop(self.inner.shutdown());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray as _;
    #[test]
    fn missing_host_returns_unavailable_on_an_isolated_session_bus() {
        const MARKER: &str = "KERNAL_TEST_TRAY_NO_HOST";
        if std::env::var_os(MARKER).is_none() {
            let status = std::process::Command::new("dbus-run-session")
                .arg("--").arg(std::env::current_exe().unwrap())
                .args(["--exact", "platform_linux::system_tray::tests::missing_host_returns_unavailable_on_an_isolated_session_bus"])
                .env(MARKER, "1").status().unwrap();
            assert!(status.success());
            return;
        }
        let runtime = crate::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = TrayOptions::new("test.tray", "Test", 1, vec![255; 4]).unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            runtime.run_within(crate::system_tray::TrayHandle::register(options), std::time::Duration::from_secs(60)).expect("test runtime budget"),
            Err(TrayError::Unavailable)
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn host_loss_and_recovery_control_visibility_without_queued_events() {
        let (actions, receiver) = mpsc::sync_channel(1);
        let online = Arc::new(AtomicBool::new(true));
        let mut item = Item {
            options: TrayOptions::new("test.tray", "Test", 1, vec![255; 4]).unwrap(),
            actions,
            online: online.clone(),
        };
        item.activate(0, 0);
        item.activate(0, 0); // Full action queue never blocks a desktop callback.
        assert_eq!(receiver.try_recv().unwrap(), TrayEvent::Activate);
        assert!(item.watcher_offline(ksni::OfflineReason::No));
        assert!(!online.load(Ordering::Acquire));
        item.watcher_online();
        assert!(online.load(Ordering::Acquire));
    }
}
