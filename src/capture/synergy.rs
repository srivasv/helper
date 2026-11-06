//! Synergy 3 focus gate for raw key capture (physical and virtual keyboards).

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use crate::backend::EventSink;

pub(super) fn route(events: EventSink) -> EventSink {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut gate = Gate::default();
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
        let mut focus = state_home.map(|p| FocusLog::new(p.join("Synergy/synergy.jsonl")));
        loop {
            let local = focus.as_mut().is_none_or(|f| f.local());
            gate.set_local(local, &events);
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(event) => {
                    // Refresh after waking: focus may have changed while waiting.
                    gate.set_local(focus.as_mut().is_none_or(|f| f.local()), &events);
                    gate.forward(event, &events);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    tx
}

#[derive(Default)]
struct Gate {
    local: bool,
    held: HashSet<u32>,
    index: u64,
}

impl Gate {
    fn set_local(&mut self, local: bool, events: &EventSink) {
        if self.local && !local {
            for vk in self.held.drain().collect::<Vec<_>>() {
                let event = crate::proto::request(
                    "KeypressEvent",
                    serde_json::json!({"payload": {
                        "eventType": "key_event_release", "key": vk,
                        "inputType": "keyboard"
                    }}),
                    "focus-release",
                );
                self.send(event, events);
            }
        }
        self.local = local;
    }

    fn forward(&mut self, event: Value, events: &EventSink) {
        let payload = &event["HelperAPIRequest"]["KeypressEvent"]["payload"];
        let Some(vk) = payload["key"].as_u64().and_then(|v| u32::try_from(v).ok()) else {
            return;
        };
        let press = payload["eventType"] == "key_event_press";
        if press {
            if !self.local {
                return;
            }
            self.held.insert(vk);
        } else if !self.held.remove(&vk) {
            return;
        }
        self.send(event, events);
    }

    fn send(&mut self, mut event: Value, events: &EventSink) {
        self.index += 1;
        event["HelperAPIRequest"]["KeypressEvent"]["payload"]["index"] = self.index.into();
        event["HelperAPIRequest"]["uuid"] =
            format!("kp-{}-{}", std::process::id(), self.index).into();
        let _ = events.send(event);
    }
}

struct FocusLog {
    path: PathBuf,
    core: Option<(PathBuf, String)>,
    next_scan: std::time::Instant,
    tail: Option<Tail>,
    local: Option<bool>,
    reported: Option<bool>,
}

impl FocusLog {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            core: None,
            next_scan: std::time::Instant::now(),
            tail: None,
            local: None,
            reported: None,
        }
    }

    fn local(&mut self) -> bool {
        let now = std::time::Instant::now();
        if now >= self.next_scan {
            self.next_scan = now + Duration::from_secs(1);
            let core = find_core();
            if self.core != core {
                self.core = core;
                self.tail = None;
                self.local = None;
            }
        }
        let Some((proc_path, since)) = &self.core else {
            return true;
        };
        if !proc_path.exists() {
            return true;
        }
        // shortcut: Synergy 3 INFO JSONL logs are the focus interface; replace
        // this adapter when Synergy exposes active-screen state over IPC.
        let result = (|| -> std::io::Result<()> {
            let file = File::open(&self.path)?;
            let metadata = file.metadata()?;
            let identity = (metadata.dev(), metadata.ino());
            if self.tail.as_ref().is_none_or(|t| t.identity != identity) {
                if let Some(old) = &mut self.tail {
                    old.update(since, &mut self.local)?;
                } else if let Ok(previous) = File::open(self.path.with_extension("jsonl.1")) {
                    Tail::new(previous)?.update(since, &mut self.local)?;
                }
                self.tail = Some(Tail::new(file)?);
            }
            self.tail.as_mut().unwrap().update(since, &mut self.local)
        })();
        if let Err(e) = result {
            if self.local.is_some() {
                log::warn!("Synergy focus log unavailable: {e}");
            }
            self.local = None;
        }
        let local = self.local.unwrap_or(false);
        if self.reported != Some(local) {
            if self.local.is_none() {
                log::warn!(
                    "Synergy focus unknown: pausing shortcuts until startup or a screen entry/exit is logged"
                );
            } else {
                log::info!(
                    "Synergy focus: {}",
                    if local {
                        "local"
                    } else {
                        "remote; shortcuts paused"
                    }
                );
            }
            self.reported = Some(local);
        }
        local
    }
}

struct Tail {
    file: File,
    identity: (u64, u64),
    offset: u64,
    pending: String,
}

impl Tail {
    fn new(mut file: File) -> std::io::Result<Self> {
        let meta = file.metadata()?;
        // Bound startup work even if the user's log rotation is disabled.
        let offset = meta.len().saturating_sub(2 * 1024 * 1024);
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            file,
            identity: (meta.dev(), meta.ino()),
            offset,
            pending: String::new(),
        })
    }

    fn update(&mut self, since: &str, local: &mut Option<bool>) -> std::io::Result<()> {
        if self.file.metadata()?.len() < self.offset {
            self.file.seek(SeekFrom::Start(0))?;
            self.offset = 0;
            self.pending.clear();
        }
        let mut bytes = Vec::new();
        let n = self
            .file
            .by_ref()
            .take(2 * 1024 * 1024)
            .read_to_end(&mut bytes)?;
        self.offset += n as u64;
        self.pending.push_str(&String::from_utf8_lossy(&bytes));
        if let Some(end) = self.pending.rfind('\n') {
            for line in self.pending[..end].lines() {
                if let Some(state) = focus_record(line, since) {
                    *local = Some(state);
                }
            }
            self.pending.drain(..=end);
        }
        if self.pending.len() > 64 * 1024 {
            self.pending.clear();
        }
        Ok(())
    }
}

fn focus_record(line: &str, since: &str) -> Option<bool> {
    let record: Value = serde_json::from_str(line).ok()?;
    if record["source"] != "core" || record["ts"].as_str()?.get(..19)? < since {
        return None;
    }
    match record["msg"].as_str()? {
        // A fresh core starts locally before any screen transition is logged.
        "starting core" => Some(true),
        "entering screen" => Some(true),
        "leaving screen" => Some(false),
        _ => None,
    }
}

fn find_core() -> Option<(PathBuf, String)> {
    let proc_path = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        })
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.uid() == unsafe { libc::getuid() })
                && std::fs::read_link(p.join("exe")).ok().is_some_and(|exe| {
                    exe.file_name() == Some(std::ffi::OsStr::new("synergy-core"))
                })
        })?;
    let stat = std::fs::read_to_string(proc_path.join("stat")).ok()?;
    let ticks: u64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let boot: u64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .parse()
        .ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let start = (boot + ticks / hz as u64) as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::gmtime_r(&start, &mut tm) }.is_null() {
        return None;
    }
    let since = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    );
    Some((proc_path, since))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicU64;

    fn record(message: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "ts": "2026-11-05T18:00:00.000Z", "source": "core", "msg": message
            })
        )
    }

    #[test]
    fn focus_records_require_core_exact_message_and_current_process_time() {
        assert_eq!(focus_record(&record("starting core"), ""), Some(true));
        assert_eq!(
            focus_record(&record("starting core"), "2026-11-05T19:00:00"),
            None
        );
        assert_eq!(
            focus_record(
                &record("starting core").replace("\"source\":\"core\"", "\"source\":\"service\""),
                ""
            ),
            None
        );
        assert_eq!(
            focus_record(&record("leaving screen"), "2026-11-05T17:00:00"),
            Some(false)
        );
        assert_eq!(
            focus_record(&record("entering screen"), "2026-11-05T17:00:00"),
            Some(true)
        );
        assert_eq!(
            focus_record(&record("leaving screen"), "2026-11-05T19:00:00"),
            None
        );
        assert_eq!(
            focus_record(&record("leaving screen").replace("core", "service"), ""),
            None
        );
        assert_eq!(focus_record(&record("not leaving screen"), ""), None);
        assert_eq!(focus_record("incomplete JSON", ""), None);
    }

    #[test]
    fn focus_log_handles_bootstrap_partial_lines_rotation_truncation_and_core_exit() {
        let dir = std::env::temp_dir().join(format!("wispr-synergy-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("synergy.jsonl");
        let previous = dir.join("synergy.jsonl.1");
        std::fs::write(&previous, record("starting core")).unwrap();
        std::fs::write(&path, "").unwrap();
        let mut focus = FocusLog::new(path.clone());
        focus.core = Some((dir.clone(), "2026-11-05T17:00:00".into()));
        focus.next_scan = std::time::Instant::now() + Duration::from_secs(3600);
        assert!(
            focus.local(),
            "startup enables shortcuts before the first screen switch"
        );
        let mut previous_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&previous)
            .unwrap();
        previous_file
            .write_all(record("leaving screen").as_bytes())
            .unwrap();
        focus.tail = None;
        assert!(!focus.local(), "bootstrap remote focus from rotated log");
        let entry = record("entering screen");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&entry.as_bytes()[..entry.len() - 1])
            .unwrap();
        assert!(!focus.local(), "partial record must not change focus");
        file.write_all(b"\n").unwrap();
        assert!(focus.local());
        std::fs::rename(&path, &previous).unwrap();
        // The old inode can receive its last record after the rename.
        file.write_all(record("leaving screen").as_bytes()).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(!focus.local());
        std::fs::write(&path, record("entering screen")).unwrap();
        assert!(focus.local());
        std::fs::write(&path, record("leaving screen")).unwrap();
        assert!(
            !focus.local(),
            "copy-truncate rotation must reset the offset"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(!focus.local(), "unreadable focus pauses shortcuts");
        std::fs::remove_file(&previous).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert!(focus.local(), "core exit restores ordinary shortcuts");
    }

    fn key(gate: &mut Gate, events: &EventSink, vk: u32, press: bool) {
        let (tx, rx) = mpsc::channel();
        super::super::emit_keypress(&tx, &AtomicU64::new(0), 1, vk, press);
        gate.forward(rx.recv().unwrap(), events);
    }

    #[test]
    #[ignore = "requires Synergy 3 and an operator moving between screens"]
    fn live_synergy_capture() {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/state")
            });
        let mut focus = FocusLog::new(state_home.join("Synergy/synergy.jsonl"));
        assert!(find_core().is_some(), "Synergy must be running");
        let (raw_tx, raw_rx) = mpsc::channel();
        let (gated_tx, gated_rx) = mpsc::channel();
        let _raw = super::super::evdev::start(raw_tx).expect("raw evdev capture");
        let _gated = super::super::spawn(gated_tx);
        let mut local_frames = 0;
        let mut remote_raw = 0;
        let mut remote_leaks = 0;
        let mut local_after_remote = 0;
        let mut previous_local = focus.local();
        println!("LIVE READY: 120 seconds; Linux, Mac, then Linux. Test physical keyboard and OpenDeck on each.");
        for second in 1..=120 {
            std::thread::sleep(Duration::from_secs(1));
            let local = focus.local();
            let raw = raw_rx.try_iter().count();
            let gated = gated_rx.try_iter().count();
            // Transition buckets may contain pre-switch events or key releases.
            if local == previous_local {
                if local {
                    local_frames += gated;
                    if remote_raw > 0 {
                        local_after_remote += gated;
                    }
                } else {
                    remote_raw += raw;
                    remote_leaks += gated;
                }
            }
            println!(
                "LIVE {second:02}s focus={} raw={raw} gated={gated}",
                if local { "Linux" } else { "remote" }
            );
            previous_local = local;
        }
        println!("LIVE RESULT local={local_frames} remote_raw={remote_raw} remote_leaks={remote_leaks} return_local={local_after_remote}");
        assert!(local_frames > 0, "no local shortcut input tested");
        assert!(remote_raw > 0, "no remote shortcut input tested");
        assert_eq!(remote_leaks, 0, "keys reached Wispr while remote");
        assert!(
            local_after_remote > 0,
            "no input tested after returning to Linux"
        );
    }

    #[test]
    fn shortcuts_only_reach_app_while_local_and_keys_are_released_on_leave() {
        let (tx, rx) = mpsc::channel();
        let mut gate = Gate::default();
        gate.set_local(true, &tx);
        key(&mut gate, &tx, 162, true);
        gate.set_local(false, &tx);
        key(&mut gate, &tx, 65, true);
        key(&mut gate, &tx, 65, false);
        key(&mut gate, &tx, 162, false);
        gate.set_local(true, &tx);
        key(&mut gate, &tx, 65, false); // remote press, local release
        key(&mut gate, &tx, 66, true);
        key(&mut gate, &tx, 66, false);
        let frames: Vec<_> = rx.try_iter().collect();
        let payloads: Vec<_> = frames
            .iter()
            .map(|e| &e["HelperAPIRequest"]["KeypressEvent"]["payload"])
            .collect();
        assert_eq!(payloads.len(), 4);
        assert_eq!(payloads[0]["key"], 162);
        assert_eq!(payloads[1]["eventType"], "key_event_release");
        assert_eq!(payloads[1]["key"], 162);
        assert_eq!(payloads[2]["key"], 66);
        for (i, p) in payloads.iter().enumerate() {
            assert_eq!(p["index"], i + 1);
        }
    }
}
