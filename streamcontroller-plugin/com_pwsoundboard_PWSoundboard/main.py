from src.backend.PluginManager.PluginBase import PluginBase
from src.backend.PluginManager.ActionHolder import ActionHolder
from src.backend.DeckManagement.InputIdentifier import Input
from src.backend.PluginManager.ActionInputSupport import ActionInputSupport

from .actions.PlaySound.PlaySound import PlaySound
from .actions.StopAll.StopAll import StopAll


class PWSoundboardPlugin(PluginBase):
    def __init__(self):
        super().__init__()
        self.lm = self.locale_manager

        self.add_action_holder(ActionHolder(
            plugin_base=self,
            action_base=PlaySound,
            action_id_suffix="PlaySound",
            action_name=self.lm.get("actions.play-sound.name"),
            action_support={
                Input.Key: ActionInputSupport.SUPPORTED,
                Input.Dial: ActionInputSupport.UNTESTED,
                Input.Touchscreen: ActionInputSupport.UNSUPPORTED,
            },
        ))
        self.add_action_holder(ActionHolder(
            plugin_base=self,
            action_base=StopAll,
            action_id_suffix="StopAll",
            action_name=self.lm.get("actions.stop-all.name"),
            action_support={
                Input.Key: ActionInputSupport.SUPPORTED,
                Input.Dial: ActionInputSupport.UNTESTED,
                Input.Touchscreen: ActionInputSupport.UNSUPPORTED,
            },
        ))

        self.register(
            plugin_name=self.lm.get("plugin.name"),
            github_repo="https://github.com/TheYopi/pw-soundboard",
            plugin_version="0.1.0",
            app_version="1.5.0-beta.16",
        )
