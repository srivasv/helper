#!/usr/bin/env python3
"""Exercise paste parity through real IPC in a private display and D-Bus session."""

import json, os, queue, subprocess, sys, threading, time, tempfile
import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk

if len(sys.argv) == 2:
    # Never inject into the caller's desktop.
    raise SystemExit(
        subprocess.call(
            [
                "dbus-run-session",
                "--",
                "xvfb-run",
                "-a",
                "env",
                "-u",
                "WAYLAND_DISPLAY",
                "-u",
                "KDE_FULL_SESSION",
                "XDG_CURRENT_DESKTOP=",
                "XDG_SESSION_DESKTOP=",
                "NO_AT_BRIDGE=0",
                sys.executable,
                os.path.abspath(__file__),
                "--isolated",
                os.path.abspath(sys.argv[1]),
            ]
        )
    )
if len(sys.argv) != 3 or sys.argv[1] != "--isolated":
    raise SystemExit("usage: parity_test.py <helper-binary>")
binary = sys.argv[2]
logs = tempfile.TemporaryDirectory(prefix="wispr-parity-")


def pump():
    while Gtk.events_pending():
        Gtk.main_iteration_do(False)


def wait_until(predicate, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        pump()
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError("condition timed out")


class Helper:
    def __init__(self, accessible=True, override=None):
        self.responses = queue.Queue()
        rfd, wfd = os.pipe()
        env = dict(os.environ, RUST_LOG="debug")
        if not accessible:
            env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=/tmp/wispr-unavailable-bus"
        if override:
            env["WISPR_LINUX_HELPER_PASTE_KEYS"] = override
        self.log = open(os.path.join(logs.name, "helper.log"), "a")
        self.proc = subprocess.Popen(
            ["bash", "-c", 'exec 3>&"$0"; exec "$@"', str(wfd), binary],
            pass_fds=(wfd,),
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=self.log,
            env=env,
        )
        os.close(wfd)

        def reader():
            buf = b""
            try:
                while chunk := os.read(rfd, 4096):
                    buf += chunk
                    while b"|" in buf:
                        raw, buf = buf.split(b"|", 1)
                        self.responses.put(
                            json.loads(
                                raw.decode().replace("+2", "|").replace("+1", "+")
                            )
                        )
            finally:
                os.close(rfd)

        self.reader = threading.Thread(target=reader, daemon=True)
        self.reader.start()

    def send(self, command, payload, uid):
        raw = json.dumps(
            {"HelperAPIRequest": {command: {"payload": payload}, "uuid": uid}}
        )
        self.proc.stdin.write(raw.replace("+", "+1").replace("|", "+2").encode() + b"|")
        self.proc.stdin.flush()

    def receive(self, key, uid=None, timeout=10):
        result = []

        def found():
            while not self.responses.empty():
                msg = self.responses.get()
                for envelope in msg.values():
                    if key in envelope and (uid is None or envelope.get("uuid") == uid):
                        result.append(envelope[key])
                        return True
            return False

        wait_until(found, timeout)
        return result[0]

    def close(self):
        self.proc.terminate()
        self.proc.wait(timeout=5)
        self.log.close()


window = Gtk.Window(title="Wispr helper parity test")
window.set_wmclass("kitty", "kitty")
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
entry = Gtk.Entry()
other = Gtk.Entry()
box.pack_start(entry, True, True, 0)
box.pack_start(other, True, True, 0)
window.add(box)
window.show_all()
window.present()
entry.grab_focus()
pump()
xid = window.get_window().get_xid()
subprocess.run(["xdotool", "windowfocus", str(xid)], check=True)
subprocess.run(
    [
        "xprop",
        "-root",
        "-f",
        "_NET_ACTIVE_WINDOW",
        "32x",
        "-set",
        "_NET_ACTIVE_WINDOW",
        hex(xid),
    ],
    check=True,
)
seen = []


def on_key(widget, event):
    if (
        event.keyval in (Gdk.KEY_v, Gdk.KEY_V)
        and event.state & Gdk.ModifierType.CONTROL_MASK
    ):
        shift = bool(event.state & Gdk.ModifierType.SHIFT_MASK)
        seen.append(shift)
        widget.emit("paste-clipboard")
        return True
    return False


entry.connect("key-press-event", on_key)
clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
helper = Helper(accessible=False)
try:
    helper.send("PasteText", {"text": "terminal paste"}, "terminal")
    helper.receive("ACK", "terminal")
    wait_until(lambda: entry.get_text() == "terminal paste")
    assert seen == [True], seen
    print(
        "PASS: native X11 terminal paste uses Ctrl+Shift+V without an app provider",
        flush=True,
    )
    subprocess.run(
        [
            "xprop",
            "-id",
            str(xid),
            "-f",
            "WM_CLASS",
            "8s",
            "-set",
            "WM_CLASS",
            "Warpinator",
        ],
        check=True,
    )
    entry.set_text("")
    seen.clear()
    helper.send("PasteText", {"text": "normal paste"}, "normal")
    helper.receive("ACK", "normal")
    wait_until(lambda: entry.get_text() == "normal paste")
    assert seen == [False], seen
    print("PASS: non-terminal native X11 app keeps Ctrl+V", flush=True)
finally:
    helper.close()
helper = Helper(accessible=False, override="ctrl+x")
try:
    clipboard.set_text("keep clipboard", -1)
    pump()
    helper.send("PasteText", {"text": "must not paste"}, "invalid")
    helper.receive("HelperAPIError", "invalid")
    assert clipboard.wait_for_text() == "keep clipboard"
    print("PASS: invalid override preserves clipboard", flush=True)
finally:
    helper.close()
helper = Helper()
try:
    entry.set_text("")
    entry.grab_focus()
    pump()
    helper.send(
        "PasteText", {"text": "hello wisper 日本", "pasteId": "edit-1"}, "paste"
    )
    helper.receive("ACK", "paste")
    wait_until(lambda: entry.get_text() == "hello wisper 日本")
    deadline = time.monotonic() + 1.5
    while time.monotonic() < deadline:
        pump()
        time.sleep(0.01)
    entry.set_text("hello whisper 日本")
    deadline = time.monotonic() + 0.8
    while time.monotonic() < deadline:
        pump()
        time.sleep(0.01)
    other.grab_focus()
    pump()
    result = helper.receive("UpdateEditedText", timeout=12)["payload"]
    assert result["pasteId"] == "edit-1", result
    assert result["pastedText"] == "hello wisper 日本", result
    assert result["editedText"] == "hello whisper 日本", result
    print(
        "PASS: GTK post-paste correction reports edited Unicode text over IPC",
        flush=True,
    )
finally:
    helper.close()
    window.destroy()
    pump()
