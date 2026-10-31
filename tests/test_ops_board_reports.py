import os, sys, unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "bridges"))
import ops_board


class Client:
    def __init__(self, fail=False):
        self.fail = fail

    def call(self, name, **kwargs):
        if self.fail:
            raise OSError("down")
        return {"report_list": {"reports": [{"name": f"{i}.md", "size": 1, "at": 1.0} for i in range(7)], "count": 7},
                "poolrad_progress": {"credited": 5, "identities": 9, "ceiling": 815}}[name]


class OpsBoardReports(unittest.TestCase):
    def test_reports_last_five_newest_open(self):
        board = ops_board._report_board(Client(), ops_board._now())
        self.assertEqual(len(board["items"]), 5)
        self.assertEqual([i["open"] for i in board["items"]], [True, False, False, False, False])

    def test_poolrad_says_ceiling(self):
        board = ops_board._poolrad_board(Client())
        self.assertEqual(board["ceiling"], 815)
        self.assertIn("CEILING", board["ceiling_note"])

    def test_unavailable_is_reported(self):
        self.assertEqual(ops_board._poolrad_board(Client(True))["state"], "unavailable")
        self.assertEqual(ops_board._report_board(Client(True), ops_board._now())["state"], "unavailable")


if __name__ == "__main__":
    unittest.main()
