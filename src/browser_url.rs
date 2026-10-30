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
use std::collections::HashSet;
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

pub async fn read(browser: &str) -> Option<String> {
    if !["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()))
    {
        return None;
    }
    tokio::time::timeout(Duration::from_millis(200), read_active(browser))
        .await
        .ok()
        .flatten()
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
    for app in root.get_children().await.ok()?.into_iter().take(128) {
        let proxy = app.as_accessible_proxy(conn).await.ok()?;
        let name = proxy.name().await.unwrap_or_default();
        let (_, exe) =
            crate::backend::atspi_app::identity_from_sender(conn, app.name.as_str()).await;
        if identity(&exe) != browser && identity(&name) != browser {
            continue;
        }
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
            if !state.contains(State::Active) || state.contains(State::Defunct) {
                continue;
            }
            windows.push(window);
        }
    }
    // More than one supposedly-active window is ambiguous, even with equal titles.
    if windows.len() != 1 {
        return None;
    }
    let window = &windows[0];
    // Compositor caches may retain the previous tab's title. Trust the active
    // accessible window, then verify its title did not change during this read.
    let title = window
        .as_accessible_proxy(conn)
        .await
        .ok()?
        .name()
        .await
        .ok()?;
    let mut stack = vec![(window.clone(), 0usize)];
    let mut seen = HashSet::new();
    let mut document: Option<ObjectRef> = None;
    while let Some((obj, depth)) = stack.pop() {
        if depth > 32 || seen.len() >= 300 || !seen.insert(obj.clone()) {
            return None;
        }
        let p = obj.as_accessible_proxy(conn).await.ok()?;
        let state = p.get_state().await.ok()?;
        if state.contains(State::Defunct) || !state.contains(State::Showing) {
            continue;
        }
        let role = p.get_role().await.ok()?;
        if matches!(role, Role::DocumentWeb | Role::DocumentFrame) {
            if document.replace(obj).is_some() {
                return None;
            }
            // Never descend into iframe documents or read editable page contents.
            continue;
        }
        for child in p.get_children().await.ok()?.into_iter().rev() {
            if child.name != window.name {
                return None;
            }
            stack.push((child, depth + 1));
        }
    }
    let doc = document?;
    let p = DocumentProxy::builder(conn)
        .destination(doc.name.as_str())
        .ok()?
        .path(doc.path.as_str())
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    // Chromium calls this URI; Firefox calls it DocURL.
    let mut attribute = "URI";
    let mut url = p.get_attribute_value(attribute).await.unwrap_or_default();
    if url.is_empty() {
        attribute = "DocURL";
        url = p.get_attribute_value(attribute).await.ok()?;
    }
    let result = site(&url)?;
    let frame = window.as_accessible_proxy(conn).await.ok()?;
    if !frame.get_state().await.ok()?.contains(State::Active) {
        return None;
    }
    if frame.name().await.ok()? != title {
        return None;
    }
    if p.get_attribute_value(attribute).await.ok()? != url {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
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
