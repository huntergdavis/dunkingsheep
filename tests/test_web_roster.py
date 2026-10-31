"""t1147: a tmux pane reports agent == "" - the desk must still be messageable."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from web.roster import build_roster, is_agent_pane, match_pane  # noqa: E402

# Shapes measured from the live daemon (2026-10-30): the backend reports agent "" for every pane.
DESK = {"pane_id": "%133", "agent": "", "agent_status": "working", "tab_label": "sn-desk",
        "workspace_label": "bootstrap", "cwd": "/home/hunter/workspace/desk"}
FRESH = {"pane_id": "%135", "agent": "", "agent_status": None, "tab_label": "sn-reviewer-4",
         "workspace_label": "bootstrap", "cwd": "/home/hunter/workspace"}
SHELL = {"pane_id": "%3", "agent": "", "agent_status": None, "tab_label": "bash",
         "workspace_label": "par-a", "cwd": "/home/hunter"}
COS = {"id": "cos", "name": "Front Desk", "match": "desk", "kind": "active"}


class AgentPaneTests(unittest.TestCase):
    def test_a_pane_with_an_empty_agent_string_but_a_status_is_an_agent(self):
        self.assertTrue(is_agent_pane(DESK))
        self.assertTrue(is_agent_pane({"agent": "claude", "agent_status": None}))

    def test_a_plain_shell_and_no_pane_are_not_agents(self):
        self.assertFalse(is_agent_pane(SHELL))
        self.assertFalse(is_agent_pane(None))

    def test_the_desk_channel_can_message_the_live_desk_pane(self):
        row = next(r for r in build_roster([COS], [SHELL, DESK]) if r["id"] == "cos")
        self.assertEqual("%133", row["pane_id"])
        self.assertTrue(row["online"])
        self.assertTrue(row["can_message"], "this was False: every pane's agent string is empty")
        self.assertEqual("active", row["activity_group"])

    def test_the_cwd_rule_matches_the_desk_directory(self):
        self.assertEqual("%133", match_pane(COS, [SHELL, DESK])["pane_id"])

    def test_a_missing_desk_stays_offline_and_unmessageable(self):
        row = next(r for r in build_roster([COS], [SHELL]) if r["id"] == "cos")
        self.assertFalse(row["online"])
        self.assertFalse(row["can_message"])


if __name__ == "__main__":
    unittest.main()
