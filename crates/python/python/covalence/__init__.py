"""Python bindings for Covalence."""
import abc

from ._native import (
    Blob,
    BlobFact,
    Cas,
    Cid,
    Codec,
    Error,
    Hash,
    IntegrityError,
    Kv,
    StoreError,
    UnsupportedError,
    Value,
    __version__,
)


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
    "Fact",
    "Hash",
    "IntegrityError",
    "Kv",
    "StoreError",
    "UnsupportedError",
    "Value",
    "__version__",
]
