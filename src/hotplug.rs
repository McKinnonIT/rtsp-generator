use std::time::Duration;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::debug;
use udev::{MonitorBuilder, MonitorSocket};

/// USB enumeration can fire multiple udev add/remove events per physical connect/disconnect;
/// wait for this long a quiet period before treating the device set as settled.
const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum HotplugError {
    #[error("failed to set up udev video4linux monitor: {0}")]
    Monitor(#[source] std::io::Error),
}

/// Watches udev for `video4linux` add/remove events and sends a debounced notification on `tx`
/// each time the device set appears to have settled. Carries no event payload: the receiver is
/// expected to re-run device detection and diff against its previously known camera set.
pub async fn watch(tx: mpsc::Sender<()>) -> Result<(), HotplugError> {
    let socket: MonitorSocket = MonitorBuilder::new()
        .map_err(HotplugError::Monitor)?
        .match_subsystem("video4linux")
        .map_err(HotplugError::Monitor)?
        .listen()
        .map_err(HotplugError::Monitor)?;

    let mut async_fd = AsyncFd::new(socket).map_err(HotplugError::Monitor)?;

    loop {
        let saw_event = {
            let mut guard = async_fd
                .readable_mut()
                .await
                .map_err(HotplugError::Monitor)?;
            let mut saw_event = false;
            for event in guard.get_inner().iter() {
                debug!(
                    action = %event.event_type(),
                    device = ?event.device().devnode(),
                    "udev video4linux event"
                );
                saw_event = true;
            }
            guard.clear_ready();
            saw_event
        };

        if !saw_event {
            continue;
        }

        // Debounce: keep draining events until a quiet period passes.
        loop {
            match tokio::time::timeout(DEBOUNCE, async_fd.readable_mut()).await {
                Ok(Ok(mut guard)) => {
                    for _ in guard.get_inner().iter() {}
                    guard.clear_ready();
                }
                Ok(Err(e)) => return Err(HotplugError::Monitor(e)),
                Err(_elapsed) => break,
            }
        }

        if tx.send(()).await.is_err() {
            return Ok(()); // receiver dropped: daemon is shutting down
        }
    }
}
