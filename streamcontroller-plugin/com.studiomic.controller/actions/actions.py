"""Studio Mic Stream Deck actions."""
from .base import StudioMicAction, sm


class MicMuteToggle(StudioMicAction):
    def on_key_down(self):
        self.run("--toggle-mic-mute")
        self.refresh()

    def refresh(self):
        muted = sm.state().get("mic_muted", False)
        self.show("mic_muted" if muted else "mic_live", "MUTED" if muted else "LIVE")


class PushToMute(StudioMicAction):
    """Cough button: muted while held."""
    def on_key_down(self):
        self.run("--set-mic-mute", "on")
        self.show("mic_muted", "COUGH")

    def on_key_up(self):
        self.run("--set-mic-mute", "off")
        self.refresh()

    def refresh(self):
        muted = sm.state().get("mic_muted", False)
        self.show("mic_muted" if muted else "mic_live", "HOLD")


class MonitorMuteToggle(StudioMicAction):
    def on_key_down(self):
        self.run("--toggle-monitor-mute")
        self.refresh()

    def refresh(self):
        muted = sm.state().get("monitor_muted", False)
        self.show("monitor_muted" if muted else "monitor_on", "PHONES")


class ChannelMute(StudioMicAction):
    SETTINGS = [("channel", "Channel (e.g. Capture Card, Game, Music)", "Game"),
                ("target", "Target: stream | monitor | vod | all", "stream")]

    def on_key_down(self):
        self.run("--channel-mute", self.setting("channel", "Game"), self.setting("target", "stream"))
        self.refresh()

    def refresh(self):
        ch = self.setting("channel", "Game")
        target = self.setting("target", "stream")
        info = sm.state().get("channels", {}).get(ch.lower(), {})
        if target == "vod":
            off = not info.get("vod", True)
        elif target == "monitor":
            off = info.get("monitor_muted", False)
        elif target == "all":
            off = info.get("stream_muted", False) and info.get("monitor_muted", False)
        else:
            off = info.get("stream_muted", False)
        self.show("channel_muted" if off else "channel_on", f"{ch[:10]}\n{target}")


class ChannelSolo(StudioMicAction):
    SETTINGS = [("channel", "Channel", "Game")]

    def on_key_down(self):
        self.run("--channel-solo", self.setting("channel", "Game"))
        self.refresh()

    def refresh(self):
        ch = self.setting("channel", "Game")
        solo = sm.state().get("channels", {}).get(ch.lower(), {}).get("solo", False)
        self.show("solo_on" if solo else "solo_off", f"SOLO\n{ch[:10]}")


class ChannelGain(StudioMicAction):
    """Button: one step. Dial: step per detent."""
    SETTINGS = [("channel", "Channel", "Game"), ("bus", "Bus: stream | monitor", "stream"), ("step", "Step in dB (e.g. 2 or -2)", "2")]

    def _step(self, direction):
        try:
            step = float(self.setting("step", "2"))
        except ValueError:
            step = 2.0
        delta = step * direction
        self.run("--channel-gain", self.setting("channel", "Game"), self.setting("bus", "stream"), f"{delta:+.1f}dB")
        self.refresh()

    def on_key_down(self):
        self._step(1)

    def on_dial(self, direction):
        self._step(direction)

    def refresh(self):
        ch = self.setting("channel", "Game")
        bus = self.setting("bus", "stream")
        info = sm.state().get("channels", {}).get(ch.lower(), {})
        db = info.get(f"{bus}_gain_db", 0.0)
        self.show("gain", f"{ch[:8]}\n{db:+.1f} dB")


class ProfileSwitcher(StudioMicAction):
    SETTINGS = [("profile", "Profile name", "Streaming")]

    def on_key_down(self):
        self.run("--load-profile", self.setting("profile", "Streaming"))

    def refresh(self):
        self.show("profile", self.setting("profile", "Streaming")[:12])


class ThemeSwitcher(StudioMicAction):
    THEMES = ["oled_black", "material_dark", "midnight_slate", "light"]
    SETTINGS = [("theme", "Theme (empty = cycle)", "")]

    def on_key_down(self):
        t = self.setting("theme", "")
        if not t:
            cur = sm.state().get("theme", "material_dark")
            t = self.THEMES[(self.THEMES.index(cur) + 1) % len(self.THEMES)] if cur in self.THEMES else self.THEMES[0]
        self.run("--set-theme", t)
        self.refresh()

    def refresh(self):
        self.show("theme", sm.state().get("theme", "")[:12])


class DspToggle(StudioMicAction):
    """clipguard | view | autoduck:<Channel> | denoiser:<deepfilter|rnnoise|speex|off>"""
    SETTINGS = [("toggle", "clipguard | view | autoduck:Channel | denoiser:engine", "clipguard")]

    def on_key_down(self):
        t = self.setting("toggle", "clipguard")
        if t == "clipguard":
            self.run("--toggle-clipguard")
        elif t == "view":
            self.run("--toggle-view")
        elif t.startswith("autoduck:"):
            self.run("--toggle-autoduck", t.split(":", 1)[1])
        elif t.startswith("denoiser:"):
            self.run("--set-denoiser", t.split(":", 1)[1])
        self.refresh()

    def refresh(self):
        st = sm.state()
        t = self.setting("toggle", "clipguard")
        if t == "clipguard":
            on, label = st.get("clipguard", False), "CLIPGUARD"
        elif t.startswith("denoiser:"):
            on, label = st.get("denoiser") == t.split(":", 1)[1], t.split(":", 1)[1].upper()
        elif t.startswith("autoduck:"):
            ch = t.split(":", 1)[1]
            on, label = st.get("channels", {}).get(ch.lower(), {}).get("autoduck", False), f"DUCK {ch[:6]}"
        else:
            on, label = st.get("view_mode") == "simple", "SIMPLE"
        self.show("toggle_on" if on else "toggle_off", label)
