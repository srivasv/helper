//! Post-paste edit observation — feeds the app's "auto-learn corrected words".
//!
//! After `PasteText` lands, the app expects one `UpdateEditedText` request per
//! `pasteId` once the user has finished editing the dictated text; it diffs
//! `pastedText` against `editedText` itself and adds the changed words to the
//! dictionary. macOS/Windows read the field through AX/UIA; here we read it over
//! AT-SPI.
//!
//! Method: right after the paste, find the focused accessible, read its full text
//! and locate the pasted span (anchored on the caret). Remember the text before
//! and after that span, then poll the same field. The "edited text" is whatever
//! sits between those two anchors. Observation ends on focus loss, a quiet
//! period, a hard cap, or the next paste. If the field can't be read (no Text
//! interface, terminal, a11y off) nothing is sent — same as before this module.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::text::TextProxy;
use atspi::{Role, State};
use serde_json::{json, Value};

use super::atspi_app::LAST_FOCUSED_WIDGET;
use super::atspi_sel::{descend_for_focus, desktop_root};
use crate::proto;

/// Let the Ctrl+V land (and the toolkit emit its text update) before baselining.
const SETTLE: Duration = Duration::from_millis(300);
const BASELINE_TRIES: u32 = 4;
const POLL: Duration = Duration::from_millis(400);
/// Quiet time after the last observed change before we call the edit finished.
const IDLE_END: Duration = Duration::from_secs(15);
/// Never watch one paste longer than this.
const MAX_WATCH: Duration = Duration::from_secs(120);
/// Skip huge documents; the IPC frame cap is 30k and these aren't dictation fields.
const MAX_FIELD_CHARS: i32 = 100_000;
const WALK_TIMEOUT: Duration = Duration::from_secs(8);
const WALK_DEPTH: usize = 40;
const WALK_NODES: usize = 6_000;
const MAX_REPORT_CHARS: usize = 8_000;

/// Bumped by every successful paste so superseded observations are discarded.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Start observing the field the paste just went into. Returns immediately.
pub fn watch(paste_id: String, pasted: String, tx: Sender<Value>) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if paste_id.is_empty() || pasted.trim().is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => return log::debug!("atspi_edit: runtime: {e}"),
        };
        match rt.block_on(until_cancelled(
            generation,
            &GENERATION,
            MAX_WATCH + WALK_TIMEOUT + SETTLE,
            run(&paste_id, &pasted, generation),
        )) {
            Some(Ok(Some((edited, reason)))) => {
                log::info!(
                    "atspi_edit: pasteId={paste_id} end={reason} changed={}",
                    edited != pasted
                );
                let payload = json!({ "payload": {
                    "pasteId": paste_id,
                    "pastedText": pasted,
                    "editedText": edited,
                    "contentObservationEndReason": reason,
                }});
                let report =
                    proto::request("UpdateEditedText", payload, &format!("edit-{paste_id}"));
                if GENERATION.load(Ordering::SeqCst) == generation && proto::encode(&report).is_ok()
                {
                    let _ = tx.send(report);
                }
            }
            Some(Err(e)) => log::debug!("atspi_edit: pasteId={paste_id} not observed: {e}"),
            _ => {}
        }
    });
}

/// `Ok(None)`: observation ended with nothing trustworthy to report.
async fn run(
    paste_id: &str,
    pasted: &str,
    generation: u64,
) -> Result<Option<(String, &'static str)>, String> {
    // GNOME/KWin identity providers do not start the AT-SPI focus tracker.
    let _ = atspi::connection::set_session_accessibility(true).await;
    let a11y = AccessibilityConnection::new()
        .await
        .map_err(|e| format!("a11y connect: {e}"))?;
    let conn = a11y.connection();

    // Prefer the widget the focus tracker last saw; fall back to walking the active window.
    let tracked = LAST_FOCUSED_WIDGET.lock().ok().and_then(|g| g.clone());
    let mut focused = None;
    if let Some(obj) = tracked {
        if let Ok(p) = obj.as_accessible_proxy(conn).await {
            if p.get_state()
                .await
                .map(|s| s.contains(State::Focused) && s.contains(State::Editable))
                .unwrap_or(false)
            {
                focused = Some(obj);
            }
        }
    }
    let focused = match focused {
        Some(f) => f,
        None => tokio::time::timeout(WALK_TIMEOUT, async {
            loop {
                if let Some(field) = find_focused_field(conn).await? {
                    break Ok::<_, String>(field);
                }
                // Toolkits register asynchronously when the a11y registry starts.
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .map_err(|_| "focus walk timed out".to_string())??,
    };
    log::debug!("atspi_edit: focused {}{}", focused.name, focused.path);
    let text = TextProxy::builder(conn)
        .destination(focused.name.as_str())
        .and_then(|b| b.path(focused.path.as_str()))
        .map_err(|e| format!("text proxy: {e}"))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| format!("no Text interface: {e}"))?;
    let acc = focused
        .as_accessible_proxy(conn)
        .await
        .map_err(|e| format!("accessible: {e}"))?;
    if !observable_role(acc.get_role().await.ok()) {
        return Ok(None);
    }

    let read = || async {
        if !observable_role(acc.get_role().await.ok()) {
            return Err("field role is unavailable or private".to_string());
        }
        let states = acc.get_state().await.map_err(|e| e.to_string())?;
        if !states.contains(State::Focused) || !states.contains(State::Editable) {
            return Err("field is not focused and editable".to_string());
        }
        let n = text.character_count().await.map_err(|e| e.to_string())?;
        if !(0..=MAX_FIELD_CHARS).contains(&n) {
            return Err("field too large".to_string());
        }
        text.get_text(0, n)
            .await
            .map(|s| s.chars().collect::<Vec<char>>())
            .map_err(|e| e.to_string())
    };

    // The paste can land a beat after Ctrl+V (Chromium reads the clipboard async),
    // so retry the baseline for a while before giving up.
    tokio::time::sleep(SETTLE).await;
    let pasted_chars: Vec<char> = pasted.chars().collect();
    let mut found = None;
    for attempt in 0..BASELINE_TRIES {
        let baseline = read().await?;
        let caret = text.caret_offset().await.unwrap_or(-1);
        if let Some(start) = locate(&baseline, &pasted_chars, caret) {
            found = Some((baseline, start));
            break;
        }
        if attempt + 1 < BASELINE_TRIES {
            tokio::time::sleep(POLL).await;
        }
    }
    let (baseline, start) = found.ok_or("pasted text not found in field")?;
    let before = baseline[..start].to_vec();
    let after = baseline[start + pasted_chars.len()..].to_vec();
    log::debug!(
        "atspi_edit: pasteId={paste_id} observing (before={} after={})",
        before.len(),
        after.len()
    );

    let began = Instant::now();
    let mut last_change = Instant::now();
    let mut current = baseline;
    let reason = loop {
        tokio::time::sleep(POLL).await;
        if GENERATION.load(Ordering::SeqCst) != generation {
            return Ok(None);
        }
        let still_focused = acc
            .get_state()
            .await
            .map(|s| s.contains(State::Focused))
            .unwrap_or(false);
        if !still_focused {
            break "focus_changed";
        }
        let now = match read().await {
            Ok(now) => now,
            Err(_) => return Ok(None),
        };
        if now != current {
            current = now;
            last_change = Instant::now();
        }
        if last_change.elapsed() >= IDLE_END {
            break "idle_timeout";
        }
        if began.elapsed() >= MAX_WATCH {
            break "max_duration";
        }
    };

    Ok(edited_span(&current, &before, &after).map(|edited| (edited, reason)))
}

// A stalled provider must not retain a watcher after another paste or the cap.
async fn until_cancelled<T>(
    generation: u64,
    counter: &AtomicU64,
    budget: Duration,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    let deadline = Instant::now() + budget;
    let mut future = std::pin::pin!(future);
    loop {
        if counter.load(Ordering::SeqCst) != generation || Instant::now() >= deadline {
            return None;
        }
        let wait = POLL.min(deadline.saturating_duration_since(Instant::now()));
        if let Ok(result) = tokio::time::timeout(wait, future.as_mut()).await {
            return (counter.load(Ordering::SeqCst) == generation).then_some(result);
        }
    }
}

fn edited_span(current: &[char], before: &[char], after: &[char]) -> Option<String> {
    if current.len() < before.len() + after.len()
        || !current.starts_with(before)
        || !current.ends_with(after)
    {
        return None;
    }
    let edited = &current[before.len()..current.len() - after.len()];
    if edited.len() > MAX_REPORT_CHARS {
        return None;
    }
    let edited: String = edited.iter().collect();
    (!edited.trim().is_empty()).then_some(edited)
}

/// Locate the focused field, preferring active windows without requiring that flag.
/// Web content (Chromium/Electron) nests the field far deeper than the shared
/// shallow walk used for selection reads reaches, so this one gets a larger
/// depth/node budget — affordable because it runs off the paste path.
async fn find_focused_field(conn: &zbus::Connection) -> Result<Option<atspi::ObjectRef>, String> {
    let root = desktop_root(conn).await?;
    let mut windows = Vec::new();
    for app in root.get_children().await.unwrap_or_default() {
        let Ok(app_proxy) = app.as_accessible_proxy(conn).await else {
            continue;
        };
        for win in app_proxy.get_children().await.unwrap_or_default() {
            let Ok(win_proxy) = win.as_accessible_proxy(conn).await else {
                continue;
            };
            let active = win_proxy
                .get_state()
                .await
                .map(|s| s.contains(State::Active))
                .unwrap_or(false);
            windows.push((active, win));
        }
    }
    // GTK4 dialogs can expose a focused field without an Active parent.
    windows.sort_by_key(|(active, _)| !active);
    let mut budget = WALK_NODES;
    for (_, win) in windows {
        if let Some(f) = descend_for_focus(conn, &win, 0, WALK_DEPTH, &mut budget, true).await {
            return Ok(Some(f));
        }
        if budget == 0 {
            break;
        }
    }
    Ok(None)
}

fn observable_role(role: Option<Role>) -> bool {
    matches!(role, Some(r) if r != Role::PasswordText && r != Role::Terminal)
}

/// Char offset of `needle` in `hay`: the occurrence ending at the caret if the
/// toolkit reports one (a paste leaves the caret right after the text), otherwise
/// require a unique occurrence. Ambiguous text is not safe to auto-learn from.
fn locate(hay: &[char], needle: &[char], caret: i32) -> Option<usize> {
    let n = needle.len();
    if n == 0 || n > hay.len() {
        return None;
    }
    if caret >= 0 {
        let start = (caret as usize).checked_sub(n)?;
        return (hay.get(start..start + n) == Some(needle)).then_some(start);
    }
    let mut matches = hay
        .windows(n)
        .enumerate()
        .filter(|(_, span)| *span == needle);
    let (start, _) = matches.next()?;
    matches.next().is_none().then_some(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn caret_anchor_beats_earlier_duplicate() {
        let hay = c("hi hi");
        assert_eq!(locate(&hay, &c("hi"), 5), Some(3));
        assert_eq!(locate(&hay, &c("hi"), 2), Some(0));
    }

    #[test]
    fn unique_occurrence_without_caret_and_misses() {
        assert_eq!(locate(&c("a b c"), &c("b"), -1), Some(2));
        assert_eq!(locate(&c("abc"), &c("x"), 3), None);
        assert_eq!(locate(&c("é日本 x"), &c("日本"), 3), Some(1));
    }

    #[test]
    fn skips_password_terminal_and_unknown_roles() {
        assert!(!observable_role(Some(Role::PasswordText)));
        assert!(!observable_role(Some(Role::Terminal)));
        assert!(!observable_role(None));
        assert!(observable_role(Some(Role::Entry)));
        assert!(observable_role(Some(Role::Text)));
    }

    #[test]
    fn ambiguous_or_mismatched_caret_is_not_guessed() {
        assert_eq!(locate(&c("hi hi"), &c("hi"), -1), None);
        assert_eq!(locate(&c("hi later"), &c("hi"), 8), None);
    }

    #[test]
    fn extracts_only_the_edited_span_with_intact_anchors() {
        assert_eq!(
            edited_span(&c("before 日本 after"), &c("before "), &c(" after")),
            Some("日本".into())
        );
        assert_eq!(
            edited_span(&c("changed word after"), &c("before "), &c(" after")),
            None
        );
        assert_eq!(
            edited_span(&c("before word changed"), &c("before "), &c(" after")),
            None
        );
        assert_eq!(
            edited_span(&c("short"), &c("long before"), &c("after")),
            None
        );
        assert_eq!(edited_span(&c("  "), &[], &[]), None);
        assert_eq!(
            edited_span(&vec!['x'; MAX_REPORT_CHARS + 1], &[], &[]),
            None
        );
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn stalled_observation_is_bounded() {
        let counter = AtomicU64::new(1);
        let result = runtime().block_on(until_cancelled(
            1,
            &counter,
            Duration::from_millis(5),
            std::future::pending::<()>(),
        ));
        assert_eq!(result, None);
    }

    #[test]
    fn superseded_observation_is_discarded_even_when_ready() {
        let counter = AtomicU64::new(1);
        let result = runtime().block_on(until_cancelled(
            1,
            &counter,
            Duration::from_secs(1),
            async {
                counter.store(2, Ordering::SeqCst);
                "stale edit"
            },
        ));
        assert_eq!(result, None);
        assert_eq!(
            runtime().block_on(until_cancelled(
                2,
                &counter,
                Duration::from_secs(1),
                async { "current edit" },
            )),
            Some("current edit")
        );
    }

    #[test]
    fn paste_without_an_id_still_cancels_previous_observation() {
        let previous = GENERATION.load(Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        watch(String::new(), "next paste".into(), tx);
        assert_eq!(GENERATION.load(Ordering::SeqCst), previous + 1);
        assert!(rx.try_recv().is_err());
    }

    struct FocusFixture {
        editable: bool,
        children: Vec<atspi::ObjectRef>,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl FocusFixture {
        fn get_state(&self) -> atspi::StateSet {
            let mut states = atspi::StateSet::from(State::Focused);
            if self.editable {
                states.insert(State::Editable);
            }
            states
        }

        fn get_children(&self) -> Vec<atspi::ObjectRef> {
            self.children.clone()
        }
    }

    async fn check_noneditable_focus() {
        let server = zbus::Connection::session().await.unwrap();
        let client = zbus::Connection::session().await.unwrap();
        let child = atspi::ObjectRef {
            name: server.unique_name().unwrap().clone(),
            path: "/fixture/child".try_into().unwrap(),
        };
        let parent = atspi::ObjectRef {
            name: child.name.clone(),
            path: "/fixture/parent".try_into().unwrap(),
        };
        server
            .object_server()
            .at(
                child.path.clone(),
                FocusFixture {
                    editable: true,
                    children: vec![],
                },
            )
            .await
            .unwrap();
        server
            .object_server()
            .at(
                parent.path.clone(),
                FocusFixture {
                    editable: false,
                    children: vec![child.clone()],
                },
            )
            .await
            .unwrap();
        assert_eq!(
            descend_for_focus(&client, &parent, 0, 8, &mut 10, true).await,
            Some(child)
        );
        // Selection reads must still support focused, read-only widgets.
        assert_eq!(
            descend_for_focus(&client, &parent, 0, 8, &mut 10, false).await,
            Some(parent)
        );
    }

    #[test]
    #[ignore = "requires gtk4_edit_test.py's isolated Zenity fixture"]
    fn gtk4_observation_without_focus_tracker() {
        assert_eq!(
            std::env::var("WISPR_GTK4_TEST_ISOLATED").as_deref(),
            Ok("1")
        );
        let _ = env_logger::try_init();
        runtime().block_on(check_noneditable_focus());
        let generation = GENERATION.load(Ordering::SeqCst);
        let result = runtime().block_on(until_cancelled(
            generation,
            &GENERATION,
            Duration::from_secs(15),
            run("gtk4-regression", "hello wisper world", generation),
        ));
        assert_eq!(
            result.unwrap().unwrap(),
            Some(("hello whisper world".into(), "focus_changed"))
        );
    }
}
