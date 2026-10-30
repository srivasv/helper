//! X11 backend.
//!
//! Confident/implemented:
//!   * GetActiveAppInfo  — `_NET_ACTIVE_WINDOW` -> `_NET_WM_PID` (/proc) + `_NET_WM_NAME` + `WM_CLASS`
//!   * GetRunningApps    — `_NET_CLIENT_LIST` -> per-window `WM_CLASS` / `_NET_WM_NAME`
//!   * SimulateKeyPress  — VK -> keysym (keymap.rs) -> keycode (server mapping, active
//!     XKB group first) -> XTEST
//!   * PasteText         — set clipboard + synth Ctrl+V (Ctrl+V via XTEST)
//!   * GetSelectedText   — copy-probe: save clipboard, Ctrl+C, read, restore (approximate)
//!
//! Pragmatic-baseline caveats (marked TODO):
//!   * Clipboard get/set currently shells out to `xclip`/`xsel`. Moving to an in-process
//!     X11 selection owner (offer text/plain + text/html simultaneously, mirror the Windows
//!     helper's OpenClipboard exponential backoff) is the proper fix.
//!   * Modifier snapshot/restore (release physically-held mods around injection, like the
//!     Windows helper's GetKeyState dance) is not yet implemented.

use std::io::Write;
use std::process::{Command, Stdio};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _; // provides `sync()`

use super::{ActiveApp, Backend, Result, RunningApp, Selection};
use crate::{keymap, paste};

// XTEST `type` field == X event type: KeyPress=2, KeyRelease=3.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

pub struct X11Backend {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    /// Server keyboard mapping, fetched once at connect.
    keyboard: KeyboardMapping,
    has_xclip: bool,
    has_xsel: bool,
}

struct Atoms {
    net_active_window: Atom,
    net_client_list: Atom,
    net_wm_pid: Atom,
    net_wm_name: Atom,
    wm_name: Atom,
    wm_class: Atom,
}

impl X11Backend {
    pub fn connect() -> Result<X11Backend> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("x11 connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;

        // XTEST is required for key injection; surface a clear error early.
        conn.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .map_err(|e| format!("xtest query: {e}"))?
            .ok_or("XTEST extension not available on this X server")?;

        let intern = |name: &[u8]| -> Result<Atom> {
            conn.intern_atom(false, name)
                .map_err(|e| format!("intern_atom: {e}"))?
                .reply()
                .map_err(|e| format!("intern_atom reply: {e}"))
                .map(|r| r.atom)
        };
        let atoms = Atoms {
            net_active_window: intern(b"_NET_ACTIVE_WINDOW")?,
            net_client_list: intern(b"_NET_CLIENT_LIST")?,
            net_wm_pid: intern(b"_NET_WM_PID")?,
            net_wm_name: intern(b"_NET_WM_NAME")?,
            wm_name: intern(b"WM_NAME")?,
            wm_class: intern(b"WM_CLASS")?,
        };

        let keyboard = fetch_keyboard_mapping(&conn)?;
        let has_xclip = which("xclip");
        let has_xsel = which("xsel");
        if !has_xclip && !has_xsel {
            log::warn!("neither xclip nor xsel found — PasteText/clipboard ops will fail until one is installed (or an in-process selection owner is implemented)");
        }

        Ok(X11Backend {
            conn,
            root,
            atoms,
            keyboard,
            has_xclip,
            has_xsel,
        })
    }

    fn get_property_raw(&self, win: Window, prop: Atom) -> Result<Vec<u8>> {
        let reply = self
            .conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, u32::MAX)
            .map_err(|e| format!("get_property: {e}"))?
            .reply()
            .map_err(|e| format!("get_property reply: {e}"))?;
        Ok(reply.value)
    }

    fn get_property_u32(&self, win: Window, prop: Atom) -> Result<Option<u32>> {
        let reply = self
            .conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, 4)
            .map_err(|e| format!("get_property: {e}"))?
            .reply()
            .map_err(|e| format!("get_property reply: {e}"))?;
        Ok(reply.value32().and_then(|mut it| it.next()))
    }

    fn active_window(&self) -> Result<Option<Window>> {
        if let Some(win) = self
            .get_property_u32(self.root, self.atoms.net_active_window)?
            .filter(|&w| w != 0 && w != self.root)
        {
            return Ok(Some(win));
        }
        // Minimal WMs may omit EWMH; input focus can be a child of the app window.
        let mut win = self
            .conn
            .get_input_focus()
            .map_err(|e| format!("get_input_focus: {e}"))?
            .reply()
            .map_err(|e| format!("get_input_focus reply: {e}"))?
            .focus;
        for _ in 0..64 {
            if win <= 1 || win == self.root {
                return Ok(None);
            }
            let (instance, class) = self.window_class(win);
            if !class.is_empty() || !instance.is_empty() || self.window_pid(win).is_some() {
                return Ok(Some(win));
            }
            let tree = self
                .conn
                .query_tree(win)
                .map_err(|e| format!("query_tree: {e}"))?
                .reply()
                .map_err(|e| format!("query_tree reply: {e}"))?;
            if tree.parent == self.root {
                return Ok(Some(win));
            }
            win = tree.parent;
        }
        Ok(None)
    }

    fn app_for_window(&self, win: Window) -> ActiveApp {
        let (instance, class) = self.window_class(win);
        let title = self.window_title(win);
        // Resolve a friendlier app name from the pid's executable when available.
        let mut app_name = class.clone();
        if let Some(pid) = self.window_pid(win) {
            if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                let comm = comm.trim();
                if !comm.is_empty() {
                    app_name = comm.to_string();
                }
            }
        }
        let bundle_id = if !class.is_empty() {
            class
        } else if !instance.is_empty() {
            instance
        } else {
            app_name.clone()
        };
        ActiveApp {
            app_name,
            bundle_id,
            window_title: title,
            url: String::new(),
        }
    }

    fn window_title(&self, win: Window) -> String {
        // Prefer _NET_WM_NAME (UTF-8), fall back to WM_NAME.
        for prop in [self.atoms.net_wm_name, self.atoms.wm_name] {
            if let Ok(bytes) = self.get_property_raw(win, prop) {
                if !bytes.is_empty() {
                    return String::from_utf8_lossy(&bytes)
                        .trim_matches('\0')
                        .to_string();
                }
            }
        }
        String::new()
    }

    /// WM_CLASS is two NUL-separated strings: instance\0class\0. We use `class` as the app id.
    fn window_class(&self, win: Window) -> (String, String) {
        if let Ok(bytes) = self.get_property_raw(win, self.atoms.wm_class) {
            let parts: Vec<&str> = bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| std::str::from_utf8(s).unwrap_or(""))
                .collect();
            let instance = parts.first().copied().unwrap_or("").to_string();
            let class = parts.get(1).copied().unwrap_or(&instance).to_string();
            return (instance, class);
        }
        (String::new(), String::new())
    }

    fn window_pid(&self, win: Window) -> Option<u32> {
        self.get_property_u32(win, self.atoms.net_wm_pid)
            .ok()
            .flatten()
    }

    fn keycode_for_vk(&self, vk: u32) -> Result<u8> {
        let keysym = keymap::vk_to_keysym(vk).ok_or_else(|| format!("unmapped VK {vk}"))?;
        self.keyboard
            .keycode_for(keysym, self.active_group())
            .ok_or_else(|| format!("no keycode for keysym {keysym:#x} (VK {vk})"))
    }

    /// Effective XKB group (0-3), read from bits 13-14 of the core keyboard state.
    /// Falls back to group 0 if the query fails.
    fn active_group(&self) -> usize {
        self.conn
            .query_pointer(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| ((u16::from(reply.mask) >> 13) & 0x3) as usize)
            .unwrap_or(0)
    }

    fn fake_key(&self, keycode: u8, press: bool) -> Result<()> {
        let ty = if press { KEY_PRESS } else { KEY_RELEASE };
        self.conn
            .xtest_fake_input(ty, keycode, 0, self.root, 0, 0, 0)
            .map_err(|e| format!("xtest_fake_input: {e}"))?;
        // Push this event to the X server immediately. x11rb buffers requests;
        // without an explicit flush per event, only the first chord in a process
        // reliably reaches the wire (the trailing `sync()` flushed the first one,
        // but later chords were silently dropped). Flushing each fake-input — they
        // are tiny and infrequent — makes every chord land. (Verified on i3/Xorg.)
        self.conn.flush().map_err(|e| format!("xtest flush: {e}"))?;
        Ok(())
    }

    /// Press a chord: hold modifiers, tap the key, release everything (reverse order).
    fn press_chord(&self, key_keysym: u32, mod_keysyms: &[u32]) -> Result<()> {
        let group = self.active_group();
        let key_kc = self
            .keyboard
            .keycode_for(key_keysym, group)
            .ok_or_else(|| format!("no keycode for keysym {key_keysym:#x}"))?;
        let mod_kcs: Vec<u8> = mod_keysyms
            .iter()
            .filter_map(|&ks| self.keyboard.keycode_for(ks, group))
            .collect();
        // TODO: snapshot & release physically-held modifiers first (XQueryKeymap),
        // restore after — mirrors the Windows helper's GetKeyState dance.
        for kc in &mod_kcs {
            self.fake_key(*kc, true)?;
        }
        self.fake_key(key_kc, true)?;
        self.fake_key(key_kc, false)?;
        for kc in mod_kcs.iter().rev() {
            self.fake_key(*kc, false)?;
        }
        self.conn.sync().map_err(|e| format!("sync: {e}"))?;
        Ok(())
    }

    fn clipboard_set(&self, text: &str) -> Result<()> {
        // TODO: in-process X11 selection owner offering text/plain + text/html
        // simultaneously, with OpenClipboard-style retry/backoff (see Windows helper).
        if self.has_xclip {
            run_with_stdin("xclip", &["-selection", "clipboard"], text)
        } else if self.has_xsel {
            run_with_stdin("xsel", &["--clipboard", "--input"], text)
        } else {
            Err("no clipboard tool (xclip/xsel) available".into())
        }
    }

    fn clipboard_get(&self) -> Result<String> {
        if self.has_xclip {
            run_capture("xclip", &["-selection", "clipboard", "-o"])
        } else if self.has_xsel {
            run_capture("xsel", &["--clipboard", "--output"])
        } else {
            Err("no clipboard tool (xclip/xsel) available".into())
        }
    }
}

impl Backend for X11Backend {
    fn paste_text(&mut self, text: &str, _html: Option<&str>) -> Result<()> {
        let chord = paste::chord()?;
        // Clipboard-based paste: set clipboard, synthesize the configured paste chord.
        // TODO: save & restore the user's prior clipboard around the paste; offer text/html.
        self.clipboard_set(text)?;
        // brief settle so the new owner is registered before the paste chord reads it
        std::thread::sleep(std::time::Duration::from_millis(20));
        let key_keysym = keymap::vk_to_keysym(chord.key_vk)
            .ok_or_else(|| format!("unmapped paste key VK {}", chord.key_vk))?;
        let mods: Vec<u32> = chord
            .flags
            .iter()
            .filter_map(|flag| keymap::flag_to_keysym(flag))
            .collect();
        self.press_chord(key_keysym, &mods)?;
        Ok(())
    }

    fn simulate_key_press(&mut self, keycode_vk: u32, flags: &[String]) -> Result<()> {
        let key_keysym =
            keymap::vk_to_keysym(keycode_vk).ok_or_else(|| format!("unmapped VK {keycode_vk}"))?;
        let mods: Vec<u32> = flags
            .iter()
            .filter_map(|f| keymap::flag_to_keysym(f))
            .collect();
        // verify the base key resolves to a keycode for a clearer error
        let _ = self.keycode_for_vk(keycode_vk)?;
        self.press_chord(key_keysym, &mods)
    }

    fn get_active_app(&mut self) -> Result<ActiveApp> {
        let Some(win) = self.active_window()? else {
            return Ok(ActiveApp::default());
        };
        Ok(self.app_for_window(win))
    }

    fn get_running_apps(&mut self) -> Result<Vec<RunningApp>> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_client_list,
                AtomEnum::ANY,
                0,
                u32::MAX,
            )
            .map_err(|e| format!("get_property: {e}"))?
            .reply()
            .map_err(|e| format!("get_property reply: {e}"))?;
        let mut apps = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if let Some(wins) = reply.value32() {
            for win in wins {
                let (_instance, class) = self.window_class(win);
                if class.is_empty() || !seen.insert(class.clone()) {
                    continue;
                }
                let name = {
                    let t = self.window_title(win);
                    if t.is_empty() {
                        class.clone()
                    } else {
                        t
                    }
                };
                apps.push(RunningApp {
                    bundle_id: class,
                    name,
                });
            }
        }
        Ok(apps)
    }

    fn get_selected_text(&mut self) -> Result<Selection> {
        // Prefer the non-destructive AT-SPI2 Text-interface read (no synthetic
        // copy, no clipboard churn). Fall back to the copy-probe below when the
        // a11y bus is unreachable or the focused object exposes no selection.
        match super::atspi_sel::get_selection() {
            Ok(Some(sel)) => return Ok(sel),
            Ok(None) => log::debug!("AT-SPI: no selection, falling back to copy-probe"),
            Err(e) => log::debug!("AT-SPI unavailable ({e}); falling back to copy-probe"),
        }
        super::copy_probe::get_selection(
            self,
            Self::clipboard_get,
            Self::clipboard_set,
            |backend| {
                let ctrl = keymap::flag_to_keysym("Control").unwrap();
                backend.press_chord(b'c' as u32, &[ctrl])?;
                std::thread::sleep(std::time::Duration::from_millis(60));
                Ok(())
            },
        )
    }

    fn accessibility_status(&mut self) -> bool {
        // X11 + XTEST present == we can inject/read; AT-SPI reachability check lands
        // with the AT-SPI selection work. Report true when the connection is live.
        true
    }

    fn name(&self) -> &'static str {
        "x11"
    }
}

/// Core-protocol keyboard mapping: `per` keysyms for each keycode from `min_keycode`.
/// Columns 2g and 2g+1 hold levels 1-2 of XKB group g for groups 0 and 1.
struct KeyboardMapping {
    min_keycode: u8,
    per: usize,
    keysyms: Vec<u32>,
}

impl KeyboardMapping {
    /// Keycode that types `keysym` in `group`, falling back to the first keycode that
    /// carries it in any column. Without the group pass, a multi-layout map (GNOME
    /// always appends `us`, e.g. `us(dvorak),us`) resolves `v` to the QWERTY V
    /// keycode, which types `k` in Dvorak, so Ctrl+V lands as Ctrl+K.
    fn keycode_for(&self, keysym: u32, group: usize) -> Option<u8> {
        if keysym == 0 || self.per == 0 {
            return None;
        }
        let find = |cols: std::ops::Range<usize>| {
            self.keysyms
                .chunks(self.per)
                .position(|syms| {
                    syms.iter()
                        .take(cols.end)
                        .skip(cols.start)
                        .any(|&s| s == keysym)
                })
                .map(|i| self.min_keycode + i as u8)
        };
        // Core columns 4+ interleave higher levels with groups 3-4; only 0 and 1 are reliable.
        let group_cols = if group < 2 {
            2 * group..2 * group + 2
        } else {
            0..0
        };
        find(group_cols).or_else(|| find(0..self.per))
    }
}

fn fetch_keyboard_mapping(conn: &RustConnection) -> Result<KeyboardMapping> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let count = max - min + 1;
    let mapping = conn
        .get_keyboard_mapping(min, count)
        .map_err(|e| format!("get_keyboard_mapping: {e}"))?
        .reply()
        .map_err(|e| format!("get_keyboard_mapping reply: {e}"))?;
    Ok(KeyboardMapping {
        min_keycode: min,
        per: mapping.keysyms_per_keycode as usize,
        keysyms: mapping.keysyms,
    })
}

fn which(prog: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
        .unwrap_or(false)
}

fn run_with_stdin(prog: &str, args: &[&str], input: &str) -> Result<()> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {prog}: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(input.as_bytes())
        .map_err(|e| format!("write {prog} stdin: {e}"))?;
    let status = child.wait().map_err(|e| format!("wait {prog}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{prog} exited with {status}"))
    }
}

fn run_capture(prog: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(prog)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {prog}: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const XK_K_LOWER: u32 = 0x6b;
    const XK_K_UPPER: u32 = 0x4b;
    const XK_V_LOWER: u32 = 0x76;
    const XK_V_UPPER: u32 = 0x56;
    const XK_PERIOD: u32 = 0x2e;
    const XK_GREATER: u32 = 0x3e;
    const XK_CYRILLIC_EM: u32 = 0x6cd;
    const XK_CYRILLIC_EM_UPPER: u32 = 0x6ed;

    /// Mapping with 4 keysyms per keycode (two groups), min keycode 8, and the
    /// given keycodes filled in; every other keycode is NoSymbol.
    fn mapping(keys: &[(u8, [u32; 4])]) -> KeyboardMapping {
        let min_keycode = 8u8;
        let per = 4;
        let mut keysyms = vec![0u32; (255 - min_keycode as usize + 1) * per];
        for (keycode, syms) in keys {
            let start = (*keycode - min_keycode) as usize * per;
            keysyms[start..start + per].copy_from_slice(syms);
        }
        KeyboardMapping {
            min_keycode,
            per,
            keysyms,
        }
    }

    /// `us(dvorak),us` — what GNOME applies for a lone Dvorak input source.
    fn dvorak_with_us() -> KeyboardMapping {
        mapping(&[
            (55, [XK_K_LOWER, XK_K_UPPER, XK_V_LOWER, XK_V_UPPER]),
            (60, [XK_V_LOWER, XK_V_UPPER, XK_PERIOD, XK_GREATER]),
        ])
    }

    /// Ctrl+V must use the key that types `v` in the active layout, not the first
    /// keycode carrying `v` in any group (keycode 55 types `k` in Dvorak).
    #[test]
    fn active_group_wins_over_lower_keycode() {
        let map = dvorak_with_us();
        assert_eq!(map.keycode_for(XK_V_LOWER, 0), Some(60));
        assert_eq!(map.keycode_for(XK_K_LOWER, 0), Some(55));
    }

    #[test]
    fn second_group_active() {
        let map = dvorak_with_us();
        assert_eq!(map.keycode_for(XK_V_LOWER, 1), Some(55));
        assert_eq!(map.keycode_for(XK_PERIOD, 1), Some(60));
    }

    /// Non-Latin active layout: `v` exists only in the appended `us` group, so the
    /// lookup falls back to it (toolkits resolve Ctrl+V via that Latin group).
    #[test]
    fn falls_back_to_any_group() {
        let map = mapping(&[(
            55,
            [XK_CYRILLIC_EM, XK_CYRILLIC_EM_UPPER, XK_V_LOWER, XK_V_UPPER],
        )]);
        assert_eq!(map.keycode_for(XK_V_LOWER, 0), Some(55));
    }

    /// Groups 3-4 have no fixed core columns, so they use the any-column fallback.
    #[test]
    fn high_group_uses_fallback() {
        let map = dvorak_with_us();
        assert_eq!(map.keycode_for(XK_V_LOWER, 2), Some(55));
    }

    #[test]
    fn missing_keysym() {
        let map = dvorak_with_us();
        assert_eq!(map.keycode_for(XK_CYRILLIC_EM, 0), None);
        assert_eq!(map.keycode_for(0, 0), None);
    }
}

/// Native X11 focus events must not depend on an application's accessibility tree.
pub struct FocusTracker {
    state: std::sync::Arc<std::sync::Mutex<FocusState>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct FocusState {
    backend: X11Backend,
    events: super::EventSink,
    active: bool,
    last: Option<(Option<Window>, serde_json::Value)>,
    sequence: u64,
}

impl FocusState {
    fn announce(&mut self, force: bool) {
        if !self.active {
            return;
        }
        let window = self.backend.active_window().ok().flatten();
        let app = window
            .map(|win| self.backend.app_for_window(win))
            .unwrap_or_default();
        let payload = super::active_app_payload(&app);
        let key = (window, payload.clone());
        if !force && self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);
        self.sequence += 1;
        let uuid = format!("x11-focus-{}-{}", std::process::id(), self.sequence);
        let _ = self
            .events
            .send(crate::proto::request("AppInfoUpdate", payload, &uuid));
    }
}

impl FocusTracker {
    pub fn start(events: super::EventSink) -> Result<Self> {
        use std::sync::{atomic::AtomicBool, Arc, Mutex};
        let state = Arc::new(Mutex::new(FocusState {
            backend: X11Backend::connect()?,
            events,
            active: true,
            last: None,
            sequence: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("x11-focus-watch".into())
            .spawn(move || {
                while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(mut state) = thread_state.lock() {
                        state.announce(false);
                    }
                    // Bound idle work while keeping app changes responsive during dictation.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .map_err(|e| format!("X11 focus thread: {e}"))?;
        Ok(Self { state, stop })
    }
}

impl Drop for FocusTracker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl super::ActiveAppProvider for FocusTracker {
    fn current(&self) -> Option<ActiveApp> {
        self.state.lock().ok()?.backend.get_active_app().ok()
    }

    fn get_running_apps(&self) -> Vec<RunningApp> {
        self.state
            .lock()
            .ok()
            .and_then(|mut s| s.backend.get_running_apps().ok())
            .unwrap_or_default()
    }

    fn set_focus_detection(&self, active: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.active = active;
            state.announce(true);
        }
    }

    fn name(&self) -> &'static str {
        "x11"
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;
    use std::time::Duration;
    use x11rb::protocol::xproto::{CreateWindowAux, InputFocus, WindowClass};

    // Run only under a private Xvfb: this test changes the server's input focus.
    #[test]
    #[ignore = "requires an isolated X server: xvfb-run cargo test native_focus -- --ignored"]
    fn native_focus_without_accessibility() {
        let (conn, screen) = x11rb::connect(None).unwrap();
        let root = conn.setup().roots[screen].root;
        let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let active = atom(b"_NET_ACTIVE_WINDOW");
        let class = atom(b"WM_CLASS");
        let title = atom(b"_NET_WM_NAME");
        let pid = atom(b"_NET_WM_PID");
        let mut windows = Vec::new();
        for _ in 0..2 {
            let win = conn.generate_id().unwrap();
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                root,
                0,
                0,
                100,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
            conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                win,
                class,
                AtomEnum::STRING,
                b"code\0Code\0",
            )
            .unwrap();
            conn.change_property8(
                x11rb::protocol::xproto::PropMode::REPLACE,
                win,
                title,
                AtomEnum::STRING,
                b"same title",
            )
            .unwrap();
            conn.map_window(win).unwrap().check().unwrap();
            windows.push(win);
        }
        let focus = |win| {
            conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
        };
        conn.delete_property(root, active).unwrap().check().unwrap();
        focus(windows[0]);
        let (tx, rx) = std::sync::mpsc::channel();
        let tracker = crate::backend::pick_active_app_provider(tx).unwrap();
        assert_eq!(tracker.name(), "x11");
        let next = || {
            rx.recv_timeout(Duration::from_secs(2))
                .expect("focus event")
        };
        let app = |event: serde_json::Value| {
            event["HelperAPIRequest"]["AppInfoUpdate"]["payload"].clone()
        };
        assert_eq!(app(next())["bundleId"], "Code");
        let child = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            child,
            windows[0],
            0,
            0,
            20,
            20,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        conn.map_window(child).unwrap().check().unwrap();
        focus(child);
        assert_eq!(tracker.current().unwrap().bundle_id, "Code");
        // Identical titles/classes must not suppress a different native window.
        focus(windows[1]);
        assert_eq!(app(next())["bundleId"], "Code");
        tracker.set_focus_detection(false);
        while rx.try_recv().is_ok() {}
        focus(windows[0]);
        assert!(rx.recv_timeout(Duration::from_millis(250)).is_err());
        tracker.set_focus_detection(true);
        assert_eq!(app(next())["bundleId"], "Code");
        tracker.set_focus_detection(true);
        assert_eq!(app(next())["bundleId"], "Code");
        // A classless window still has a stable process identity, never its title.
        conn.delete_property(windows[0], class).unwrap();
        conn.change_property32(
            x11rb::protocol::xproto::PropMode::REPLACE,
            windows[0],
            pid,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )
        .unwrap();
        conn.sync().unwrap();
        let name = std::fs::read_to_string(format!("/proc/{}/comm", std::process::id())).unwrap();
        assert_eq!(app(next())["bundleId"], name.trim());
        // Missing identity and no focus clear app-specific style context.
        conn.delete_property(windows[0], pid)
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(app(next())["bundleId"], "");
        focus(root);
        assert_eq!(app(next())["windowTitle"], "");
        // EWMH takes precedence over the input-focus fallback.
        conn.change_property32(
            x11rb::protocol::xproto::PropMode::REPLACE,
            root,
            active,
            AtomEnum::WINDOW,
            &[windows[1]],
        )
        .unwrap()
        .check()
        .unwrap();
        assert_eq!(app(next())["bundleId"], "Code");
        drop(tracker);
        for win in windows {
            conn.destroy_window(win).unwrap();
        }
        conn.delete_property(root, active).unwrap();
        conn.sync().unwrap();
    }
}
