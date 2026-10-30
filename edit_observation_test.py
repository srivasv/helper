#!/usr/bin/env python3
"""Exercise GTK4 and Qt edit observation with focus events and bounded search."""

import os
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

if len(sys.argv) == 2:
    messages = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
    binary = next(
        m["executable"]
        for m in messages
        if m.get("reason") == "compiler-artifact"
        and m.get("profile", {}).get("test")
        and m.get("executable")
    )
    with tempfile.TemporaryDirectory(prefix="wispr-gtk4-") as config:
        env = dict(
            os.environ,
            XDG_CONFIG_HOME=config,
            GSETTINGS_BACKEND="memory",
            XDG_CURRENT_DESKTOP="",
            XDG_SESSION_DESKTOP="",
            GDK_BACKEND="x11",
            NO_AT_BRIDGE="0",
            GTK_A11Y="atspi",
            QT_QPA_PLATFORM="xcb",
            QT_LINUX_ACCESSIBILITY_ALWAYS_ON="1",
            WISPR_EDIT_TEST_ISOLATED="1",
            RUST_LOG="wispr_flow_linux_helper=debug",
        )
        for key in ("WAYLAND_DISPLAY", "AT_SPI_BUS_ADDRESS", "KDE_FULL_SESSION"):
            env.pop(key, None)
        for toolkit in ("gtk4", "qt"):
            for focus_events in ("0", "1"):
                env["WISPR_TEST_TOOLKIT"] = toolkit
                env["WISPR_TEST_FOCUS_EVENTS"] = focus_events
                result = subprocess.call(
                    ["xvfb-run", "-a", "dbus-run-session", "--", sys.executable,
                     os.path.abspath(__file__), "--isolated", binary], env=env,
                )
                if result:
                    raise SystemExit(result)
        raise SystemExit(0)

if len(sys.argv) != 3 or sys.argv[1] != "--isolated":
    raise SystemExit("usage: edit_observation_test.py <cargo-test-build.json>")


toolkit = os.environ["WISPR_TEST_TOOLKIT"]
if toolkit == "gtk4":
    version = subprocess.check_output(["zenity", "--version"], text=True).strip()
    assert int(version.split(".")[0]) >= 4, "This regression requires GTK4 Zenity"
    dialog_command = ["zenity", "--entry", "--title=Wispr edit regression",
                      "--text=Test field", "--entry-text=hello wisper world"]
else:
    dialog_command = ["kdialog", "--title", "Wispr edit regression",
                      "--inputbox", "Test field", "hello wisper world"]


def key(*args):
    subprocess.run(["xdotool", *args], check=True)


with tempfile.TemporaryDirectory(prefix="wispr-gtk4-logs-") as logs:
    logs = Path(logs)
    with open(logs / "zenity.log", "w") as log:
        zenity = subprocess.Popen(
            dialog_command,
            stdout=subprocess.DEVNULL,
            stderr=log,
        )
    test = None
    try:
        deadline = time.monotonic() + 8
        windows = []
        while not windows:
            windows = subprocess.run(
                [
                    "xdotool",
                    "search",
                    "--onlyvisible",
                    "--pid",
                    str(zenity.pid),
                ],
                capture_output=True,
                text=True,
            ).stdout.split()
            assert time.monotonic() < deadline, "Zenity did not open"
            if not windows:
                time.sleep(0.05)
        key("windowfocus", "--sync", windows[0])
        subprocess.run(
            [
                "xprop",
                "-root",
                "-f",
                "_NET_ACTIVE_WINDOW",
                "32x",
                "-set",
                "_NET_ACTIVE_WINDOW",
                hex(int(windows[0])),
            ],
            check=True,
        )
        key("key", "End")
        with open(logs / "observer.log", "w") as log:
            test = subprocess.Popen(
                [
                    sys.argv[2],
                    "--ignored",
                    "--exact",
                    "backend::atspi_edit::tests::toolkit_edit_observation",
                    "--nocapture",
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        if os.environ.get("WISPR_TEST_FOCUS_EVENTS") == "1":
            deadline = time.monotonic() + 8
            while "atspi_focus: listening" not in (logs / "observer.log").read_text():
                assert test.poll() is None, (logs / "observer.log").read_text()
                assert time.monotonic() < deadline, "Focus listener did not start"
                time.sleep(0.02)
            key("key", "Tab")
            key("key", "shift+Tab")
            key("key", "End")
        deadline = time.monotonic() + 12
        while "observing (before=" not in (logs / "observer.log").read_text():
            if test.poll() is not None:
                raise AssertionError((logs / "observer.log").read_text())
            assert time.monotonic() < deadline, "Observer did not baseline the field"
            time.sleep(0.02)
        if os.environ.get("WISPR_TEST_FOCUS_EVENTS") == "1":
            assert "using focus-event target" in (logs / "observer.log").read_text()
        key("key", "ctrl+a")
        key("type", "--clearmodifiers", "hello whisper world")
        # Allow two observer polls to capture the completed correction.
        time.sleep(0.85)
        key("key", "Tab")
        result = test.wait(timeout=5)
        assert result == 0, (logs / "observer.log").read_text()
        print("PASS: " + toolkit + " correction observed; focus events=" + os.environ.get("WISPR_TEST_FOCUS_EVENTS", "0"))
    finally:
        if test is not None and test.poll() is None:
            test.terminate()
            test.wait(timeout=3)
        zenity.terminate()
        zenity.wait(timeout=3)
