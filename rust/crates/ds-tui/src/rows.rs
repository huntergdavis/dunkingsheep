//! Pure row formatting from `dunking_sheep_tui.py` (`clip`, `format_row`,
//! `format_task_row`, `_format_target_row`). No Python test covers these; the tests below
//! pin the Python's arithmetic by hand. Widths count characters, as Python's `len` does.

fn len(text: &str) -> usize {
    text.chars().count()
}

fn take(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// `clip(value, width)`: truncate with "..." when width allows.
pub fn clip(value: &str, width: i64) -> String {
    if width <= 0 {
        return String::new();
    }
    let width = width as usize;
    if len(value) <= width {
        return value.to_string();
    }
    if width <= 3 {
        return take(value, width);
    }
    format!("{}...", take(value, width - 3))
}

/// Python `str.ljust`: pads, never truncates.
pub fn ljust(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(len(text))))
}

/// Python `str.rjust`.
pub fn rjust(text: &str, width: usize) -> String {
    format!("{}{text}", " ".repeat(width.saturating_sub(len(text))))
}

#[allow(clippy::too_many_arguments)]
pub fn format_row(
    num: &str,
    status: &str,
    window: &str,
    minutes: &str,
    sent: &str,
    gate: &str,
    text: &str,
    width: i64,
) -> String {
    let columns = [
        ljust(&clip(num, 4), 4),
        ljust(&clip(status, 20), 20),
        ljust(&clip(window, 26), 26),
        rjust(&clip(minutes, 6), 6),
        rjust(&clip(sent, 6), 6),
        ljust(&clip(gate, 9), 9),
        clip(text, (width - 77).max(10)),
    ];
    let row = clip(&columns.join(" "), width - 1);
    ljust(&row, (width - 1).max(0) as usize)
}

pub fn format_task_row(
    num: &str,
    status: &str,
    priority: &str,
    scope: &str,
    assignees: &str,
    title: &str,
    width: i64,
) -> String {
    let columns = [
        ljust(&clip(num, 5), 5),
        ljust(&clip(status, 11), 11),
        ljust(&clip(priority, 6), 6),
        ljust(&clip(scope, 10), 10),
        ljust(&clip(assignees, 18), 18),
        clip(title, (width - 55).max(10)),
    ];
    ljust(
        &clip(&columns.join(" "), width - 1),
        (width - 1).max(0) as usize,
    )
}

pub fn format_target_row(
    tab: &str,
    agent: &str,
    status: &str,
    cwd: &str,
    width: i64,
    indent: i64,
) -> String {
    let avail = width - 4 - indent;
    // fixed cols + 3 separators: 20 + 1 + 10 + 1 + 9 + 1 = 42
    let dir_w = (avail - 42).max(10);
    let columns = [
        ljust(&clip(tab, 20), 20),
        ljust(&clip(agent, 10), 10),
        ljust(&clip(status, 9), 9),
        ljust(&clip(cwd, dir_w), dir_w as usize),
    ];
    clip(&columns.join(" "), avail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_follows_the_pythons_width_rules() {
        assert_eq!(clip("abc", 0), "");
        assert_eq!(clip("abc", -2), "");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcdef", 3), "abc");
        assert_eq!(clip("abcdef", 4), "a...");
        assert_eq!(clip("héllo wörld", 8), "héllo...");
    }

    #[test]
    fn format_row_is_exactly_width_minus_one_wide() {
        let row = format_row("1", "running", "w1:p2", "5", "3", "ok", "continue", 100);
        assert_eq!(len(&row), 99);
        assert!(row.starts_with("1    running"));
        assert!(row.trim_end().ends_with("continue"));
        // minutes and sent are right-aligned in 6 columns
        assert!(row.contains("     5      3 ok"));
    }

    #[test]
    fn format_row_on_a_narrow_screen_clips_instead_of_overflowing() {
        let row = format_row("1", "running", "w1:p2", "5", "3", "ok", "continue", 40);
        assert_eq!(len(&row), 39);
        let tiny = format_row("1", "s", "w", "5", "3", "ok", "t", 0);
        assert_eq!(tiny, "");
    }

    #[test]
    fn format_task_row_pads_to_width_minus_one() {
        let row = format_task_row("t7", "open", "high", "global", "%1", "Do it", 80);
        assert_eq!(len(&row), 79);
        assert!(row.starts_with("t7    open        high   global     %1"));
    }

    #[test]
    fn format_target_row_keeps_the_42_column_floor() {
        let row = format_target_row("tab", "claude", "idle", "/home/x", 100, 0);
        assert_eq!(len(&row), 96);
        assert!(row.starts_with("tab                  claude     idle      /home/x"));
        // dir column never shrinks below 10, so a narrow width clips the whole row
        let narrow = format_target_row("tab", "claude", "idle", "/home/x", 50, 2);
        assert_eq!(len(&narrow), 44);
    }
}
