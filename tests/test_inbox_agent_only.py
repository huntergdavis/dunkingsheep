"""Explicit agent coordination stays distinguishable without changing legacy messages."""
import unittest
import tempfile
import sqlite3
from contextlib import closing
from importlib.machinery import SourceFileLoader
from pathlib import Path
from unittest.mock import Mock

CLI = Path(__file__).resolve().parent.parent / 'dunkingsheep'


class AgentOnlyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cli = SourceFileLoader('t3776_cli', str(CLI)).load_module()

    def test_marked_and_default_requests(self):
        for flag in (True, False):
            args = self.cli.build_parser().parse_args(
                ['inbox', 'send', '%155', 'body', '-s', 'plan'] + (['--agent-only'] if flag else []))
            client = Mock()
            self.cli.run_client_command(client, args)
            self.assertEqual(client.call.call_args.kwargs['subject'], 'plan')
            self.assertEqual(client.call.call_args.args[0], 'inbox_send_agent_only' if flag else 'inbox_send')
            self.assertEqual(client.call.call_args.kwargs['body'], 'body')

    def test_human_destinations_and_decisions_are_rejected_before_send(self):
        for to, subject in [('owner', 'plan'), ('owner (%1)', 'plan'), ('%155', '[owner-decision] choose'),
                            ('%155', '[owner-blocker] help')]:
            args = self.cli.build_parser().parse_args(
                ['inbox', 'send', to, 'body', '--agent-only', '-s', subject])
            client = Mock()
            with self.assertRaisesRegex(SystemExit, '--agent-only cannot'):
                self.cli.run_client_command(client, args)
            client.call.assert_not_called()

    def test_python_store_persists_explicit_audience_without_changing_subject(self):
        from dunk_inbox import Inbox
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = Inbox(path=str(root / 'inbox.db'), blob_dir=str(root / 'blobs'))
            ordinary = store.send('agent', 'ordinary', subject='[agent-coordination] literal')
            marked = store.send('agent', 'private', subject='normal', agent_only=True)
            with closing(sqlite3.connect(root / 'inbox.db')) as conn:
                rows = conn.execute('SELECT message_id, audience FROM message_audiences').fetchall()
                self.assertEqual(rows, [(marked['message_id'], 'agent')])
                self.assertEqual(conn.execute('SELECT subject FROM messages WHERE id=?', (marked['message_id'],)).fetchone()[0], 'normal')
                self.assertNotEqual(ordinary['message_id'], marked['message_id'])
            store.db.close()

    def test_classification_failure_rolls_back_message(self):
        from dunk_inbox import Inbox
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = Inbox(path=str(root / 'inbox.db'), blob_dir=str(root / 'blobs'))
            store.db.execute("CREATE TRIGGER fail_audience BEFORE INSERT ON message_audiences BEGIN SELECT RAISE(ABORT, 'test'); END")
            store.db.commit()
            with self.assertRaises(sqlite3.IntegrityError):
                store.send('agent', 'private', agent_only=True)
            self.assertEqual(store.db.execute('SELECT COUNT(*) FROM messages').fetchone()[0], 0)
            self.assertEqual(store.db.execute('SELECT COUNT(*) FROM message_audiences').fetchone()[0], 0)
            store.db.close()

    def test_explicit_rpc_forces_audience_even_if_argument_conflicts(self):
        import dunk_api
        flock = Mock()
        dunk_api.h_inbox_send_agent_only(flock, {'to': 'lane', 'body': 'text', 'agent_only': False}, '%1')
        self.assertIs(flock.inbox_send.call_args.kwargs['agent_only'], True)

    def test_delete_and_purge_remove_only_their_audience_rows(self):
        from dunk_inbox import Inbox
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = Inbox(path=str(root / 'inbox.db'), blob_dir=str(root / 'blobs'))
            deleted = store.send('agent', 'delete', agent_only=True)['message_id']
            purged = store.send('agent', 'purge', agent_only=True)['message_id']
            retained = store.send('agent', 'unread', agent_only=True)['message_id']
            store.delete([deleted])
            with closing(sqlite3.connect(root / 'inbox.db')) as other:
                self.assertEqual(other.execute('SELECT COUNT(*) FROM messages WHERE id=?', (deleted,)).fetchone()[0], 0)
                self.assertEqual(other.execute('SELECT COUNT(*) FROM message_audiences WHERE message_id=?', (deleted,)).fetchone()[0], 0)
            store.ack([purged])
            store.purge(older_than_s=-1)
            with closing(sqlite3.connect(root / 'inbox.db')) as other:
                self.assertEqual(other.execute('SELECT COUNT(*) FROM messages WHERE id=?', (purged,)).fetchone()[0], 0)
                self.assertEqual(other.execute('SELECT COUNT(*) FROM message_audiences WHERE message_id=?', (purged,)).fetchone()[0], 0)
            rows = store.db.execute('SELECT message_id FROM message_audiences').fetchall()
            self.assertEqual([row[0] for row in rows], [retained])
            store.db.close()

    def test_delete_failure_rolls_back_message_and_audience(self):
        from dunk_inbox import Inbox
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = Inbox(path=str(root / 'inbox.db'), blob_dir=str(root / 'blobs'))
            message = store.send('agent', 'private', agent_only=True)['message_id']
            store.db.execute("CREATE TRIGGER fail_message_delete BEFORE DELETE ON messages BEGIN SELECT RAISE(ABORT, 'test'); END")
            store.db.commit()
            with self.assertRaises(sqlite3.IntegrityError):
                store.delete([message])
            with closing(sqlite3.connect(root / 'inbox.db')) as other:
                self.assertEqual(other.execute('SELECT COUNT(*) FROM messages WHERE id=?', (message,)).fetchone()[0], 1)
                self.assertEqual(other.execute('SELECT COUNT(*) FROM message_audiences WHERE message_id=?', (message,)).fetchone()[0], 1)
            store.db.close()

    def test_opening_legacy_database_keeps_existing_message(self):
        from dunk_inbox import Inbox
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = Inbox(path=str(root / 'inbox.db'), blob_dir=str(root / 'blobs'))
            message = store.send('agent', 'legacy', subject='[agent-coordination] literal')['message_id']
            store.db.execute('DROP TABLE message_audiences')
            store.db.commit()
            store.db.close()
            reopened = Inbox(path=str(root / 'inbox.db'), blob_dir=str(root / 'blobs'))
            self.assertEqual(reopened.get(message)['body'], 'legacy')
            self.assertEqual(reopened.db.execute('SELECT COUNT(*) FROM message_audiences').fetchone()[0], 0)
            reopened.db.close()
