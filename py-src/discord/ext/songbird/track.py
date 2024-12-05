from typing import Optional, Self

from .backend import AudioSource, IntoTrack


class Track:
    def __init__(self, source: AudioSource):
        self.source = source
        self.volume: float = 1.0
        self.loop = False
        self.loop_count: Optional[int] = None

    def set_volume(self, volume: float) -> Self:
        self.volume = volume
        return self

    def set_loop(self, loop: bool) -> Self:
        self.loop = loop
        return self

    def set_loop_count(self, count: int) -> Self:
        self.loop_count = count
        return self

    def into_songbird_track(self) -> IntoTrack:
        return IntoTrack(self.source, self.volume, self.loop, self.loop_count)
