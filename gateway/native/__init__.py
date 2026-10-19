"""Native OAuth providers that run inside the gateway process.

Each integration is one module exporting a :class:`~gateway.native.base.NativeProvider`
subclass. Adding a provider means adding the module here and a nav entry in the
dashboard; no protocol code is duplicated because the provider only has to
produce OpenAI chat stream frames.
"""
from pathlib import Path

from .antigravity import Antigravity
from .copilot import Copilot
from .grok import Grok

BUILDERS = (Antigravity, Grok, Copilot)
PROVIDERS = {builder.name: builder for builder in BUILDERS}
NAMES = tuple(PROVIDERS)


def build(directory):
    """Instantiate every native provider under its own data directory."""
    directory = Path(directory)
    return {name: builder(directory / name) for name, builder in PROVIDERS.items()}
