#!/usr/bin/env python3
"""Exercise real browser document URL -> helper IPC in a private X11/D-Bus session."""
import http.server
import json
import os
from pathlib import Path
import queue
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
import time

if len(sys.argv) == 2:
    env = dict(os.environ, NO_PROXY="*", no_proxy="*", GSETTINGS_BACKEND="memory", RUST_LOG="wispr_flow_linux_helper=debug")
    for key in ("WAYLAND_DISPLAY", "AT_SPI_BUS_ADDRESS", "DBUS_SESSION_BUS_ADDRESS"):
        env.pop(key, None)
    with tempfile.TemporaryDirectory(prefix="wispr-browser-config-") as config:
        env["XDG_CONFIG_HOME"] = config
        raise SystemExit(subprocess.call([
            "dbus-run-session", "--", "xvfb-run", "-a", sys.executable,
            __file__, "--isolated", str(Path(sys.argv[1]).resolve())], env=env))
if len(sys.argv) != 3 or sys.argv[1] != "--isolated":
    raise SystemExit("usage: browser_style_test.py <helper-binary>")

class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'<title>Wispr browser style test</title><textarea autofocus></textarea>'
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Page)

port = server.server_port
cases = [("mail.google.com", "mail.google.com"), ("app.slack.com", "app.slack.com"),
         ("www.linkedin.com", "linkedin.com"), ("web.whatsapp.com", "web.whatsapp.com"),
         ("web.telegram.org", "web.telegram.org"), ("example.test", "")]
messages = queue.Queue()
def read_pipe(fd):
    buf = b""
    with os.fdopen(fd, "rb", buffering=0) as pipe:
        while chunk := pipe.read(4096):
            buf += chunk
            while b"|" in buf:
                raw, buf = buf.split(b"|", 1)
                messages.put(json.loads(raw.decode().replace("+2", "|").replace("+1", "+")))

def send(helper, command, uid):
    msg = json.dumps({"HelperAPIRequest": {command: True, "uuid": uid}})
    helper.stdin.write(msg.replace("+", "+1").replace("|", "+2").encode() + b"|")
    helper.stdin.flush()

def wait_message(predicate, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            msg = messages.get(timeout=0.1)
            if predicate(msg):
                return msg
        except queue.Empty:
            pass
    raise AssertionError("expected IPC message did not arrive")

with tempfile.TemporaryDirectory(prefix="wispr-browser-style-") as tmp:
    cert = str(Path(tmp) / "cert.pem")
    key = str(Path(tmp) / "key.pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
        "-keyout", key, "-out", cert, "-days", "1", "-subj", "/CN=localhost"],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    server.socket = tls.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    helper = browser = None
    with open(Path(tmp) / "helper.log", "w+") as log:
        try:
            rfd, wfd = os.pipe()
            helper = subprocess.Popen(["bash", "-c", 'exec 3>&"$0"; exec "$@"',
                str(wfd), sys.argv[2]], pass_fds=(wfd,), stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL, stderr=log)
            os.close(wfd)
            threading.Thread(target=read_pipe, args=(rfd,), daemon=True).start()
            chromium = os.environ.get("CHROMIUM_BIN") or shutil.which("chromium")
            assert chromium, "Chromium is required"
            browser = subprocess.Popen([chromium, "--no-sandbox", "--disable-gpu",
                "--no-first-run", "--no-proxy-server", "--ignore-certificate-errors", "--password-store=basic", "--disable-background-networking",
                "--disable-features=HttpsUpgrades", "--force-renderer-accessibility=complete",
                "--ozone-platform=x11", "--user-data-dir=" + tmp + "/profile",
                "--host-resolver-rules=MAP * 127.0.0.1",
                f"https://{cases[0][0]}:{port}/"], stdout=subprocess.DEVNULL, stderr=log)
            for index, (host, expected) in enumerate(cases):
                if index:
                    subprocess.run(["xdotool", "key", "ctrl+l"], check=True)
                    subprocess.run(["xdotool", "type", "--clearmodifiers", f"https://{host}:{port}/"], check=True)
                    subprocess.run(["xdotool", "key", "Return"], check=True)
                else:
                    end = time.monotonic() + 15
                    while True:
                        search = subprocess.run(["xdotool", "search", "--name", "Wispr browser style test"], capture_output=True, text=True)
                        if search.returncode == 0:
                            xid = search.stdout.splitlines()[0]
                            subprocess.run(["xdotool", "windowfocus", xid], check=True)
                            break
                        assert time.monotonic() < end, "browser window did not open"
                        time.sleep(.1)
                # Same title throughout: polling must notice site changes by document URL.
                wait_message(lambda msg: msg.get("HelperAPIRequest", {}).get("AppInfoUpdate", {}).get("payload", {}).get("url") == expected
                    and msg.get("HelperAPIRequest", {}).get("AppInfoUpdate", {}).get("payload", {}).get("bundleId") == "com.google.Chrome")
                # Navigation can invalidate an in-flight snapshot. Require stable
                # answers after it settles, rather than accepting a stale URL.
                deadline = time.monotonic() + 5
                stable = 0
                attempt = 0
                while stable < 2:
                    uid = f"query-{index}-{attempt}"
                    attempt += 1
                    send(helper, "GetActiveAppInfo", uid)
                    reply = wait_message(lambda msg: msg.get("HelperAPIResponse", {}).get("uuid") == uid)
                    payload = reply["HelperAPIResponse"]["ActiveAppInfo"]["payload"]
                    stable = stable + 1 if payload["url"] == expected else 0
                    assert time.monotonic() < deadline, payload
                    time.sleep(.15)
                print(f"PASS: {host} -> {expected or 'Other'} (event and query)", flush=True)
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read()[-6000:], file=sys.stderr)
            raise
        finally:
            for proc in (browser, helper):
                if proc:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
server.server_close()
