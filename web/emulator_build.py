"""Identify the selected emulator without executing it or trusting its filename."""
from functools import lru_cache
import hashlib
from pathlib import Path
import shutil


PINNED_BUILD_ID = "dosbox-x-5a9c7c53-parity-o1"
PINNED_SHA256 = "fc66e91e8e8d48cc0b176bc350febab753bff297f2d93fc6c1e9d1f21cb6efae"
PINNED_RELEASE = "2026.01.02"


def pinned_binary() -> str:
    return str(Path.home() / ".local/share/vp-of-agentry/dosbox-x-5a9c7c53/dosbox-x")


def _fingerprint(path: Path) -> tuple:
    info = path.stat()
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


@lru_cache(maxsize=64)
def _cached_hash(path: str, fingerprint: tuple) -> str:
    target = Path(path)
    digest = hashlib.sha256()
    with target.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if _fingerprint(target) != fingerprint:
        raise OSError("File changed while hashing")
    return digest.hexdigest()


def file_sha256(path: Path) -> str:
    target = path.resolve(strict=True)
    before = _fingerprint(target)
    digest = _cached_hash(str(target), before)
    if _fingerprint(target) != before:
        raise OSError("File changed while hashing")
    return digest


def describe_emulator(command: str) -> dict:
    result = dict(kind="other", build_id=None, sha256=None, release=None)
    executable = shutil.which(command)
    if executable is None:
        return result
    try:
        result["sha256"] = file_sha256(Path(executable))
    except OSError:
        return result
    if result["sha256"] == PINNED_SHA256:
        result.update(kind="checkpoint", build_id=PINNED_BUILD_ID, release=PINNED_RELEASE)
    return result
