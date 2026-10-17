from os import PathLike
from typing import BinaryIO

__version__: str

def sdigest(
    source: str | PathLike[str] | BinaryIO,
    level: int | None = None,
    *,
    coverage: float | None = None,
) -> str:
    """Sparse digest of a path or seekable binary file object.

    Returns "sd1:<size>:<coverage>:<blake3 hex>". Give either a level (0-9,
    default 5) or a coverage percent (0-100).
    """

def _main() -> int:
    """Entry point for the `sdigest` command."""

def may_equal(a: str, b: str) -> bool | None:
    """False if fingerprints prove the content differs, True if it may be
    identical, None if they aren't comparable."""
