"""Studio Mic plugin for StreamController.

Untested against a live StreamController in this repo's CI; the protocol side (socket + CLI
fallback) is shared with `studio-mic --status --json` and is covered by the Studio Mic test plan.
"""
from src.backend.PluginManager.PluginBase import PluginBase
from src.backend.PluginManager.ActionHolder import ActionHolder

try:
    from src.backend.DeckManagement.InputIdentifier import Input
    from src.backend.PluginManager.ActionInputSupport import ActionInputSupport
    KEY_ONLY = {Input.Key: ActionInputSupport.SUPPORTED, Input.Dial: ActionInputSupport.UNSUPPORTED, Input.Touchscreen: ActionInputSupport.UNSUPPORTED}
    KEY_AND_DIAL = {Input.Key: ActionInputSupport.SUPPORTED, Input.Dial: ActionInputSupport.SUPPORTED, Input.Touchscreen: ActionInputSupport.UNTESTED}
except ImportError:  # older StreamController
    KEY_ONLY = KEY_AND_DIAL = None

from .actions.actions import (ChannelGain, ChannelMute, ChannelSolo, DspToggle, MicMuteToggle,
                              MonitorMuteToggle, ProfileSwitcher, PushToMute, ThemeSwitcher)

ACTIONS = [
    (MicMuteToggle, "MicMuteToggle", "Mic Mute Toggle", KEY_ONLY),
    (PushToMute, "PushToMute", "Push-to-Mute (Cough)", KEY_ONLY),
    (MonitorMuteToggle, "MonitorMuteToggle", "Headphone Monitor Mute", KEY_ONLY),
    (ChannelMute, "ChannelMute", "Channel Mute (Stream / Monitor / VOD)", KEY_ONLY),
    (ChannelSolo, "ChannelSolo", "Channel Solo (Headphones)", KEY_ONLY),
    (ChannelGain, "ChannelGain", "Channel Gain (+/- dB, dial)", KEY_AND_DIAL),
    (ProfileSwitcher, "ProfileSwitcher", "Load Profile", KEY_ONLY),
    (ThemeSwitcher, "ThemeSwitcher", "Theme Switcher", KEY_ONLY),
    (DspToggle, "DspToggle", "DSP Quick Toggle", KEY_ONLY),
]


class StudioMicPlugin(PluginBase):
    def __init__(self):
        super().__init__()
        for cls, action_id, name, support in ACTIONS:
            kwargs = dict(plugin_base=self, action_base=cls,
                          action_id=f"com_studiomic_controller::{action_id}", action_name=name)
            if support is not None:
                kwargs["action_support"] = support
            self.add_action_holder(ActionHolder(**kwargs))
        self.register(plugin_name="Studio Mic", github_repo="https://github.com/studiomic/studiomic",
                      plugin_version="1.0.0", app_version="1.5.0-beta.6")
