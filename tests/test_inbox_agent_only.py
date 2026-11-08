"""Explicit agent coordination stays distinguishable without changing legacy messages."""
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path
from unittest.mock import Mock

CLI = Path(__file__).resolve().parent.parent / 'dunkingsheep'


class AgentOnlyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cli = SourceFileLoader('t3776_cli', str(CLI)).load_module()

    def test_marked_and_default_requests(self):
        for flag, expected in [(True, '[agent-coordination] plan'), (False, 'plan')]:
            args = self.cli.build_parser().parse_args(
                ['inbox', 'send', '%155', 'body', '-s', 'plan'] + (['--agent-only'] if flag else []))
            client = Mock()
            self.cli.run_client_command(client, args)
            self.assertEqual(client.call.call_args.kwargs['subject'], expected)
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
