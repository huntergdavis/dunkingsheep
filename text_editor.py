"""Logical-line editing state for Dunking Sheep's curses text editor.

The standard-library ``curses.textpad.Textbox`` edits the curses window as one
flat character grid.  In insert mode, a character displaced from the end of a
row is carried into the next row, which can shift every line below it.  This
buffer keeps text as logical lines instead; screen width only affects scrolling
and never changes the stored text.
"""


class TextEditorBuffer:
    """Small, UI-independent multiline text buffer with a movable cursor."""

    def __init__(self, text=""):
        text = str(text).replace("\r\n", "\n").replace("\r", "\n")
        self.lines = text.split("\n")
        if not self.lines:
            self.lines = [""]
        self.row = len(self.lines) - 1
        self.col = len(self.lines[self.row])
        self.preferred_col = self.col

    @property
    def text(self):
        return "\n".join(self.lines)

    def set_cursor(self, row, col):
        self.row = max(0, min(int(row), len(self.lines) - 1))
        self.col = max(0, min(int(col), len(self.lines[self.row])))
        self.preferred_col = self.col

    def insert(self, value):
        """Insert text at the cursor, preserving logical newline boundaries."""
        value = str(value).replace("\r\n", "\n").replace("\r", "\n")
        for char in value:
            if char == "\n":
                self.newline()
            else:
                line = self.lines[self.row]
                self.lines[self.row] = line[:self.col] + char + line[self.col:]
                self.col += 1
                self.preferred_col = self.col

    def newline(self):
        line = self.lines[self.row]
        self.lines[self.row] = line[:self.col]
        self.lines.insert(self.row + 1, line[self.col:])
        self.row += 1
        self.col = 0
        self.preferred_col = 0

    def backspace(self):
        if self.col > 0:
            line = self.lines[self.row]
            self.lines[self.row] = line[:self.col - 1] + line[self.col:]
            self.col -= 1
        elif self.row > 0:
            previous_length = len(self.lines[self.row - 1])
            self.lines[self.row - 1] += self.lines[self.row]
            del self.lines[self.row]
            self.row -= 1
            self.col = previous_length
        self.preferred_col = self.col

    def delete(self):
        line = self.lines[self.row]
        if self.col < len(line):
            self.lines[self.row] = line[:self.col] + line[self.col + 1:]
        elif self.row + 1 < len(self.lines):
            self.lines[self.row] += self.lines[self.row + 1]
            del self.lines[self.row + 1]
        self.preferred_col = self.col

    def move_left(self):
        if self.col > 0:
            self.col -= 1
        elif self.row > 0:
            self.row -= 1
            self.col = len(self.lines[self.row])
        self.preferred_col = self.col

    def move_right(self):
        if self.col < len(self.lines[self.row]):
            self.col += 1
        elif self.row + 1 < len(self.lines):
            self.row += 1
            self.col = 0
        self.preferred_col = self.col

    def move_up(self, amount=1):
        self.row = max(0, self.row - max(1, int(amount)))
        self.col = min(self.preferred_col, len(self.lines[self.row]))

    def move_down(self, amount=1):
        self.row = min(len(self.lines) - 1, self.row + max(1, int(amount)))
        self.col = min(self.preferred_col, len(self.lines[self.row]))

    def move_home(self):
        self.col = 0
        self.preferred_col = 0

    def move_end(self):
        self.col = len(self.lines[self.row])
        self.preferred_col = self.col
