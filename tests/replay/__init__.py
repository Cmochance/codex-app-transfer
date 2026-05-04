"""Record-replay 测试基建.

参见 docs/migration-plan.md 第 7 节与 tests/replay/fixtures/_schema.md.
"""

from .fixture import Fixture, FrameRecord, load_fixture, dump_fixture, list_fixtures
from .player import build_respx_mock, replay_against_app

__all__ = [
    "Fixture",
    "FrameRecord",
    "load_fixture",
    "dump_fixture",
    "list_fixtures",
    "build_respx_mock",
    "replay_against_app",
]
