//! Read only the active browser's top-level document URL through AT-SPI.
use crate::app_context::identity;
use atspi::{
    connection::AccessibilityConnection,
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt},
        document::DocumentProxy,
    },
    ObjectRef, Role, State,
};
use std::collections::{HashSet, VecDeque};
use std::time::Duration;

/// Only return a recognized site hostname, never paths, queries, or page content.
fn site(raw: &str) -> Option<String> {
    let (_, rest) = raw.split_once("://")?;
    if !raw.starts_with("https://") && !raw.starts_with("http://") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') || authority.contains('\\') {
        return None;
    }
    let host = if let Some((host, port)) = authority.split_once(':') {
        if port.is_empty() || !port.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        host
    } else {
        authority
    };
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    match host.as_str() {
        "mail.google.com" => Some("mail.google.com".into()),
        "linkedin.com" | "www.linkedin.com" => Some("linkedin.com".into()),
        "web.whatsapp.com" => Some("web.whatsapp.com".into()),
        "web.telegram.org" => Some("web.telegram.org".into()),
        "app.slack.com" => Some("app.slack.com".into()),
        _ if host.ends_with(".slack.com") && host.split('.').all(valid_label) => {
            Some("app.slack.com".into())
        }
        _ => None,
    }
}
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn is_browser_footer(url: &str) -> bool {
    matches!(url, "chrome://newtab-footer/" | "chrome://newtab-footer")
}

fn is_vivaldi_ui(browser: &str, url: &str) -> bool {
    browser == "com.vivaldi.Vivaldi"
        && url.starts_with("chrome-extension://mpognobbkildjkofajifpdfhcoklimli/")
}

pub async fn read(browser: &str) -> Option<String> {
    if !["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()))
    {
        return None;
    }
    let result = tokio::time::timeout(Duration::from_millis(250), read_active(browser)).await;
    result.ok().flatten()
}

async fn read_active(browser: &str) -> Option<String> {
    let a11y = AccessibilityConnection::new().await.ok()?;
    let conn = a11y.connection();
    let root = AccessibleProxy::builder(conn)
        .destination("org.a11y.atspi.Registry")
        .ok()?
        .path("/org/a11y/atspi/accessible/root")
        .ok()?
        .build()
        .await
        .ok()?;
    let mut windows = Vec::new();
    let mut apps = root.get_children().await.ok()?.into_iter().take(128);
    let mut reads = tokio::task::JoinSet::new();
    loop {
        while reads.len() < 16 {
            let Some(app) = apps.next() else { break };
            let conn = conn.clone();
            let browser = browser.to_owned();
            reads.spawn(async move { browser_windows(&conn, app, &browser).await });
        }
        let Some(result) = reads.join_next().await else {
            break;
        };
        windows.extend(result.ok()??);
        if windows.len() > 1 {
            return None;
        }
    }
    // More than one supposedly-active window is ambiguous, even with equal titles.
    if windows.len() != 1 {
        return None;
    }
    read_window(conn, &windows[0], browser).await
}

async fn browser_windows(
    conn: &zbus::Connection,
    app: ObjectRef,
    browser: &str,
) -> Option<Vec<ObjectRef>> {
    let proxy = app.as_accessible_proxy(conn).await.ok()?;
    // The bus daemon identifies the process without waiting on an unrelated UI.
    let (process, exe) =
        crate::backend::atspi_app::identity_from_sender(conn, app.name.as_str()).await;
    if identity(&exe) != browser && identity(&process) != browser {
        let name = tokio::time::timeout(Duration::from_millis(10), proxy.name())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        if identity(&name) != browser {
            return Some(Vec::new());
        }
    }
    let mut windows = Vec::new();
    for window in proxy
        .get_children()
        .await
        .unwrap_or_default()
        .into_iter()
        .take(64)
    {
        let Ok(p) = window.as_accessible_proxy(conn).await else {
            continue;
        };
        if !matches!(p.get_role().await.ok(), Some(Role::Frame | Role::Window)) {
            continue;
        }
        let state = p.get_state().await.ok()?;
        if state.contains(State::Active) && !state.contains(State::Defunct) {
            windows.push(window);
        }
    }
    Some(windows)
}

async fn read_window(conn: &zbus::Connection, window: &ObjectRef, browser: &str) -> Option<String> {
    let (_, url) = document_in_window(conn, window, browser).await?;
    site(&url)
}

pub(crate) async fn document_in_window(
    conn: &zbus::Connection,
    window: &ObjectRef,
    browser: &str,
) -> Option<(ObjectRef, String)> {
    // Compositor caches may retain the previous tab's title. Trust the active
    // accessible window, then verify its title did not change during this read.
    let title = window
        .as_accessible_proxy(conn)
        .await
        .ok()?
        .name()
        .await
        .ok()?;
    let mut pending = VecDeque::from([(window.clone(), 0usize)]);
    let mut seen = HashSet::new();
    let mut document: Option<(ObjectRef, String, String)> = None;
    let mut reads = tokio::task::JoinSet::new();
    loop {
        while reads.len() < 16 {
            let Some((obj, depth)) = pending.pop_front() else {
                break;
            };
            if depth > 32 || seen.len() >= 300 || !seen.insert(obj.clone()) {
                return None;
            }
            let conn = conn.clone();
            let browser = browser.to_owned();
            reads.spawn(async move { (depth, read_node(&conn, obj, &browser).await) });
        }
        let Some(result) = reads.join_next().await else {
            break;
        };
        let (depth, node) = result.ok()?;
        match node? {
            BrowserNode::Skip => {}
            BrowserNode::Document(obj, attribute, url) => {
                if document.replace((obj, attribute, url)).is_some() {
                    return None;
                }
            }
            BrowserNode::Children(children) => {
                for child in children {
                    if child.name != window.name {
                        return None;
                    }
                    pending.push_back((child, depth + 1));
                }
            }
        }
    }
    let (doc, attribute, url) = document?;
    let p = DocumentProxy::builder(conn)
        .destination(doc.name.as_str())
        .ok()?
        .path(doc.path.as_str())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    let frame = window.as_accessible_proxy(conn).await.ok()?;
    if !frame.get_state().await.ok()?.contains(State::Active) {
        return None;
    }
    if frame.name().await.ok()? != title {
        return None;
    }
    if p.get_attribute_value(&attribute).await.ok()? != url {
        return None;
    }
    Some((doc, url))
}

enum BrowserNode {
    Skip,
    Children(Vec<ObjectRef>),
    Document(ObjectRef, String, String),
}

async fn read_node(conn: &zbus::Connection, obj: ObjectRef, browser: &str) -> Option<BrowserNode> {
    let p = obj.as_accessible_proxy(conn).await.ok()?;
    let state = p.get_state().await.ok()?;
    if state.contains(State::Defunct) || !state.contains(State::Showing) {
        return Some(BrowserNode::Skip);
    }
    let role = p.get_role().await.ok()?;
    // Browser tab selectors contain labels, not the selected page document.
    if role == Role::PageTabList {
        return Some(BrowserNode::Skip);
    }
    if matches!(role, Role::DocumentWeb | Role::DocumentFrame) {
        let doc = DocumentProxy::builder(conn)
            .destination(obj.name.as_str())
            .ok()?
            .path(obj.path.as_str())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        let mut attribute = "URI";
        let mut url = doc.get_attribute_value(attribute).await.unwrap_or_default();
        if url.is_empty() {
            attribute = "DocURL";
            url = doc.get_attribute_value(attribute).await.ok()?;
        }
        // Chrome exposes its new-tab footer alongside the active web page.
        if is_browser_footer(&url) {
            return Some(BrowserNode::Skip);
        }
        if !is_vivaldi_ui(browser, &url) {
            // Never descend into page iframes or read editable page contents.
            return Some(BrowserNode::Document(obj, attribute.into(), url));
        }
        // Vivaldi's built-in interface wraps the actual page document.
    }
    Some(BrowserNode::Children(p.get_children().await.ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Accessible {
        role: Role,
        children: Vec<ObjectRef>,
        delay_ms: u64,
        name: &'static str,
        name_delay_ms: u64,
    }
    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl Accessible {
        #[zbus(property)]
        async fn name(&self) -> &str {
            tokio::time::sleep(Duration::from_millis(self.name_delay_ms)).await;
            self.name
        }
        fn get_role(&self) -> Role {
            self.role
        }
        async fn get_state(&self) -> atspi::StateSet {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            let mut states = atspi::StateSet::from(State::Active);
            states.insert(State::Showing);
            states.insert(State::Visible);
            states
        }
        fn get_children(&self) -> Vec<ObjectRef> {
            self.children.clone()
        }
    }
    struct Document(std::sync::Arc<std::sync::Mutex<String>>);
    #[zbus::interface(name = "org.a11y.atspi.Document")]
    impl Document {
        fn get_attribute_value(&self, name: &str) -> String {
            if name == "URI" {
                self.0.lock().unwrap().clone()
            } else {
                String::new()
            }
        }
    }

    struct Bus(String);
    #[zbus::interface(name = "org.a11y.Bus")]
    impl Bus {
        fn get_address(&self) -> &str {
            &self.0
        }
    }

    #[test]
    #[ignore = "requires a private dbus-run-session"]
    fn browser_document_boundaries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let conn = zbus::Connection::session().await.unwrap();
            let client = zbus::Connection::session().await.unwrap();
            let object = |path: &str| ObjectRef {
                name: conn.unique_name().unwrap().to_owned(),
                path: path.try_into().unwrap(),
            };
            let frame = object("/frame");
            let page = object("/page");
            let extra = object("/extra");
            let wrapper = object("/wrapper");
            let tabs = object("/tabs");
            for (obj, role, children) in [
                (&frame, Role::Frame, vec![wrapper.clone(), extra.clone()]),
                (
                    &wrapper,
                    Role::DocumentWeb,
                    vec![tabs.clone(), page.clone()],
                ),
                (
                    &tabs,
                    Role::PageTabList,
                    vec![object("/unreadable_tab_label")],
                ),
                (
                    &page,
                    Role::DocumentWeb,
                    vec![object("/must_not_read_page_contents")],
                ),
                (&extra, Role::DocumentWeb, vec![]),
            ] {
                conn.object_server()
                    .at(
                        obj.path.as_str(),
                        Accessible {
                            role,
                            children,
                            delay_ms: 0,
                            name: "same title",
                            name_delay_ms: 0,
                        },
                    )
                    .await
                    .unwrap();
            }
            let wrapper_url = std::sync::Arc::new(std::sync::Mutex::new(
                "chrome-extension://mpognobbkildjkofajifpdfhcoklimli/browser.html".into(),
            ));
            let extra_url =
                std::sync::Arc::new(std::sync::Mutex::new("chrome://newtab-footer/".into()));
            for (obj, url) in [
                (&wrapper, wrapper_url.clone()),
                (
                    &page,
                    std::sync::Arc::new(std::sync::Mutex::new("https://web.whatsapp.com/".into())),
                ),
                (&extra, extra_url.clone()),
            ] {
                conn.object_server()
                    .at(obj.path.as_str(), Document(url))
                    .await
                    .unwrap();
            }
            assert_eq!(
                read_window(&client, &frame, "com.vivaldi.Vivaldi")
                    .await
                    .as_deref(),
                Some("web.whatsapp.com")
            );
            // An unrelated extension is not a browser wrapper.
            assert_eq!(
                read_window(&client, &frame, "com.google.Chrome").await,
                None
            );
            *wrapper_url.lock().unwrap() = "https://www.linkedin.com/messaging/".into();
            assert_eq!(
                read_window(&client, &frame, "com.google.Chrome")
                    .await
                    .as_deref(),
                Some("linkedin.com")
            );
            // Latent siblings must be scanned concurrently within the caller's budget.
            let mut siblings = Vec::new();
            for i in 0..64 {
                let obj = object(&format!("/slow{i}"));
                conn.object_server()
                    .at(
                        obj.path.as_str(),
                        Accessible {
                            role: Role::Panel,
                            children: vec![],
                            delay_ms: 10,
                            name: "same title",
                            name_delay_ms: 0,
                        },
                    )
                    .await
                    .unwrap();
                siblings.push(obj);
            }
            conn.object_server()
                .interface::<_, Accessible>(frame.path.as_str())
                .await
                .unwrap()
                .get_mut()
                .await
                .children
                .extend(siblings);
            let result = tokio::time::timeout(
                Duration::from_millis(250),
                read_window(&client, &frame, "com.google.Chrome"),
            )
            .await;
            assert_eq!(result.unwrap().as_deref(), Some("linkedin.com"));

            // Exercise the actual worker and runtime against this private bus.
            conn.request_name("org.a11y.Bus").await.unwrap();
            conn.request_name("org.a11y.atspi.Registry").await.unwrap();
            conn.object_server()
                .at(
                    "/org/a11y/bus",
                    Bus(std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap()),
                )
                .await
                .unwrap();
            let app = object("/app");
            conn.object_server()
                .at(
                    app.path.as_str(),
                    Accessible {
                        role: Role::Application,
                        children: vec![frame.clone()],
                        delay_ms: 0,
                        name: "chrome",
                        name_delay_ms: 0,
                    },
                )
                .await
                .unwrap();
            let busy = object("/busy_unrelated_app");
            conn.object_server()
                .at(
                    busy.path.as_str(),
                    Accessible {
                        role: Role::Application,
                        children: vec![],
                        delay_ms: 0,
                        name: "unrelated",
                        name_delay_ms: 1000,
                    },
                )
                .await
                .unwrap();
            conn.object_server()
                .at(
                    "/org/a11y/atspi/accessible/root",
                    Accessible {
                        role: Role::DesktopFrame,
                        children: vec![app, busy],
                        delay_ms: 0,
                        name: "desktop",
                        name_delay_ms: 0,
                    },
                )
                .await
                .unwrap();
            std::env::set_var("WAYLAND_DISPLAY", "isolated-fixture");
            let checked = tokio::task::spawn_blocking(|| {
                let (output, received) = std::sync::mpsc::channel();
                let input = crate::app_context::route(output);
                for i in 0..3 {
                    input
                        .send(crate::proto::response(
                            "ActiveAppInfo",
                            serde_json::json!({"payload": {
                                "bundleId":"com.google.Chrome", "windowTitle":"same title", "url":""
                            }}),
                            &format!("worker-{i}"),
                        ))
                        .unwrap();
                    let got = received.recv_timeout(Duration::from_millis(300)).unwrap();
                    assert_eq!(
                        got["HelperAPIResponse"]["ActiveAppInfo"]["payload"]["url"],
                        "linkedin.com"
                    );
                }
            });
            checked.await.unwrap();

            // A second real page, even unknown or unreadable, remains ambiguous.
            for url in ["https://web.telegram.org/", "https://example.com/", ""] {
                *extra_url.lock().unwrap() = url.into();
                assert_eq!(
                    read_window(&client, &frame, "com.google.Chrome").await,
                    None
                );
            }
        });
    }

    #[test]
    #[ignore = "requires a focused browser and WISPR_EXPECTED_BROWSER/HOST"]
    fn live_browser_url() {
        let browser = std::env::var("WISPR_EXPECTED_BROWSER").unwrap();
        let expected = std::env::var("WISPR_EXPECTED_HOST").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let repeats = std::env::var("WISPR_URL_SAMPLES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1);
        let mut failures = 0;
        let mut slow = 0;
        for _ in 0..repeats {
            let started = std::time::Instant::now();
            let actual = rt.block_on(read(&browser));
            let elapsed = started.elapsed().as_millis();
            let matched = actual.as_deref() == Some(expected.as_str());
            eprintln!("URL sample: matched={matched} elapsed_ms={elapsed}");
            failures += usize::from(!matched);
            slow += usize::from(elapsed >= 250);
        }
        assert_eq!(failures, 0, "URL samples failed");
        if repeats > 1 {
            assert_eq!(slow, 0, "URL samples exceed recording-start budget");
        }
    }

    #[test]
    fn only_vivaldi_builtin_ui_is_traversed() {
        let ui = "chrome-extension://mpognobbkildjkofajifpdfhcoklimli/browser.html";
        assert!(is_vivaldi_ui("com.vivaldi.Vivaldi", ui));
        assert!(!is_vivaldi_ui("com.google.Chrome", ui));
        for url in [
            "https://example.com/",
            "chrome-extension://another/browser.html",
            "chrome-extension://mpognobbkildjkofajifpdfhcoklimli.evil/browser.html",
        ] {
            assert!(!is_vivaldi_ui("com.vivaldi.Vivaldi", url));
        }
    }

    #[test]
    fn only_browser_owned_footer_is_excluded() {
        assert!(is_browser_footer("chrome://newtab-footer/"));
        for url in [
            "https://web.whatsapp.com/",
            "https://example.com/",
            "chrome://newtab/",
            "chrome://settings/",
            "",
            "https://newtab-footer/",
            "chrome://newtab-footer.evil/",
            "chrome://newtab-footer/other",
        ] {
            assert!(!is_browser_footer(url), "{url}");
        }
    }

    #[test]
    fn only_recognized_http_hosts_never_titles_or_lookalikes() {
        for (url, expected) in [
            ("https://mail.google.com/mail/u/0/#inbox", "mail.google.com"),
            (
                "https://workspace.slack.com/client/x?secret=y",
                "app.slack.com",
            ),
            ("https://www.linkedin.com/messaging/", "linkedin.com"),
            ("https://web.whatsapp.com/", "web.whatsapp.com"),
            ("https://web.telegram.org/a/", "web.telegram.org"),
        ] {
            assert_eq!(site(url).as_deref(), Some(expected));
        }
        for url in [
            "Gmail - Chrome",
            "https://mail.google.com.evil.test/",
            "https://evil.test/?q=mail.google.com",
            "https://mail.google.com@evil.test/",
            "file:///mail.google.com",
            "https://slack.com.evil.test",
            "https://.slack.com",
            "https://evil.test\\@mail.google.com",
            "about:blank",
            "",
            "https://example.com",
        ] {
            assert_eq!(site(url), None, "{url}");
        }
    }
}
