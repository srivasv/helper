//! Normalize Linux identity and enrich app IPC without delaying key events.
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

pub static FOCUS_ACTIVE: AtomicBool = AtomicBool::new(true);

pub fn identity(id: &str) -> &str {
    let key = id.trim().to_ascii_lowercase();
    let key = key.strip_suffix(".desktop").unwrap_or(&key);
    match key {
        "slack" | "com.slack.slack" | "com.tinyspeck.slackmacgap" => "Slack",
        "telegram"
        | "telegram-desktop"
        | "telegramdesktop"
        | "org.telegram.desktop"
        | "org.telegram" => "Telegram",
        "whatsapp" | "whatsapp-for-linux" | "com.github.eneshecan.whatsappforlinux" => "WhatsApp",
        "teams"
        | "msteams"
        | "ms-teams"
        | "teams-for-linux"
        | "com.github.ismaelmartinez.teams_for_linux" => "msteams",
        "thunderbird" | "org.mozilla.thunderbird" => "thunderbird",
        "code" | "com.visualstudio.code" => "Code",
        "cursor" | "com.todesktop.230313mzl4w4u92" => "Cursor",
        "chrome" | "google-chrome" | "google-chrome-stable" | "com.google.chrome" => {
            "com.google.Chrome"
        }
        "chromium" | "chromium-browser" | "org.chromium.chromium" => "com.google.Chrome",
        "vivaldi"
        | "vivaldi-bin"
        | "vivaldi-stable"
        | "vivaldi-snapshot"
        | "com.vivaldi.vivaldi" => "com.vivaldi.Vivaldi",
        "firefox" | "firefox-esr" | "org.mozilla.firefox" => "org.mozilla.firefox",
        "brave" | "brave-browser" | "com.brave.browser" => "com.brave.Browser",
        "msedge" | "microsoft-edge" | "microsoft-edge-stable" => "com.microsoft.edgemac",
        _ => id,
    }
}

pub fn is_browser(id: &str) -> bool {
    matches!(
        identity(id),
        "com.google.Chrome"
            | "org.chromium.Chromium"
            | "org.mozilla.firefox"
            | "com.vivaldi.Vivaldi"
            | "com.brave.Browser"
            | "com.microsoft.edgemac"
    )
}

fn app_payload(message: &mut Value) -> Option<&mut Value> {
    if message.get("HelperAPIRequest").is_some() {
        message
            .get_mut("HelperAPIRequest")?
            .get_mut("AppInfoUpdate")?
            .get_mut("payload")
    } else {
        let response = message.get_mut("HelperAPIResponse")?;
        let key = if response.get("ActiveAppInfo").is_some() {
            "ActiveAppInfo"
        } else {
            "AppInfo"
        };
        response.get_mut(key)?.get_mut("payload")
    }
}

fn normalize(payload: &mut Value) {
    if let Some(id) = payload.get("bundleId").and_then(Value::as_str) {
        payload["bundleId"] = Value::String(identity(id).into());
    }
}

/// Route app messages to a bounded URL reader; keyboard and correction IPC bypass it.
pub fn route(output: mpsc::Sender<Value>) -> mpsc::Sender<Value> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok();
    route_with_reader(output, move |id, _| {
        runtime
            .as_ref()
            .and_then(|rt| rt.block_on(crate::browser_url::read(id)))
    })
}

fn route_with_reader(
    output: mpsc::Sender<Value>,
    read_url: impl FnMut(&str, &str) -> Option<String> + Send + 'static,
) -> mpsc::Sender<Value> {
    let (input, incoming) = mpsc::channel::<Value>();
    let (apps, app_messages) = mpsc::channel();
    let generation = Arc::new(AtomicU64::new(0));
    let current = generation.clone();
    let app_output = output.clone();
    std::thread::spawn(move || app_loop(app_messages, app_output, current, read_url));
    std::thread::spawn(move || {
        for mut message in incoming {
            if let Some(payload) = app_payload(&mut message) {
                normalize(payload);
                let seq = if message["HelperAPIRequest"].get("AppInfoUpdate").is_some() {
                    generation.fetch_add(1, Ordering::SeqCst) + 1
                } else {
                    generation.load(Ordering::SeqCst)
                };
                let _ = apps.send((seq, message));
            } else {
                if let Some(list) = message
                    .get_mut("HelperAPIResponse")
                    .and_then(|v| v.get_mut("RunningApps"))
                    .and_then(|v| v.get_mut("payload"))
                    .and_then(|v| v.get_mut("apps"))
                    .and_then(Value::as_array_mut)
                {
                    for app in list {
                        normalize(app);
                    }
                }
                if output.send(message).is_err() {
                    break;
                }
            }
        }
    });
    input
}

fn app_loop(
    input: mpsc::Receiver<(u64, Value)>,
    output: mpsc::Sender<Value>,
    generation: Arc<AtomicU64>,
    mut read_url: impl FnMut(&str, &str) -> Option<String>,
) {
    let mut last: Option<(u64, Value)> = None;
    let mut sequence = 0u64;
    let mut pending = VecDeque::new();
    loop {
        let incoming = pending
            .pop_front()
            .map(Ok)
            .unwrap_or_else(|| input.recv_timeout(Duration::from_millis(250)));
        let (seq, mut message, polling) = match incoming {
            Ok((seq, message)) => (seq, message, false),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some((seq, message)) = &last else {
                    continue;
                };
                if !FOCUS_ACTIVE.load(Ordering::SeqCst) {
                    continue;
                }
                (*seq, message.clone(), true)
            }
        };
        let event = message["HelperAPIRequest"].get("AppInfoUpdate").is_some();
        if event && seq != generation.load(Ordering::SeqCst) {
            continue;
        }
        let payload = app_payload(&mut message).expect("routed app message");
        let id = payload["bundleId"].as_str().unwrap_or("").to_string();
        let browser = is_browser(&id);
        if browser {
            let title = payload["windowTitle"].as_str().unwrap_or("").to_owned();
            let url = read_url(&id, &title).unwrap_or_default();
            payload["url"] = Value::String(url.clone());
            // Queries queued during this scan can use its freshly validated result.
            // Never reuse it across focus generations or a different window title.
            for _ in 0..64 {
                let Ok((query_seq, mut query)) = input.try_recv() else {
                    break;
                };
                let response = query.get("HelperAPIResponse").is_some();
                let query_payload = app_payload(&mut query).expect("routed app message");
                if response
                    && query_seq == seq
                    && seq == generation.load(Ordering::SeqCst)
                    && query_payload["bundleId"].as_str() == Some(id.as_str())
                    && query_payload["windowTitle"].as_str() == Some(title.as_str())
                {
                    query_payload["url"] = Value::String(url.clone());
                    if output.send(query).is_err() {
                        return;
                    }
                } else {
                    pending.push_back((query_seq, query));
                }
            }
        }
        if seq != generation.load(Ordering::SeqCst) {
            if event {
                continue;
            }
            for key in ["appName", "bundleId", "url", "windowTitle"] {
                if let Some(value) = payload.get_mut(key) {
                    *value = Value::String(String::new());
                }
            }
        }
        if event && !FOCUS_ACTIVE.load(Ordering::SeqCst) {
            continue;
        }
        if polling && last.as_ref().is_some_and(|(_, old)| old == &message) {
            continue;
        }
        if event {
            last = if browser {
                Some((seq, message.clone()))
            } else {
                None
            };
            if polling {
                sequence += 1;
                message["HelperAPIRequest"]["uuid"] =
                    Value::String(format!("browser-{}-{sequence}", std::process::id()));
            }
        }
        if output.send(message).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(id: &str) -> Value {
        crate::proto::request(
            "AppInfoUpdate",
            serde_json::json!({"payload": {
                "appName": id, "bundleId": id, "windowTitle": "same", "url": ""
            }}),
            "focus",
        )
    }

    #[test]
    fn slow_url_probe_does_not_block_keys_and_stale_focus_is_discarded() {
        let (output, received) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let input = route_with_reader(output, move |_, _| {
            started.send(()).unwrap();
            blocked.recv().unwrap();
            Some("mail.google.com".into())
        });
        input.send(event("firefox")).unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        input.send(event("org.telegram.desktop")).unwrap();
        let key = crate::proto::request(
            "KeypressEvent",
            serde_json::json!({"payload":{"key":65}}),
            "key",
        );
        input.send(key.clone()).unwrap();
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), key);
        release.send(()).unwrap();
        let next = received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            next["HelperAPIRequest"]["AppInfoUpdate"]["payload"]["bundleId"],
            "Telegram"
        );
        assert!(received.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn overlapping_query_shares_fresh_focus_scan() {
        let (output, received) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let calls = Arc::new(AtomicU64::new(0));
        let reads = calls.clone();
        let input = route_with_reader(output, move |_, _| {
            if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                started.send(()).unwrap();
                blocked.recv().unwrap();
            }
            Some("linkedin.com".into())
        });
        input.send(event("google-chrome")).unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let response = crate::proto::response(
            "ActiveAppInfo",
            serde_json::json!({"payload": {"appName":"google-chrome",
                "bundleId":"google-chrome","windowTitle":"same","url":""}}),
            "query-overlap",
        );
        input.send(response).unwrap();
        let key = crate::proto::request("KeypressEvent", serde_json::json!({}), "barrier");
        input.send(key.clone()).unwrap();
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), key);
        release.send(()).unwrap();
        let mut reply = None;
        for _ in 0..2 {
            let message = received.recv_timeout(Duration::from_secs(1)).unwrap();
            if message["HelperAPIResponse"]["uuid"] == "query-overlap" {
                reply = Some(message);
            }
        }
        assert_eq!(
            reply.unwrap()["HelperAPIResponse"]["ActiveAppInfo"]["payload"]["url"],
            "linkedin.com"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a queued query must not repeat the same scan"
        );
    }

    #[test]
    fn overlapping_query_does_not_share_another_window() {
        let (output, received) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let input = route_with_reader(output, move |_, title| {
            if title == "same" {
                let _ = started.send(());
                let _ = blocked.recv();
                Some("linkedin.com".into())
            } else {
                Some("app.slack.com".into())
            }
        });
        input.send(event("google-chrome")).unwrap();
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        input
            .send(crate::proto::response(
                "ActiveAppInfo",
                serde_json::json!({"payload": {"bundleId":"google-chrome",
                "windowTitle":"another", "url":""}}),
                "other-window",
            ))
            .unwrap();
        let barrier = crate::proto::request("KeypressEvent", serde_json::json!({}), "barrier");
        input.send(barrier.clone()).unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            barrier
        );
        release.send(()).unwrap();
        let mut response = None;
        for _ in 0..2 {
            let message = received.recv_timeout(Duration::from_secs(1)).unwrap();
            if message["HelperAPIResponse"]["uuid"] == "other-window" {
                response = Some(message);
            }
        }
        assert_eq!(
            response.unwrap()["HelperAPIResponse"]["ActiveAppInfo"]["payload"]["url"],
            "app.slack.com"
        );
    }

    #[test]
    fn polling_updates_same_title_and_clears_failed_urls() {
        let (output, received) = mpsc::channel();
        let mut reads = 0;
        let input = route_with_reader(output, move |_, _| {
            reads += 1;
            match reads {
                1 => Some("mail.google.com".into()),
                2 => Some("app.slack.com".into()),
                _ => None,
            }
        });
        input.send(event("firefox")).unwrap();
        for url in ["mail.google.com", "app.slack.com", ""] {
            let next = received.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(
                next["HelperAPIRequest"]["AppInfoUpdate"]["payload"]["url"],
                url
            );
        }
        assert!(received.recv_timeout(Duration::from_millis(350)).is_err());
        drop(input);
    }

    #[test]
    fn response_uuid_and_unrelated_envelopes_survive_routing() {
        let (output, received) = mpsc::channel();
        let input = route_with_reader(output, |_, _| None);
        let response = crate::proto::response(
            "ActiveAppInfo",
            serde_json::json!({"payload": {
                "appName":"Telegram", "bundleId":"org.telegram.desktop", "url":""
            }}),
            "query-42",
        );
        input.send(response).unwrap();
        let got = received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(got["HelperAPIResponse"]["uuid"], "query-42");
        assert_eq!(
            got["HelperAPIResponse"]["ActiveAppInfo"]["payload"]["bundleId"],
            "Telegram"
        );
        let correction = crate::proto::request(
            "UpdateEditedText",
            serde_json::json!({"payload":{"pasteId":"x"}}),
            "edit",
        );
        input.send(correction.clone()).unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            correction
        );
    }

    #[test]
    #[ignore = "requires a focused browser and WISPR_EXPECTED_BROWSER/HOST"]
    fn live_browser_message_worker() {
        let browser = std::env::var("WISPR_EXPECTED_BROWSER").unwrap();
        let host = std::env::var("WISPR_EXPECTED_HOST").unwrap();
        let (output, received) = mpsc::channel();
        let input = route(output);
        let mut failures = 0;
        for i in 0..20 {
            let uid = format!("live-query-{i}");
            let response = crate::proto::response(
                "ActiveAppInfo",
                serde_json::json!({"payload":{"bundleId":browser,"windowTitle":"same","url":""}}),
                &uid,
            );
            let started = std::time::Instant::now();
            input.send(response).unwrap();
            let got = received.recv_timeout(Duration::from_secs(2)).unwrap();
            let elapsed = started.elapsed().as_millis();
            let matched = got["HelperAPIResponse"]["ActiveAppInfo"]["payload"]["url"] == host;
            eprintln!("Worker sample: matched={matched} elapsed_ms={elapsed}");
            failures += usize::from(!matched || elapsed >= 300);
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(failures, 0);
    }

    #[test]
    fn vivaldi_identifiers_enable_browser_url_detection() {
        for id in [
            "vivaldi",
            "vivaldi-bin",
            "vivaldi-stable",
            "vivaldi-snapshot",
            "com.vivaldi.Vivaldi",
        ] {
            assert_eq!(identity(id), "com.vivaldi.Vivaldi");
            assert!(is_browser(id));
        }
        assert!(!is_browser("not-vivaldi"));
    }

    #[test]
    fn exact_linux_aliases_preserve_unknown_apps() {
        for (id, expected) in [
            ("org.telegram.desktop.desktop", "Telegram"),
            ("org.telegram.desktop", "Telegram"),
            ("TelegramDesktop", "Telegram"),
            ("org.mozilla.Thunderbird.desktop", "thunderbird"),
            ("com.slack.Slack.desktop", "Slack"),
            ("com.visualstudio.code.desktop", "Code"),
            ("org.chromium.Chromium.desktop", "com.google.Chrome"),
            ("not-slack", "not-slack"),
            ("", ""),
        ] {
            assert_eq!(identity(id), expected);
        }
    }
}
