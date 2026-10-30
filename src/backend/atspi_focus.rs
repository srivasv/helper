//! Field focus tracking, independent of compositor app identification.
use atspi::connection::AccessibilityConnection;
use atspi::events::focus::{FocusEvent, FocusEvents};
use atspi::events::object::{ObjectEvents, StateChangedEvent};
use atspi::{Event, ObjectRef, State};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::export::futures_util::StreamExt;

pub static LAST_FOCUSED_WIDGET: Mutex<Option<ObjectRef>> = Mutex::new(None);

pub struct FocusTracker(Arc<AtomicBool>);
impl Drop for FocusTracker {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn start() -> Option<FocusTracker> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let result = std::thread::Builder::new()
        .name("atspi-field-focus".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match result {
                Ok(rt) => rt.block_on(async {
                    if let Err(e) = watch(&thread_stop).await {
                        log::warn!("AT-SPI field focus unavailable; using bounded search: {e}");
                    }
                }),
                Err(e) => log::warn!("AT-SPI field focus runtime: {e}"),
            }
        });
    match result {
        Ok(_) => Some(FocusTracker(stop)),
        Err(e) => {
            log::warn!("AT-SPI field focus thread: {e}");
            None
        }
    }
}

async fn watch(stop: &AtomicBool) -> Result<(), Box<dyn std::error::Error>> {
    let _ = atspi::connection::set_session_accessibility(true).await;
    let a11y = AccessibilityConnection::new().await?;
    a11y.register_event::<StateChangedEvent>().await?;
    // Qt emits the legacy focus event rather than GTK's focused state change.
    a11y.register_event::<FocusEvent>().await?;
    let mut events = std::pin::pin!(a11y.event_stream());
    log::debug!("atspi_focus: listening");
    while !stop.load(Ordering::Relaxed) {
        match tokio::time::timeout(Duration::from_millis(500), events.next()).await {
            Ok(Some(Ok(Event::Object(ObjectEvents::StateChanged(e)))))
                if e.state == State::Focused =>
            {
                if let Ok(mut cached) = LAST_FOCUSED_WIDGET.lock() {
                    update(&mut cached, e.item, e.enabled == 1);
                }
            }
            Ok(Some(Ok(Event::Focus(FocusEvents::Focus(e))))) => {
                if let Ok(mut cached) = LAST_FOCUSED_WIDGET.lock() {
                    update(&mut cached, e.item, true);
                }
            }
            Ok(None) => break,
            _ => {}
        }
    }
    Ok(())
}

fn update(cached: &mut Option<ObjectRef>, source: ObjectRef, gained: bool) {
    if gained {
        *cached = Some(source);
    } else if cached.as_ref() == Some(&source) {
        *cached = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_blur_does_not_clear_new_focus() {
        let first = ObjectRef {
            name: ":1.42".try_into().unwrap(),
            path: "/first".try_into().unwrap(),
        };
        let second = ObjectRef {
            name: first.name.clone(),
            path: "/second".try_into().unwrap(),
        };
        let mut cached = None;
        update(&mut cached, first.clone(), true);
        update(&mut cached, second.clone(), true);
        update(&mut cached, first, false);
        assert_eq!(cached, Some(second.clone()));
        update(&mut cached, second, false);
        assert_eq!(cached, None);
    }
}
