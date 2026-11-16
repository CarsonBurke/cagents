"""Seeded randomness, identifiers and time for a reproducible workspace.

Every part of the generator draws from its own named stream, so adding records
to one part (say, Codex rollouts) leaves every other part byte-identical for
the same profile, seed and root (paths in the files name the root). Identifiers come from those streams too; nothing reads the
clock or the process's own randomness.
"""

import datetime
import random
import uuid

# Every timestamp is an offset back from this instant (2026-06-01T00:00:00Z),
# not from the time the generator ran, so the same profile, seed and root
# always write the same bytes. Pass --epoch to move it, for example to make history look recent.
EPOCH = 1780272000


class Streams:
    """One independent random stream per part of the workspace."""

    def __init__(self, seed):
        self.seed = seed
        self._streams = {}

    def __call__(self, name):
        stream = self._streams.get(name)
        if stream is None:
            stream = self._streams[name] = Stream(f"{self.seed}:{name}")
        return stream


class Stream(random.Random):
    def uuid4(self):
        return str(uuid.UUID(int=self.getrandbits(128), version=4))

    def uuid7(self, unix_ms):
        """A version 7 UUID, as Codex names its threads: the creation time in
        milliseconds leads, so Codex finds the rollout by its creation day."""
        value = (unix_ms & ((1 << 48) - 1)) << 80
        value |= 0x7 << 76
        value |= self.getrandbits(12) << 64
        value |= 0b10 << 62
        value |= self.getrandbits(62)
        return str(uuid.UUID(int=value))

    def hex(self, length):
        return "".join(self.choice("0123456789abcdef") for _ in range(length))

    def token(self, length, alphabet="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"):
        return "".join(self.choice(alphabet) for _ in range(length))

    def weighted(self, table):
        """Picks a key of {value: weight}."""
        return self.choices(list(table), weights=list(table.values()))[0]

    def skewed(self, low, high, power=2.5):
        """An integer in [low, high], mostly near low: real sizes are a long
        tail of small things and a few large ones."""
        return low + int((high - low) * self.random() ** power)


def iso(seconds):
    """The millisecond UTC timestamp both providers write."""
    moment = datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def day_path(seconds):
    return datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc).strftime("%Y/%m/%d")


def file_stamp(seconds):
    return datetime.datetime.fromtimestamp(seconds, datetime.timezone.utc).strftime("%Y-%m-%dT%H-%M-%S")
