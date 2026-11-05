//! Global key capture — the push-to-talk + shortcut-recorder event source.
//!
//! Wispr Flow has no hotkey detection of its own: its renderer "Keyboard
//! Service" is fed entirely by `KeypressEvent` IPC frames from the helper (the
//! macOS/Windows helpers supply them via OS key hooks). Without that stream
//! push-to-talk never fires and the in-app shortcut recorder captures nothing —
//! the two symptoms share one cause. This module produces the stream.
//!
//! Two backends, selected like [`crate::backend::detect`]:
//!
//! - [`xinput`] — XInput2 raw key events on a **true X11** session. Needs no
//!   device access (no root, no `input` group), works across every X11 WM/DE.
//!   Not used on Wayland: under XWayland, raw events only cover XWayland's own
//!   surfaces, not global input.
//! - [`evdev`] — reads `/dev/input/event*` directly, **below** the display
//!   server, so it works identically on Wayland and X11. Needs read access to
//!   the input devices (logind `uaccess` ACL or the `input` group).
//!
//! Both translate each press/release to the Windows Virtual-Key code the app
//! expects (`keymap::evdev_to_vk`) — XInput2 keycodes are evdev codes + 8, so
//! both paths converge on the same VK and the same emission code here.

mod evdev;
mod synergy;
mod xinput;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use crate::backend::EventSink;

/// Query the keys physically held right now (Windows VK codes). Used to answer
/// `CheckStaleKeys`: a key the app believes is down but that is absent here has
/// been released (or its device removed) and is stale. The implementation
/// matches the chosen capture backend (evdev `EVIOCGKEY` vs X11 `QueryKeymap`).
pub trait HeldKeys {
    fn held_vks(&self) -> HashSet<u32>;
}

/// A held-keys querier that always reports nothing — used when no capture
/// backend is available (every queried key then reads as stale, which is the
/// safe answer: the app drops keys it can't confirm are held).
struct NoHeldKeys;
impl HeldKeys for NoHeldKeys {
    fn held_vks(&self) -> HashSet<u32> {
        HashSet::new()
    }
}

/// Start global key capture. Spawns the reader(s) and returns a [`HeldKeys`]
/// handle for stale-key queries. Prefers the no-privilege XInput2 path on a true
/// X11 session, falling back to evdev (Wayland, or X11 where XInput2 fails).
pub fn spawn(events: EventSink) -> Box<dyn HeldKeys> {
    let events = synergy::route(events);
    if is_true_x11_session() {
        match xinput::start(events.clone()) {
            Ok(held) => {
                log::info!("key capture: XInput2 (X11, no device access needed)");
                return held;
            }
            Err(e) => log::warn!("key capture: XInput2 unavailable ({e}); trying evdev"),
        }
    }
    match evdev::start(events) {
        Some(held) => {
            log::info!("key capture: evdev (/dev/input)");
            held
        }
        None => Box::new(NoHeldKeys),
    }
}

/// True on an X11 session but not Wayland. On Wayland `DISPLAY` is usually also
/// set (XWayland), so require `WAYLAND_DISPLAY` to be absent.
fn is_true_x11_session() -> bool {
    std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

/// Emit one `KeypressEvent` on fd 3. `index` is a process-wide monotonic
/// sequence the app cross-checks against its own counter (it warns on a gap), so
/// every backend shares a single counter regardless of how many readers feed it.
fn emit_keypress(events: &EventSink, index: &AtomicU64, pid: u32, vk: u32, press: bool) {
    let idx = index.fetch_add(1, Ordering::Relaxed) + 1;
    let env = crate::proto::request(
        "KeypressEvent",
        json!({ "payload": {
            "eventType": if press { "key_event_press" } else { "key_event_release" },
            "key": vk,
            "index": idx,
            "inputType": "keyboard",
        } }),
        &format!("kp-{pid}-{idx}"),
    );
    let _ = events.send(env);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact `KeypressEvent` frame shape is the contract the app's keyboard
    // service decodes; pin it so silent protocol drift fails the build.
    #[test]
    fn emit_keypress_builds_keypress_event_frame() {
        let (tx, rx) = std::sync::mpsc::channel();
        let index = AtomicU64::new(0);

        emit_keypress(&tx, &index, 4242, 65, true);
        emit_keypress(&tx, &index, 4242, 65, false);

        let press = rx.recv().expect("press frame");
        let kp = &press["HelperAPIRequest"]["KeypressEvent"]["payload"];
        assert_eq!(kp["eventType"], "key_event_press");
        assert_eq!(kp["key"], 65);
        assert_eq!(kp["index"], 1); // counter starts at 1, not 0
        assert_eq!(kp["inputType"], "keyboard");
        assert_eq!(press["HelperAPIRequest"]["uuid"], "kp-4242-1");

        let release = rx.recv().expect("release frame");
        let kp = &release["HelperAPIRequest"]["KeypressEvent"]["payload"];
        assert_eq!(kp["eventType"], "key_event_release");
        assert_eq!(kp["index"], 2); // shared monotonic counter advances
        assert_eq!(release["HelperAPIRequest"]["uuid"], "kp-4242-2");
    }

    // XInput2 is chosen only on a true X11 session: DISPLAY set and
    // WAYLAND_DISPLAY absent (under XWayland DISPLAY is also set, so its presence
    // alone must not select the X11 path).
    #[test]
    fn true_x11_requires_display_without_wayland() {
        // Snapshot + restore so the test leaves the process env untouched. No
        // other test reads these vars, so owning them here is race-free.
        let saved_display = std::env::var_os("DISPLAY");
        let saved_wayland = std::env::var_os("WAYLAND_DISPLAY");
        let restore = |key: &str, val: &Option<std::ffi::OsString>| match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        };

        // X11: DISPLAY set, no WAYLAND_DISPLAY.
        std::env::set_var("DISPLAY", ":0");
        std::env::remove_var("WAYLAND_DISPLAY");
        assert!(is_true_x11_session());

        // Wayland with XWayland: both set -> not a true X11 session.
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
        assert!(!is_true_x11_session());

        // Headless: neither set.
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        assert!(!is_true_x11_session());

        restore("DISPLAY", &saved_display);
        restore("WAYLAND_DISPLAY", &saved_wayland);
    }
}
