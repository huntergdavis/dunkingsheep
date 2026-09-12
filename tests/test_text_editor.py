import unittest

from text_editor import TextEditorBuffer


class TextEditorBufferTests(unittest.TestCase):
    def test_insert_only_changes_current_logical_line(self):
        editor = TextEditorBuffer("alpha\nbeta\ngamma")
        editor.set_cursor(0, 2)

        editor.insert("   ")

        self.assertEqual(["al   pha", "beta", "gamma"], editor.lines)
        self.assertEqual("al   pha\nbeta\ngamma", editor.text)

    def test_long_lines_are_not_truncated_or_reflowed(self):
        first = "x" * 5000
        editor = TextEditorBuffer(first + "\nsecond")
        editor.set_cursor(0, 2500)

        editor.insert(" ")

        self.assertEqual(5001, len(editor.lines[0]))
        self.assertEqual("second", editor.lines[1])
        self.assertEqual(first[:2500] + " " + first[2500:] + "\nsecond", editor.text)

    def test_initial_crlf_is_normalized_to_logical_lines(self):
        editor = TextEditorBuffer("first\r\nsecond\rthird")

        self.assertEqual(["first", "second", "third"], editor.lines)
        self.assertEqual("first\nsecond\nthird", editor.text)

    def test_newline_splits_and_backspace_rejoins_lines(self):
        editor = TextEditorBuffer("firstsecond")
        editor.set_cursor(0, 5)

        editor.newline()
        self.assertEqual(["first", "second"], editor.lines)
        editor.backspace()

        self.assertEqual(["firstsecond"], editor.lines)
        self.assertEqual((0, 5), (editor.row, editor.col))

    def test_delete_at_end_joins_the_next_line(self):
        editor = TextEditorBuffer("first\nsecond")
        editor.set_cursor(0, 5)

        editor.delete()

        self.assertEqual("firstsecond", editor.text)
        self.assertEqual((0, 5), (editor.row, editor.col))

    def test_vertical_motion_remembers_preferred_column(self):
        editor = TextEditorBuffer("long line\nx\nlong line")
        editor.set_cursor(0, 7)

        editor.move_down()
        self.assertEqual((1, 1), (editor.row, editor.col))
        editor.move_down()

        self.assertEqual((2, 7), (editor.row, editor.col))


if __name__ == "__main__":
    unittest.main()
