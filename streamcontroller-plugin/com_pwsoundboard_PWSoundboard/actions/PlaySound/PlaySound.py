"""Key that plays one PW Soundboard sound, exactly like clicking its tile in the app.

What a press does (restart vs. stop, overlap, speakers/mic) is decided by the soundboard's own
settings; this action only stores which sound it triggers.
"""

import colorsys
import os
import threading

from PIL import Image, ImageDraw, ImageEnhance, ImageFont, ImageOps

from src.backend.PluginManager.ActionBase import ActionBase

import gi
gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Adw, Gtk

from ...client import get_client, request

SIZE = 256
PROGRESS_STEPS = 32  # redraw granularity of the progress bar
PLAYING_COLOR = (90, 200, 120, 255)

_thumb_cache: dict[str, Image.Image] = {}
_thumb_lock = threading.Lock()


def _srgb(c: float) -> int:
    c = 12.92 * c if c <= 0.0031308 else 1.055 * c ** (1 / 2.4) - 0.055
    return round(max(0.0, min(1.0, c)) * 255)


def tile_color(sound_id: int) -> tuple[int, int, int, int]:
    """Same colour the app uses for tiles without a thumbnail (`tile_color` in app.rs)."""
    h = (((sound_id * 0x9E3779B97F4A7C15) & 0xFFFF_FFFF_FFFF_FFFF) >> 40) / (1 << 24)
    r, g, b = colorsys.hsv_to_rgb(h, 0.45, 0.55)  # egui's Hsva is linear RGB
    return (_srgb(r), _srgb(g), _srgb(b), 255)


def load_thumbnail(path: str) -> Image.Image | None:
    with _thumb_lock:
        if path in _thumb_cache:
            return _thumb_cache[path]
    try:
        with Image.open(path) as img:
            # Crop to fill, like the app's tiles.
            thumb = ImageOps.fit(img.convert("RGBA"), (SIZE, SIZE))
    except OSError:
        thumb = None
    with _thumb_lock:
        _thumb_cache[path] = thumb
    return thumb


def placeholder(sound_id: int) -> Image.Image:
    img = Image.new("RGBA", (SIZE, SIZE), tile_color(sound_id))
    draw = ImageDraw.Draw(img)
    font = ImageFont.load_default(size=120)
    draw.text((SIZE / 2, SIZE / 2), "♫", font=font, anchor="mm", fill=(255, 255, 255, 200))
    return img


class PlaySound(ActionBase):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.has_configuration = True
        self.client = get_client()
        self._render_lock = threading.Lock()
        self._last_visual = None
        self._last_label = None

    # --- lifecycle -----------------------------------------------------------------------------
    def on_ready(self):
        self.client.add_listener(self)
        self._last_visual = None
        self._last_label = None
        self.render()

    def on_remove(self):
        self.client.remove_listener(self)

    def on_soundboard_update(self):
        if getattr(self, "on_ready_called", False):
            self.render()

    # --- input ---------------------------------------------------------------------------------
    def on_key_down(self):
        sound_id = self.get_settings().get("sound_id")
        if sound_id is None:
            self.show_error(1)
            return
        reply = request({"cmd": "press", "id": sound_id})
        if not reply or not reply.get("ok"):
            self.show_error(1)

    # --- drawing -------------------------------------------------------------------------------
    def render(self):
        settings = self.get_settings()
        sound_id = settings.get("sound_id")
        sound = self.client.get_sound(sound_id) if sound_id is not None else None
        connected = self.client.connected
        progress = self.client.progress(sound_id) if sound_id is not None else None
        title = sound["title"] if sound else settings.get("title", "")
        thumbnail = sound.get("thumbnail") if sound else None
        step = None if progress is None else round(progress * PROGRESS_STEPS)

        visual = (sound_id, thumbnail, connected, sound is not None, step)
        label = title if settings.get("show_title", True) else ""

        with self._render_lock:
            if visual != self._last_visual:
                self._last_visual = visual
                self.set_media(image=self._compose(sound_id, thumbnail, connected, sound, progress), size=1)
            if label != self._last_label:
                self._last_label = label
                self.set_bottom_label(label, font_size=12, outline_width=2)

    def _compose(self, sound_id, thumbnail, connected, sound, progress) -> Image.Image:
        if sound_id is None:
            return Image.open(os.path.join(self.plugin_base.PATH, "assets", "icon.png")).convert("RGBA")
        img = load_thumbnail(thumbnail) if thumbnail else None
        img = img.copy() if img is not None else placeholder(sound_id)
        if not connected or sound is None:
            # Soundboard closed, or the sound was deleted there.
            img = ImageEnhance.Brightness(img).enhance(0.3)
        if progress is not None:
            draw = ImageDraw.Draw(img)
            bar = round(SIZE * 0.06)
            draw.rectangle((0, SIZE - bar, SIZE, SIZE), fill=(0, 0, 0, 160))
            draw.rectangle((0, SIZE - bar, round(SIZE * progress), SIZE), fill=PLAYING_COLOR)
            draw.rectangle((0, 0, SIZE - 1, SIZE - 1), outline=PLAYING_COLOR, width=8)
        return img

    # --- configuration -------------------------------------------------------------------------
    def get_config_rows(self) -> list:
        lm = self.plugin_base.lm
        self.client.refresh()
        sounds = sorted(self.client.sound_list(), key=lambda s: s["title"].lower())
        settings = self.get_settings()
        current = settings.get("sound_id")

        self._choices = [s["id"] for s in sounds]
        names = [s["title"] for s in sounds]
        if current is None:
            # A ComboRow always selects something; without this entry the first sound would look
            # chosen while nothing is saved.
            self._choices.insert(0, None)
            names.insert(0, lm.get("actions.play-sound.choose"))
        elif current not in self._choices:
            # Keep showing a sound the soundboard doesn't have (anymore / not running).
            self._choices.insert(0, current)
            names.insert(0, lm.get("actions.play-sound.missing") + settings.get("title", str(current)))

        self.sound_row = Adw.ComboRow(title=lm.get("actions.play-sound.sound"), model=Gtk.StringList.new(names))
        if not self.client.connected:
            self.sound_row.set_subtitle(lm.get("actions.play-sound.not-running"))
        self.sound_row.set_selected(self._choices.index(current))
        self.sound_row.connect("notify::selected", self._on_sound_selected)

        self.title_row = Adw.SwitchRow(title=lm.get("actions.play-sound.show-title"))
        self.title_row.set_active(settings.get("show_title", True))
        self.title_row.connect("notify::active", self._on_show_title)

        return [self.sound_row, self.title_row]

    def _on_sound_selected(self, row, *_):
        index = row.get_selected()
        if index == Gtk.INVALID_LIST_POSITION or index >= len(self._choices):
            return
        sound_id = self._choices[index]
        if sound_id is None:
            return
        settings = self.get_settings()
        settings["sound_id"] = sound_id
        sound = self.client.get_sound(sound_id)
        if sound:
            settings["title"] = sound["title"]
        self.set_settings(settings)
        self.render()

    def _on_show_title(self, row, *_):
        settings = self.get_settings()
        settings["show_title"] = row.get_active()
        self.set_settings(settings)
        self.render()
