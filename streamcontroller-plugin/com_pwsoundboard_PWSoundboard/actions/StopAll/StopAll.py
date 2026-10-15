"""Key that stops every sound playing in PW Soundboard."""

import os

from src.backend.PluginManager.ActionBase import ActionBase

from ...client import request


class StopAll(ActionBase):
    def on_ready(self):
        self.set_media(media_path=os.path.join(self.plugin_base.PATH, "assets", "stop.png"), size=0.75)

    def on_key_down(self):
        reply = request({"cmd": "stop_all"})
        if not reply or not reply.get("ok"):
            self.show_error(1)
