//! Operator execution — stage 5 of the Vim pipeline.
//!
//! Everything here works on rope slices and bounded line ranges. Nothing calls
//! `buffer.to_string()`, and none of it runs per-frame; edits are O(edit size),
//! not O(file). See `docs/VIM_MODE_DESIGN.md` §2.5 and §7.

use super::super::buffer::TextBuffer;
use super::super::cursor::Cursor;
use super::command::Span;
use super::motion::{line_len, line_text};
use super::registers::RegisterValue;

/// How many spaces `>>` and `<<` shift by, until `:set shiftwidth` changes it.
pub const DEFAULT_SHIFT_WIDTH: usize = 4;

/// A character range to operate on, already normalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditRange {
    /// Inclusive start, in character positions.
    pub start: usize,
    /// Exclusive end, in character positions.
    pub end: usize,
    /// Whether this covers whole lines.
    pub linewise: bool,
    /// First line touched (used to place the cursor afterwards).
    pub start_line: usize,
    /// Last line touched.
    pub end_line: usize,
}

impl EditRange {
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// Character position of a cursor, clamped to the buffer.
pub fn char_pos(buffer: &TextBuffer, c: Cursor) -> usize {
    let line = c.line.min(buffer.line_count().saturating_sub(1));
    let start = buffer.try_line_to_char(line).unwrap_or(buffer.len());
    (start + c.column).min(buffer.len())
}

fn order(a: Cursor, b: Cursor) -> (Cursor, Cursor) {
    if (a.line, a.column) <= (b.line, b.column) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Builds the range an operator acts on, given two positions and a span.
///
/// The span is what makes `dw` differ from `de`, and `dj` delete two whole lines
/// rather than a character range.
pub fn edit_range(buffer: &TextBuffer, a: Cursor, b: Cursor, span: Span) -> EditRange {
    let (from, to) = order(a, b);
    let last = buffer.line_count().saturating_sub(1);

    match span {
        Span::Linewise => {
            let start_line = from.line.min(last);
            let end_line = to.line.min(last);
            let start_char = buffer.try_line_to_char(start_line).unwrap_or(0);
            let (start, end) = if end_line < last {
                (
                    start_char,
                    buffer
                        .try_line_to_char(end_line + 1)
                        .unwrap_or(buffer.len()),
                )
            } else if start_line > 0 {
                // Deleting through the end of the buffer: take the newline
                // *before* the range so no blank line is left behind.
                (start_char.saturating_sub(1), buffer.len())
            } else {
                (start_char, buffer.len())
            };
            EditRange {
                start,
                end,
                linewise: true,
                start_line,
                end_line,
            }
        }
        Span::Exclusive => EditRange {
            start: char_pos(buffer, from),
            end: char_pos(buffer, to),
            linewise: false,
            start_line: from.line,
            end_line: to.line,
        },
        Span::Inclusive => {
            let start = char_pos(buffer, from);
            let end = (char_pos(buffer, to) + 1).min(buffer.len());
            EditRange {
                start,
                end,
                linewise: false,
                start_line: from.line,
                end_line: to.line,
            }
        }
    }
}

/// The text a range covers. For linewise ranges the trailing newline is trimmed:
/// the `linewise` flag on the register value carries that information instead.
pub fn range_text(buffer: &TextBuffer, r: &EditRange) -> String {
    if r.is_empty() {
        return String::new();
    }
    let text = buffer.slice(r.start, r.end);
    if r.linewise {
        text.trim_end_matches('\n').to_string()
    } else {
        text
    }
}

/// The register value a range yields.
pub fn range_value(buffer: &TextBuffer, r: &EditRange) -> RegisterValue {
    RegisterValue::new(range_text(buffer, r), r.linewise)
}

/// Deletes a range and returns where the cursor lands.
pub fn delete_range(buffer: &mut TextBuffer, r: &EditRange) -> Cursor {
    if r.is_empty() {
        return Cursor::new(r.start_line, 0);
    }
    buffer.remove(r.start, r.len());

    if r.linewise {
        let last = buffer.line_count().saturating_sub(1);
        let line = r.start_line.min(last);
        // Vim leaves the cursor on the first non-blank of the following line.
        let text = line_text(buffer, line);
        let col = text
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or_else(|| text.chars().count());
        Cursor::new(line, col)
    } else {
        let line = buffer.char_to_line(r.start.min(buffer.len()));
        let line_start = buffer.try_line_to_char(line).unwrap_or(0);
        Cursor::new(line, r.start.saturating_sub(line_start))
    }
}

/// Applies `p`/`P` and returns the new cursor position.
pub fn put(
    buffer: &mut TextBuffer,
    cursor: Cursor,
    value: &RegisterValue,
    before: bool,
    count: usize,
) -> Cursor {
    if value.is_empty() {
        return cursor;
    }
    let repeated = value.text.repeat(count.max(1));

    if value.linewise {
        let last = buffer.line_count().saturating_sub(1);
        let line = cursor.line.min(last);
        if before {
            let pos = buffer.try_line_to_char(line).unwrap_or(0);
            buffer.insert(pos, &format!("{repeated}\n"));
            Cursor::new(line, 0)
        } else if line >= last {
            // Appending past the final line: add the newline first.
            let pos = buffer.len();
            buffer.insert(pos, &format!("\n{repeated}"));
            Cursor::new(line + 1, 0)
        } else {
            let pos = buffer.try_line_to_char(line + 1).unwrap_or(buffer.len());
            buffer.insert(pos, &format!("{repeated}\n"));
            Cursor::new(line + 1, 0)
        }
    } else {
        let len = line_len(buffer, cursor.line);
        let col = if before {
            cursor.column
        } else {
            (cursor.column + 1).min(len)
        };
        let pos = char_pos(buffer, Cursor::new(cursor.line, col));
        buffer.insert(pos, &repeated);
        let added = repeated.chars().count();
        Cursor::new(cursor.line, col + added.saturating_sub(1))
    }
}

/// Indents or dedents whole lines. Returns the cursor position afterwards.
pub fn shift_lines(
    buffer: &mut TextBuffer,
    start_line: usize,
    end_line: usize,
    dedent: bool,
    width: usize,
) -> Cursor {
    let last = buffer.line_count().saturating_sub(1);
    let end_line = end_line.min(last);
    let pad: String = " ".repeat(width);

    // Walk backwards so earlier edits do not shift later line offsets.
    for line in (start_line..=end_line).rev() {
        let text = line_text(buffer, line);
        let line_start = buffer.try_line_to_char(line).unwrap_or(0);

        if dedent {
            let removable = text
                .chars()
                .take(width)
                .take_while(|c| *c == ' ' || *c == '\t')
                .count();
            if removable > 0 {
                buffer.remove(line_start, removable);
            }
        } else if !text.trim().is_empty() {
            // Vim leaves blank lines alone when indenting.
            buffer.insert(line_start, &pad);
        }
    }

    let text = line_text(
        buffer,
        start_line.min(buffer.line_count().saturating_sub(1)),
    );
    let col = text
        .chars()
        .position(|c| !c.is_whitespace())
        .unwrap_or_else(|| text.chars().count());
    Cursor::new(start_line, col)
}

/// How a case operator transforms text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseChange {
    Lower,
    Upper,
    Toggle,
}

/// Applies a case change over a range.
pub fn change_case(buffer: &mut TextBuffer, r: &EditRange, how: CaseChange) {
    if r.is_empty() {
        return;
    }
    let text = buffer.slice(r.start, r.end);
    let mapped: String = text
        .chars()
        .map(|c| match how {
            CaseChange::Lower => c.to_lowercase().next().unwrap_or(c),
            CaseChange::Upper => c.to_uppercase().next().unwrap_or(c),
            CaseChange::Toggle => {
                if c.is_uppercase() {
                    c.to_lowercase().next().unwrap_or(c)
                } else if c.is_lowercase() {
                    c.to_uppercase().next().unwrap_or(c)
                } else {
                    c
                }
            }
        })
        .collect();

    if mapped != text {
        buffer.remove(r.start, r.len());
        buffer.insert(r.start, &mapped);
    }
}

/// `J`: joins `count` lines onto the current one, collapsing whitespace to a
/// single space as Vim does. Returns the cursor position at the join.
pub fn join_lines(buffer: &mut TextBuffer, line: usize, count: usize) -> Cursor {
    let joins = count.max(2) - 1;
    let mut col = line_len(buffer, line);

    for _ in 0..joins {
        let last = buffer.line_count().saturating_sub(1);
        if line >= last {
            break;
        }
        let this_len = line_len(buffer, line);
        let line_start = buffer.try_line_to_char(line).unwrap_or(0);
        let eol = line_start + this_len;
        let next_start = buffer.try_line_to_char(line + 1).unwrap_or(buffer.len());
        let next_text = line_text(buffer, line + 1);
        let lead = next_text.chars().take_while(|c| c.is_whitespace()).count();

        // Remove the newline plus the next line's indent.
        let remove_len = (next_start + lead).saturating_sub(eol);
        if remove_len > 0 {
            buffer.remove(eol, remove_len);
        }

        // Vim inserts one space unless the line already ends in whitespace or the
        // next line starts with `)`.
        let needs_space = this_len > 0
            && !line_text(buffer, line)
                .chars()
                .nth(this_len.saturating_sub(1))
                .map(|c| c.is_whitespace())
                .unwrap_or(false)
            && !next_text.trim_start().starts_with(')')
            && !next_text.trim().is_empty();

        if needs_space {
            buffer.insert(eol, " ");
        }
        col = this_len;
    }

    Cursor::new(line, col)
}

/// `r{char}`: replaces `count` characters under the cursor.
pub fn replace_chars(
    buffer: &mut TextBuffer,
    cursor: Cursor,
    ch: char,
    count: usize,
) -> Option<Cursor> {
    let len = line_len(buffer, cursor.line);
    if cursor.column + count > len {
        return None; // Vim refuses rather than replacing past the line end
    }
    let start = char_pos(buffer, cursor);
    buffer.remove(start, count);
    let replacement: String = std::iter::repeat_n(ch, count).collect();
    buffer.insert(start, &replacement);
    Some(Cursor::new(cursor.line, cursor.column + count - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> TextBuffer {
        TextBuffer::from_string(s)
    }

    #[test]
    fn exclusive_and_inclusive_ranges_differ_by_one_character() {
        let b = buf("hello");
        let excl = edit_range(&b, Cursor::new(0, 0), Cursor::new(0, 4), Span::Exclusive);
        let incl = edit_range(&b, Cursor::new(0, 0), Cursor::new(0, 4), Span::Inclusive);
        assert_eq!(range_text(&b, &excl), "hell", "dw-style");
        assert_eq!(range_text(&b, &incl), "hello", "de-style");
    }

    #[test]
    fn linewise_range_covers_whole_lines_with_the_newline() {
        let b = buf("a\nb\nc");
        let r = edit_range(&b, Cursor::new(0, 0), Cursor::new(1, 0), Span::Linewise);
        assert!(r.linewise);
        assert_eq!(
            range_text(&b, &r),
            "a\nb",
            "trailing newline trimmed for the register"
        );
    }

    #[test]
    fn deleting_the_last_line_leaves_no_blank_line() {
        let mut b = buf("a\nb");
        let r = edit_range(&b, Cursor::new(1, 0), Cursor::new(1, 0), Span::Linewise);
        delete_range(&mut b, &r);
        assert_eq!(b.to_string(), "a", "the preceding newline goes too");
    }

    #[test]
    fn deleting_a_middle_line_keeps_the_rest_intact() {
        let mut b = buf("a\nb\nc");
        let r = edit_range(&b, Cursor::new(1, 0), Cursor::new(1, 0), Span::Linewise);
        let cursor = delete_range(&mut b, &r);
        assert_eq!(b.to_string(), "a\nc");
        assert_eq!(cursor.line, 1);
    }

    #[test]
    fn linewise_put_opens_a_new_line() {
        let mut b = buf("a\nc");
        let v = RegisterValue::new("b", true);
        let cursor = put(&mut b, Cursor::new(0, 0), &v, false, 1);
        assert_eq!(b.to_string(), "a\nb\nc");
        assert_eq!((cursor.line, cursor.column), (1, 0));
    }

    #[test]
    fn linewise_put_after_the_final_line_appends() {
        let mut b = buf("a");
        let v = RegisterValue::new("b", true);
        put(&mut b, Cursor::new(0, 0), &v, false, 1);
        assert_eq!(b.to_string(), "a\nb");
    }

    #[test]
    fn charwise_put_inserts_after_the_cursor() {
        let mut b = buf("ac");
        let v = RegisterValue::new("b", false);
        put(&mut b, Cursor::new(0, 0), &v, false, 1);
        assert_eq!(b.to_string(), "abc");
    }

    #[test]
    fn charwise_put_before_inserts_at_the_cursor() {
        let mut b = buf("bc");
        let v = RegisterValue::new("a", false);
        put(&mut b, Cursor::new(0, 0), &v, true, 1);
        assert_eq!(b.to_string(), "abc");
    }

    #[test]
    fn put_honours_a_count() {
        let mut b = buf("x");
        let v = RegisterValue::new("-", false);
        put(&mut b, Cursor::new(0, 0), &v, false, 3);
        assert_eq!(b.to_string(), "x---");
    }

    #[test]
    fn indent_and_dedent_shift_by_the_shift_width() {
        let mut b = buf("a\nb");
        shift_lines(&mut b, 0, 1, false, 4);
        assert_eq!(b.to_string(), "    a\n    b");
        shift_lines(&mut b, 0, 1, true, 4);
        assert_eq!(b.to_string(), "a\nb");
    }

    #[test]
    fn indent_leaves_blank_lines_alone() {
        let mut b = buf("a\n\nb");
        shift_lines(&mut b, 0, 2, false, 2);
        assert_eq!(b.to_string(), "  a\n\n  b");
    }

    #[test]
    fn dedent_stops_at_the_existing_indent() {
        let mut b = buf("  a");
        shift_lines(&mut b, 0, 0, true, 4);
        assert_eq!(b.to_string(), "a", "removes only what is there");
    }

    #[test]
    fn case_operators_transform_the_range() {
        let mut b = buf("aBc");
        let r = edit_range(&b, Cursor::new(0, 0), Cursor::new(0, 2), Span::Inclusive);
        change_case(&mut b, &r, CaseChange::Upper);
        assert_eq!(b.to_string(), "ABC");
        change_case(&mut b, &r, CaseChange::Lower);
        assert_eq!(b.to_string(), "abc");
        change_case(&mut b, &r, CaseChange::Toggle);
        assert_eq!(b.to_string(), "ABC");
    }

    #[test]
    fn join_collapses_the_next_lines_indent_to_one_space() {
        let mut b = buf("foo\n    bar");
        join_lines(&mut b, 0, 2);
        assert_eq!(b.to_string(), "foo bar");
    }

    #[test]
    fn join_with_a_count_joins_several_lines() {
        let mut b = buf("a\nb\nc");
        join_lines(&mut b, 0, 3);
        assert_eq!(b.to_string(), "a b c");
    }

    #[test]
    fn join_adds_no_space_before_a_closing_paren() {
        let mut b = buf("f(\n)");
        join_lines(&mut b, 0, 2);
        assert_eq!(b.to_string(), "f()");
    }

    #[test]
    fn replace_char_refuses_past_the_line_end() {
        let mut b = buf("ab");
        assert!(replace_chars(&mut b, Cursor::new(0, 1), 'x', 5).is_none());
        assert_eq!(b.to_string(), "ab", "buffer untouched on refusal");
        let c = replace_chars(&mut b, Cursor::new(0, 0), 'z', 2).unwrap();
        assert_eq!(b.to_string(), "zz");
        assert_eq!(c.column, 1);
    }
}
