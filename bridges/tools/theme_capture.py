"""Theme evidence must show the page, without a dialog or backdrop dimming it.

Guard selectors and evidence design follow codex-design handoff i25587.
This records capture preconditions, not full-page accessibility compliance.
"""
import json
from pathlib import Path

GUARD_SCRIPT = '''() => ({
  role_dialog: document.querySelectorAll('[role="dialog"]').length,
  modal: document.querySelectorAll(':modal').length,
  backdrop: document.querySelectorAll('.modal-backdrop').length,
  open_dialog: document.querySelectorAll('dialog[open]').length
})'''


class CaptureBlocked(AssertionError):
    pass


class ThemeCapture:
    def __init__(self, directory, metadata=None):
        self.directory = Path(directory)
        self.directory.mkdir(parents=True, exist_ok=True)
        self.evidence = {'metadata': metadata or {}, 'captures': []}

    def write(self):
        path = self.directory / 'evidence.json'
        path.write_text(json.dumps(self.evidence, indent=2) + '\n')

    def capture(self, page, filename, **metadata):
        if Path(filename).name != filename:
            raise ValueError('Capture filename must stay inside its evidence directory')
        record = {'file': filename, **metadata, 'dialog_closed': False, 'captured': False}
        self.evidence['captures'].append(record)
        try:
            counts = page.evaluate(GUARD_SCRIPT)
            record['dialog_counts'] = counts
            record['dialog_closed'] = (set(counts) == {'role_dialog', 'modal', 'backdrop', 'open_dialog'}
                                       and all(type(value) is int and value == 0 for value in counts.values()))
            self.write()  # The assertion is recorded before every capture, even a refusal.
            if not record['dialog_closed']:
                raise CaptureBlocked(f'Theme capture refused: dialog/backdrop present: {counts}')
            page.screenshot(path=str(self.directory / filename), animations='disabled')
            record['captured'] = True
            self.write()
        except Exception as error:
            record['error'] = str(error)
            self.write()
            raise
