#!/usr/bin/env python3
"""Exercise edit observation without a focus tracker against a real GTK4 dialog."""

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
            WISPR_GTK4_TEST_ISOLATED="1",
            RUST_LOG="wispr_flow_linux_helper=debug",
        )
        for key in ("WAYLAND_DISPLAY", "AT_SPI_BUS_ADDRESS", "KDE_FULL_SESSION"):
            env.pop(key, None)
        raise SystemExit(
            subprocess.call(
                [
                    "xvfb-run",
                    "-a",
                    "dbus-run-session",
                    "--",
                    sys.executable,
                    os.path.abspath(__file__),
                    "--isolated",
                    binary,
                ],
                env=env,
            )
        )
if len(sys.argv) != 3 or sys.argv[1] != "--isolated":
    raise SystemExit("usage: gtk4_edit_test.py <cargo-test-build.json>")


version = subprocess.check_output(["zenity", "--version"], text=True).strip()
assert int(version.split(".")[0]) >= 4, "This regression requires GTK4 Zenity"


def key(*args):
    subprocess.run(["xdotool", *args], check=True)


with tempfile.TemporaryDirectory(prefix="wispr-gtk4-logs-") as logs:
    logs = Path(logs)
    with open(logs / "zenity.log", "w") as log:
        zenity = subprocess.Popen(
            [
                "zenity",
                "--entry",
                "--title=Wispr GTK4 edit regression",
                "--text=Test field",
                "--entry-text=hello wisper world",
            ],
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
                    "backend::atspi_edit::tests::gtk4_observation_without_focus_tracker",
                    "--nocapture",
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        deadline = time.monotonic() + 12
        while "observing (before=" not in (logs / "observer.log").read_text():
            if test.poll() is not None:
                raise AssertionError((logs / "observer.log").read_text())
            assert time.monotonic() < deadline, "Observer did not baseline the field"
            time.sleep(0.02)
        key("key", "ctrl+a")
        key("type", "--clearmodifiers", "hello whisper world")
        # Allow two observer polls to capture the completed correction.
        time.sleep(0.85)
        key("key", "Tab")
        result = test.wait(timeout=5)
        assert result == 0, (logs / "observer.log").read_text()
        print("PASS: GTK4 correction observed without an AT-SPI focus tracker")
    finally:
        if test is not None and test.poll() is None:
            test.terminate()
            test.wait(timeout=3)
        zenity.terminate()
        zenity.wait(timeout=3)
