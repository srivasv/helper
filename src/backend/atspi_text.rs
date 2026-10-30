//! Bounded Text/Hypertext snapshots of one focused editor, never its neighbours.
use std::{collections::HashSet, future::Future, pin::Pin, time::Duration};

use atspi::{
    proxy::{
        accessible::ObjectRefExt, hyperlink::HyperlinkProxy, hypertext::HypertextProxy,
        text::TextProxy,
    },
    ObjectRef, Role, State,
};

const MAX_CHARS: usize = 100_000;
const MAX_NODES: usize = 256;
const MAX_DEPTH: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

type Result<T> = std::result::Result<T, String>;
type NodeRead<'a> = Pin<Box<dyn Future<Output = Result<(Vec<char>, Role)>> + 'a>>;

#[derive(Debug, PartialEq)]
pub(super) struct Snapshot {
    pub chars: Vec<char>,
    pub caret: i32,
}

pub(super) async fn read(conn: &zbus::Connection, root: &ObjectRef) -> Result<Snapshot> {
    tokio::time::timeout(SNAPSHOT_TIMEOUT, async {
        let first = snapshot(conn, root).await?;
        // No atomic snapshot API: require two consistent reads across a changing DOM.
        let second = snapshot(conn, root).await?;
        if first != second {
            return Err("editor changed during snapshot".into());
        }
        Ok(second)
    })
    .await
    .map_err(|_| "editor snapshot timed out".to_string())?
}

async fn snapshot(conn: &zbus::Connection, root: &ObjectRef) -> Result<Snapshot> {
    let acc = root.as_accessible_proxy(conn).await.map_err(err)?;
    let parent = acc.parent().await.map_err(err)?;
    let mut reader = Reader {
        conn,
        seen: HashSet::new(),
        chars_read: 0,
        expanded: false,
    };
    let (chars, _) = reader.node(root, None, 0).await?;
    let text = TextProxy::builder(conn)
        .destination(root.name.as_str())
        .map_err(err)?
        .path(root.path.as_str())
        .map_err(err)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(err)?;
    // shortcut: expanded offsets are not root caret offsets; require a unique paste
    // until toolkit-independent caret mapping is available.
    let caret = if reader.expanded {
        -1
    } else {
        text.caret_offset().await.unwrap_or(-1)
    };
    check_root(&acc).await?;
    if acc.parent().await.map_err(err)? != parent {
        return Err("editor was reparented".into());
    }
    Ok(Snapshot { chars, caret })
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

async fn check_root(acc: &atspi::proxy::accessible::AccessibleProxy<'_>) -> Result<()> {
    let role = acc.get_role().await.map_err(err)?;
    if matches!(role, Role::PasswordText | Role::Terminal | Role::Invalid) {
        return Err("private or unsupported editor".into());
    }
    let states = acc.get_state().await.map_err(err)?;
    if !states.contains(State::Focused)
        || !states.contains(State::Editable)
        || states.contains(State::Defunct)
    {
        return Err("editor is not focused and editable".into());
    }
    Ok(())
}

struct Reader<'a> {
    conn: &'a zbus::Connection,
    seen: HashSet<ObjectRef>,
    chars_read: usize,
    expanded: bool,
}

impl Reader<'_> {
    fn node<'a>(
        &'a mut self,
        obj: &'a ObjectRef,
        parent: Option<&'a ObjectRef>,
        depth: usize,
    ) -> NodeRead<'a> {
        Box::pin(async move {
            if depth > MAX_DEPTH || self.seen.len() >= MAX_NODES || !self.seen.insert(obj.clone()) {
                return Err("embedded text cycle or budget exceeded".into());
            }
            if parent.is_some_and(|p| p.name != obj.name) {
                return Err("embedded object belongs to another application".into());
            }
            let acc = obj.as_accessible_proxy(self.conn).await.map_err(err)?;
            let role = acc.get_role().await.map_err(err)?;
            if let Some(parent) = parent {
                // Direct-parent links inductively keep every read inside the editor.
                if acc.parent().await.map_err(err)? != *parent {
                    return Err("embedded object is outside its parent".into());
                }
                if !matches!(
                    role,
                    Role::Paragraph | Role::Static | Role::Text | Role::Link | Role::Section
                ) {
                    return Err("unsupported embedded object".into());
                }
                let states = acc.get_state().await.map_err(err)?;
                if states.contains(State::Defunct)
                    || (states.contains(State::Editable) && states.contains(State::Focusable))
                {
                    return Err("embedded object is defunct".into());
                }
            } else {
                check_root(&acc).await?;
            }
            let text = TextProxy::builder(self.conn)
                .destination(obj.name.as_str())
                .map_err(err)?
                .path(obj.path.as_str())
                .map_err(err)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await
                .map_err(err)?;
            let n = text.character_count().await.map_err(err)?;
            if n < 0 || n as usize > MAX_CHARS.saturating_sub(self.chars_read) {
                return Err("editor text budget exceeded".into());
            }
            let raw: Vec<char> = text.get_text(0, n).await.map_err(err)?.chars().collect();
            if raw.len() != n as usize {
                return Err("text length changed during read".into());
            }
            self.chars_read += raw.len();
            let mut out = Vec::new();
            let mut previous_block = false;
            for (offset, ch) in raw.iter().enumerate() {
                if *ch != '\u{fffc}' {
                    out.push(*ch);
                    previous_block = false;
                    continue;
                }
                self.expanded = true;
                let hyper = HypertextProxy::builder(self.conn)
                    .destination(obj.name.as_str())
                    .map_err(err)?
                    .path(obj.path.as_str())
                    .map_err(err)?
                    .build()
                    .await
                    .map_err(err)?;
                let index = hyper.get_link_index(offset as i32).await.map_err(err)?;
                if index < 0 {
                    return Err("missing embedded link".into());
                }
                let link = hyper.get_link(index).await.map_err(err)?;
                if link.name != obj.name {
                    return Err("foreign embedded link".into());
                }
                let proxy = HyperlinkProxy::builder(self.conn)
                    .destination(link.name.as_str())
                    .map_err(err)?
                    .path(link.path.as_str())
                    .map_err(err)?
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .map_err(err)?;
                // atspi-proxies 0.6 uses the wrong Nanchors spelling and i16 type.
                if proxy
                    .inner()
                    .get_property::<i32>("NAnchors")
                    .await
                    .map_err(err)?
                    != 1
                    || !proxy.is_valid().await.map_err(err)?
                    || proxy.start_index().await.map_err(err)? != offset as i32
                    || proxy.end_index().await.map_err(err)? != offset as i32 + 1
                {
                    return Err("ambiguous embedded link".into());
                }
                let child = proxy.get_object(0).await.map_err(err)?;
                let (chars, child_role) = self.node(&child, Some(obj), depth + 1).await?;
                let block = child_role == Role::Paragraph;
                if previous_block
                    && block
                    && out.last() != Some(&'\n')
                    && chars.first() != Some(&'\n')
                {
                    out.push('\n');
                }
                out.extend(chars);
                previous_block = block;
                if out.len() > MAX_CHARS {
                    return Err("expanded text budget exceeded".into());
                }
                if hyper.get_link_index(offset as i32).await.map_err(err)? != index
                    || hyper.get_link(index).await.map_err(err)? != link
                    || proxy.get_object(0).await.map_err(err)? != child
                {
                    return Err("embedded link changed during read".into());
                }
            }
            if text.character_count().await.map_err(err)? != n
                || text.get_text(0, n).await.map_err(err)?.chars().ne(raw)
            {
                return Err("text changed during read".into());
            }
            if let Some(parent) = parent {
                if acc.parent().await.map_err(err)? != *parent
                    || acc.get_role().await.map_err(err)? != role
                {
                    return Err("embedded object changed during read".into());
                }
            } else {
                check_root(&acc).await?;
            }
            Ok((out, role))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Node {
        role: Role,
        parent: ObjectRef,
        states: atspi::StateSet,
        text: String,
        count: Option<i32>,
        links: Vec<ObjectRef>,
        target: ObjectRef,
        start: i32,
        anchors: i32,
        valid: bool,
        reads: usize,
        mutate: bool,
        stall: bool,
        blur: bool,
        detach: bool,
    }
    type Shared = Arc<Mutex<Node>>;
    struct Accessible(Shared);
    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl Accessible {
        fn get_role(&self) -> Role {
            self.0.lock().unwrap().role
        }
        fn get_state(&self) -> atspi::StateSet {
            self.0.lock().unwrap().states
        }
        #[zbus(property)]
        fn parent(&self) -> ObjectRef {
            self.0.lock().unwrap().parent.clone()
        }
    }
    struct Text(Shared);
    #[zbus::interface(name = "org.a11y.atspi.Text")]
    impl Text {
        #[zbus(property)]
        fn character_count(&self) -> i32 {
            let n = self.0.lock().unwrap();
            n.count.unwrap_or(n.text.chars().count() as i32)
        }
        #[zbus(property)]
        fn caret_offset(&self) -> i32 {
            self.character_count()
        }
        async fn get_text(&self, _start: i32, _end: i32) -> String {
            let (text, stall) = {
                let mut n = self.0.lock().unwrap();
                n.reads += 1;
                if n.blur {
                    n.states.remove(State::Focused);
                }
                if n.detach {
                    n.parent.path = "/detached".try_into().unwrap();
                }
                let text = if n.mutate && n.reads > 1 {
                    "changed".into()
                } else {
                    n.text.clone()
                };
                (text, n.stall)
            };
            if stall {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            text
        }
    }
    struct Hypertext(Shared);
    #[zbus::interface(name = "org.a11y.atspi.Hypertext")]
    impl Hypertext {
        fn get_link_index(&self, offset: i32) -> i32 {
            if (offset as usize) < self.0.lock().unwrap().links.len() {
                offset
            } else {
                -1
            }
        }
        fn get_link(&self, index: i32) -> zbus::fdo::Result<ObjectRef> {
            self.0
                .lock()
                .unwrap()
                .links
                .get(index as usize)
                .cloned()
                .ok_or_else(|| zbus::fdo::Error::Failed("missing link".into()))
        }
    }
    struct Hyperlink(Shared);
    #[zbus::interface(name = "org.a11y.atspi.Hyperlink")]
    impl Hyperlink {
        fn get_object(&self, _i: i32) -> ObjectRef {
            self.0.lock().unwrap().target.clone()
        }
        fn is_valid(&self) -> bool {
            self.0.lock().unwrap().valid
        }
        #[zbus(property, name = "NAnchors")]
        fn n_anchors(&self) -> i32 {
            self.0.lock().unwrap().anchors
        }
        #[zbus(property)]
        fn start_index(&self) -> i32 {
            self.0.lock().unwrap().start
        }
        #[zbus(property)]
        fn end_index(&self) -> i32 {
            self.start_index() + 1
        }
    }

    async fn fixture(
        conn: &zbus::Connection,
        path: &str,
        parent: &ObjectRef,
        role: Role,
        text: &str,
    ) -> (ObjectRef, Shared) {
        let obj = ObjectRef {
            name: conn.unique_name().unwrap().clone(),
            path: path.try_into().unwrap(),
        };
        let node = Arc::new(Mutex::new(Node {
            role,
            parent: parent.clone(),
            states: if role == Role::Entry {
                {
                    let mut s = atspi::StateSet::from(State::Editable);
                    s.insert(State::Focused);
                    s
                }
            } else {
                State::Editable.into()
            },
            text: text.into(),
            count: None,
            links: vec![],
            target: obj.clone(),
            start: 0,
            anchors: 1,
            valid: true,
            reads: 0,
            mutate: false,
            stall: false,
            blur: false,
            detach: false,
        }));
        conn.object_server()
            .at(obj.path.clone(), Accessible(node.clone()))
            .await
            .unwrap();
        conn.object_server()
            .at(obj.path.clone(), Text(node.clone()))
            .await
            .unwrap();
        conn.object_server()
            .at(obj.path.clone(), Hypertext(node.clone()))
            .await
            .unwrap();
        conn.object_server()
            .at(obj.path.clone(), Hyperlink(node.clone()))
            .await
            .unwrap();
        (obj, node)
    }

    async fn checks() {
        let server = zbus::Connection::session().await.unwrap();
        let client = zbus::Connection::session().await.unwrap();
        let null = ObjectRef {
            name: server.unique_name().unwrap().clone(),
            path: "/null".try_into().unwrap(),
        };
        let (root, r) = fixture(&server, "/editor", &null, Role::Entry, "\u{fffc}\u{fffc}").await;
        let (p1, a) = fixture(
            &server,
            "/paragraph1",
            &root,
            Role::Paragraph,
            "hello 日本 🦀",
        )
        .await;
        let (p2, b) = fixture(&server, "/paragraph2", &root, Role::Paragraph, "world").await;
        b.lock().unwrap().start = 1;
        r.lock().unwrap().links = vec![p1.clone(), p2.clone()];
        // This duplicate is never linked and must never be read.
        let (_, duplicate) = fixture(&server, "/static", &p1, Role::Static, "hello 日本 🦀").await;
        let good = r.lock().unwrap().clone();
        let child_good = a.lock().unwrap().clone();
        let result = read(&client, &root).await.unwrap();
        assert_eq!(
            result,
            Snapshot {
                chars: "hello 日本 🦀\nworld".chars().collect(),
                caret: -1
            }
        );
        assert_eq!(duplicate.lock().unwrap().reads, 0);
        // Recreated paragraph IDs are fine between stable snapshots.
        let (replacement, _) = fixture(
            &server,
            "/replacement",
            &root,
            Role::Paragraph,
            "hello 日本 🦀",
        )
        .await;
        r.lock().unwrap().links[0] = replacement;
        assert_eq!(read(&client, &root).await.unwrap(), result);
        *r.lock().unwrap() = good.clone();

        for role in [
            Role::PasswordText,
            Role::Entry,
            Role::Image,
            Role::PushButton,
            Role::Terminal,
        ] {
            *a.lock().unwrap() = child_good.clone();
            a.lock().unwrap().role = role;
            assert!(read(&client, &root).await.is_err(), "accepted {role:?}");
            assert_eq!(a.lock().unwrap().reads, 0, "read private/unsupported child");
        }
        for kind in 0..12 {
            *r.lock().unwrap() = good.clone();
            *a.lock().unwrap() = child_good.clone();
            match kind {
                0 => a.lock().unwrap().parent = null.clone(),
                1 => a.lock().unwrap().target = root.clone(), // cycle
                2 => a.lock().unwrap().anchors = 2,
                3 => a.lock().unwrap().valid = false,
                4 => a.lock().unwrap().start = 9,
                5 => a.lock().unwrap().states.insert(State::Focusable), // nested editor
                6 => r.lock().unwrap().links.clear(),
                7 => r.lock().unwrap().links[0].name = ":999.999".try_into().unwrap(),
                8 => a.lock().unwrap().target.name = ":999.999".try_into().unwrap(),
                9 => r.lock().unwrap().role = Role::PasswordText,
                10 => r.lock().unwrap().states.remove(State::Focused),
                11 => r.lock().unwrap().states.remove(State::Editable),
                _ => unreachable!(),
            };
            assert!(
                read(&client, &root).await.is_err(),
                "accepted unsafe case {kind}"
            );
            assert_eq!(a.lock().unwrap().reads, 0, "read unsafe case {kind}");
        }
        for blur in [true, false] {
            *r.lock().unwrap() = good.clone();
            *a.lock().unwrap() = child_good.clone();
            if blur {
                r.lock().unwrap().blur = true;
            } else {
                r.lock().unwrap().detach = true;
            }
            assert!(
                read(&client, &root).await.is_err(),
                "accepted focus/identity change"
            );
        }
        *r.lock().unwrap() = good.clone();
        *a.lock().unwrap() = child_good.clone();
        a.lock().unwrap().text = "x".repeat(MAX_CHARS / 2);
        b.lock().unwrap().text = "x".repeat(MAX_CHARS / 2);
        assert!(read(&client, &root).await.unwrap_err().contains("budget"));
        b.lock().unwrap().text = "world".into();
        for kind in 0..5 {
            *r.lock().unwrap() = good.clone();
            *a.lock().unwrap() = child_good.clone();
            match kind {
                0 => a.lock().unwrap().count = Some(MAX_CHARS as i32 + 1),
                1 => a.lock().unwrap().count = Some(-1),
                2 => a.lock().unwrap().count = Some(1),
                3 => a.lock().unwrap().mutate = true,
                4 => a.lock().unwrap().stall = true,
                _ => unreachable!(),
            }
            assert!(
                read(&client, &root).await.is_err(),
                "accepted inconsistent case {kind}"
            );
        }
        *r.lock().unwrap() = good.clone();
        *a.lock().unwrap() = child_good;
        // Depth and node exhaustion use the real reader and proxies.
        let mut parent = p1;
        let mut last = a;
        for i in 0..MAX_DEPTH + 1 {
            let (child, data) =
                fixture(&server, &format!("/deep{i}"), &parent, Role::Paragraph, "x").await;
            last.lock().unwrap().text = "\u{fffc}".into();
            last.lock().unwrap().links = vec![child.clone()];
            parent = child;
            last = data;
        }
        assert!(read(&client, &root).await.unwrap_err().contains("budget"));
        assert_eq!(last.lock().unwrap().reads, 0);
        r.lock().unwrap().text = "\u{fffc}".repeat(MAX_NODES);
        r.lock().unwrap().links.clear();
        let mut last = r.clone();
        for i in 0..MAX_NODES {
            let (child, data) =
                fixture(&server, &format!("/wide{i}"), &root, Role::Paragraph, "x").await;
            data.lock().unwrap().start = i as i32;
            r.lock().unwrap().links.push(child);
            last = data;
        }
        // Use snapshot without its wall-clock cap to specifically exercise node budget.
        assert!(snapshot(&client, &root)
            .await
            .unwrap_err()
            .contains("budget"));
        assert_eq!(last.lock().unwrap().reads, 0);
        *r.lock().unwrap() = good;
        r.lock().unwrap().text = "plain 🦀".into();
        assert_eq!(read(&client, &root).await.unwrap().caret, 7);
    }

    #[test]
    #[ignore = "requires a private dbus-run-session"]
    fn rich_text_boundaries() {
        assert_eq!(
            std::env::var("WISPR_EDIT_TEST_ISOLATED").as_deref(),
            Ok("1")
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(checks());
    }
}
