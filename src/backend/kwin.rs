//! KDE/KWin active-window bridge + focus-event source.
//!
//! Wayland has no portable protocol for "what app owns the focused window", so
//! on KDE we use KWin's scripting engine. KWin JS scripts can't return data
//! directly, but they can `callDBus(...)`. So we:
//!
//!   1. Host a tiny D-Bus service (`org.wisprflow.kwinbridge_<pid>`) with a
//!      `Report(s)` method. We run it on the **async** zbus API driven by a
//!      dedicated tokio runtime thread (see "Threading / runtime" below).
//!   2. Load a KWin script (via `org.kde.kwin.Scripting.loadScript`) that
//!      connects `workspace.windowActivated` to a function pushing the active
//!      window's `{resourceClass, resourceName, caption, pid}` to our service,
//!      and fires once for the current `workspace.activeWindow`.
//!   3. Cache the latest report; `get_active_app()` reads the cache, and — when
//!      focus detection is enabled (`SetFocusChangeDetectorState`) — each change
//!      is emitted on fd 3 as an `AppInfoUpdate` helper-initiated request.
//!
//! KDE-specific by nature (GNOME would need a shell extension).
//!
//! ## Threading / runtime
//!
//! This bridge previously used zbus's **blocking** API on the assumption that
//! the connection spawns its own internal executor thread to dispatch incoming
//! method calls. That assumption is FALSE in this crate: `atspi` enables zbus's
//! `tokio` feature tree-wide, which disables zbus's internal async-io executor
//! thread — so a blocking connection's incoming calls are only dispatched when
//! *we* make an outgoing blocking call (which ticks the executor once). The
//! symptom (observed live on Fedora 43 KDE / Plasma 6.4.5): KWin's `Report`
//! callbacks queued and only flushed at shutdown (when `Drop` called
//! `unloadScript`), so `GetActiveAppInfo` / `GetRunningApps` came back empty.
//!
//! The fix mirrors `atspi_app.rs`: run the service on the **async** zbus API
//! inside a dedicated tokio current-thread runtime that stays alive for the
//! tracker's lifetime (`block_on` of a long-running loop). A live runtime drives
//! zbus's spawned dispatch tasks, so incoming `Report` calls are processed
//! promptly. All cache reads (`current`/`running_apps`) and the focus toggle
//! touch only the shared `Mutex`/atomics, so they stay synchronous and
//! independent of the runtime; the only D-Bus teardown (`unloadScript`) runs on
//! the runtime thread when `stop` is signalled.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::{active_app_payload, ActiveApp, EventSink, RunningApp};

/// State shared between the zbus dispatcher thread (which calls `Report`) and
/// the main dispatch loop (which reads `current()` / toggles focus detection).
struct Shared {
    cache: Mutex<Option<ActiveApp>>,
    /// All normal windows from the last `ReportList` walk, de-duped by
    /// resourceClass (one entry per app, like a taskbar / `GetRunningApps`).
    apps: Mutex<Vec<RunningApp>>,
    events: EventSink,
    /// Gates `AppInfoUpdate` emission (toggled by `SetFocusChangeDetectorState`).
    focus_active: AtomicBool,
    /// Dedup key of the last emitted app, so an activation that doesn't change
    /// the app+title doesn't spam duplicate events.
    last_emitted: Mutex<Option<String>>,
    counter: AtomicU64,
    pid: u32,
}

impl Shared {
    fn on_activation(&self, app: ActiveApp) {
        // The recording pill takes focus briefly; it is never the paste target.
        if app.bundle_id == "wispr-flow" && app.window_title == "Status" {
            return;
        }
        // KWin fires windowActivated(null) when focus lands on the desktop / no
        // window (e.g. transiently while one app closes and the next maps),
        // which parses to a fully-empty ActiveApp. Don't let that blank out the
        // last known good app or emit an empty focus event — a dictation helper
        // wants the most recent *real* target. (Mirrors the AT-SPI tracker, which
        // skips totally-empty resolutions.)
        if app.app_name.is_empty() && app.bundle_id.is_empty() && app.window_title.is_empty() {
            return;
        }
        if let Ok(mut g) = self.cache.lock() {
            *g = Some(app.clone());
        }
        if self.focus_active.load(Ordering::Relaxed) {
            self.emit(&app);
        }
    }

    /// Emit an `AppInfoUpdate` request on fd 3 unless it duplicates the last one.
    fn emit(&self, app: &ActiveApp) {
        let key = format!("{}\u{1f}{}", app.bundle_id, app.window_title);
        if let Ok(mut last) = self.last_emitted.lock() {
            if last.as_deref() == Some(key.as_str()) {
                return;
            }
            *last = Some(key);
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let uuid = format!("evt-{}-{}", self.pid, n);
        let env = crate::proto::request("AppInfoUpdate", active_app_payload(app), &uuid);
        let _ = self.events.send(env);
    }

    fn set_focus_detection(&self, active: bool) {
        self.focus_active.store(active, Ordering::Relaxed);
        if active {
            // Re-enabling must re-announce even an unchanged app (see
            // RecordingStarted in main.rs), so forget the dedup key first.
            if let Ok(mut last) = self.last_emitted.lock() {
                *last = None;
            }
            // Push the current focus immediately so a fresh subscriber gets state
            // without waiting for the next activation.
            let current = self.cache.lock().ok().and_then(|g| g.clone());
            if let Some(app) = current {
                self.emit(&app);
            }
        } else if let Ok(mut last) = self.last_emitted.lock() {
            *last = None;
        }
    }
}

/// The D-Bus interface object KWin calls into. Holds the shared state.
struct Bridge {
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.wisprflow.kwinbridge")]
impl Bridge {
    /// Called by the KWin script on every window activation. `payload` is the
    /// JSON object described above (or `{}` when no window is active).
    fn report(&self, payload: String) {
        let app = parse_report(&payload);
        log::debug!("kwin report: {payload} -> {app:?}");
        self.shared.on_activation(app);
    }

    /// Called by the KWin script with a JSON array of all normal windows
    /// (`[{resourceClass, resourceName, caption, pid}, ...]`). We resolve names,
    /// de-dupe by resourceClass, and cache for `GetRunningApps`.
    fn report_list(&self, payload: String) {
        let apps = parse_report_list(&payload);
        log::debug!("kwin report_list: {} apps", apps.len());
        if let Ok(mut g) = self.shared.apps.lock() {
            *g = apps;
        }
    }
}

/// Build an `ActiveApp` from the KWin script's JSON payload, resolving a
/// friendlier app name from the pid's `/proc/<pid>/comm` when available
/// (mirrors the X11 backend), else falling back to the resource class.
fn parse_report(payload: &str) -> ActiveApp {
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let class = v
        .get("resourceClass")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let caption = v
        .get("caption")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pid = v.get("pid").and_then(Value::as_u64).unwrap_or(0);

    let resource_name = v.get("resourceName").and_then(Value::as_str).unwrap_or("");
    ActiveApp {
        app_name: app_name(pid, resource_name, &class),
        bundle_id: class,
        window_title: caption,
        url: String::new(),
    }
}

/// Resolve a friendly app name: prefer the pid's `/proc/<pid>/comm`, then the
/// window's `resourceName`, then the `resourceClass`.
fn app_name(pid: u64, resource_name: &str, class: &str) -> String {
    if pid != 0 {
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            let comm = comm.trim();
            if !comm.is_empty() {
                return comm.to_string();
            }
        }
    }
    if !resource_name.is_empty() {
        return resource_name.to_string();
    }
    class.to_string()
}

/// Parse the `ReportList` JSON array into a de-duped (by resourceClass) list of
/// `RunningApp`, like a real `GetRunningApps`.
fn parse_report_list(payload: &str) -> Vec<RunningApp> {
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out: Vec<RunningApp> = Vec::new();
    for w in arr {
        let class = w.get("resourceClass").and_then(Value::as_str).unwrap_or("");
        if class.is_empty() {
            continue;
        }
        // De-dupe by resourceClass: one entry per app.
        if out.iter().any(|a| a.bundle_id == class) {
            continue;
        }
        let resource_name = w.get("resourceName").and_then(Value::as_str).unwrap_or("");
        let pid = w.get("pid").and_then(Value::as_u64).unwrap_or(0);
        out.push(RunningApp {
            bundle_id: class.to_string(),
            name: app_name(pid, resource_name, class),
        });
    }
    out
}

/// Owns the background runtime thread hosting the zbus service + the loaded KWin
/// script. Dropping it signals the thread to unload the script and tear down.
pub struct KwinTracker {
    shared: Arc<Shared>,
    /// Set on drop to ask the runtime thread to unload the script and exit.
    stop: Arc<AtomicBool>,
}

impl KwinTracker {
    /// Start the bridge: host the service, write+load+start the KWin script.
    /// Returns an error (and changes nothing observable) if KWin scripting isn't
    /// reachable, so the caller can fall back to the generic-Wayland behavior.
    ///
    /// The zbus service + KWin proxy calls all happen on a dedicated tokio
    /// runtime thread (see the module "Threading / runtime" note); `start()`
    /// blocks on a channel until that thread reports the bridge is up (or
    /// failed), so the return value still reflects real reachability for the
    /// caller's fallback decision.
    pub fn start(events: EventSink) -> Result<KwinTracker, String> {
        let pid = std::process::id();

        let shared = Arc::new(Shared {
            cache: Mutex::new(None),
            apps: Mutex::new(Vec::new()),
            events,
            focus_active: AtomicBool::new(true),
            last_emitted: Mutex::new(None),
            counter: AtomicU64::new(0),
            pid,
        });
        let stop = Arc::new(AtomicBool::new(false));

        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread_shared = shared.clone();
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("kwin-bridge".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(run_bridge(pid, thread_shared, thread_stop, ready_tx));
            })
            .map_err(|e| format!("spawn kwin-bridge thread: {e}"))?;

        // Wait for the bridge to come up (or fail). Bounded so a wedged session
        // bus / KWin can't hang helper startup; on timeout we treat it as
        // unreachable and the caller falls back.
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(KwinTracker { shared, stop }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("KWin bridge startup timed out".into())
            }
        }
    }

    /// Latest active window, or None if nothing has been reported yet.
    pub fn current(&self) -> Option<ActiveApp> {
        self.shared.cache.lock().ok().and_then(|g| g.clone())
    }

    /// All normal windows from the last `workspace.windowList` walk, de-duped by
    /// resourceClass. Empty until the script's first `ReportList` lands.
    pub fn running_apps(&self) -> Vec<RunningApp> {
        self.shared
            .apps
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Enable/disable `AppInfoUpdate` focus events.
    pub fn set_focus_detection(&self, active: bool) {
        self.shared.set_focus_detection(active);
    }
}

impl Drop for KwinTracker {
    fn drop(&mut self) {
        // Ask the runtime thread to unload the script and exit at its next tick.
        // The thread is detached (it tears down the bus connection + temp file
        // itself), so we don't join.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Body run on the dedicated tokio runtime thread: bring up the zbus service,
/// load+start the KWin script, signal readiness, then keep the runtime alive
/// (driving zbus dispatch) until `stop` is set — at which point it unloads the
/// script and removes the temp file before returning (dropping the connection).
async fn run_bridge(
    pid: u32,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), String>>,
) {
    let service = format!("org.wisprflow.kwinbridge_{pid}");
    let path = "/active";
    let plugin = format!("wispr-flow-active-{pid}");

    // Host the service.
    let conn = match zbus::connection::Builder::session()
        .and_then(|b| b.name(service.as_str()))
        .and_then(|b| {
            b.serve_at(
                path,
                Bridge {
                    shared: shared.clone(),
                },
            )
        }) {
        Ok(b) => match b.build().await {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("build connection: {e}")));
                return;
            }
        },
        Err(e) => {
            let _ = ready_tx.send(Err(format!("session bus: {e}")));
            return;
        }
    };

    // Write the KWin script (service name/path templated in).
    let script = kwin_script(&service, path);
    let script_path = std::env::temp_dir().join(format!("wispr-flow-active-{pid}.js"));
    if let Err(e) = std::fs::write(&script_path, script) {
        let _ = ready_tx.send(Err(format!("write script: {e}")));
        return;
    }

    // loadScript(filePath, pluginName) -> id, then start() runs all scripts.
    let scripting = match zbus::Proxy::new(
        &conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&script_path);
            let _ = ready_tx.send(Err(format!("scripting proxy: {e}")));
            return;
        }
    };

    let load: Result<i32, _> = scripting
        .call(
            "loadScript",
            &(script_path.to_string_lossy().as_ref(), plugin.as_str()),
        )
        .await;
    if let Err(e) = load {
        let _ = std::fs::remove_file(&script_path);
        let _ = ready_tx.send(Err(format!("loadScript (KWin not available?): {e}")));
        return;
    }
    if let Err(e) = scripting.call::<_, _, ()>("start", &()).await {
        let _ = std::fs::remove_file(&script_path);
        let _ = ready_tx.send(Err(format!("start: {e}")));
        return;
    }

    log::info!("KWin active-window bridge started (service {service}, plugin {plugin})");
    let _ = ready_tx.send(Ok(()));

    // Keep the runtime alive so zbus dispatches incoming Report/ReportList calls
    // promptly. Poll the stop flag on a short interval.
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Teardown: unload the script, remove the temp file, drop the connection.
    let _ = scripting
        .call::<_, _, bool>("unloadScript", &(plugin.as_str(),))
        .await;
    let _ = std::fs::remove_file(&script_path);
    drop(conn);
}

/// The KWin JS pushed to the scripting engine. `workspace.activeWindow` is the
/// Plasma 6 API (Plasma 5 used `activeClient`); we read it defensively so the
/// same script degrades gracefully. `workspace.windowList()` (Plasma 6; Plasma 5
/// used `workspace.clientList()`) enumerates every window — we filter to normal,
/// taskbar-visible windows (the way a taskbar shows running apps) and push the
/// full list via `ReportList`, re-walking whenever the window set changes.
fn kwin_script(service: &str, path: &str) -> String {
    format!(
        r#"
function wisprReport(w) {{
    var payload = "{{}}";
    if (w) {{
        payload = JSON.stringify({{
            resourceClass: w.resourceClass,
            resourceName: w.resourceName,
            caption: w.caption,
            pid: w.pid
        }});
    }}
    callDBus("{service}", "{path}", "org.wisprflow.kwinbridge", "Report", payload);
}}
function wisprWindowList() {{
    if (typeof workspace.windowList === "function") {{
        return workspace.windowList();
    }}
    if (typeof workspace.clientList === "function") {{
        return workspace.clientList();
    }}
    return [];
}}
function wisprReportList() {{
    var wins = wisprWindowList();
    var out = [];
    for (var i = 0; i < wins.length; i++) {{
        var w = wins[i];
        if (!w) {{ continue; }}
        // Keep only normal, taskbar-visible windows with a class — drops the
        // desktop, docks/panels, OSDs, splashes, etc. (what a taskbar shows).
        if (w.normalWindow !== true) {{ continue; }}
        if (w.skipTaskbar === true) {{ continue; }}
        if (!w.resourceClass) {{ continue; }}
        out.push({{
            resourceClass: w.resourceClass,
            resourceName: w.resourceName,
            caption: w.caption,
            pid: w.pid
        }});
    }}
    callDBus("{service}", "{path}", "org.wisprflow.kwinbridge", "ReportList", JSON.stringify(out));
}}
workspace.windowActivated.connect(function(w) {{
    wisprReport(w);
    wisprReportList();
}});
if (typeof workspace.windowAdded !== "undefined") {{
    workspace.windowAdded.connect(wisprReportList);
}} else if (typeof workspace.clientAdded !== "undefined") {{
    workspace.clientAdded.connect(wisprReportList);
}}
if (typeof workspace.windowRemoved !== "undefined") {{
    workspace.windowRemoved.connect(wisprReportList);
}} else if (typeof workspace.clientRemoved !== "undefined") {{
    workspace.clientRemoved.connect(wisprReportList);
}}
wisprReport(workspace.activeWindow || workspace.activeClient);
wisprReportList();
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_status_preserves_target_but_scratchpad_can_take_focus() {
        let (tx, rx) = mpsc::channel();
        let shared = Shared {
            cache: Mutex::new(None),
            apps: Mutex::new(Vec::new()),
            events: tx,
            focus_active: AtomicBool::new(true),
            last_emitted: Mutex::new(None),
            counter: AtomicU64::new(0),
            pid: 1,
        };
        shared.on_activation(ActiveApp {
            bundle_id: "google-chrome".into(),
            window_title: "Document - Google Chrome".into(),
            ..ActiveApp::default()
        });
        rx.recv().unwrap();
        shared.on_activation(ActiveApp {
            bundle_id: "wispr-flow".into(),
            window_title: "Status".into(),
            ..ActiveApp::default()
        });
        assert_eq!(
            shared.cache.lock().unwrap().as_ref().unwrap().bundle_id,
            "google-chrome"
        );
        assert!(rx.try_recv().is_err());
        shared.on_activation(ActiveApp {
            bundle_id: "wispr-flow".into(),
            window_title: "Scratchpad".into(),
            ..ActiveApp::default()
        });
        assert_eq!(
            shared.cache.lock().unwrap().as_ref().unwrap().window_title,
            "Scratchpad"
        );
        assert!(rx.try_recv().is_ok());
    }
}
