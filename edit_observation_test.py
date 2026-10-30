#!/usr/bin/env python3
"""Exercise GTK4 and Qt edit observation with focus events and bounded search."""

import os
import shutil
import signal
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

CHROMIUM_CASES = {
    "paragraph": (
        "<p>hello wisper world</p>",
        "<p>hello whisper world</p>",
        "hello wisper world",
        "hello whisper world",
    ),
    "omitted_space": (
        "<p>before hello wisper world</p>",
        "<p>before hello whisper world</p>",
        "hello wisper world ",
        "hello whisper world",
    ),
    "omitted_space_unchanged": (
        "<p>hello wisper world</p>",
        "<p>hello <b>wisper</b> world</p>",
        "hello wisper world ",
        "hello wisper world ",
    ),
    "omitted_space_ambiguous": (
        "<p>hello wisper world, hello wisper world</p>",
        "",
        "hello wisper world ",
        None,
    ),
    "omitted_newline": (
        "<p>hello wisper world</p>",
        "",
        "hello wisper world\n",
        None,
    ),
    "textarea": (
        "hello wisper world",
        "hello whisper world",
        "hello wisper world",
        "hello whisper world",
    ),
    "formatting": (
        "<p>hello <b>wisper</b> world</p>",
        "<p>hello <i>whisper</i> world</p>",
        "hello wisper world",
        "hello whisper world",
    ),
    "format_only": (
        "<p>hello wisper world</p>",
        "<p>hello <b>wisper</b> world</p>",
        "hello wisper world",
        "hello wisper world",
    ),
    "paragraphs": (
        "<p>hello wisper</p><p>日本 🦀 world</p>",
        "<p>hello whisper</p><p>日本 🦀 world</p>",
        "hello wisper\n日本 🦀 world",
        "hello whisper\n日本 🦀 world",
    ),
    "link": (
        '<p>hello <a href="https://example.invalid/">wisper</a> world</p>',
        '<p>hello <a href="https://example.invalid/">whisper</a> world</p>',
        "hello wisper world",
        "hello whisper world",
    ),
    "mention": (
        '<p>hello <span contenteditable="false">@wisper</span> world</p>',
        '<p>hello <span contenteditable="false">@whisper</span> world</p>',
        "hello @wisper world",
        "hello @whisper world",
    ),
    "anchors": (
        "<p>before hello wisper world after</p>",
        "<p>before hello whisper world after</p>",
        "hello wisper world",
        "hello whisper world",
    ),
    "replace_editor": (
        "<p>hello wisper world</p>",
        "<p>private replacement</p>",
        "hello wisper world",
        None,
    ),
    "ambiguous": (
        "<p>hello wisper world hello wisper world</p>",
        "",
        "hello wisper world",
        None,
    ),
    "image": (
        '<p>hello wisper world<img alt="private" src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7"></p>',
        "",
        "hello wisper world",
        None,
    ),
    "nested": (
        '<p>hello wisper world</p><div contenteditable="false"><div contenteditable="true" role="textbox">private</div></div>',
        "",
        "hello wisper world",
        None,
    ),
    "password": ("hello wisper world", "", "hello wisper world", None),
}

if len(sys.argv) == 2 or (
    len(sys.argv) == 3 and sys.argv[2] in ("--qtquick", "--chromium")
):
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
            ACCESSIBILITY_ENABLED="1",
            GTK_A11Y="atspi",
            QT_QPA_PLATFORM="xcb",
            QT_LINUX_ACCESSIBILITY_ALWAYS_ON="1",
            WISPR_EDIT_TEST_ISOLATED="1",
            RUST_LOG="wispr_flow_linux_helper=debug",
        )
        for key in ("WAYLAND_DISPLAY", "AT_SPI_BUS_ADDRESS", "KDE_FULL_SESSION"):
            env.pop(key, None)
        toolkits = (
            ("chromium",)
            if "--chromium" in sys.argv
            else ("qtquick",)
            if "--qtquick" in sys.argv
            else ("gtk4", "qt")
        )
        for toolkit in toolkits:
            for case in CHROMIUM_CASES if toolkit == "chromium" else ("plain",):
                for focus_events in ("0", "1"):
                    env["WISPR_TEST_TOOLKIT"] = toolkit
                    env["WISPR_TEST_FOCUS_EVENTS"] = focus_events
                    env["WISPR_TEST_CASE"] = case
                    if toolkit == "chromium":
                        _, _, pasted, edited = CHROMIUM_CASES[case]
                        env["WISPR_TEST_PASTED"] = pasted
                        env["WISPR_TEST_EDITED"] = edited or ""
                        env["WISPR_TEST_REJECT"] = "1" if edited is None else "0"
                    result = subprocess.call(
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
                    if result:
                        raise SystemExit(result)
        raise SystemExit(0)

if len(sys.argv) != 3 or sys.argv[1] != "--isolated":
    raise SystemExit("usage: edit_observation_test.py <cargo-test-build.json>")


toolkit = os.environ["WISPR_TEST_TOOLKIT"]
if toolkit == "gtk4":
    version = subprocess.check_output(["zenity", "--version"], text=True).strip()
    assert int(version.split(".")[0]) >= 4, "This regression requires GTK4 Zenity"
    dialog_command = [
        "zenity",
        "--entry",
        "--title=Wispr edit regression",
        "--text=Test field",
        "--entry-text=hello wisper world",
    ]
elif toolkit == "qt":
    dialog_command = [
        "kdialog",
        "--title",
        "Wispr edit regression",
        "--inputbox",
        "Test field",
        "hello wisper world",
    ]


def key(*args):
    subprocess.run(["xdotool", *args], check=True)


with tempfile.TemporaryDirectory(prefix="wispr-gtk4-logs-") as logs:
    logs = Path(logs)
    if toolkit == "chromium":
        browser = os.environ.get("CHROMIUM_BIN") or shutil.which("chromium")
        assert browser, "Set CHROMIUM_BIN to a Chromium executable"
        fixture = logs / "field.html"
        case = os.environ["WISPR_TEST_CASE"]
        before, after, _, expected = CHROMIUM_CASES[case]
        if case == "textarea":
            field = '<textarea id="editor">' + before + "</textarea>"
        elif case == "password":
            field = '<input type="password" id="editor" value="' + before + '">'
        else:
            field = (
                '<div contenteditable="true" role="textbox" id="editor">'
                + before
                + "</div>"
            )
        fixture.write_text(
            '<!doctype html><meta charset="utf-8"><title>Wispr edit regression</title>'
            + field
            + '<button id="done">Done</button><aside>private history</aside><script>'
            + 'editor.focus(); document.addEventListener("keydown", e => {'
            + 'if(e.key === "F8") { e.preventDefault(); editor.'
            + (
                "outerHTML"
                if case == "replace_editor"
                else "value"
                if case == "textarea"
                else "innerHTML"
            )
            + "="
            + json.dumps(after)
            + "; editor.focus(); }"
            + 'if(e.key === "F7") { e.preventDefault(); editor.focus(); }'
            + 'if(e.key === "F9") { e.preventDefault(); done.focus(); }'
            + "});</script>"
        )
        dialog_command = [
            browser,
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            "--password-store=basic",
            "--disable-background-networking",
            "--ozone-platform=x11",
            "--user-data-dir=" + str(logs / "profile"),
            "--app=" + fixture.as_uri(),
        ]
        if os.environ.get("WISPR_TEST_FOCUS_EVENTS") == "1":
            dialog_command.append("--force-renderer-accessibility=complete")
    if toolkit == "qtquick":
        qml = shutil.which("qml6") or shutil.which("qml")
        assert qml, "Qt Quick testing requires a Qt 6 qml runtime"
        fixture = logs / "field.qml"
        fixture.write_text("""import QtQuick
import QtQuick.Controls
ApplicationWindow {
 visible: true; width: 400; height: 160; title: "Wispr edit regression"
 Column {
  TextField { text: "hello wisper world"; focus: true; width: 350 }
  Button { text: "Done" }
 }
}
""")
        dialog_command = [qml, str(fixture)]
    if toolkit == "chromium":
        import gi

        gi.require_version("Atspi", "2.0")
        from gi.repository import Atspi

        Atspi.get_desktop(0)
    with open(logs / "zenity.log", "w") as log:
        zenity = subprocess.Popen(
            dialog_command,
            stdout=subprocess.DEVNULL,
            stderr=log,
            start_new_session=True,
        )
    test = None
    try:
        deadline = time.monotonic() + 20
        windows = []
        while not windows:
            assert zenity.poll() is None, (logs / "zenity.log").read_text()
            windows = subprocess.run(
                [
                    "xdotool",
                    "search",
                    "--onlyvisible",
                    "--name" if toolkit == "chromium" else "--pid",
                    "Wispr edit regression"
                    if toolkit == "chromium"
                    else str(zenity.pid),
                ],
                capture_output=True,
                text=True,
            ).stdout.split()
            assert time.monotonic() < deadline, (
                "Fixture did not open: " + (logs / "zenity.log").read_text()
            )
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
        if toolkit == "chromium":
            key("mousemove", "--window", windows[0], "50", "20")
            key("click", "1")
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
            # Registration reaches toolkit bridges asynchronously on a fresh bus.
            deadline = time.monotonic() + 7
            while "using focus-event target" not in (logs / "observer.log").read_text():
                if os.environ.get("WISPR_TEST_REJECT") == "1" and test.poll() == 0:
                    break
                assert test.poll() is None, (logs / "observer.log").read_text()
                assert time.monotonic() < deadline, (logs / "observer.log").read_text()
                key("key", "F9" if toolkit == "chromium" else "Tab")
                key("key", "F7" if toolkit == "chromium" else "shift+Tab")
                key("key", "End")
                time.sleep(0.5)
        if (
            os.environ.get("WISPR_TEST_REJECT") == "1"
            and os.environ.get("WISPR_TEST_CASE") != "replace_editor"
        ):
            assert test.wait(timeout=15) == 0, (logs / "observer.log").read_text()
            print("PASS: rejected chromium " + os.environ["WISPR_TEST_CASE"])
            raise SystemExit(0)
        deadline = time.monotonic() + 12
        while "observing (before=" not in (logs / "observer.log").read_text():
            if test.poll() is not None:
                raise AssertionError(
                    (logs / "observer.log").read_text()
                    + (logs / "zenity.log").read_text()
                )
            assert time.monotonic() < deadline, "Observer did not baseline the field"
            time.sleep(0.02)
        if os.environ.get("WISPR_TEST_FOCUS_EVENTS") == "1":
            assert "using focus-event target" in (logs / "observer.log").read_text()
        if toolkit == "chromium":
            key("key", "F8")
        else:
            key("key", "ctrl+a")
            key("type", "--clearmodifiers", "hello whisper world")
        # Allow two observer polls to capture the completed correction.
        time.sleep(0.85)
        key("key", "F9" if toolkit == "chromium" else "Tab")
        result = test.wait(timeout=5)
        assert result == 0, (logs / "observer.log").read_text()
        print(
            "PASS: "
            + toolkit
            + " "
            + os.environ.get("WISPR_TEST_CASE", "plain")
            + " correction observed; focus events="
            + os.environ.get("WISPR_TEST_FOCUS_EVENTS", "0")
        )
    finally:
        if test is not None and test.poll() is None:
            test.terminate()
            test.wait(timeout=3)
        try:
            os.killpg(zenity.pid, signal.SIGTERM)
            zenity.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(zenity.pid, signal.SIGKILL)
            zenity.wait()
        finally:
            # Chromium workers can outlive the browser and keep writing its profile.
            try:
                os.killpg(zenity.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
