//! Normalize Linux identity and enrich app IPC without delaying key events.
use serde_json::Value;
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
    loop {
        let (seq, mut message, polling) = match input.recv_timeout(Duration::from_millis(250)) {
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
            let title = payload["windowTitle"].as_str().unwrap_or("");
            let url = read_url(&id, title);
            payload["url"] = Value::String(url.unwrap_or_default());
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
