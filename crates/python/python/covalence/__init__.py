"""Python bindings for Covalence."""
import abc
import enum as _enum
from collections.abc import Callable

from ._native import (
    Blob,
    BlobFact,
    Cas,
    Cid,
    Codec,
    Error,
    EvictionCandidate,
    Hash,
    IntegrityError,
    Kv,
    StoreError,
    UnsupportedError,
    Value,
    __version__,
)


class Flush(_enum.IntFlag):
    """The guarantees requested by Cas.flush.

    PERSIST makes accepted writes durable locally. REMOTE copies content to
    the remote CAS. EVICT releases cached copies only after durable backing
    is confirmed. Combine REMOTE | EVICT to write back and then release.
    Unsupported requests raise UnsupportedError; zero does nothing.
    """

    PERSIST = 1
    REMOTE = 2
    EVICT = 4


class EvictionPolicy(abc.ABC):
    """Select which durably backed cached copies a flush may release.

    A supplied policy replaces the store's configured selection for that call.
    The store confirms backing before offering a candidate; approval never
    permits deleting backing content. Use from_callable for a function.
    """

    @abc.abstractmethod
    def should_evict(self, candidate: EvictionCandidate) -> bool:
        """Whether this durably backed cached copy may be released.

        Return bool. False retains it; True permits release. A raised exception
        retains this candidate and propagates; earlier work may be complete.
        """

    @staticmethod
    def from_callable(callback: Callable[[EvictionCandidate], bool]) -> "EvictionPolicy":
        """Adapt a function of one candidate to an eviction policy."""
        return _CallableEvictionPolicy(callback)


class _CallableEvictionPolicy(EvictionPolicy):
    def __init__(self, callback: Callable[[EvictionCandidate], bool]) -> None:
        self._callback = callback

    def should_evict(self, candidate: EvictionCandidate) -> bool:
        return self._callback(candidate)


class Fact(abc.ABC):
    """An interface for bytes and the CID they claim to hash to.

    BlobFact is checked; isinstance(x, Fact) alone proves nothing.
    """

    __slots__ = ()

    @property
    @abc.abstractmethod
    def cid(self) -> Cid:
        """The CID the bytes claim to hash to."""

    @abc.abstractmethod
    def value(self) -> Value:
        """The bytes decoded as the CID's codec says."""


Fact.register(BlobFact)

__all__ = [
    "Blob",
    "BlobFact",
    "Cas",
    "Cid",
    "Codec",
    "Error",
    "EvictionCandidate",
    "EvictionPolicy",
    "Fact",
    "Flush",
    "Hash",
    "IntegrityError",
    "Kv",
    "StoreError",
    "UnsupportedError",
    "Value",
    "__version__",
]
