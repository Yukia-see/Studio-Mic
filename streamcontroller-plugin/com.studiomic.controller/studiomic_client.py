"""Tiny client for the Studio Mic control socket.

The socket lives at ~/.cache/studio-mic/studio-mic.sock. Inside the StreamController Flatpak the
real home is visible (XDG_CACHE_HOME points elsewhere, so we build the path from $HOME). If the
socket cannot be reached, the `studio-mic` CLI is run on the host via flatpak-spawn as a fallback.
"""
import json
import os
import shutil
import socket
import subprocess

def _home():
    return os.environ.get("HOME") or os.path.expanduser("~")

SOCKET_PATH = os.path.join(_home(), ".cache", "studio-mic", "studio-mic.sock")
STATE_DIR = os.path.join(_home(), ".cache", "studio-mic", "streamcontroller")


def send(*args, timeout=5.0):
    """Runs one Studio Mic command. Returns (ok, message)."""
    args = [str(a) for a in args]
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(SOCKET_PATH)
            s.sendall((json.dumps(args) + "\n").encode())
            buf = b""
            while not buf.endswith(b"\n"):
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
        reply = json.loads(buf.decode() or "{}")
        return bool(reply.get("ok")), reply.get("message", "")
    except (OSError, ValueError):
        return _send_via_cli(args)


def _send_via_cli(args):
    cmd = ["studio-mic", *args]
    if os.path.exists("/.flatpak-info") and shutil.which("flatpak-spawn"):
        cmd = ["flatpak-spawn", "--host", *cmd]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=20)
        return p.returncode == 0, (p.stdout or p.stderr).strip()
    except (OSError, subprocess.SubprocessError) as e:
        return False, str(e)


def state():
    """Live state written by Studio Mic for Stream Deck icons (may be empty if not running)."""
    try:
        with open(os.path.join(STATE_DIR, "state.json"), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def icon(name):
    """Path of a live state icon (falls back to the bundled one)."""
    live = os.path.join(STATE_DIR, f"{name}.svg")
    if os.path.exists(live):
        return live
    return os.path.join(os.path.dirname(__file__), "assets", f"{name}.svg")
