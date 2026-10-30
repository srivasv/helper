//! OS-integration backend abstraction.
//!
//! One trait, swappable implementations. `x11` is the first target (this file's
//! `detect()` picks it when `$DISPLAY` is set). Wayland (`wayland` + `libei`/
//! `ydotool`) and a no-op `stub` come later. Keeping the protocol/dispatch layer
//! (main.rs, proto.rs) backend-agnostic means new compositors are a new module,
//! not a rewrite.

pub mod atspi_app;
pub mod atspi_edit;
pub mod atspi_focus;
pub mod atspi_sel;
mod atspi_text;
mod copy_probe;
pub mod gnome;
pub mod kwin;
pub mod stub;
pub mod uinput;
pub mod wayland;
pub mod wl_clipboard;
pub mod x11;

pub type Result<T> = std::result::Result<T, String>;

/// Channel a backend uses to push **helper-initiated** events to fd 3 (e.g.
/// `AppInfoUpdate` focus events). A single writer thread owns fd 3 and drains
/// this, so emitting from any thread (e.g. the KWin zbus dispatcher) is safe.
pub type EventSink = std::sync::mpsc::Sender<serde_json::Value>;

/// Result of `GetActiveAppInfo` / `GetAppInfo` (subset we can fill on Linux).
#[derive(Debug, Default, Clone)]
pub struct ActiveApp {
    pub app_name: String,
    /// No real "bundle id" on Linux — we use WM_CLASS / desktop-file id / exe basename.
    pub bundle_id: String,
    pub window_title: String,
    /// Browser URL if derivable (else empty). Not wired yet.
    pub url: String,
}

/// Result of `GetSelectedTextViaCopy`.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub selected_text: String,
    pub before_text: String,
    pub after_text: String,
    pub contents: String,
}

/// A running app entry for `GetRunningApps` (bundleId, name).
#[derive(Debug, Clone)]
pub struct RunningApp {
    pub bundle_id: String,
    pub name: String,
}

pub trait Backend: Send {
    /// `PasteText`: set the clipboard to `text` (+ optional `html`) and synthesize Ctrl+V.
    fn paste_text(&mut self, text: &str, html: Option<&str>) -> Result<()>;

    /// `SimulateKeyPress`: `keycode` is a **Windows VK code** (see keymap.rs); `flags`
    /// are modifier names ("Control"/"Shift"/"Alt"/"Command").
    fn simulate_key_press(&mut self, keycode_vk: u32, flags: &[String]) -> Result<()>;

    /// `GetActiveAppInfo` / `GetAppInfo`.
    fn get_active_app(&mut self) -> Result<ActiveApp>;

    /// `GetRunningApps`.
    fn get_running_apps(&mut self) -> Result<Vec<RunningApp>>;

    /// `GetSelectedTextViaCopy`: copy-based selection probe (save clipboard, Ctrl+C, read, restore).
    fn get_selected_text(&mut self) -> Result<Selection>;

    /// `GetAccessibilityStatus`: whether the accessibility/automation path is usable.
    fn accessibility_status(&mut self) -> bool;

    /// `SetFocusChangeDetectorState`: enable/disable emitting `AppInfoUpdate`
    /// focus events on fd 3. Default no-op for backends without focus tracking.
    fn set_focus_detection(&mut self, active: bool) {
        let _ = active;
    }

    fn name(&self) -> &'static str;
}

/// Build the `{ "payload": { appName, bundleId, windowTitle, url } }` body shared
/// by `ActiveAppInfo`/`AppInfo` responses and `AppInfoUpdate` events.
///
/// `codingCliAgent` / `codingCliAgentConfidence` are OPTIONAL enums; the app's
/// validator rejects a present-but-null optional, so we OMIT them (absent ==
/// undefined == accepted) until terminal/CLI-agent detection lands.
pub fn active_app_payload(a: &ActiveApp) -> serde_json::Value {
    serde_json::json!({ "payload": {
        "appName": a.app_name,
        "bundleId": a.bundle_id,
        "windowTitle": a.window_title,
        "url": a.url,
    } })
}

/// A source of active-window / running-app identity + focus events, chosen
/// **independently of the injection backend** (so active-app works even where
/// injection doesn't, e.g. a GNOME session without `/dev/uinput`).
///
/// Session-specific providers:
///   * Tier-2 premium, per-compositor: `kwin::KwinTracker` (KDE),
///     `gnome::GnomeTracker` (GNOME Shell extension) — richest, most reliable
///     identity (true app-id, full window list, robust focus).
///   * Tier-1 universal baseline: `atspi_app::AtspiTracker` — desktop-agnostic
///     over the accessibility bus; the fallback that covers wlroots
///     (Sway/Hyprland/niri), GNOME pre-relogin, and incomplete-`_NET_*` X11.
///
/// The compositor/accessibility trackers expose `current` / `set_focus_detection` and a
/// running-apps accessor; this trait unifies them (inherent methods win in
/// resolution, so the delegating bodies don't recurse). `KwinTracker` names its
/// accessor `running_apps()`, the others `get_running_apps()` — bridged here.
pub trait ActiveAppProvider: Send {
    fn current(&self) -> Option<ActiveApp>;
    fn get_running_apps(&self) -> Vec<RunningApp>;
    fn set_focus_detection(&self, active: bool);
    fn name(&self) -> &'static str;
}

impl ActiveAppProvider for kwin::KwinTracker {
    fn current(&self) -> Option<ActiveApp> {
        self.current()
    }
    fn get_running_apps(&self) -> Vec<RunningApp> {
        self.running_apps()
    }
    fn set_focus_detection(&self, active: bool) {
        self.set_focus_detection(active)
    }
    fn name(&self) -> &'static str {
        "kwin"
    }
}

impl ActiveAppProvider for gnome::GnomeTracker {
    fn current(&self) -> Option<ActiveApp> {
        self.current()
    }
    fn get_running_apps(&self) -> Vec<RunningApp> {
        self.get_running_apps()
    }
    fn set_focus_detection(&self, active: bool) {
        self.set_focus_detection(active)
    }
    fn name(&self) -> &'static str {
        "gnome-shell-extension"
    }
}

impl ActiveAppProvider for atspi_app::AtspiTracker {
    fn current(&self) -> Option<ActiveApp> {
        self.current()
    }
    fn get_running_apps(&self) -> Vec<RunningApp> {
        self.get_running_apps()
    }
    fn set_focus_detection(&self, active: bool) {
        self.set_focus_detection(active)
    }
    fn name(&self) -> &'static str {
        "atspi"
    }
}

/// Composes an injection backend with an active-app provider. Injection,
/// clipboard, and selection delegate to `inner`; active-app / running-apps /
/// focus use native X11 on X11, and exclusively the compositor/accessibility
/// provider on Wayland. An XWayland injector must never supply app identity.
struct Composed {
    inner: Box<dyn Backend>,
    provider: Option<Box<dyn ActiveAppProvider>>,
    wayland: bool,
}

impl Backend for Composed {
    fn paste_text(&mut self, text: &str, html: Option<&str>) -> Result<()> {
        self.inner.paste_text(text, html)
    }

    fn simulate_key_press(&mut self, keycode_vk: u32, flags: &[String]) -> Result<()> {
        self.inner.simulate_key_press(keycode_vk, flags)
    }

    fn get_active_app(&mut self) -> Result<ActiveApp> {
        // XWayland can retain an old X11 window while a native window is focused.
        if self.wayland {
            return Ok(self
                .provider
                .as_ref()
                .and_then(|p| p.current())
                .unwrap_or_default());
        }
        let inner = self.inner.get_active_app().unwrap_or_default();
        if !inner.app_name.is_empty()
            || !inner.bundle_id.is_empty()
            || !inner.window_title.is_empty()
        {
            return Ok(inner);
        }
        if let Some(app) = self.provider.as_ref().and_then(|p| p.current()) {
            return Ok(app);
        }
        Ok(inner)
    }

    fn get_running_apps(&mut self) -> Result<Vec<RunningApp>> {
        let inner = self.inner.get_running_apps().unwrap_or_default();
        if !inner.is_empty() {
            return Ok(inner);
        }
        Ok(self
            .provider
            .as_ref()
            .map(|p| p.get_running_apps())
            .unwrap_or_default())
    }

    fn get_selected_text(&mut self) -> Result<Selection> {
        self.inner.get_selected_text()
    }

    fn accessibility_status(&mut self) -> bool {
        self.inner.accessibility_status()
    }

    fn set_focus_detection(&mut self, active: bool) {
        // Forward to both: the inner backend no-ops on Wayland/X11 today; the
        // provider is what actually emits `AppInfoUpdate` focus events.
        self.inner.set_focus_detection(active);
        if let Some(provider) = &self.provider {
            provider.set_focus_detection(active);
        }
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

/// Pick a backend for the current session: an injection backend and (when
/// available) an active-app provider, chosen **independently** and composed.
///
/// Injection order: on a Wayland session XWayland usually also sets `$DISPLAY`,
/// but the X11 backend is largely blind there (XTEST doesn't reach native
/// Wayland windows). So prefer Wayland+uinput whenever `$WAYLAND_DISPLAY` is set
/// and uinput is usable, fall through to X11 for a genuine X11 session (or if
/// uinput is unavailable), then a no-op stub that keeps the helper handshaking.
///
/// The active-app provider is selected separately (and even attaches to the stub
/// injector), so active-window identity survives an injection-less session.
pub fn detect(events: EventSink) -> Box<dyn Backend> {
    let provider = pick_active_app_provider(events);
    let injector = pick_injector();
    log::info!(
        "backend: {} injection + {} active-app provider",
        injector.name(),
        provider.as_ref().map(|p| p.name()).unwrap_or("none")
    );
    Box::new(Composed {
        inner: injector,
        provider,
        wayland: env_set("WAYLAND_DISPLAY"),
    })
}

/// True iff env var `var` is set to a **non-empty** value. An empty
/// `WAYLAND_DISPLAY`/`DISPLAY` (some launchers/sessions export a blank value)
/// must be treated as unset: `var_os(...).is_some()` is `true` for an empty
/// value, which would otherwise make `detect()` pick the Wayland backend on a
/// pure-X11 host and fail injection ("could not find wayland compositor").
fn env_set(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty())
}

/// Choose the injection/clipboard/selection backend for this session.
fn pick_injector() -> Box<dyn Backend> {
    if env_set("WAYLAND_DISPLAY") {
        if uinput::UInput::available() {
            match wayland::WaylandBackend::connect() {
                Ok(b) => {
                    log::info!("injection: Wayland (uinput + wl-clipboard)");
                    return Box::new(b);
                }
                Err(e) => log::error!("Wayland backend init failed, trying X11: {e}"),
            }
        } else {
            log::warn!("Wayland session but /dev/uinput is not writable — injection unavailable. \
                        Grant access via a logind uaccess udev rule, the `uinput` group, or run the \
                        ydotoold daemon. Trying X11/XWayland fallback (limited).");
        }
    }
    if env_set("DISPLAY") {
        match x11::X11Backend::connect() {
            Ok(b) => {
                log::info!("injection: X11 (XTEST)");
                return Box::new(b);
            }
            Err(e) => log::error!("X11 backend init failed, falling back to stub: {e}"),
        }
    }
    log::warn!("injection: stub (no-op) — OS integration disabled");
    Box::new(stub::StubBackend)
}

/// Choose the active-app provider for this session — independent of injection.
fn pick_active_app_provider(events: EventSink) -> Option<Box<dyn ActiveAppProvider>> {
    if env_set("WAYLAND_DISPLAY") {
        // Tier-2 compositor bridge first (richest), AT-SPI as the universal fallback.
        if wayland::is_kde() {
            match kwin::KwinTracker::start(events.clone()) {
                Ok(t) => {
                    log::info!("active-app: KWin scripting bridge (KDE)");
                    return Some(Box::new(t));
                }
                Err(e) => log::warn!("KWin bridge unavailable ({e}); falling back to AT-SPI"),
            }
        } else if is_gnome() {
            match gnome::GnomeTracker::start(events.clone()) {
                Ok(t) => {
                    log::info!("active-app: GNOME Shell extension bridge");
                    return Some(Box::new(t));
                }
                // Err is the expected first-run state (extension installed but the
                // shell only scans extensions at login) — fall back to AT-SPI so
                // identity is never blank in the gap before the user re-logs in.
                Err(e) => {
                    log::warn!("GNOME extension bridge not active ({e}); falling back to AT-SPI")
                }
            }
        }
        return start_atspi(events, "Wayland");
    }
    if env_set("DISPLAY") {
        match x11::FocusTracker::start(events.clone()) {
            Ok(tracker) => return Some(Box::new(tracker)),
            Err(e) => log::warn!("X11 focus tracker unavailable ({e}); trying AT-SPI"),
        }
        return start_atspi(events, "X11 fallback");
    }
    None
}

fn start_atspi(events: EventSink, ctx: &str) -> Option<Box<dyn ActiveAppProvider>> {
    match atspi_app::AtspiTracker::start(events) {
        Ok(t) => {
            log::info!("active-app: AT-SPI universal tracker [{ctx}]");
            Some(Box::new(t))
        }
        Err(e) => {
            log::warn!("AT-SPI active-app unavailable ({e}) [{ctx}]");
            None
        }
    }
}

/// Heuristic: is this a GNOME session? Checks the standard desktop-environment
/// env vars; `contains("GNOME")` handles Ubuntu's `XDG_CURRENT_DESKTOP=ubuntu:GNOME`.
fn is_gnome() -> bool {
    let probe = |k: &str| std::env::var(k).unwrap_or_default().to_ascii_uppercase();
    probe("XDG_CURRENT_DESKTOP").contains("GNOME")
        || probe("XDG_SESSION_DESKTOP").contains("GNOME")
        || std::env::var_os("GNOME_SHELL_SESSION_MODE").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AppBackend;
    impl Backend for AppBackend {
        fn get_active_app(&mut self) -> Result<ActiveApp> {
            Ok(ActiveApp {
                bundle_id: "Code".into(),
                ..Default::default()
            })
        }
        fn get_running_apps(&mut self) -> Result<Vec<RunningApp>> {
            Ok(vec![])
        }
        fn paste_text(&mut self, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        fn simulate_key_press(&mut self, _: u32, _: &[String]) -> Result<()> {
            Ok(())
        }
        fn get_selected_text(&mut self) -> Result<Selection> {
            Ok(Selection::default())
        }
        fn accessibility_status(&mut self) -> bool {
            false
        }
        fn name(&self) -> &'static str {
            "test"
        }
    }
    struct Provider(Option<&'static str>);
    impl ActiveAppProvider for Provider {
        fn current(&self) -> Option<ActiveApp> {
            self.0.map(|id| ActiveApp {
                bundle_id: id.into(),
                ..Default::default()
            })
        }
        fn get_running_apps(&self) -> Vec<RunningApp> {
            vec![]
        }
        fn set_focus_detection(&self, _: bool) {}
        fn name(&self) -> &'static str {
            "test"
        }
    }

    #[test]
    fn native_wayland_identity_wins_over_xwayland_and_fails_to_generic() {
        let mut be = Composed {
            inner: Box::new(AppBackend),
            provider: Some(Box::new(Provider(Some("Cursor")))),
            wayland: true,
        };
        assert_eq!(be.get_active_app().unwrap().bundle_id, "Cursor");
        be.provider = Some(Box::new(Provider(None)));
        assert_eq!(be.get_active_app().unwrap().bundle_id, "");
        be.provider = None;
        assert_eq!(be.get_active_app().unwrap().bundle_id, "");
        be.wayland = false;
        assert_eq!(be.get_active_app().unwrap().bundle_id, "Code");
    }
}

#[cfg(test)]
mod headless_tests {
    use super::*;

    #[test]
    #[ignore = "run with DISPLAY and WAYLAND_DISPLAY unset"]
    fn headless_skips_app_and_field_focus_detection() {
        assert!(!env_set("DISPLAY") && !env_set("WAYLAND_DISPLAY"));
        let (tx, _) = std::sync::mpsc::channel();
        assert!(pick_active_app_provider(tx.clone()).is_none());
        assert!(atspi_focus::start().is_none());
        let mut backend = detect(tx);
        assert!(backend.get_active_app().unwrap().bundle_id.is_empty());
    }
}
