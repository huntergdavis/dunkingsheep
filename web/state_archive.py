"""Report archive evidence; never create, migrate, or modify saved states."""
from datetime import datetime
import json
from pathlib import Path
import re

from server.emulator_build import file_sha256


def archive_status(manifest: Path) -> dict:
    empty = dict(status="pending", verified_count=0, archived_at=None)
    try:
        if not manifest.exists():
            return empty
        data = json.loads(manifest.read_text())
        stamp = datetime.fromisoformat(data["created"])
        entries = data["entries"]
        if (type(data["schema"]) is not int or data["schema"] != 1
                or stamp.tzinfo is None or not isinstance(entries, list)
                or not entries or type(data["expected_count"]) is not int
                or len(entries) != data["expected_count"]):
            raise ValueError("Incomplete archive manifest")
        root = manifest.parent.resolve()
        seen = set()
        for entry in entries:
            relative = Path(entry["path"])
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("Archive entry escapes archive directory")
            target = (root / relative).resolve(strict=True)
            target.relative_to(root)
            if target in seen or not target.is_file():
                raise ValueError("Duplicate or non-file archive entry")
            seen.add(target)
            if (type(entry["size"]) is not int or entry["size"] <= 0
                    or target.stat().st_size != entry["size"]
                    or not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"])
                    or file_sha256(target) != entry["sha256"]):
                raise ValueError("Archive entry failed integrity check")
        return dict(status="verified", verified_count=len(seen), archived_at=data["created"])
    except (OSError, ValueError, KeyError, TypeError, AttributeError, RuntimeError):
        return dict(status="unavailable", verified_count=0, archived_at=None)
