"""Connection to PW Soundboard's control socket.

The soundboard listens on ~/.local/share/pw-soundboard/control.sock (one JSON object per line).
StreamController runs as a Flatpak with home access, so the socket is reachable at the host path;
$XDG_DATA_HOME inside the sandbox points elsewhere, so it is not used here.
"""

import json
import os
import socket
import threading
import time
import weakref

from loguru import logger as log

SOCKET_PATH = os.path.expanduser("~/.local/share/pw-soundboard/control.sock")


def request(payload: dict, timeout: float = 1.0) -> dict | None:
    """Send one request and return the reply, or None if the soundboard is not reachable."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(SOCKET_PATH)
            s.sendall((json.dumps(payload) + "\n").encode())
            with s.makefile("r", encoding="utf-8") as f:
                line = f.readline()
        return json.loads(line) if line else None
    except (OSError, ValueError):
        return None


class SoundboardClient:
    """Keeps a subscription open, mirrors the sound list and playback state, and notifies actions."""

    def __init__(self):
        self.lock = threading.Lock()
        self.connected = False
        self.version = None
        self.sounds: dict[int, dict] = {}
        self.playing: dict[int, float] = {}
        self._listeners = weakref.WeakSet()
        threading.Thread(target=self._run, name="pw-soundboard-client", daemon=True).start()

    # --- listeners ---------------------------------------------------------------------------
    def add_listener(self, action) -> None:
        """`action.on_soundboard_update()` is called (from this client's thread) on every change."""
        self._listeners.add(action)

    def remove_listener(self, action) -> None:
        self._listeners.discard(action)

    def _notify(self) -> None:
        for action in list(self._listeners):
            try:
                action.on_soundboard_update()
            except Exception:
                log.exception("PW Soundboard: action update failed")

    # --- queries -------------------------------------------------------------------------------
    def get_sound(self, sound_id) -> dict | None:
        with self.lock:
            return self.sounds.get(sound_id)

    def sound_list(self) -> list[dict]:
        with self.lock:
            return list(self.sounds.values())

    def progress(self, sound_id) -> float | None:
        with self.lock:
            return self.playing.get(sound_id)

    def refresh(self) -> None:
        reply = request({"cmd": "list"})
        if not reply or not reply.get("ok"):
            return
        with self.lock:
            self.sounds = {s["id"]: s for s in reply["sounds"]}
            self.version = reply["version"]

    # --- subscription thread -----------------------------------------------------------------
    def _set_disconnected(self) -> None:
        if self.connected:
            with self.lock:
                self.connected = False
                self.playing = {}
                self.version = None
            self._notify()

    def _run(self) -> None:
        while True:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.connect(SOCKET_PATH)
                    # The server sends a ping every 3 s, so silence means it's gone.
                    s.settimeout(10)
                    s.sendall(b'{"cmd":"subscribe"}\n')
                    with s.makefile("r", encoding="utf-8") as f:
                        for line in f:
                            self._handle(json.loads(line))
            except (OSError, ValueError):
                pass
            self._set_disconnected()
            time.sleep(2)

    def _handle(self, msg: dict) -> None:
        if msg.get("event") != "state":
            return
        if msg["version"] != self.version:
            self.refresh()
        with self.lock:
            self.connected = True
            self.playing = {int(k): v for k, v in msg["playing"].items()}
        self._notify()


_client = None
_client_lock = threading.Lock()


def get_client() -> SoundboardClient:
    global _client
    with _client_lock:
        if _client is None:
            _client = SoundboardClient()
        return _client
