from typing import Optional, Self

from .backend import AudioSource, IntoTrack


class Track:
    def __init__(self, source: AudioSource):
        self.source = source
        self.volume: float = 1.0
        self.loop = False
        self.loop_count: Optional[int] = None

    def set_volume(self, volume: float) -> Self:
        """
        Set the volume of this track.

        Parameters
        ----------
        volume : float
            The volume to set between 0.0 and 1.0

        Returns
        -------
        Self
            The track instance for method chaining
        """
        self.volume = volume
        return self

    def set_loop(self, loop: bool) -> Self:
        """
        Set whether this track should loop.

        Parameters
        ----------
        loop : bool
            Whether to enable looping for this track

        Returns
        -------
        Self
            The track instance for method chaining
        """
        self.loop = loop
        return self

    def set_loop_count(self, count: int) -> Self:
        """
        Set the number of times this track should loop.

        Parameters
        ----------
        count : int
            The number of times to loop this track. Set to None for infinite looping.

        Returns
        -------
        Self
            The track instance for method chaining
        """
        self.loop_count = count
        return self

    def into_songbird_track(self) -> IntoTrack:
        return IntoTrack(self.source, self.volume, self.loop, self.loop_count)
