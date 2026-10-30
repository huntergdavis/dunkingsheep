#!/usr/bin/env python3
"""Mechanically judge a free model's DECODE of one Poolrad body.

The nvidia 550B free model, given tools, decoded `OVL_0025_06AF` correctly: right body,
right overlay base arithmetic (31618 - 0x6AF = 0x74D3), every instruction byte matching
the file - and one endianness slip on the `retf 4` immediate (`CA 00 04` instead of
`CA 04 00`). That is the shape of free-model error: shallow and mechanical, not deep and
plausible. So a free decode is cheap to check and cheap to fix.

This tool does exactly one thing: it takes the report's claimed FILE OFFSET and byte dump,
reads those bytes from the game file itself, and compares. It prints one of

    BYTES MATCH        the dump is byte-identical to the file at the claimed offset
    BYTES DIFFER       shows the first differing byte, file value vs claimed value
    PREFIX ONLY        the dump is shorter than --expect-length: bytes may match but it is not a decode
    NO DUMP            the report printed no checkable byte dump
    OFFSET UNRESOLVED  the one strict offset declaration is missing, competing, malformed or wrong

Requires exactly one FILE OFFSET: <decimal> (0x<hex>) declaration. All parsed
byte sequences must match; no matching dump can rescue a conflicting one.
Addressed rows must be contiguous. Address labels may be VMAs; their relocation
provenance and all assembly/prose semantics are NOT verified by this tool.
Every nonblank line has exactly one disposition: parsed-dump, dropped-in-dump-context,
noise, or prose-outside-dump. Every verdict prints these counts and drop locations.
Any dropped dump fails the entire report; noise outside dumps is a warning.
Explicit sections survive blank lines, end at a matching code fence or EOF, and
require every content line to parse. Structural headings/fences count as prose.
It must not be read as a credit decision.

Usage: free_body_check.py <report.md> [--ovr PATH]
Exit: 0 match, 1 differ/unresolved, 2 no dump / usage error.
"""
from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_OVR = Path("resources/DOS/POOLRAD/GAME.OVR")

# "file offset 31618" / "offset 0x7B82" / "+31618" / "@ 31618"
OFFSET_RE = re.compile(
    r"(?:file\s+offset|offset|@|\bat\b)[\s:=`'\"]*(0x[0-9A-Fa-f]+|\d{4,7})", re.I
)
# a run of >=6 hex byte pairs, tolerating "55 89 e5", "5589E5" and "0x55, 0x89"
BYTES_RE = re.compile(r"(?:\b[0-9A-Fa-f]{2}\b[\s,]*){6,}")
BYTES_COMPACT_RE = re.compile(r"\b(?:[0-9A-Fa-f]{2}){12,}\b")


def claimed_offsets(text: str) -> list[int]:
    """Every plausible offset the report names, in the order it names them."""
    vals: list[int] = []
    for m in OFFSET_RE.findall(text):
        try:
            v = int(m, 16) if m.lower().startswith("0x") else int(m)
        except ValueError:
            continue
        if 0 < v < 2 * 1024 * 1024 and v not in vals:
            vals.append(v)
    return vals



# Address labels may be file offsets or a disassembler VMA. Check their
# continuity, not relocation provenance; the strict header fixes the file slice.
HEX_TOKEN_RE = re.compile(r"(?:[0-9A-Fa-f]{2})+")
INLINE_BYTES_RE = re.compile(
    r"^(?:\d+[.)]\s*)?(?:disassembly\s*\(bytes\)|byte dump|bytes):\s*", re.I
)


def byte_column(text: str) -> bytes:
    # A double space/tab separates the bytes from xxd ASCII or assembly text.
    # Within the byte column accept byte pairs and xxd/objdump packed groups.
    column = re.split(r"\t| {2,}", text.lstrip(), maxsplit=1)[0]
    tokens = []
    for token in column.split():
        if not HEX_TOKEN_RE.fullmatch(token):
            break
        tokens.append(token)
    return bytes.fromhex("".join(tokens))


@dataclass(frozen=True)
class DiscardedDump:
    line: int
    report_offset: int
    address: int | None
    reason: str
    address_token: str | None = None

    def describe(self) -> str:
        location = f"line {self.line}, report offset {self.report_offset}"
        if self.address is not None:
            location += f", address 0x{self.address:x}"
        elif self.address_token is not None:
            location += f", address token {self.address_token!r}"
        return f"{location}: {self.reason}"


@dataclass(frozen=True)
class LineAccount:
    line: int
    report_offset: int
    bucket: str


@dataclass
class DumpParse:
    candidates: list[bytes] = field(default_factory=list)
    dropped: list[DiscardedDump] = field(default_factory=list)
    noise_runs: int = 0
    lines: list[LineAccount] = field(default_factory=list)
    nonblank_lines: int = 0

    @property
    def line_counts(self) -> dict[str, int]:
        return {bucket: sum(line.bucket == bucket for line in self.lines) for bucket in LINE_BUCKETS}

    def assert_accounted(self, text: str):
        expected = [i for i, line in enumerate(text.splitlines(), 1) if line.strip()]
        assert len(expected) == self.nonblank_lines, "input line count mismatch"
        assert [line.line for line in self.lines] == expected, "missing or duplicate line disposition"
        assert all(line.bucket in LINE_BUCKETS for line in self.lines), "unknown line bucket"
        assert sum(self.line_counts.values()) == len(expected), "line accounting does not balance"
        assert self.line_counts["dropped-in-dump-context"] == len(self.dropped), "discard count mismatch"
        assert self.line_counts["noise"] == self.noise_runs, "noise count mismatch"

    def diagnostic(self) -> str:
        details = "; ".join(item.describe() for item in self.dropped)
        return (f"dropped: {len(self.dropped)}" + (f" [{details}]" if details else "") +
                f"; noise: {self.noise_runs}; lines: " +
                ", ".join(f"{key}={value}" for key, value in self.line_counts.items()) +
                f"; accounted: {sum(self.line_counts.values())}/{self.nonblank_lines}")


LINE_BUCKETS = ("parsed-dump", "dropped-in-dump-context", "noise", "prose-outside-dump")
VALID_ADDRESS_RE = re.compile(r"(?:0x)?[0-9a-f]{1,8}", re.I)
FENCE_RE = re.compile(r"(\x60{3,}|~{3,})([A-Za-z0-9_-]*)")


def addressed_column(line: str) -> tuple[str, str] | None:
    """Recognize column syntax before requiring a valid address or valid bytes.

    A row marker, a digit-bearing address token, a valid hex address, or a
    byte-shaped payload establishes the candidate. Colonless columns retain
    the width/spacing conventions of xxd/objdump. Ordinary prose labels and
    SHA metadata outside dump sections are not addressed columns. This is a
    documented lexical grammar, not an exhaustive format-recognition proof.
    """
    colon = re.fullmatch(r"\s*(?:(row)\s+)?([^:]+):[ \t]*(.*)", line, re.I)
    if colon:
        marker, token, payload = colon.groups()
        token = token.strip()
    else:
        spaced = re.fullmatch(r"\s*(?:(row)\s+)?(\S+)([ \t]+)(.*)", line, re.I)
        if not spaced:
            return None
        marker, token, spacing, payload = spaced.groups()
        if not marker and not (6 <= len(token) <= 10 or
                               (4 <= len(token) <= 10 and len(spacing) >= 2)):
            return None
    words = payload.split()
    byte_shaped = bool(words) and all(
        len(word) in (2, 4, 8) and word.isalnum() for word in words[:2])
    if marker or any(char.isdigit() for char in token) or VALID_ADDRESS_RE.fullmatch(token) or byte_shaped:
        return token, payload
    return None


def parse_dump_report(text: str) -> DumpParse:
    """Assign each nonblank input line exactly one disposition.

    BYTE DUMP labels and empty byte labels open explicit sections. Blank lines
    split independent candidates but do not close an explicit section. A
    matching Markdown fence closes a fenced section; an unfenced section ends
    at EOF. Section headers/fences are structural prose, not byte rows. Every
    other line inside a section must parse or be counted as a dropped error,
    even if its first token fails validation. Implicit dump runs end at a blank
    line. Byte columns may have annotations after a tab or two spaces.
    """
    result = DumpParse(nonblank_lines=sum(bool(line.strip()) for line in text.splitlines()))
    current = bytearray()
    mode = None
    next_address = None
    declared_dump = False
    implicit_dump = False
    fence = None
    report_offset = 0

    def flush():
        if current:
            result.candidates.append(bytes(current))
        current.clear()

    for line_number, raw_line in enumerate(text.splitlines(keepends=True), 1):
        line = raw_line.rstrip("\r\n")
        stripped = line.strip()
        line_offset = report_offset
        report_offset += len(raw_line)
        if not stripped:
            flush(); mode = None; next_address = None; implicit_dump = False
            continue

        def account(bucket, reason="", address=None, address_token=None):
            result.lines.append(LineAccount(line_number, line_offset, bucket))
            if bucket == "dropped-in-dump-context":
                result.dropped.append(DiscardedDump(line_number, line_offset, address, reason, address_token))
            elif bucket == "noise":
                result.noise_runs += 1

        fence_match = FENCE_RE.fullmatch(stripped)
        if fence_match:
            marker, language = fence_match.groups()
            if fence is None:
                fence = marker
                declared_dump = declared_dump or implicit_dump or language.lower() in ("hex", "bytes", "xxd", "hexdump")
                account("prose-outside-dump")
            elif marker == fence and not language:
                fence = None; declared_dump = False
                account("prose-outside-dump")
            elif declared_dump or implicit_dump:
                account("dropped-in-dump-context", "mismatched dump fence")
            else:
                account("prose-outside-dump")
            flush(); mode = None; next_address = None; implicit_dump = False
            continue

        label = INLINE_BYTES_RE.match(stripped)
        if label:
            payload = stripped[label.end():]
            declared_dump = declared_dump or "byte dump" in label[0].lower() or not payload
            flush(); mode = None; next_address = None
            if not payload:
                account("prose-outside-dump")  # structural section heading
                continue
            address_info = None
        else:
            # Metadata is prose only outside a dump; it cannot escape a section.
            metadata = re.match(r"(?:[0-9]+[.)]\s*)?FILE\s+OFFSET\b|SHA(?:-?256|-?1)?:",
                                stripped.replace("*", "").replace("`", ""), re.I)
            address_info = None if metadata else addressed_column(line)
            payload = address_info[1] if address_info else stripped
        in_dump = declared_dump or implicit_dump or bool(label) or address_info is not None
        address_token = address_info[0] if address_info else None
        address = None
        reason = None
        if address_info:
            if not VALID_ADDRESS_RE.fullmatch(address_token):
                reason = "malformed address token"
            else:
                address = int(address_token, 16)

        column = re.split(r"\t| {2,}", payload.lstrip(), maxsplit=1)[0]
        tokens = column.split()
        valid_column = bool(tokens) and all(HEX_TOKEN_RE.fullmatch(token) for token in tokens)
        chunk = byte_column(payload)
        if not valid_column:
            if in_dump or len(chunk) >= 6:
                reason = reason or "empty or malformed dump byte column"
            elif chunk:
                account("noise")
                flush(); mode = None; next_address = None; implicit_dump = False
                continue
            else:
                account("prose-outside-dump")
                flush(); mode = None; next_address = None; implicit_dump = False
                continue
        if reason is None and address_info and mode == "addressed" and address != next_address:
            reason = f"addressed dump gap/overlap: expected 0x{next_address:x}"
        if reason:
            account("dropped-in-dump-context", reason, address, address_token)
            flush(); mode = None; next_address = None
            implicit_dump = True
            continue
        account("parsed-dump")
        new_mode = "addressed" if address_info else "plain"
        if mode != new_mode:
            flush()
        mode = new_mode
        next_address = address + len(chunk) if address_info else None
        implicit_dump = True
        current.extend(chunk)
    flush()
    result.assert_accounted(text)
    return result


def dump_candidates(text: str) -> list[bytes]:
    """Compatibility helper: discarded dump-shaped content is always an error."""
    result = parse_dump_report(text)
    if result.dropped:
        raise ValueError(result.diagnostic())
    return result.candidates


def claimed_bytes(text: str) -> bytes | None:
    """Compatibility helper; main checks EVERY candidate before choosing a size."""
    return max(dump_candidates(text), key=len, default=None)


def declared_file_offset(text: str) -> int | None:
    """Count all declarations, including legacy/malformed/competing ones."""
    plain = text.replace("*", "").replace("`", "")
    declarations = re.findall(r"\bFILE\s+OFFSET\b", plain, re.I)
    headers = re.findall(
        r"^[ \t]*(?:1[.)][ \t]*)?FILE OFFSET:[ \t]*([0-9]+)[ \t]+"
        r"\((0x[0-9a-fA-F]+)\)[ \t]*$", plain, re.M | re.I,
    )
    if len(declarations) != 1 or len(headers) != 1:
        return None
    decimal, hexadecimal = headers[0]
    value = int(decimal, 10)
    return value if value == int(hexadecimal, 16) else None


def main(argv: list[str]) -> int:
    parsed = DumpParse()

    def verdict(message: str, *, stderr: bool = False):
        print(f"   {message}; {parsed.diagnostic()}", file=sys.stderr if stderr else sys.stdout)

    if len(argv) < 2:
        verdict("usage: free_body_check.py <report.md> [--ovr PATH]", stderr=True)
        return 2
    rep = Path(argv[1])
    ovr = DEFAULT_OVR
    expect: int | None = None
    expect_len: int | None = None
    rest = argv[2:]
    while rest:
        if rest[0] == "--ovr" and len(rest) > 1:
            ovr = Path(rest[1]); rest = rest[2:]
        elif rest[0] == "--expect-offset" and len(rest) > 1:
            expect = int(rest[1], 0); rest = rest[2:]
        elif rest[0] == "--expect-length" and len(rest) > 1:
            expect_len = int(rest[1], 0); rest = rest[2:]
        else:
            rest = rest[1:]
    if not rep.exists():
        verdict(f"REFUSED: no report at {rep}", stderr=True)
        return 2
    text = rep.read_text(errors="replace")
    parsed = parse_dump_report(text)
    if not ovr.exists():
        verdict(f"REFUSED: no game file at {ovr}", stderr=True)
        return 2
    data = ovr.read_bytes()
    print(f"== {rep}")
    if parsed.noise_runs:
        print(f"   WARNING: ignored ambiguous-prose noise runs: {parsed.noise_runs}.")
    off = declared_file_offset(text)
    if off is None or (expect is not None and off != expect):
        verdict("OFFSET UNRESOLVED: require exactly one FILE OFFSET declaration: "
              "FILE OFFSET: <decimal> (0x<hex>), with both values equal to the expected body offset.")
        return 1
    if parsed.dropped:
        verdict("BYTES DIFFER: discarded dump-shaped content is an error")
        return 1
    candidates = parsed.candidates
    if not candidates:
        verdict("NO DUMP: the report printed no byte dump, so nothing is checkable.")
        return 2
    for cb in candidates:
        got = data[off:off + len(cb)]
        if got != cb:
            for i, (x, y) in enumerate(zip(got, cb)):
                if x != y:
                    verdict(f"BYTES DIFFER: first difference at +{i} (offset {off + i}): "
                          f"file {x:02x}, claimed {y:02x}.")
                    print(f"      file   : {' '.join(f'{v:02x}' for v in got[:48])}")
                    print(f"      claimed: {' '.join(f'{v:02x}' for v in cb[:48])}")
                    return 1
            verdict(f"BYTES DIFFER: dump extends outside the file ({len(got)} vs {len(cb)}).")
            return 1
    cb = max(candidates, key=len)
    if expect_len is not None and len(cb) < expect_len:
        verdict(f"PREFIX ONLY: dump has {len(cb)} of the expected {expect_len} bytes "
              "(matching the file); not a decode.")
        return 1
    verdict(f"BYTES MATCH: {len(cb)} bytes at {off} are byte-identical to {ovr.name}.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
