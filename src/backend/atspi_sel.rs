//! AT-SPI2 selection reads — the non-destructive path for `GetSelectedTextViaCopy`.
//!
//! Instead of synthesizing Ctrl+C and snooping the clipboard (destructive: it
//! mutates the user's clipboard and pushes a keystroke into their app), this
//! queries the accessibility bus directly: find the focused accessible object,
//! grab its `org.a11y.atspi.Text` interface, and read the current selection
//! (`GetSelection` -> `GetText`). No input synthesis, no clipboard churn.
//!
//! The `atspi` crate is pure-Rust (over `zbus`, the same crate family the KWin
//! bridge uses) and async (tokio). The rest of the helper is blocking/threaded,
//! so we spin up a tiny single-thread tokio runtime per call and `block_on` it.
//! That keeps the single-static-binary property and avoids threading an async
//! runtime through the whole backend.
//!
//! The focused object is located by walking the accessibility tree from the
//! registry desktop root: enumerate applications, then descend each looking for
//! the node whose `StateSet` contains `State::Focused`. The walk is depth- and
//! breadth-bounded so an unresponsive or pathological app can't hang the call.
//!
//! Returns `Ok(Some(Selection))` when a focused Text object with a non-empty
//! selection is found, `Ok(None)` when AT-SPI is reachable but nothing usable is
//! selected (callers should fall back to the copy-probe), and `Err` when the
//! bus itself is unreachable.

use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::text::TextProxy;
use atspi::{ObjectRef, State};
use zbus::Connection;

use super::Selection;

/// Overall wall-clock budget for the whole AT-SPI probe (connect + walk +
/// reads). The copy-probe fallback is fast, so we'd rather give up early than
/// stall dictation behind a slow/buggy a11y provider.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Bounds for the read-only-widget fallback. Editable fields use the deeper,
/// active-window-first search shared with correction observation.
const MAX_DEPTH: usize = 12;
const MAX_NODES: usize = 400;

/// Try to read the current selection over the AT-SPI2 bus.
///
/// `Ok(None)` means "bus reachable, nothing selected" (fall back to copy-probe);
/// `Err` means the accessibility bus is unavailable (also a fallback trigger,
/// but worth logging distinctly).
pub fn get_selection() -> Result<Option<Selection>, String> {
    // A current-thread runtime is enough: all the work is D-Bus round-trips, no
    // CPU-bound parallelism. Created per call so we hold no runtime between
    // requests (selection reads are infrequent and latency-tolerant).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;

    rt.block_on(async {
        match tokio::time::timeout(PROBE_TIMEOUT, probe()).await {
            Ok(res) => res,
            Err(_) => Err("AT-SPI probe timed out".to_string()),
        }
    })
}

/// Detect embedded terminals from the focused widget, not the host app name.
pub fn focused_is_terminal() -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(800), async {
            let a11y = AccessibilityConnection::new().await.ok()?;
            let conn = a11y.connection();
            let field = find_focused(conn).await.ok()??;
            Some(is_terminal_widget(conn, &field).await)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
    })
}

async fn is_terminal_widget(conn: &Connection, field: &ObjectRef) -> bool {
    let Ok(proxy) = field.as_accessible_proxy(conn).await else {
        return false;
    };
    let terminal = proxy.get_role().await.ok() == Some(atspi::Role::Terminal)
        || proxy
            .get_attributes()
            .await
            .ok()
            .and_then(|attrs| attrs.get("class").cloned())
            .is_some_and(|classes| {
                classes
                    .split_ascii_whitespace()
                    .any(|class| class == "xterm-helper-textarea")
            });
    if !terminal {
        return false;
    }
    // Inactive Electron windows retain Focused; never paste with their chord.
    let mut obj = field.clone();
    for _ in 0..64 {
        let Ok(proxy) = obj.as_accessible_proxy(conn).await else {
            return false;
        };
        if proxy
            .get_state()
            .await
            .is_ok_and(|s| s.contains(State::Active))
        {
            return true;
        }
        let Ok(parent) = proxy.parent().await else {
            return false;
        };
        if parent == obj {
            return false;
        }
        obj = parent;
    }
    false
}

async fn probe() -> Result<Option<Selection>, String> {
    // Announce ourselves as an assistive-tech client so toolkits (Qt, GTK) expose
    // their accessible tree + Text interfaces. Without this the focused object's
    // selection is often silently empty on a stock session — and on KDE the
    // active-app provider is the KWin bridge (not the AT-SPI tracker), so nothing
    // else flips this flag. Best-effort + idempotent; a failure isn't fatal.
    if let Err(e) = atspi::connection::set_session_accessibility(true).await {
        log::debug!("atspi_sel: set_session_accessibility(true) failed: {e}");
    }

    let a11y = AccessibilityConnection::new()
        .await
        .map_err(|e| format!("a11y bus connect: {e}"))?;
    let conn = a11y.connection();

    let Some(focused) = find_focused(conn).await? else {
        log::debug!("atspi: no focused accessible found");
        return Ok(None);
    };

    read_selection(conn, &focused).await
}

/// The accessibility hierarchy root: the registry's desktop object. All running
/// applications hang off it as children.
pub(super) async fn desktop_root(conn: &Connection) -> Result<AccessibleProxy<'static>, String> {
    AccessibleProxy::builder(conn)
        .destination("org.a11y.atspi.Registry")
        .map_err(|e| format!("root destination: {e}"))?
        .path("/org/a11y/atspi/accessible/root")
        .map_err(|e| format!("root path: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| format!("root proxy: {e}"))
}

/// Locate the focused accessible by walking from the desktop root. Applications
/// are the desktop's direct children; we descend each looking for the node whose
/// state set contains `Focused`. Bounded by depth/node count so a misbehaving
/// provider can't make us walk forever.
async fn find_focused(conn: &Connection) -> Result<Option<ObjectRef>, String> {
    let tracked = super::atspi_focus::LAST_FOCUSED_WIDGET
        .lock()
        .ok()
        .and_then(|g| g.clone());
    if let Some(obj) = tracked {
        if let Ok(proxy) = obj.as_accessible_proxy(conn).await {
            if proxy
                .get_state()
                .await
                .is_ok_and(|states| states.contains(State::Focused))
            {
                return Ok(Some(obj));
            }
        }
    }
    // Chromium editors are deeply nested and inactive windows retain Focused.
    if let Some(field) = super::atspi_edit::find_focused_field(conn).await? {
        return Ok(Some(field));
    }
    // Keep support for focused read-only widgets without Editable state.
    let root = desktop_root(conn).await?;
    let apps = root.get_children().await.unwrap_or_default();

    let mut budget = MAX_NODES;
    for app in apps {
        if let Some(found) = descend_for_focus(conn, &app, 0, MAX_DEPTH, &mut budget).await {
            return Ok(Some(found));
        }
        if budget == 0 {
            break;
        }
    }
    Ok(None)
}

/// Depth-first search under `obj` for an accessible holding `State::Focused`.
/// `budget` is a shared node cap decremented across the whole walk.
pub(super) async fn descend_for_focus(
    conn: &Connection,
    obj: &ObjectRef,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
) -> Option<ObjectRef> {
    if depth > max_depth || *budget == 0 {
        return None;
    }
    *budget -= 1;

    let proxy = obj.as_accessible_proxy(conn).await.ok()?;

    // Selection reads also support focused, read-only widgets.
    if let Ok(states) = proxy.get_state().await {
        if states.contains(State::Focused) {
            return Some(obj.clone());
        }
    }

    // Otherwise recurse into children. Bail the moment the budget runs out so a
    // huge subtree can't starve sibling applications.
    let children = proxy.get_children().await.ok()?;
    for child in children {
        if *budget == 0 {
            break;
        }
        if let Some(found) = Box::pin(descend_for_focus(
            conn,
            &child,
            depth + 1,
            max_depth,
            budget,
        ))
        .await
        {
            return Some(found);
        }
    }
    None
}

/// Read the selection from `obj`'s Text interface. Returns `Ok(None)` when the
/// object has no Text interface or no (non-empty) selection.
async fn read_selection(conn: &Connection, obj: &ObjectRef) -> Result<Option<Selection>, String> {
    let text = TextProxy::builder(conn)
        .destination(obj.name.as_str())
        .map_err(|e| format!("text destination: {e}"))?
        .path(obj.path.as_str())
        .map_err(|e| format!("text path: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        // No Text interface on the focused object: not an error, just nothing
        // for us to read — let the caller fall back.
        .map_err(|e| format!("text proxy: {e}"))?;

    // atspi 0.22 generates GetNselections; the wire method is GetNSelections.
    let n: i32 = match text.inner().call("GetNSelections", &()).await {
        Ok(n) => n,
        // GetNSelections fails when the object doesn't really implement Text.
        Err(_) => return Ok(None),
    };
    if n <= 0 {
        return Ok(None);
    }

    // Concatenate every active selection range (most apps expose exactly one).
    let mut selected = String::new();
    for i in 0..n {
        let (start, end) = match text.get_selection(i).await {
            Ok(range) => range,
            Err(_) => continue,
        };
        if end <= start {
            continue;
        }
        if let Ok(s) = text.get_text(start, end).await {
            selected.push_str(&s);
        }
    }

    if selected.is_empty() {
        return Ok(None);
    }

    // `contents` is the full text of the focused field when cheaply available;
    // best-effort, and capped so a huge document doesn't bloat the IPC reply.
    let contents = match text.character_count().await {
        Ok(count) if count > 0 && count <= 100_000 => {
            text.get_text(0, count).await.unwrap_or_default()
        }
        _ => String::new(),
    };

    Ok(Some(Selection {
        selected_text: selected,
        contents,
        before_text: String::new(),
        after_text: String::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Node {
        focused: bool,
        active: bool,
        children: Vec<ObjectRef>,
        class: String,
        parent: ObjectRef,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl Node {
        fn get_state(&self) -> atspi::StateSet {
            let mut states = atspi::StateSet::empty();
            if self.focused {
                states.insert(State::Focused);
                states.insert(State::Editable);
            }
            if self.active {
                states.insert(State::Active);
            }
            states
        }
        fn get_children(&self) -> Vec<ObjectRef> {
            self.children.clone()
        }
        fn get_attributes(&self) -> std::collections::HashMap<String, String> {
            std::collections::HashMap::from([("class".into(), self.class.clone())])
        }
        #[zbus(property)]
        fn parent(&self) -> ObjectRef {
            self.parent.clone()
        }
        fn get_role(&self) -> atspi::Role {
            atspi::Role::Entry
        }
    }

    struct SelectedText;
    #[zbus::interface(name = "org.a11y.atspi.Text")]
    impl SelectedText {
        fn get_n_selections(&self) -> i32 {
            1
        }
        fn get_selection(&self, _index: i32) -> (i32, i32) {
            (0, 8)
        }
        fn get_text(&self, _start: i32, _end: i32) -> String {
            "selected".into()
        }
        #[zbus(property)]
        fn character_count(&self) -> i32 {
            8
        }
    }

    #[test]
    #[ignore = "requires a focused terminal in the live desktop"]
    fn live_terminal_detection() {
        assert_eq!(
            std::env::var("WISPR_SELECTION_TEST_LIVE").as_deref(),
            Ok("1")
        );
        assert!(focused_is_terminal(), "focused terminal not detected");
    }

    #[test]
    #[ignore = "requires selected non-sensitive text in the live desktop"]
    fn live_selection_read() {
        assert_eq!(
            std::env::var("WISPR_SELECTION_TEST_LIVE").as_deref(),
            Ok("1")
        );
        assert!(get_selection().unwrap().is_some(), "no selected text found");
    }

    #[test]
    #[ignore = "requires a private dbus-run-session"]
    fn selection_discovers_deep_active_editor() {
        assert_eq!(
            std::env::var("WISPR_EDIT_TEST_ISOLATED").as_deref(),
            Ok("1")
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let server = zbus::Connection::session().await.unwrap();
                server
                    .request_name("org.a11y.atspi.Registry")
                    .await
                    .unwrap();
                let client = zbus::Connection::session().await.unwrap();
                let object = |path: String| ObjectRef {
                    name: server.unique_name().unwrap().clone(),
                    path: path.try_into().unwrap(),
                };
                let nodes: Vec<_> = (0..=42).map(|i| object(format!("/deep/n{i}"))).collect();
                for (i, node) in nodes.iter().enumerate() {
                    server
                        .object_server()
                        .at(
                            node.path.clone(),
                            Node {
                                class: String::new(),
                                parent: nodes[i.saturating_sub(1)].clone(),
                                focused: i == 42,
                                active: i == 1,
                                children: nodes.get(i + 1).cloned().into_iter().collect(),
                            },
                        )
                        .await
                        .unwrap();
                }
                server
                    .object_server()
                    .at(
                        "/org/a11y/atspi/accessible/root",
                        Node {
                            class: String::new(),
                            parent: nodes[0].clone(),
                            focused: false,
                            active: false,
                            children: vec![nodes[0].clone()],
                        },
                    )
                    .await
                    .unwrap();
                server
                    .object_server()
                    .at(nodes[42].path.clone(), SelectedText)
                    .await
                    .unwrap();
                let field = find_focused(&client).await.unwrap().expect("deep editor");
                assert_eq!(
                    read_selection(&client, &field)
                        .await
                        .unwrap()
                        .unwrap()
                        .selected_text,
                    "selected"
                );
                assert!(!is_terminal_widget(&client, &field).await);
                for (class, expected) in [
                    ("xterm-helper-textarea", true),
                    ("input xterm-helper-textarea focused", true),
                    ("monaco-editor", false),
                    ("not-xterm-helper-textarea", false),
                ] {
                    let interface = server
                        .object_server()
                        .interface::<_, Node>(field.path.clone())
                        .await
                        .unwrap();
                    interface.get_mut().await.class = class.into();
                    let terminal = is_terminal_widget(&client, &field).await;
                    assert_eq!(terminal, expected, "{class}");
                    crate::paste::set_focused_is_terminal(terminal);
                    assert_eq!(
                        crate::paste::chord().unwrap().flags.contains(&"Shift"),
                        expected
                    );
                }
                let terminal = server
                    .object_server()
                    .interface::<_, Node>(field.path.clone())
                    .await
                    .unwrap();
                terminal.get_mut().await.class = "xterm-helper-textarea".into();
                let window = server
                    .object_server()
                    .interface::<_, Node>(nodes[1].path.clone())
                    .await
                    .unwrap();
                window.get_mut().await.active = false;
                assert!(
                    !is_terminal_widget(&client, &field).await,
                    "inactive terminal must not control paste"
                );
                window.get_mut().await.active = true;
                assert_eq!(
                    find_focused(&client).await.unwrap(),
                    Some(nodes[42].clone()),
                    "selected editor deeper than 12 levels must be reachable"
                );
            });
    }
}
