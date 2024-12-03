from __future__ import annotations
from abc import ABC, abstractmethod
from typing import Generic, TypeVar, AsyncIterator, Callable, Self, Union, ClassVar

R = TypeVar('R')
W = TypeVar('W')


class AsyncStreamReaderABC(Generic[R], ABC):
    def __init__(self, stream: AsyncStreamWriterABC[R]):
        self._stream = stream


class AsyncStreamWriterABC(Generic[W], ABC):
    def __aiter__(self) -> AsyncIterator[W]:
        return self

    @abstractmethod
    async def __anext__(self) -> W:
        raise StopAsyncIteration

    def subscribe(self, reader: Union[Callable[[Self], AsyncStreamReaderABC[W]], ClassVar[AsyncStreamReaderABC[W]]]) -> AsyncStreamReaderABC[W]:
        return reader(self)


class AsyncStreamABC(Generic[R, W], AsyncStreamReaderABC[R], AsyncStreamWriterABC[W]):
    @abstractmethod
    async def __anext__(self) -> W:
        return await anext(self._stream)
