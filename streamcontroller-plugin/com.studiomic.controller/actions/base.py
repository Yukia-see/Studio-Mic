"""Shared base for Studio Mic actions."""
import os
import sys

from src.backend.PluginManager.ActionBase import ActionBase

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import studiomic_client as sm  # noqa: E402

import gi  # noqa: E402
gi.require_version("Adw", "1")
from gi.repository import Adw  # noqa: E402


class StudioMicAction(ActionBase):
    """Adds a settings helper, a 1 s state poll and key/dial routing."""

    SETTINGS = []  # (key, title, default)

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._last_icon = None

    def setting(self, key, default=""):
        return (self.get_settings() or {}).get(key, default)

    def get_config_rows(self):
        rows = []
        for key, title, default in self.SETTINGS:
            row = Adw.EntryRow(title=title)
            row.set_text(str(self.setting(key, default)))

            def changed(entry, k=key):
                s = self.get_settings() or {}
                s[k] = entry.get_text()
                self.set_settings(s)
                self.on_ready()

            row.connect("changed", changed)
            rows.append(row)
        return rows

    def show(self, icon_name, label=None):
        path = sm.icon(icon_name)
        if path != self._last_icon:
            self.set_media(media_path=path, size=0.75)
            self._last_icon = path
        if label is not None:
            self.set_bottom_label(label, font_size=12)

    def run(self, *args):
        ok, msg = sm.send(*args)
        if not ok and msg:
            self.show_error(duration=1)
        return ok, msg

    def refresh(self):
        """Override: update icon/label from sm.state()."""

    def on_ready(self):
        self.refresh()

    def on_tick(self):
        self.refresh()

    # Dial support (StreamController 1.5+ routes dial events here).
    def event_callback(self, event, data):
        try:
            from src.backend.DeckManagement.InputIdentifier import Input
            if event == Input.Key.Events.DOWN:
                self.on_key_down()
            elif event == Input.Key.Events.UP:
                self.on_key_up()
            elif event == Input.Dial.Events.TURN_CW:
                self.on_dial(+1)
            elif event == Input.Dial.Events.TURN_CCW:
                self.on_dial(-1)
            elif event == Input.Dial.Events.DOWN:
                self.on_key_down()
        except Exception:  # older StreamController: fall back to key handlers
            super().event_callback(event, data)

    def on_key_down(self):
        pass

    def on_key_up(self):
        pass

    def on_dial(self, direction):
        pass
