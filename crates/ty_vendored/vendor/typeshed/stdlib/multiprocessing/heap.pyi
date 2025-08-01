import sys
from _typeshed import Incomplete
from collections.abc import Callable
from mmap import mmap
from typing import Protocol
from typing_extensions import TypeAlias

__all__ = ["BufferWrapper"]

class Arena:
    """
    A shared memory area backed by a temporary file (POSIX).
    """

    size: int
    buffer: mmap
    if sys.platform == "win32":
        name: str
        def __init__(self, size: int) -> None: ...
    else:
        fd: int
        def __init__(self, size: int, fd: int = -1) -> None: ...

_Block: TypeAlias = tuple[Arena, int, int]

if sys.platform != "win32":
    class _SupportsDetach(Protocol):
        def detach(self) -> int: ...

    def reduce_arena(a: Arena) -> tuple[Callable[[int, _SupportsDetach], Arena], tuple[int, Incomplete]]: ...
    def rebuild_arena(size: int, dupfd: _SupportsDetach) -> Arena: ...

class Heap:
    def __init__(self, size: int = ...) -> None: ...
    def free(self, block: _Block) -> None: ...
    def malloc(self, size: int) -> _Block: ...

class BufferWrapper:
    def __init__(self, size: int) -> None: ...
    def create_memoryview(self) -> memoryview: ...
