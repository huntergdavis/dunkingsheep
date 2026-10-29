"""Automatic retirement sweep: actual SQLite rows, no backend/process control."""
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch
from tests import _bootstrap  # noqa: F401
from dunk_core import Flock
from dunk_inbox import Inbox


class Once:
    def __init__(self): self.calls = 0
    def wait(self, _):
        self.calls += 1
        return self.calls > 1


class RetiredArchiveTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.inbox = Inbox(path=str(Path(self.tmp.name)/'inbox.db'), blob_dir=str(Path(self.tmp.name)/'blobs'))
        self.addCleanup(self.inbox.close)
        self.now = time.time()
        self.flock = Flock.__new__(Flock)
        self.flock.inbox = self.inbox
        self.flock.herdr = Mock(live=False)
        self.view = [{'pane_id':'%999','label':'active-sentinel'}]
        self.flock.herdr.list_panes.return_value = self.view
        self.flock._task_stop = Once()
        self.flock._snapshot_launches = Mock()
        self.flock._clear_historical_errors = Mock()
        self.flock._lane_registry = lambda **_: {}
        self.flock.metrics = Mock()
        self.records = [{'pane_id': 'wOld:p1', 'reason': 'superseded', 'retired_at':100, 'labels': ['legacy'], 'mailboxes': []}]
        p = patch('dunk_panes.all_retired', side_effect=lambda: self.records)
        p.start(); self.addCleanup(p.stop)
        for target in ('subprocess.Popen', 'socket.socket', 'os.kill'):
            p = patch(target, side_effect=AssertionError('external operation'))
            p.start(); self.addCleanup(p.stop)

    def message(self, mailbox, *, age=90000, state='queued', pane_id=None, attachments=()):
        mid = self.inbox.send(mailbox, 'retained body', subject='retained subject', pane_id=pane_id, attachments=attachments)['message_id']
        self.inbox.db.execute('UPDATE messages SET created_at=?,state=? WHERE id=?', (self.now-age,state,mid))
        self.inbox.db.commit()
        return mid

    def state(self, mid): return self.inbox.status(mid)['state']

    def test_tick_archives_legacy_labels_without_deleting_or_rewriting_payload(self):
        ids = [self.message('legacy'), self.message('legacy (wOld:p1)')]
        controls = [self.message('active'), self.message('unrouted:codex'), self.message('unknown')]
        before = [tuple(r) for r in self.inbox.db.execute('SELECT id,body,subject,created_at FROM messages ORDER BY id')]
        Flock._task_reaper_loop(self.flock)
        self.assertEqual(['archived','archived'],[self.state(i) for i in ids])
        self.assertEqual(['queued']*3,[self.state(i) for i in controls])
        self.assertEqual(before,[tuple(r) for r in self.inbox.db.execute('SELECT id,body,subject,created_at FROM messages ORDER BY id')])
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view), 'repeat must be idempotent')

    def test_active_label_registry_and_message_binding_protect_whole_boxes(self):
        self.records[0]['labels'] = ['live-label','rebound','bound']
        self.flock._lane_registry = lambda **_: {'%2': {'mailbox':'rebound','declared':True}}
        ids = [self.message('live-label'),self.message('rebound'),self.message('bound'),self.message('bound',pane_id='%2')]
        self.assertEqual(0,self.flock._archive_retired_inbox([{'pane_id':'%2','label':'live-label'}]))
        self.assertTrue(all(self.state(i)=='queued' for i in ids))

    def test_conflicting_suffix_unknown_quarantine_and_missing_view_are_preserved(self):
        self.records[0]['mailboxes'] = ['unrouted:codex']
        ids = [self.message('legacy (%9)'),self.message('unknown'),self.message('unrouted:codex')]
        old = self.message('legacy')
        self.assertEqual(0,self.flock._archive_retired_inbox(None))
        self.assertEqual('queued',self.state(old))
        self.assertEqual(1,self.flock._archive_retired_inbox(self.view))
        self.assertTrue(all(self.state(i)=='queued' for i in ids))

    def test_tmux_alias_and_cutoff_state_controls(self):
        self.records = [{'pane_id':'%8','reason':'retired','retired_at':100,'mailboxes':['named'],'labels':[]}]
        old = [self.message('named'),self.message('named (%8)'),self.message('%8',state='undeliverable')]
        controls = [self.message('named',age=0),self.message('named',state='read'),self.message('named',state='acked'),self.message('named',state='cancelled')]
        self.assertEqual(3,self.flock._archive_retired_inbox(self.view))
        self.assertTrue(all(self.state(i)=='archived' for i in old))
        self.assertEqual(['queued','read','acked','cancelled'],[self.state(i) for i in controls])
        self.assertTrue(all(self.inbox.status(i)['read_at'] is not None for i in old))

    def test_retired_pane_still_live_is_not_swept(self):
        mid = self.message('legacy')
        self.assertEqual(0,self.flock._archive_retired_inbox([{'pane_id':'wOld:p1'}]))
        self.assertEqual('queued',self.state(mid))

    def test_malformed_alias_fields_cannot_retire_arbitrary_keys(self):
        self.records = [{'pane_id':'%8','reason':'retired','retired_at':100,'labels':'legacy','mailboxes':[99,None]}]
        ids = [self.message('l'),self.message('99'),self.message('legacy')]
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertTrue(all(self.state(i)=='queued' for i in ids))

    def test_archive_preserves_attachment_and_starts_normal_retention(self):
        import hashlib
        attachment = Path(self.tmp.name)/'evidence.txt'
        attachment.write_text('retained evidence')
        mid = self.message('legacy',age=15*86400,attachments=[str(attachment)])
        before = [tuple(r) for r in self.inbox.db.execute('SELECT * FROM attachments')]
        self.assertEqual(1,self.flock._archive_retired_inbox(self.view))
        self.assertEqual(before,[tuple(r) for r in self.inbox.db.execute('SELECT * FROM attachments')])
        self.assertEqual(0,self.inbox.purge()['purged'])
        self.assertEqual('archived',self.state(mid))
        blob = Path(self.inbox.blob_dir)/hashlib.sha256(attachment.read_bytes()).hexdigest()
        self.assertEqual(attachment.read_bytes(),blob.read_bytes())

    def test_reasonless_record_cannot_withdraw_binding_protection(self):
        self.records.append({'pane_id':'%2','reason':'','retired_at':100})
        self.flock._lane_registry = lambda **_: {'%2': {'mailbox':'legacy','declared':True,'declared_at':200}}
        old = self.message('legacy')
        self.message('legacy',pane_id='%2',age=0)
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('queued',self.state(old))

    def test_newer_registry_identity_reusing_retired_id_is_protected(self):
        self.flock._lane_registry = lambda **_: {'wOld:p1': {'mailbox':'rebound','declared':True,'declared_at':200}}
        old = self.message('legacy',pane_id='wOld:p1')
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('queued',self.state(old))

    def boundary_change(self, change):
        original = self.inbox.archive_retired
        def archive(*args, **kwargs):
            change()
            return original(*args, **kwargs)
        self.inbox.archive_retired = archive

    def test_binding_added_at_archive_boundary_stops_mutation(self):
        old = self.message('legacy')
        self.boundary_change(lambda: self.message('legacy',pane_id='%2',age=0))
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('queued',self.state(old))

    def test_retirement_revoked_at_archive_boundary_stops_mutation(self):
        old = self.message('legacy')
        self.boundary_change(lambda: self.records.clear())
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('queued',self.state(old))

    def test_registry_changed_at_archive_boundary_stops_mutation(self):
        registry = {}
        self.flock._lane_registry = lambda **_: registry
        old = self.message('legacy')
        self.boundary_change(lambda: registry.update({'%2':{'mailbox':'legacy','declared':True,'declared_at':200}}))
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('queued',self.state(old))

    def test_missing_retirement_time_is_not_positive_evidence(self):
        del self.records[0]['retired_at']
        old = self.message('legacy')
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('queued',self.state(old))

    def test_older_matching_registry_identity_does_not_disable_retirement(self):
        self.flock._lane_registry = lambda **_: {'wOld:p1':{'mailbox':'legacy','declared':True,'declared_at':50}}
        old = self.message('legacy',pane_id='wOld:p1')
        self.assertEqual(1,self.flock._archive_retired_inbox(self.view))
        self.assertEqual('archived',self.state(old))

    def test_empty_backend_view_is_not_liveness_evidence(self):
        old = self.message('legacy')
        self.assertEqual(0,self.flock._archive_retired_inbox([]))
        self.assertEqual('queued',self.state(old))

    def test_mutation_guard_holds_identity_locks_and_sqlite_writer_lock(self):
        import fcntl, sqlite3, dunk_panes
        old = self.message('legacy')
        original = self.inbox.archive_retired
        observed = []
        def archive(*args, **kwargs):
            authorize = kwargs['authorize']
            def check():
                self.assertTrue(self.inbox.db.in_transaction)
                for path in [dunk_panes.PATH,self.flock.lane_registry_path()]:
                    with open(path+'.lock','a') as handle:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(handle,fcntl.LOCK_EX|fcntl.LOCK_NB)
                with sqlite3.connect(self.inbox.path,timeout=0) as writer:
                    with self.assertRaises(sqlite3.OperationalError):
                        writer.execute("UPDATE messages SET pane_id='%2' WHERE id=?",(old,))
                observed.append(True)
                return authorize()
            kwargs['authorize'] = check
            return original(*args, **kwargs)
        self.inbox.archive_retired = archive
        self.assertEqual(1,self.flock._archive_retired_inbox(self.view))
        self.assertEqual([True],observed)
        self.assertEqual('archived',self.state(old))

    def test_unreadable_or_unidentified_registry_keeps_mail_queued(self):
        path = Path(self.tmp.name)/'registry.json'
        self.flock.lane_registry_path = lambda: str(path)
        self.flock._lane_registry = Flock._lane_registry.__get__(self.flock,Flock)
        old = self.message('legacy')
        for text in [None,'invalid JSON','[]','{"%2": {"declared": true}}']:
            if text is not None: path.write_text(text)
            self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
            self.assertEqual('queued',self.state(old))

    def test_existing_caller_transaction_is_not_committed_or_rolled_back(self):
        old = self.message('legacy')
        self.inbox.db.execute("UPDATE messages SET subject='caller pending' WHERE id=?",(old,))
        self.assertTrue(self.inbox.db.in_transaction)
        self.assertEqual(0,self.flock._archive_retired_inbox(self.view))
        self.assertTrue(self.inbox.db.in_transaction)
        self.assertEqual('caller pending',self.inbox.db.execute('SELECT subject FROM messages WHERE id=?',(old,)).fetchone()[0])
        self.assertEqual('queued',self.state(old))
        self.inbox.db.rollback()
