//! Fresh textbox context for each dictation; never reuse the previous field.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::ObjectRefExt;
use atspi::{ObjectRef, State};
use serde_json::{json, Value};

use super::{EventSink, Selection};

static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn start(tx: EventSink, uuid: String, window_title: String) {
    start_with_reader(tx, uuid, move || read(Some(&window_title), false));
}

fn start_with_reader(
    tx: EventSink,
    uuid: String,
    read: impl FnOnce() -> Result<Selection, String> + Send + 'static,
) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let payload = payload(read());
        // A slow accessibility provider must not populate a later dictation.
        if generation != GENERATION.load(Ordering::SeqCst) {
            return;
        }
        if tx
            .send(crate::proto::request(
                "TextBoxInfo",
                payload,
                &format!("textbox-{uuid}"),
            ))
            .is_err()
        {
            log::error!("textbox context send failed: uuid={uuid}");
        }
    });
}

pub fn query(tx: EventSink, uuid: String, window_title: String) {
    std::thread::spawn(move || {
        let payload = payload(read(Some(&window_title), true));
        if tx
            .send(crate::proto::response("TextBoxInfo", payload, &uuid))
            .is_err()
        {
            log::error!("textbox context send failed: uuid={uuid}");
        }
    });
}

fn payload(result: Result<Selection, String>) -> Value {
    let available = result.is_ok();
    let text = result.unwrap_or_else(|error| {
        log::info!("textbox context unavailable: {error}");
        Selection::default()
    });
    json!({"payload": {
        "accessibilityIsFunctioning": available,
        "couldNotGetTextBoxInfo": !available,
        "isEditable": available,
        "beforeText": text.before_text,
        "selectedText": text.selected_text,
        "afterText": text.after_text,
        "contents": text.contents,
    }})
}

fn read(window_title: Option<&str>, selection_fallback: bool) -> Result<Selection, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            let _ = atspi::connection::set_session_accessibility(true).await;
            let a11y = AccessibilityConnection::new()
                .await
                .map_err(|e| e.to_string())?;
            let conn = a11y.connection();
            read_on_connection(conn, window_title, selection_fallback).await
        })
        .await
        .map_err(|_| "textbox context timed out".to_string())?
    })
}

pub(super) async fn read_on_connection(
    conn: &zbus::Connection,
    window_title: Option<&str>,
    selection_fallback: bool,
) -> Result<Selection, String> {
    let title = window_title.filter(|title| !title.is_empty());
    let field = super::atspi_edit::find_focused_field(conn, title)
        .await?
        .ok_or("no focused editor")?;
    // Native focus is authoritative; Electron can retain stale AT-SPI Active flags.
    if title.is_none() {
        require_active_window(conn, &field).await?;
    }
    let context = match super::atspi_text::context(conn, &field).await {
        Ok(context) => context,
        Err(_) if selection_fallback => {
            // Transform queries need the whole highlight, beyond dictation's cap.
            let snapshot = super::atspi_text::read_selected(conn, &field).await?;
            Selection {
                selected_text: snapshot.chars.into_iter().collect(),
                ..Selection::default()
            }
        }
        Err(error) => return Err(error),
    };
    if super::atspi_edit::find_focused_field(conn, title)
        .await?
        .as_ref()
        != Some(&field)
    {
        return Err("focused editor changed".into());
    }
    if title.is_none() {
        require_active_window(conn, &field).await?;
    }
    Ok(context)
}

async fn require_active_window(conn: &zbus::Connection, field: &ObjectRef) -> Result<(), String> {
    let mut obj = field.clone();
    for _ in 0..64 {
        let proxy = obj
            .as_accessible_proxy(conn)
            .await
            .map_err(|e| e.to_string())?;
        if proxy
            .get_state()
            .await
            .map_err(|e| e.to_string())?
            .contains(State::Active)
        {
            return Ok(());
        }
        let parent = proxy.parent().await.map_err(|e| e.to_string())?;
        if parent == obj {
            break;
        }
        obj = parent;
    }
    Err("editor has no active window".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a live editor with the caret inside a sentence"]
    fn live_mid_sentence_context() {
        let title = std::env::var("WISPR_CONTEXT_TEST_WINDOW").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        start(tx, "live-context".into(), title);
        let message = rx.recv_timeout(Duration::from_secs(4)).unwrap();
        let context = &message["HelperAPIRequest"]["TextBoxInfo"]["payload"];
        assert_eq!(context["couldNotGetTextBoxInfo"], false);
        let before = context["beforeText"].as_str().unwrap();
        let selected = context["selectedText"].as_str().unwrap();
        let after = context["afterText"].as_str().unwrap();
        eprintln!(
            "context lengths: before={} selected={} after={}",
            before.chars().count(),
            selected.chars().count(),
            after.chars().count(),
        );
        assert!(!before.is_empty(), "missing preceding sentence");
        assert!(!after.is_empty(), "missing following sentence");
        assert!(selected.is_empty(), "expected an insertion caret");
        crate::proto::encode(&message).expect("context must fit the IPC frame");
    }

    struct WindowFixture {
        active: bool,
        parent: ObjectRef,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl WindowFixture {
        fn get_state(&self) -> atspi::StateSet {
            if self.active {
                State::Active.into()
            } else {
                State::Focused.into()
            }
        }
        #[zbus(property)]
        fn parent(&self) -> ObjectRef {
            self.parent.clone()
        }
    }

    #[test]
    #[ignore = "requires a private dbus-run-session"]
    fn context_requires_an_active_window() {
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
                let client = zbus::Connection::session().await.unwrap();
                for active in [false, true] {
                    let obj = ObjectRef {
                        name: server.unique_name().unwrap().clone(),
                        path: if active { "/active" } else { "/inactive" }
                            .try_into()
                            .unwrap(),
                    };
                    server
                        .object_server()
                        .at(
                            obj.path.clone(),
                            WindowFixture {
                                active,
                                parent: obj.clone(),
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(require_active_window(&client, &obj).await.is_ok(), active);
                }
            });
    }

    #[test]
    fn start_sends_fresh_context_and_discards_a_superseded_read() {
        let (tx, rx) = std::sync::mpsc::channel();
        for before in ["We were discussing ", "We were discussing Linux "] {
            start_with_reader(tx.clone(), "dictation".into(), move || {
                Ok(Selection {
                    before_text: before.into(),
                    ..Selection::default()
                })
            });
            let message = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(
                message["HelperAPIRequest"]["TextBoxInfo"]["payload"]["beforeText"],
                before
            );
        }
        let (started, ready) = std::sync::mpsc::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        start_with_reader(tx.clone(), "cancelled".into(), move || {
            started.send(()).unwrap();
            waiting.recv().unwrap();
            Ok(Selection {
                before_text: "stale text".into(),
                ..Selection::default()
            })
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        start_with_reader(tx, "new-session".into(), || Ok(Selection::default()));
        let current = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(current["HelperAPIRequest"]["uuid"], "textbox-new-session");
        release.send(()).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn unavailable_context_clears_previous_text() {
        let good = payload(Ok(Selection {
            before_text: "a sentence in progress ".into(),
            ..Selection::default()
        }));
        assert_eq!(good["payload"]["beforeText"], "a sentence in progress ");
        assert_eq!(good["payload"]["couldNotGetTextBoxInfo"], false);
        let missing = payload(Err("private field".into()));
        assert_eq!(missing["payload"]["beforeText"], "");
        assert_eq!(missing["payload"]["couldNotGetTextBoxInfo"], true);
        assert_eq!(missing["payload"]["isEditable"], false);
    }
}
