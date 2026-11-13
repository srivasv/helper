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
type NodeRead<'a> = Pin<Box<dyn Future<Output = Result<(Vec<char>, Role, i32)>> + 'a>>;

#[derive(Debug, PartialEq)]
pub(super) struct Snapshot {
    pub chars: Vec<char>,
    pub caret: i32,
    expanded: bool,
    selection: Option<(i32, i32)>,
}

pub(super) async fn read(conn: &zbus::Connection, root: &ObjectRef) -> Result<Snapshot> {
    read_mode(conn, root, false).await
}

pub(super) async fn read_selected(conn: &zbus::Connection, root: &ObjectRef) -> Result<Snapshot> {
    read_mode(conn, root, true).await
}

/// Read caret-relative context without copying or guessing expanded DOM offsets.
pub(super) async fn context(conn: &zbus::Connection, root: &ObjectRef) -> Result<super::Selection> {
    let first = context_snapshot(conn, root).await?;
    let second = context_snapshot(conn, root).await?;
    if first != second {
        return Err("context changed during read".into());
    }
    let (snapshot, start, end) = second;
    split_context(&snapshot.chars, start, end)
}

async fn context_snapshot(
    conn: &zbus::Connection,
    root: &ObjectRef,
) -> Result<(Snapshot, i32, i32)> {
    let text = TextProxy::builder(conn)
        .destination(root.name.as_str())
        .map_err(err)?
        .path(root.path.as_str())
        .map_err(err)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(err)?;
    let selection = text_selection(&text).await?;
    let snapshot = snapshot(conn, root, false, true).await?;
    if snapshot.caret < 0 {
        return Err("caret unavailable for editor".into());
    }
    if text_selection(&text).await? != selection {
        return Err("selection changed during read".into());
    }
    let (start, end) = match (snapshot.expanded, selection, snapshot.selection) {
        (_, None, None) => (snapshot.caret, snapshot.caret),
        (false, Some(range), _) | (true, Some(_), Some(range)) => range,
        _ => return Err("selection offsets unavailable for expanded editor".into()),
    };
    Ok((snapshot, start, end))
}

async fn text_selection(text: &TextProxy<'_>) -> Result<Option<(i32, i32)>> {
    let count: i32 = text
        .inner()
        .call("GetNSelections", &())
        .await
        .map_err(err)?;
    match count {
        0 => Ok(None),
        1 => text.get_selection(0).await.map(Some).map_err(err),
        _ => Err("ambiguous editor selection".into()),
    }
}

fn split_context(chars: &[char], start: i32, end: i32) -> Result<super::Selection> {
    // Bound both copies of the text in TextBoxInfo below the 30,000-char IPC cap.
    const LIMIT: usize = 500;
    if start < 0 || end < start || end as usize > chars.len() || (end - start) as usize > LIMIT {
        return Err("invalid or oversized context selection".into());
    }
    let (start, end) = (start as usize, end as usize);
    let before = start.saturating_sub(LIMIT);
    let after = (end + LIMIT).min(chars.len());
    Ok(super::Selection {
        before_text: chars[before..start].iter().collect(),
        selected_text: chars[start..end].iter().collect(),
        after_text: chars[end..after].iter().collect(),
        contents: chars[before..after].iter().collect(),
    })
}

async fn read_mode(conn: &zbus::Connection, root: &ObjectRef, selected: bool) -> Result<Snapshot> {
    tokio::time::timeout(SNAPSHOT_TIMEOUT, async {
        let first = snapshot(conn, root, selected, false).await?;
        // No atomic snapshot API: require two consistent reads across a changing DOM.
        let second = snapshot(conn, root, selected, false).await?;
        if first != second {
            return Err("editor changed during snapshot".into());
        }
        Ok(second)
    })
    .await
    .map_err(|_| "editor snapshot timed out".to_string())?
}

async fn snapshot(
    conn: &zbus::Connection,
    root: &ObjectRef,
    selected: bool,
    map_caret: bool,
) -> Result<Snapshot> {
    let acc = root.as_accessible_proxy(conn).await.map_err(err)?;
    let parent = acc.parent().await.map_err(err)?;
    let mut reader = Reader {
        conn,
        seen: HashSet::new(),
        chars_read: 0,
        expanded: false,
        selected,
        map_caret,
        caret_only: map_caret && docs_editor(conn, &parent).await,
        context_selected: false,
        context_selection: None,
    };
    let (chars, _, mapped_caret) = reader.node(root, None, 0).await?;
    let text = TextProxy::builder(conn)
        .destination(root.name.as_str())
        .map_err(err)?
        .path(root.path.as_str())
        .map_err(err)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(err)?;
    // Correction matching keeps its unique-paste rule for expanded editors.
    let caret = if map_caret {
        mapped_caret
    } else if reader.expanded || selected {
        -1
    } else {
        text.caret_offset().await.unwrap_or(-1)
    };
    check_root(&acc).await?;
    if acc.parent().await.map_err(err)? != parent {
        return Err("editor was reparented".into());
    }
    Ok(Snapshot {
        chars,
        caret,
        expanded: reader.expanded,
        selection: reader.context_selection,
    })
}

async fn docs_editor(conn: &zbus::Connection, parent: &ObjectRef) -> bool {
    let Ok(document) = parent.as_accessible_proxy(conn).await else {
        return false;
    };
    let Ok(frame) = document.parent().await else {
        return false;
    };
    let Ok(frame) = frame.as_accessible_proxy(conn).await else {
        return false;
    };
    frame
        .get_attributes()
        .await
        .ok()
        .and_then(|attrs| attrs.get("class").cloned())
        .is_some_and(|classes| {
            classes
                .split_whitespace()
                .any(|class| class == "docs-texteventtarget-iframe")
        })
}

fn context_block_role(role: Role) -> bool {
    matches!(
        role,
        Role::Heading
            | Role::Table
            | Role::TableRow
            | Role::TableCell
            | Role::ColumnHeader
            | Role::RowHeader
            | Role::List
            | Role::ListItem
    )
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
    selected: bool,
    map_caret: bool,
    caret_only: bool,
    context_selected: bool,
    context_selection: Option<(i32, i32)>,
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
                let text_role = matches!(
                    role,
                    Role::Paragraph | Role::Static | Role::Text | Role::Link | Role::Section
                );
                let context_block = self.caret_only && context_block_role(role);
                if !text_role && !context_block {
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
            let raw_caret = if self.map_caret {
                text.caret_offset().await.map_err(err)?
            } else {
                -1
            };
            if raw_caret < -1 || raw_caret > n {
                return Err("invalid caret offset".into());
            }
            let context_range = if self.caret_only && (parent.is_none() || self.context_selected) {
                let range = text_selection(&text).await?;
                if parent.is_none() {
                    self.context_selected = range.is_some();
                }
                if self.context_selected {
                    Some(range.ok_or("missing embedded context selection")?)
                } else {
                    None
                }
            } else {
                None
            };
            let mut range = if self.selected {
                let count: i32 = text
                    .inner()
                    .call("GetNSelections", &())
                    .await
                    .map_err(err)?;
                if count != 1 {
                    return Err("ambiguous or absent embedded selection".into());
                }
                let (start, end) = text.get_selection(0).await.map_err(err)?;
                if start < 0 || end <= start || end > n {
                    return Err("invalid embedded selection range".into());
                }
                start as usize..end as usize
            } else {
                0..n as usize
            };
            let raw: Vec<char> = text.get_text(0, n).await.map_err(err)?.chars().collect();
            if raw.len() != n as usize {
                return Err("text length changed during read".into());
            }
            let mut select_child = false;
            if let Some((start, end)) = context_range {
                if start < 0 || end <= start || end > n {
                    return Err("invalid embedded context selection".into());
                }
                if raw.get(raw_caret as usize) == Some(&'\u{fffc}') {
                    // Docs selects an ancestor marker for a range inside its child.
                    if (start, end) != (raw_caret, raw_caret + 1) {
                        return Err("context selection spans embedded objects".into());
                    }
                    select_child = true;
                } else if raw[start as usize..end as usize].contains(&'\u{fffc}') {
                    return Err("selection offsets unavailable for expanded editor".into());
                }
            }
            // Docs exposes the whole document, including unrelated tables. Follow
            // only the caret's embedded-object chain to its current text block.
            if self.caret_only && raw_caret >= 0 && raw.get(raw_caret as usize) == Some(&'\u{fffc}')
            {
                range = raw_caret as usize..raw_caret as usize + 1;
            }
            self.chars_read += raw.len();
            let mut out = Vec::new();
            let mut caret = -1;
            let mut selection_start = None;
            let mut selection_end = None;
            let mut previous_block = false;
            for (offset, ch) in raw.iter().enumerate().take(range.end).skip(range.start) {
                if let Some((start, end)) = context_range.filter(|_| !select_child) {
                    if start == offset as i32 {
                        selection_start = Some(out.len() as i32);
                    }
                    if end == offset as i32 {
                        selection_end = Some(out.len() as i32);
                    }
                }
                if *ch != '\u{fffc}' {
                    if raw_caret == offset as i32 {
                        caret = out.len() as i32;
                    }
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
                let selected_context = self.context_selected;
                self.context_selected = select_child && raw_caret == offset as i32;
                let (chars, child_role, child_caret) =
                    self.node(&child, Some(obj), depth + 1).await?;
                self.context_selected = selected_context;
                let block = child_role == Role::Paragraph
                    || (self.caret_only && context_block_role(child_role));
                if previous_block
                    && block
                    && out.last() != Some(&'\n')
                    && chars.first() != Some(&'\n')
                {
                    out.push('\n');
                }
                if raw_caret == offset as i32 && child_caret >= 0 {
                    caret = out.len() as i32 + child_caret;
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
            if raw_caret == n {
                caret = out.len() as i32;
            }
            if let Some((_, end)) = context_range.filter(|_| !select_child) {
                if end == n {
                    selection_end = Some(out.len() as i32);
                }
                self.context_selection = Some((
                    selection_start.ok_or("missing context selection start")?,
                    selection_end.ok_or("missing context selection end")?,
                ));
            }
            if self.map_caret && text.caret_offset().await.map_err(err)? != raw_caret {
                return Err("caret changed during read".into());
            }
            if context_range.is_some() && text_selection(&text).await? != context_range {
                return Err("selection changed during read".into());
            }
            if self.selected {
                let count: i32 = text
                    .inner()
                    .call("GetNSelections", &())
                    .await
                    .map_err(err)?;
                if count != 1
                    || text.get_selection(0).await.map_err(err)?
                        != (range.start as i32, range.end as i32)
                {
                    return Err("selection changed during read".into());
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
            Ok((out, role, caret))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn context_ranges_are_bounded_and_never_guess_invalid_offsets() {
        let chars: Vec<char> = "é🦀 ".repeat(1000).chars().collect();
        let text = split_context(&chars, 1500, 1500).unwrap();
        assert_eq!(text.before_text.chars().count(), 500);
        assert_eq!(text.after_text.chars().count(), 500);
        assert_eq!(text.contents.chars().count(), 1000);
        assert!(text.selected_text.is_empty());
        for (start, end) in [(-1, 0), (2, 1), (0, 3001), (0, 501)] {
            assert!(split_context(&chars, start, end).is_err());
        }
        let empty = split_context(&[], 0, 0).unwrap();
        assert!(empty.contents.is_empty());
    }

    #[derive(Clone)]
    struct Node {
        role: Role,
        classes: String,
        parent: ObjectRef,
        states: atspi::StateSet,
        text: String,
        count: Option<i32>,
        caret: Option<i32>,
        selection: Option<(i32, i32)>,
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
        change_selection: bool,
        move_caret: bool,
    }
    type Shared = Arc<Mutex<Node>>;
    struct Accessible(Shared);
    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl Accessible {
        fn get_attributes(&self) -> std::collections::HashMap<String, String> {
            std::collections::HashMap::from([(
                "class".into(),
                self.0.lock().unwrap().classes.clone(),
            )])
        }
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
        fn get_n_selections(&self) -> i32 {
            i32::from(self.0.lock().unwrap().selection.is_some())
        }
        fn get_selection(&self, _index: i32) -> (i32, i32) {
            self.0.lock().unwrap().selection.unwrap_or((0, 0))
        }
        #[zbus(property)]
        fn character_count(&self) -> i32 {
            let n = self.0.lock().unwrap();
            n.count.unwrap_or(n.text.chars().count() as i32)
        }
        #[zbus(property)]
        fn caret_offset(&self) -> i32 {
            let n = self.0.lock().unwrap();
            n.caret.unwrap_or(n.text.chars().count() as i32)
        }
        async fn get_text(&self, _start: i32, _end: i32) -> String {
            let (text, stall) = {
                let mut n = self.0.lock().unwrap();
                n.reads += 1;
                if n.move_caret {
                    n.caret = Some(n.caret.unwrap_or(0) + 1);
                }
                if n.change_selection {
                    n.selection = None;
                }
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
            classes: String::new(),
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
            caret: None,
            selection: Some((0, text.chars().count() as i32)),
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
            change_selection: false,
            move_caret: false,
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

    #[test]
    #[ignore = "requires a private dbus-run-session"]
    fn docs_table_context_follows_only_the_caret() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let server = zbus::Connection::session().await.unwrap();
                let client = zbus::Connection::session().await.unwrap();
                let null = ObjectRef {
                    name: server.unique_name().unwrap().clone(),
                    path: "/null".try_into().unwrap(),
                };
                let (frame, frame_data) =
                    fixture(&server, "/docs_frame", &null, Role::InternalFrame, "").await;
                frame_data.lock().unwrap().classes =
                    "docs-texteventtarget-iframe docs-offscreen-z-index".into();
                let (document, _) =
                    fixture(&server, "/docs_document", &frame, Role::DocumentWeb, "").await;
                let (editor, editor_data) = fixture(
                    &server,
                    "/docs_editor",
                    &document,
                    Role::Entry,
                    "\u{fffc}\u{fffc}",
                )
                .await;
                let (unrelated, unrelated_data) = fixture(
                    &server,
                    "/unrelated_image",
                    &editor,
                    Role::Image,
                    "unrelated",
                )
                .await;
                editor_data.lock().unwrap().caret = Some(1);
                editor_data.lock().unwrap().selection = None;
                let mut parent = editor.clone();
                let mut parent_data = editor_data.clone();
                for (path, role) in [
                    ("/table", Role::Table),
                    ("/row", Role::TableRow),
                    ("/cell", Role::TableCell),
                    ("/paragraph", Role::Paragraph),
                ] {
                    let paragraph = role == Role::Paragraph;
                    let (child, child_data) = fixture(
                        &server,
                        path,
                        &parent,
                        role,
                        if paragraph {
                            "We were discussing 日本 already."
                        } else {
                            "\u{fffc}"
                        },
                    )
                    .await;
                    child_data.lock().unwrap().caret = Some(if paragraph { 19 } else { 0 });
                    if parent == editor {
                        child_data.lock().unwrap().start = 1;
                        parent_data.lock().unwrap().links = vec![unrelated.clone(), child.clone()];
                    } else {
                        parent_data.lock().unwrap().links = vec![child.clone()];
                    }
                    parent = child;
                    parent_data = child_data;
                }
                let result = context(&client, &editor).await.unwrap();
                assert_eq!(result.before_text, "We were discussing ");
                assert_eq!(result.after_text, "日本 already.");
                assert!(result.selected_text.is_empty());
                assert_eq!(unrelated_data.lock().unwrap().reads, 0);
                // Docs reports a selected word as the ancestor's one object marker.
                editor_data.lock().unwrap().selection = Some((1, 2));
                parent_data.lock().unwrap().selection = Some((19, 21));
                parent_data.lock().unwrap().caret = Some(21);
                let selected = context(&client, &editor).await.unwrap();
                assert_eq!(selected.before_text, "We were discussing ");
                assert_eq!(selected.selected_text, "日本");
                assert_eq!(selected.after_text, " already.");
                assert_eq!(unrelated_data.lock().unwrap().reads, 0);
                editor_data.lock().unwrap().selection = Some((0, 2));
                assert!(context(&client, &editor).await.is_err());
                editor_data.lock().unwrap().selection = Some((1, 2));
                parent_data.lock().unwrap().selection = None;
                assert!(context(&client, &editor).await.is_err());
                parent_data.lock().unwrap().selection = Some((19, 21));
                parent_data.lock().unwrap().change_selection = true;
                assert!(context(&client, &editor).await.is_err());
                parent_data.lock().unwrap().change_selection = false;
                let (link, link_data) =
                    fixture(&server, "/preceding_link", &parent, Role::Link, "clearly ").await;
                link_data.lock().unwrap().start = 8;
                parent_data.lock().unwrap().text =
                    "We were \u{fffc}discussing 日本 already.".into();
                parent_data.lock().unwrap().links = vec![link; 9];
                parent_data.lock().unwrap().selection = Some((20, 22));
                parent_data.lock().unwrap().caret = Some(22);
                let selected = context(&client, &editor).await.unwrap();
                assert_eq!(selected.before_text, "We were clearly discussing ");
                assert_eq!(selected.selected_text, "日本");
                assert_eq!(selected.after_text, " already.");
                for range in [(20, 999), (22, 20), (20, 20)] {
                    parent_data.lock().unwrap().selection = Some(range);
                    assert!(context(&client, &editor).await.is_err());
                }
                parent_data.lock().unwrap().selection = Some((20, 22));
                parent_data.lock().unwrap().role = Role::PasswordText;
                assert!(context(&client, &editor).await.is_err());
            });
    }

    async fn checks() {
        let server = zbus::Connection::session().await.unwrap();
        let client = zbus::Connection::session().await.unwrap();
        let null = ObjectRef {
            name: server.unique_name().unwrap().clone(),
            path: "/null".try_into().unwrap(),
        };
        let (editor, data) = fixture(
            &server,
            "/context_editor",
            &null,
            Role::Entry,
            "We were discussing ",
        )
        .await;
        data.lock().unwrap().selection = None;
        let initial = context(&client, &editor).await.unwrap();
        assert_eq!(initial.before_text, "We were discussing ");
        assert_eq!(initial.after_text, "");
        // Resume must read the actual field, including edits made between segments.
        data.lock().unwrap().text = "日本 🦀 and more".into();
        data.lock().unwrap().caret = Some(4);
        let resumed = context(&client, &editor).await.unwrap();
        assert_eq!(resumed.before_text, "日本 🦀");
        assert_eq!(resumed.after_text, " and more");
        data.lock().unwrap().selection = Some((5, 8));
        let selected = context(&client, &editor).await.unwrap();
        assert_eq!(selected.before_text, "日本 🦀 ");
        assert_eq!(selected.selected_text, "and");
        assert_eq!(selected.after_text, " more");
        for role in [Role::PasswordText, Role::Terminal] {
            data.lock().unwrap().role = role;
            let reads = data.lock().unwrap().reads;
            assert!(context(&client, &editor).await.is_err());
            assert_eq!(data.lock().unwrap().reads, reads);
        }
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
                caret: -1,
                expanded: true,
                selection: None,
            }
        );
        // Chromium's root caret names the paragraph placeholder; the paragraph
        // supplies its own character offset, rather than a flattened root offset.
        r.lock().unwrap().selection = None;
        r.lock().unwrap().caret = Some(1);
        a.lock().unwrap().caret = Some(-1);
        b.lock().unwrap().caret = Some(5);
        let resumed = context(&client, &root)
            .await
            .expect("rich-text dictation must retain caret context");
        assert_eq!(resumed.before_text, "hello 日本 🦀\nworld");
        assert_eq!(resumed.after_text, "");
        b.lock().unwrap().caret = Some(2);
        let middle = context(&client, &root).await.unwrap();
        assert_eq!(middle.before_text, "hello 日本 🦀\nwo");
        assert_eq!(middle.after_text, "rld");
        r.lock().unwrap().caret = Some(0);
        a.lock().unwrap().caret = Some(6);
        b.lock().unwrap().caret = Some(-1);
        let first = context(&client, &root).await.unwrap();
        assert_eq!(first.before_text, "hello ");
        assert_eq!(first.after_text, "日本 🦀\nworld");
        a.lock().unwrap().caret = Some(-1);
        assert!(
            context(&client, &root).await.is_err(),
            "missing child caret"
        );
        a.lock().unwrap().caret = Some(99);
        assert!(
            context(&client, &root).await.is_err(),
            "invalid child caret"
        );
        a.lock().unwrap().caret = Some(6);
        a.lock().unwrap().move_caret = true;
        assert!(
            context(&client, &root).await.is_err(),
            "caret changed during read"
        );
        a.lock().unwrap().move_caret = false;
        a.lock().unwrap().caret = Some(6);
        r.lock().unwrap().selection = Some((0, 1));
        assert!(
            context(&client, &root).await.is_err(),
            "unmapped rich selection"
        );
        let (span, span_data) =
            fixture(&server, "/caret_span", &p1, Role::Text, "hello 日本 🦀").await;
        span_data.lock().unwrap().caret = Some(7);
        r.lock().unwrap().selection = None;
        a.lock().unwrap().text = "\u{fffc}".into();
        a.lock().unwrap().links = vec![span];
        a.lock().unwrap().caret = Some(0);
        let nested = context(&client, &root).await.unwrap();
        assert_eq!(nested.before_text, "hello 日");
        assert_eq!(nested.after_text, "本 🦀\nworld");
        *r.lock().unwrap() = good.clone();
        *a.lock().unwrap() = child_good.clone();
        assert_eq!(duplicate.lock().unwrap().reads, 0);
        r.lock().unwrap().selection = Some((0, 1));
        a.lock().unwrap().selection = Some((6, 10));
        b.lock().unwrap().reads = 0;
        assert_eq!(
            read_selected(&client, &root).await.unwrap().chars,
            "日本 🦀".chars().collect::<Vec<_>>(),
            "only selected characters inside the embedded paragraph may be returned"
        );
        assert_eq!(b.lock().unwrap().reads, 0, "unselected paragraph was read");
        r.lock().unwrap().selection = Some((1, 2));
        b.lock().unwrap().selection = Some((0, 3));
        assert_eq!(
            read_selected(&client, &root).await.unwrap().chars,
            "wor".chars().collect::<Vec<_>>()
        );
        r.lock().unwrap().selection = Some((0, 2));
        assert_eq!(
            read_selected(&client, &root).await.unwrap().chars,
            "日本 🦀\nwor".chars().collect::<Vec<_>>()
        );
        for range in [None, Some((-1, 2)), Some((0, 99)), Some((4, 2))] {
            a.lock().unwrap().selection = range;
            assert!(
                read_selected(&client, &root).await.is_err(),
                "invalid child selection"
            );
        }
        *r.lock().unwrap() = good.clone();
        *a.lock().unwrap() = child_good.clone();
        b.lock().unwrap().selection = Some((0, 5));
        a.lock().unwrap().change_selection = true;
        assert!(
            read_selected(&client, &root).await.is_err(),
            "selection changed during read"
        );
        *a.lock().unwrap() = child_good.clone();

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
        assert!(snapshot(&client, &root, false, false)
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
