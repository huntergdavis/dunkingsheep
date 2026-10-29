"""Authenticated adapter for inspecting uploaded saves and writing new copies.

The Poolrad CLI owns file validation and reference-edit eligibility. Browser
requests use opaque IDs and profile names, never private paths or byte offsets.
"""
from __future__ import annotations

from datetime import datetime, timezone
import hashlib
import io
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import uuid
import zipfile

from fastapi import APIRouter, Depends, File, HTTPException, Response, UploadFile
from pydantic import BaseModel, ConfigDict, Field
from starlette.concurrency import run_in_threadpool

SAVE_NAME = re.compile(r'(?:CHRDAT[A-Z0-9_]*\.(?:SAV|ITM|SPC)|SAVGAM[A-Z0-9_]*\.DAT)')
COPY_NAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9 _-]{0,63}')
IDENTIFIER = re.compile(r'[a-f0-9]{32}')
MAX_FILES, MAX_FILE_BYTES, MAX_TOTAL_BYTES = 256, 1024 * 1024, 8 * 1024 * 1024
UNVERIFIED = {'game_validation': 'UNVERIFIED', 'game_build_loaded': None}


class EditorError(ValueError):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


class ProfileChoice(BaseModel):
    model_config = ConfigDict(extra='forbid', strict=True)
    file: str
    validation_profile: str


class CopyRequest(BaseModel):
    model_config = ConfigDict(extra='forbid', strict=True)
    source_id: str
    copy_name: str
    expected_source_hashes: dict[str, str]
    profiles: list[ProfileChoice] = Field(default_factory=list, max_length=512)


class EditorStore:
    def __init__(self, root: Path, tool: Path):
        self.root, self.tool = Path(root), Path(tool)

    def _run(self, payload, *, noop=False):
        tool = self.tool.with_name('disk_save.py') if noop else self.tool
        if not tool.is_file():
            raise EditorError('The save editor backend is not configured.', 503)
        argv = [sys.executable, str(tool)]
        if noop:
            argv += [payload['source'], payload['destination']]
        try:
            result = subprocess.run(argv, input=None if noop else json.dumps(payload),
                                    capture_output=True, text=True, timeout=30, check=False)
        except (OSError, subprocess.SubprocessError):
            raise EditorError('The save editor backend did not finish. No copy is available.', 503) from None
        if result.returncode:
            # CLI errors can contain private filesystem paths; return a bounded public reason.
            code = None
            try:
                code = json.loads(result.stderr).get('error', {}).get('code')
            except (ValueError, AttributeError):
                pass
            reasons = {
                'profile_mismatch': 'This record does not match the selected reference edit.',
                'source_mismatch': 'The source changed. Inspect it again before creating a copy.',
                'source_changed': 'The source changed while copying. No copy was kept.',
            }
            raise EditorError(reasons.get(code, 'The save set or edit was refused. Check file sizes, matching sidecars, and supported profiles.'))
        try:
            report = json.loads(result.stdout)
            if not isinstance(report, dict):
                raise ValueError()
            return report
        except ValueError:
            raise EditorError('The save editor returned an invalid result.', 503) from None

    def _inspect(self, folder):
        report = self._run({'schema_version': 1, 'operation': 'inspect', 'source': str(folder)})
        return {key: report[key] for key in ('expected_source_hashes', 'characters', 'validation_scope')}

    def _new_folder(self, kind):
        parent = self.root / kind
        parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        identifier = uuid.uuid4().hex
        folder = parent / identifier
        folder.mkdir(mode=0o700)
        return identifier, folder

    def _lookup(self, identifier):
        if not isinstance(identifier, str) or not IDENTIFIER.fullmatch(identifier):
            raise EditorError('Unknown save copy.', 404)
        for kind in ('imports', 'copies'):
            folder = self.root / kind / identifier
            if folder.is_symlink() or not (folder / 'metadata.json').is_file():
                continue
            try:
                return folder, json.loads((folder / 'metadata.json').read_text())
            except (OSError, ValueError):
                raise EditorError('The saved copy metadata is unavailable.', 409) from None
        raise EditorError('Unknown save copy.', 404)

    @staticmethod
    def _metadata(identifier, name, kind, inspection):
        return dict(id=identifier, name=name, kind=kind,
                    created_at=datetime.now(timezone.utc).isoformat(),
                    inspection=inspection, **UNVERIFIED)

    def sources(self):
        items = []
        for kind in ('imports', 'copies'):
            for path in (self.root / kind).glob('*/metadata.json'):
                if not IDENTIFIER.fullmatch(path.parent.name) or path.parent.is_symlink():
                    continue
                try:
                    item = json.loads(path.read_text())
                    items.append({k: item[k] for k in ('id', 'name', 'kind', 'created_at')})
                except (OSError, ValueError, KeyError):
                    continue
        return {'available': self.tool.is_file(), 'sources': sorted(items, key=lambda item: item['created_at'], reverse=True)}

    def inspect(self, identifier):
        folder, metadata = self._lookup(identifier)
        return {**{k: metadata[k] for k in ('id', 'name', 'kind')}, **self._inspect(folder / 'files'), **UNVERIFIED}

    def import_files(self, uploads):
        if not 1 <= len(uploads) <= MAX_FILES:
            raise EditorError(f'Choose between 1 and {MAX_FILES} save files.')
        files = {}
        for name, data in uploads:
            name = name.upper()
            if not SAVE_NAME.fullmatch(name) or name in files:
                raise EditorError('Use unique DOS save filenames: CHRDAT*.SAV/ITM/SPC or SAVGAM*.DAT.')
            if len(data) > MAX_FILE_BYTES:
                raise EditorError('A save file exceeds the 1 MiB limit.', 413)
            files[name] = data
        if sum(map(len, files.values())) > MAX_TOTAL_BYTES:
            raise EditorError('The save set exceeds the 8 MiB limit.', 413)
        identifier, folder = self._new_folder('imports')
        try:
            target = folder / 'files'
            target.mkdir(mode=0o700)
            for name, data in files.items():
                with (target / name).open('xb') as stream:
                    stream.write(data)
            inspection = self._inspect(target)
            metadata = self._metadata(identifier, 'Imported save ' + identifier[:8], 'imports', inspection)
            (folder / 'metadata.json').write_text(json.dumps(metadata))
            return {**{k: metadata[k] for k in ('id', 'name', 'kind')}, **inspection, **UNVERIFIED}
        except BaseException:
            shutil.rmtree(folder)
            raise

    @staticmethod
    def _plan(inspection, choices):
        characters = {item['file']: item for item in inspection['characters']}
        available = {name: {p['validation_profile'] for p in item['profiles'] if p['eligible']}
                     for name, item in characters.items()}
        seen, changes = set(), []
        for choice in choices:
            name, profile_name = choice.file, choice.validation_profile
            key = name, profile_name
            if key in seen or profile_name not in available.get(name, set()):
                raise EditorError('Select only available profiles in their required order.')
            seen.add(key)
            profile = next(p for p in characters[name]['profiles'] if p['validation_profile'] == profile_name)
            changes.append({k: profile[k] for k in ('field', 'expected_before', 'value', 'validation_profile')} | {'file': name})
            available[name].update(profile['eligible_after_application'])
        return changes

    @staticmethod
    def _public_receipt(receipt):
        """Expose the schema-1 receipt contract, not future private CLI fields."""
        def fields(value, allowed):
            return {key: value[key] for key in allowed if key in value}

        def public_change(change):
            row = fields(change, ('file', 'field', 'expected_before', 'value',
                                 'validation_profile', 'index', 'offset',
                                 'before_sha256', 'after_sha256',
                                 'historical_evidence', 'proof_scope'))
            if 'file' in row:
                row['file'] = Path(row['file']).name
            return row

        result = fields(receipt, ('schema_version', 'operation',
                                  'historical_reference_only', 'limits'))
        result.update(UNVERIFIED)
        for key in ('applied_changes', 'rejected_changes'):
            if key in receipt:
                result[key] = [public_change(change) for change in receipt[key]]
        result['files'] = []
        for item in receipt.get('files', []):
            row = fields(item, ('operation', 'size', 'before_sha256', 'after_sha256',
                                'changed_offsets', 'byte_identical'))
            row['file'], row['source'] = Path(item['file']).name, Path(item['source']).name
            if 'before_after_bytes' in item:
                row['before_after_bytes'] = [fields(change, ('offset', 'before', 'after', 'reason'))
                                             for change in item['before_after_bytes']]
            row.update(UNVERIFIED)
            result['files'].append(row)
        return result

    def copy(self, request: CopyRequest):
        if not COPY_NAME.fullmatch(request.copy_name):
            raise EditorError('Name the new copy using 1–64 letters, numbers, spaces, hyphens or underscores.')
        source, _ = self._lookup(request.source_id)
        inspection = self._inspect(source / 'files')
        if request.expected_source_hashes != inspection['expected_source_hashes']:
            raise EditorError('The source changed. Inspect it again before creating a copy.', 409)
        changes = self._plan(inspection, request.profiles)
        identifier, folder = self._new_folder('copies')
        try:
            destination = folder / 'files'
            payload = dict(schema_version=1, operation='edit_copy', source=str(source / 'files'),
                           destination=str(destination), expected_source_hashes=request.expected_source_hashes,
                           changes=changes)
            report = self._run(payload, noop=not changes)
            if self._inspect(source / 'files')['expected_source_hashes'] != request.expected_source_hashes:
                raise EditorError('The source changed while copying. No copy was kept.', 409)
            after = self._inspect(destination)
            receipt = self._public_receipt(json.loads((destination / 'write-receipts.json').read_text()))
            (folder / 'receipt.json').write_text(json.dumps(receipt, indent=2))
            metadata = self._metadata(identifier, request.copy_name, 'copies', after)
            (folder / 'metadata.json').write_text(json.dumps(metadata))
            return dict(id=identifier, name=request.copy_name,
                        output_folder=f'character-editor/copies/{identifier}/files',
                        files_written=report['files_written'], bytes_written=report['bytes_written'],
                        byte_identical_files=report['byte_identical_files'], receipt=receipt,
                        inspection=after, download_url=f'/api/character-editor/copies/{identifier}/download',
                        **UNVERIFIED)
        except BaseException:
            shutil.rmtree(folder)
            raise

    def download(self, identifier):
        folder, metadata = self._lookup(identifier)
        if metadata['kind'] != 'copies':
            raise EditorError('Create a named output copy before downloading.', 409)
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as zipped:
            for name, expected in metadata['inspection']['expected_source_hashes'].items():
                path = folder / 'files' / name
                if not SAVE_NAME.fullmatch(name) or path.is_symlink():
                    raise EditorError('The output copy changed after export.', 409)
                data = path.read_bytes()
                if hashlib.sha256(data).hexdigest() != expected:
                    raise EditorError('The output copy changed after export.', 409)
                zipped.writestr(name, data)
            # The CLI's private-path receipt stays local; downloads use basename-only receipts.
            zipped.writestr('write-receipts.json', (folder / 'receipt.json').read_bytes())
        return metadata['name'].replace(' ', '-') + '.zip', archive.getvalue()


def router(store: EditorStore, require_auth):
    routes = APIRouter(prefix='/api/character-editor', dependencies=[Depends(require_auth)])

    def call(method, *args):
        try:
            return method(*args)
        except EditorError as exc:
            raise HTTPException(exc.status, str(exc)) from None
        except (OSError, KeyError, ValueError):
            raise HTTPException(409, 'The save copy is unavailable or changed. Inspect it again.') from None

    @routes.get('/sources')
    def sources():
        return call(store.sources)

    @routes.get('/sources/{identifier}')
    def inspect(identifier: str):
        return call(store.inspect, identifier)

    @routes.post('/imports')
    async def import_files(files: list[UploadFile] = File(...)):
        if len(files) > MAX_FILES:
            raise HTTPException(413, 'Too many save files.')
        uploads, total = [], 0
        for file in files:
            data = await file.read(MAX_FILE_BYTES + 1)
            total += len(data)
            if len(data) > MAX_FILE_BYTES or total > MAX_TOTAL_BYTES:
                raise HTTPException(413, 'Save upload exceeds the allowed size.')
            uploads.append((file.filename or '', data))
        return await run_in_threadpool(call, store.import_files, uploads)

    @routes.post('/copies')
    def copy(request: CopyRequest):
        return call(store.copy, request)

    @routes.get('/copies/{identifier}/download')
    def download(identifier: str):
        filename, data = call(store.download, identifier)
        return Response(data, media_type='application/zip',
                        headers={'Content-Disposition': f'attachment; filename="{filename}"'})

    return routes
