//! Motion and text-object resolution — stage 4 of the Vim pipeline.
//!
//! Motions resolve to a *target position plus a [`Span`]*, rather than moving the
//! cursor themselves. One implementation then serves three uses: moving the
//! cursor in Normal mode, extending the selection in Visual mode, and defining
//! the range an operator acts on.
//!
//! Complexity, per `docs/technical/editor/architecture.md`:
//! - character and word motions are O(log N) (one rope line lookup, one line scan);
//! - `%` and paragraph motions are O(window), capped by [`MAX_SCAN_LINES`];
//! - nothing here allocates the whole buffer, and none of it runs per-frame.

use super::super::buffer::TextBuffer;
use super::super::cursor::Cursor;
use super::super::grapheme;
use super::super::view::ViewState;
use super::command::{Motion, ObjectKind, Span, TextObject};

/// How far `%` and the paragraph motions will scan. Keeps them O(window)
/// rather than O(file).
pub const MAX_SCAN_LINES: usize = 200;

/// A resolved motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionResult {
    pub target: Cursor,
    pub span: Span,
}

/// The last `f`/`t`/`F`/`T` used, so `;` and `,` can repeat it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindSpec {
    pub ch: char,
    pub forward: bool,
    pub till: bool,
}

/// Read-only context a motion needs.
pub struct MotionCtx<'a> {
    pub buffer: &'a TextBuffer,
    pub cursor: Cursor,
    pub view: &'a ViewState,
    pub last_find: Option<FindSpec>,
    /// Lines in the viewport, for `H`/`M`/`L` and the page motions.
    pub visible_lines: usize,
}

/// Character classes for word motions. Vim treats a run of word characters, a
/// run of punctuation, and whitespace as three different things — `w` stops at
/// the boundary between any two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

fn class_of(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Blank
    } else if big {
        // For `W`/`B`/`E` everything non-blank is one class.
        Class::Word
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// The text of a line without its newline.
pub fn line_text(buffer: &TextBuffer, line: usize) -> String {
    buffer
        .get_line(line)
        .map(|l| l.trim_end_matches(['\r', '\n']).to_string())
        .unwrap_or_default()
}

/// Length of a line in characters, excluding the newline.
pub fn line_len(buffer: &TextBuffer, line: usize) -> usize {
    buffer
        .get_line(line)
        .map(|l| l.trim_end_matches(['\r', '\n']).chars().count())
        .unwrap_or(0)
}

fn last_line(buffer: &TextBuffer) -> usize {
    buffer.line_count().saturating_sub(1)
}

/// Clamps a column to the line, allowing one-past-the-end (needed by `$` as an
/// operator target and by Insert mode).
fn clamp_col(buffer: &TextBuffer, cursor: &mut Cursor) {
    let len = line_len(buffer, cursor.line);
    if cursor.column > len {
        cursor.column = len;
    }
}

/// Column of the first non-blank character on a line.
fn first_non_blank(buffer: &TextBuffer, line: usize) -> usize {
    let text = line_text(buffer, line);
    text.chars()
        .position(|c| !c.is_whitespace())
        .unwrap_or_else(|| text.chars().count())
}

/// Column of the last non-blank character on a line.
fn last_non_blank(buffer: &TextBuffer, line: usize) -> usize {
    let text = line_text(buffer, line);
    let chars: Vec<char> = text.chars().collect();
    let mut i = chars.len();
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i.saturating_sub(1)
}

/// One grapheme cluster right. Grapheme-aware so combining marks, emoji ZWJ
/// sequences and Hangul are not split.
fn right_one(buffer: &TextBuffer, cursor: Cursor) -> Cursor {
    let text = line_text(buffer, cursor.line);
    let col = grapheme::next_grapheme_boundary(&text, cursor.column);
    Cursor::new(cursor.line, col)
}

/// One grapheme cluster left.
fn left_one(buffer: &TextBuffer, cursor: Cursor) -> Cursor {
    let text = line_text(buffer, cursor.line);
    let col = grapheme::prev_grapheme_boundary(&text, cursor.column);
    Cursor::new(cursor.line, col)
}

/// Advances one character position, wrapping to the next line. Used by the word
/// motions, which cross line boundaries.
fn advance(buffer: &TextBuffer, c: Cursor) -> Option<Cursor> {
    let len = line_len(buffer, c.line);
    if c.column < len {
        Some(right_one(buffer, c))
    } else if c.line < last_line(buffer) {
        Some(Cursor::new(c.line + 1, 0))
    } else {
        None
    }
}

/// Retreats one character position, wrapping to the previous line.
fn retreat(buffer: &TextBuffer, c: Cursor) -> Option<Cursor> {
    if c.column > 0 {
        Some(left_one(buffer, c))
    } else if c.line > 0 {
        let prev = c.line - 1;
        Some(Cursor::new(prev, line_len(buffer, prev)))
    } else {
        None
    }
}

/// The character at a position, if any.
fn char_at(buffer: &TextBuffer, c: Cursor) -> Option<char> {
    line_text(buffer, c.line).chars().nth(c.column)
}

/// `w` / `W`: to the start of the next word.
fn word_forward(buffer: &TextBuffer, mut c: Cursor, big: bool) -> Cursor {
    let start_class = char_at(buffer, c).map(|ch| class_of(ch, big));

    // Step off the current run.
    if let Some(cls) = start_class {
        if cls != Class::Blank {
            while let Some(next) = advance(buffer, c) {
                match char_at(buffer, next).map(|ch| class_of(ch, big)) {
                    Some(k) if k == cls => c = next,
                    _ => {
                        c = next;
                        break;
                    }
                }
            }
        }
    }

    // Skip blanks to land on the next word. An empty line is a word in Vim.
    loop {
        match char_at(buffer, c) {
            Some(ch) if class_of(ch, big) == Class::Blank => match advance(buffer, c) {
                Some(next) => c = next,
                None => break,
            },
            Some(_) => break,
            None => {
                if line_len(buffer, c.line) == 0 && c.column == 0 {
                    break; // empty line counts as a word start
                }
                match advance(buffer, c) {
                    Some(next) => c = next,
                    None => break,
                }
            }
        }
    }
    c
}

/// `b` / `B`: to the start of the previous word.
fn word_back(buffer: &TextBuffer, mut c: Cursor, big: bool) -> Cursor {
    // Step back at least one position.
    match retreat(buffer, c) {
        Some(prev) => c = prev,
        None => return c,
    }

    // Skip blanks backwards.
    while char_at(buffer, c).map(|ch| class_of(ch, big)) == Some(Class::Blank)
        || char_at(buffer, c).is_none()
    {
        if line_len(buffer, c.line) == 0 && c.column == 0 {
            return c;
        }
        match retreat(buffer, c) {
            Some(prev) => c = prev,
            None => return c,
        }
    }

    // Walk to the start of this run.
    let cls = char_at(buffer, c).map(|ch| class_of(ch, big));
    while let Some(prev) = retreat(buffer, c) {
        if char_at(buffer, prev).map(|ch| class_of(ch, big)) == cls {
            c = prev;
        } else {
            break;
        }
    }
    c
}

/// `e` / `E`: to the end of the current or next word (inclusive motion).
fn word_end(buffer: &TextBuffer, mut c: Cursor, big: bool) -> Cursor {
    match advance(buffer, c) {
        Some(next) => c = next,
        None => return c,
    }

    // Skip blanks.
    while char_at(buffer, c).map(|ch| class_of(ch, big)) == Some(Class::Blank)
        || char_at(buffer, c).is_none()
    {
        match advance(buffer, c) {
            Some(next) => c = next,
            None => return c,
        }
    }

    // Walk to the end of this run.
    let cls = char_at(buffer, c).map(|ch| class_of(ch, big));
    while let Some(next) = advance(buffer, c) {
        if char_at(buffer, next).map(|ch| class_of(ch, big)) == cls {
            c = next;
        } else {
            break;
        }
    }
    c
}

/// `ge` / `gE`: back to the end of the previous word.
fn word_end_back(buffer: &TextBuffer, mut c: Cursor, big: bool) -> Cursor {
    match retreat(buffer, c) {
        Some(prev) => c = prev,
        None => return c,
    }
    while char_at(buffer, c).map(|ch| class_of(ch, big)) == Some(Class::Blank)
        || char_at(buffer, c).is_none()
    {
        match retreat(buffer, c) {
            Some(prev) => c = prev,
            None => return c,
        }
    }
    c
}

/// `f` / `F` / `t` / `T` within the current line.
fn find_char(buffer: &TextBuffer, c: Cursor, spec: FindSpec, count: usize) -> Option<Cursor> {
    let text = line_text(buffer, c.line);
    let chars: Vec<char> = text.chars().collect();
    let mut col = c.column;

    for _ in 0..count {
        if spec.forward {
            let mut i = col + 1;
            loop {
                if i >= chars.len() {
                    return None;
                }
                if chars[i] == spec.ch {
                    col = i;
                    break;
                }
                i += 1;
            }
        } else {
            let mut i = col;
            loop {
                if i == 0 {
                    return None;
                }
                i -= 1;
                if chars[i] == spec.ch {
                    col = i;
                    break;
                }
            }
        }
    }

    if spec.till {
        if spec.forward {
            col = col.saturating_sub(1);
        } else {
            col += 1;
        }
    }
    Some(Cursor::new(c.line, col))
}

/// `%`: the bracket matching the one at (or after) the cursor.
///
/// Scans at most [`MAX_SCAN_LINES`] lines either way, so this stays O(window).
fn match_pair(buffer: &TextBuffer, c: Cursor) -> Option<Cursor> {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

    let text = line_text(buffer, c.line);
    let chars: Vec<char> = text.chars().collect();

    // Find the first bracket at or after the cursor on this line.
    let mut col = c.column;
    let (open, close, forward) = loop {
        let ch = *chars.get(col)?;
        if let Some((o, cl)) = PAIRS.iter().find(|(o, _)| *o == ch) {
            break (*o, *cl, true);
        }
        if let Some((o, cl)) = PAIRS.iter().find(|(_, cl)| *cl == ch) {
            break (*o, *cl, false);
        }
        col += 1;
        if col >= chars.len() {
            return None;
        }
    };

    let mut depth = 0i32;
    let mut pos = Cursor::new(c.line, col);
    let line_limit = if forward {
        (c.line + MAX_SCAN_LINES).min(last_line(buffer))
    } else {
        c.line.saturating_sub(MAX_SCAN_LINES)
    };

    loop {
        let ch = char_at(buffer, pos);
        if let Some(ch) = ch {
            if ch == open {
                depth += if forward { 1 } else { -1 };
            } else if ch == close {
                depth += if forward { -1 } else { 1 };
            }
            if depth == 0 {
                return Some(pos);
            }
        }

        let next = if forward {
            advance(buffer, pos)
        } else {
            retreat(buffer, pos)
        };
        pos = next?;

        if (forward && pos.line > line_limit) || (!forward && pos.line < line_limit) {
            return None;
        }
    }
}

fn is_blank_line(buffer: &TextBuffer, line: usize) -> bool {
    line_text(buffer, line).trim().is_empty()
}

/// `{` / `}`: previous/next blank line, capped at [`MAX_SCAN_LINES`].
fn paragraph(buffer: &TextBuffer, c: Cursor, forward: bool, count: usize) -> Cursor {
    let mut line = c.line;
    let end = last_line(buffer);

    for _ in 0..count {
        let mut scanned = 0;
        if forward {
            // Step off a run of blanks, then find the next blank line.
            while line < end && is_blank_line(buffer, line) && scanned < MAX_SCAN_LINES {
                line += 1;
                scanned += 1;
            }
            while line < end && !is_blank_line(buffer, line) && scanned < MAX_SCAN_LINES {
                line += 1;
                scanned += 1;
            }
        } else {
            while line > 0 && is_blank_line(buffer, line) && scanned < MAX_SCAN_LINES {
                line -= 1;
                scanned += 1;
            }
            while line > 0 && !is_blank_line(buffer, line) && scanned < MAX_SCAN_LINES {
                line -= 1;
                scanned += 1;
            }
        }
    }
    Cursor::new(line, 0)
}

/// Resolves a motion to a target and a span.
///
/// Returns `None` when the motion cannot move (no match for `f`, no bracket for
/// `%`), which Vim treats as a failed command that performs no edit.
pub fn resolve(motion: Motion, count: usize, ctx: &MotionCtx) -> Option<MotionResult> {
    let buffer = ctx.buffer;
    let c = ctx.cursor;
    let end_line = last_line(buffer);

    let result = match motion {
        Motion::Left => {
            let mut cur = c;
            for _ in 0..count {
                if cur.column == 0 {
                    break;
                }
                cur = left_one(buffer, cur);
            }
            MotionResult {
                target: cur,
                span: Span::Exclusive,
            }
        }
        Motion::Right => {
            let mut cur = c;
            let len = line_len(buffer, c.line);
            for _ in 0..count {
                if cur.column >= len {
                    break;
                }
                cur = right_one(buffer, cur);
            }
            MotionResult {
                target: cur,
                span: Span::Exclusive,
            }
        }
        Motion::Up => {
            let mut cur = Cursor::new(c.line.saturating_sub(count), c.column);
            clamp_col(buffer, &mut cur);
            MotionResult {
                target: cur,
                span: Span::Linewise,
            }
        }
        Motion::Down => {
            let mut cur = Cursor::new((c.line + count).min(end_line), c.column);
            clamp_col(buffer, &mut cur);
            MotionResult {
                target: cur,
                span: Span::Linewise,
            }
        }
        Motion::WordFwd { big } => {
            let mut cur = c;
            for _ in 0..count {
                cur = word_forward(buffer, cur, big);
            }
            MotionResult {
                target: cur,
                span: Span::Exclusive,
            }
        }
        Motion::WordBack { big } => {
            let mut cur = c;
            for _ in 0..count {
                cur = word_back(buffer, cur, big);
            }
            MotionResult {
                target: cur,
                span: Span::Exclusive,
            }
        }
        Motion::WordEnd { big } => {
            let mut cur = c;
            for _ in 0..count {
                cur = word_end(buffer, cur, big);
            }
            MotionResult {
                target: cur,
                span: Span::Inclusive,
            }
        }
        Motion::WordEndBack { big } => {
            let mut cur = c;
            for _ in 0..count {
                cur = word_end_back(buffer, cur, big);
            }
            MotionResult {
                target: cur,
                span: Span::Inclusive,
            }
        }
        Motion::LineStart => MotionResult {
            target: Cursor::new(c.line, 0),
            span: Span::Exclusive,
        },
        Motion::FirstNonBlank => MotionResult {
            target: Cursor::new(c.line, first_non_blank(buffer, c.line)),
            span: Span::Exclusive,
        },
        Motion::LineEnd => {
            let line = (c.line + count - 1).min(end_line);
            MotionResult {
                target: Cursor::new(line, line_len(buffer, line)),
                span: Span::Exclusive,
            }
        }
        Motion::LastNonBlank => MotionResult {
            target: Cursor::new(c.line, last_non_blank(buffer, c.line)),
            span: Span::Inclusive,
        },
        Motion::GotoLine { first } => {
            // `gg`/`G` take an explicit line number when counted.
            let line = if count > 1 || (first && count >= 1) {
                if first && count == 1 {
                    0
                } else {
                    (count - 1).min(end_line)
                }
            } else if first {
                0
            } else {
                end_line
            };
            MotionResult {
                target: Cursor::new(line, first_non_blank(buffer, line)),
                span: Span::Linewise,
            }
        }
        Motion::FindChar { ch, forward, till } => {
            let spec = FindSpec { ch, forward, till };
            let target = find_char(buffer, c, spec, count)?;
            MotionResult {
                target,
                span: if forward {
                    Span::Inclusive
                } else {
                    Span::Exclusive
                },
            }
        }
        Motion::RepeatFind { reverse } => {
            let mut spec = ctx.last_find?;
            if reverse {
                spec.forward = !spec.forward;
            }
            let target = find_char(buffer, c, spec, count)?;
            MotionResult {
                target,
                span: if spec.forward {
                    Span::Inclusive
                } else {
                    Span::Exclusive
                },
            }
        }
        Motion::MatchPair => MotionResult {
            target: match_pair(buffer, c)?,
            span: Span::Inclusive,
        },
        Motion::Paragraph { forward } => MotionResult {
            target: paragraph(buffer, c, forward, count),
            span: Span::Exclusive,
        },
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
            let (top, bottom) = ctx.view.get_visible_line_range(buffer.line_count());
            let bottom = bottom.min(end_line);
            let line = match motion {
                Motion::ScreenTop => top,
                Motion::ScreenBottom => bottom,
                _ => top + (bottom.saturating_sub(top)) / 2,
            };
            MotionResult {
                target: Cursor::new(line, first_non_blank(buffer, line)),
                span: Span::Linewise,
            }
        }
        Motion::Scroll { down, half } => {
            let step = if half {
                (ctx.visible_lines / 2).max(1)
            } else {
                ctx.visible_lines.max(1)
            };
            let delta = step * count;
            let line = if down {
                (c.line + delta).min(end_line)
            } else {
                c.line.saturating_sub(delta)
            };
            let mut cur = Cursor::new(line, c.column);
            clamp_col(buffer, &mut cur);
            MotionResult {
                target: cur,
                span: Span::Linewise,
            }
        }
        Motion::LineFirstNonBlank { forward } => {
            let line = if forward {
                (c.line + count).min(end_line)
            } else {
                c.line.saturating_sub(count)
            };
            MotionResult {
                target: Cursor::new(line, first_non_blank(buffer, line)),
                span: Span::Linewise,
            }
        }
        // Search motions are resolved by the application, which owns the match
        // list; the executor turns these into effects instead.
        Motion::SearchNext { .. } => return None,
    };

    Some(result)
}

/// A resolved text object: an inclusive character range, or whole lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRange {
    pub start: Cursor,
    pub end: Cursor,
    pub span: Span,
}

/// Resolves `iw`, `a"`, `i(`, `ap` … to a range.
pub fn resolve_object(object: TextObject, ctx: &MotionCtx) -> Option<ObjectRange> {
    let buffer = ctx.buffer;
    let c = ctx.cursor;

    match object.kind {
        ObjectKind::Word { big } => {
            let text = line_text(buffer, c.line);
            let chars: Vec<char> = text.chars().collect();
            if chars.is_empty() {
                return None;
            }
            let col = c.column.min(chars.len() - 1);
            let cls = class_of(chars[col], big);

            let mut start = col;
            while start > 0 && class_of(chars[start - 1], big) == cls {
                start -= 1;
            }
            let mut end = col;
            while end + 1 < chars.len() && class_of(chars[end + 1], big) == cls {
                end += 1;
            }

            // `aw` also takes the trailing whitespace.
            if !object.inner {
                while end + 1 < chars.len() && chars[end + 1].is_whitespace() {
                    end += 1;
                }
            }

            Some(ObjectRange {
                start: Cursor::new(c.line, start),
                end: Cursor::new(c.line, end),
                span: Span::Inclusive,
            })
        }
        ObjectKind::Quote(q) => {
            let text = line_text(buffer, c.line);
            let chars: Vec<char> = text.chars().collect();
            // Quotes are line-local in Vim too.
            let positions: Vec<usize> = chars
                .iter()
                .enumerate()
                .filter(|(_, ch)| **ch == q)
                .map(|(i, _)| i)
                .collect();
            if positions.len() < 2 {
                return None;
            }
            // The pair surrounding (or following) the cursor.
            let mut open = None;
            for pair in positions.chunks(2) {
                if pair.len() == 2 && (c.column <= pair[1]) {
                    open = Some((pair[0], pair[1]));
                    break;
                }
            }
            let (o, cl) = open?;
            if object.inner {
                if cl == o + 1 {
                    return None; // empty quotes
                }
                Some(ObjectRange {
                    start: Cursor::new(c.line, o + 1),
                    end: Cursor::new(c.line, cl - 1),
                    span: Span::Inclusive,
                })
            } else {
                Some(ObjectRange {
                    start: Cursor::new(c.line, o),
                    end: Cursor::new(c.line, cl),
                    span: Span::Inclusive,
                })
            }
        }
        ObjectKind::Pair(open, close) => {
            let start = find_unmatched(buffer, c, open, close, false)?;
            let end = find_unmatched(buffer, c, open, close, true)?;
            if object.inner {
                let inner_start = advance(buffer, start)?;
                let inner_end = retreat(buffer, end)?;
                // Empty pair, e.g. `()`.
                if (inner_start.line, inner_start.column) > (inner_end.line, inner_end.column) {
                    return None;
                }
                Some(ObjectRange {
                    start: inner_start,
                    end: inner_end,
                    span: Span::Inclusive,
                })
            } else {
                Some(ObjectRange {
                    start,
                    end,
                    span: Span::Inclusive,
                })
            }
        }
        ObjectKind::Paragraph => {
            let mut start = c.line;
            let mut end = c.line;
            let mut scanned = 0;
            while start > 0 && !is_blank_line(buffer, start - 1) && scanned < MAX_SCAN_LINES {
                start -= 1;
                scanned += 1;
            }
            let last = last_line(buffer);
            scanned = 0;
            while end < last && !is_blank_line(buffer, end + 1) && scanned < MAX_SCAN_LINES {
                end += 1;
                scanned += 1;
            }
            if !object.inner {
                // `ap` also takes the blank lines after the paragraph.
                while end < last && is_blank_line(buffer, end + 1) && scanned < MAX_SCAN_LINES {
                    end += 1;
                    scanned += 1;
                }
            }
            Some(ObjectRange {
                start: Cursor::new(start, 0),
                end: Cursor::new(end, line_len(buffer, end)),
                span: Span::Linewise,
            })
        }
    }
}

/// Scans for the unmatched `open`/`close` bracket enclosing the cursor.
fn find_unmatched(
    buffer: &TextBuffer,
    from: Cursor,
    open: char,
    close: char,
    forward: bool,
) -> Option<Cursor> {
    let mut pos = from;
    let mut depth = 0i32;
    let limit = if forward {
        (from.line + MAX_SCAN_LINES).min(last_line(buffer))
    } else {
        from.line.saturating_sub(MAX_SCAN_LINES)
    };

    // A cursor sitting on the delimiter itself counts as enclosed.
    if let Some(ch) = char_at(buffer, pos) {
        if (forward && ch == close) || (!forward && ch == open) {
            return Some(pos);
        }
    }

    loop {
        pos = if forward {
            advance(buffer, pos)?
        } else {
            retreat(buffer, pos)?
        };

        if (forward && pos.line > limit) || (!forward && pos.line < limit) {
            return None;
        }

        if let Some(ch) = char_at(buffer, pos) {
            let (opening, closing) = if forward {
                (open, close)
            } else {
                (close, open)
            };
            if ch == opening {
                depth += 1;
            } else if ch == closing {
                if depth == 0 {
                    return Some(pos);
                }
                depth -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(
        buffer: &'a TextBuffer,
        view: &'a ViewState,
        line: usize,
        col: usize,
    ) -> MotionCtx<'a> {
        MotionCtx {
            buffer,
            cursor: Cursor::new(line, col),
            view,
            last_find: None,
            visible_lines: 10,
        }
    }

    fn at(text: &str, line: usize, col: usize, m: Motion, count: usize) -> (usize, usize, Span) {
        let buffer = TextBuffer::from_string(text);
        let view = ViewState::new();
        let c = ctx(&buffer, &view, line, col);
        let r = resolve(m, count, &c).expect("motion should resolve");
        (r.target.line, r.target.column, r.span)
    }

    #[test]
    fn word_forward_stops_at_class_boundaries() {
        // `foo.bar` — `w` from 0 lands on the `.`, not on `bar`.
        let (_, col, span) = at("foo.bar", 0, 0, Motion::WordFwd { big: false }, 1);
        assert_eq!(col, 3);
        assert_eq!(span, Span::Exclusive);
    }

    #[test]
    fn big_word_forward_ignores_punctuation() {
        let (_, col, _) = at("foo.bar baz", 0, 0, Motion::WordFwd { big: true }, 1);
        assert_eq!(col, 8, "W treats foo.bar as one word");
    }

    #[test]
    fn word_end_is_inclusive() {
        let (_, col, span) = at("hello world", 0, 0, Motion::WordEnd { big: false }, 1);
        assert_eq!(col, 4, "e lands on the last char of the word");
        assert_eq!(span, Span::Inclusive, "this is why de differs from dw");
    }

    #[test]
    fn word_motions_cross_lines() {
        let (line, col, _) = at("one\ntwo", 0, 2, Motion::WordFwd { big: false }, 1);
        assert_eq!((line, col), (1, 0));
    }

    #[test]
    fn dollar_and_caret_and_zero() {
        assert_eq!(at("  hi  ", 0, 3, Motion::LineStart, 1).1, 0);
        assert_eq!(at("  hi", 0, 0, Motion::FirstNonBlank, 1).1, 2);
        assert_eq!(at("abc", 0, 0, Motion::LineEnd, 1).1, 3);
        assert_eq!(at("  hi  ", 0, 0, Motion::LastNonBlank, 1).1, 3);
    }

    #[test]
    fn goto_line_first_and_last_and_counted() {
        let text = "a\nb\nc\nd";
        assert_eq!(
            at(text, 2, 0, Motion::GotoLine { first: true }, 1).0,
            0,
            "gg"
        );
        assert_eq!(
            at(text, 0, 0, Motion::GotoLine { first: false }, 1).0,
            3,
            "G"
        );
        assert_eq!(
            at(text, 0, 0, Motion::GotoLine { first: false }, 3).0,
            2,
            "3G is line 3"
        );
    }

    #[test]
    fn find_char_forward_and_till() {
        assert_eq!(
            at(
                "a,b,c",
                0,
                0,
                Motion::FindChar {
                    ch: ',',
                    forward: true,
                    till: false
                },
                1
            )
            .1,
            1,
            "f,"
        );
        assert_eq!(
            at(
                "a,b,c",
                0,
                0,
                Motion::FindChar {
                    ch: ',',
                    forward: true,
                    till: false
                },
                2
            )
            .1,
            3,
            "2f,"
        );
        assert_eq!(
            at(
                "abc,d",
                0,
                0,
                Motion::FindChar {
                    ch: ',',
                    forward: true,
                    till: true
                },
                1
            )
            .1,
            2,
            "t, stops before"
        );
    }

    #[test]
    fn find_char_that_is_absent_does_not_resolve() {
        let buffer = TextBuffer::from_string("abc");
        let view = ViewState::new();
        let c = ctx(&buffer, &view, 0, 0);
        assert!(resolve(
            Motion::FindChar {
                ch: 'z',
                forward: true,
                till: false
            },
            1,
            &c
        )
        .is_none());
    }

    #[test]
    fn match_pair_finds_the_partner_across_lines() {
        let text = "fn a() {\n  b();\n}";
        let (line, col, span) = at(text, 0, 7, Motion::MatchPair, 1);
        assert_eq!((line, col), (2, 0), "{{ matches }}");
        assert_eq!(span, Span::Inclusive);

        // And backwards from the closer.
        let (line, _, _) = at(text, 2, 0, Motion::MatchPair, 1);
        assert_eq!(line, 0);
    }

    #[test]
    fn paragraph_motion_walks_to_blank_lines() {
        let text = "a\nb\n\nc\nd";
        assert_eq!(at(text, 0, 0, Motion::Paragraph { forward: true }, 1).0, 2);
        assert_eq!(at(text, 4, 0, Motion::Paragraph { forward: false }, 1).0, 2);
    }

    #[test]
    fn vertical_motions_are_linewise_and_clamp_the_column() {
        let (line, col, span) = at("longer line\nab", 0, 8, Motion::Down, 1);
        assert_eq!((line, col), (1, 2), "column clamps to the shorter line");
        assert_eq!(span, Span::Linewise, "dj deletes whole lines");
    }

    #[test]
    fn motions_are_grapheme_aware() {
        // A combining sequence must not be split by `l`.
        let text = "e\u{301}x"; // é as e + combining acute
        let (_, col, _) = at(text, 0, 0, Motion::Right, 1);
        assert_eq!(col, 2, "moved past the whole cluster");
    }

    #[test]
    fn inner_word_object_covers_the_word_only() {
        let buffer = TextBuffer::from_string("foo bar baz");
        let view = ViewState::new();
        let c = ctx(&buffer, &view, 0, 5);
        let r = resolve_object(
            TextObject {
                kind: ObjectKind::Word { big: false },
                inner: true,
            },
            &c,
        )
        .unwrap();
        assert_eq!((r.start.column, r.end.column), (4, 6));
    }

    #[test]
    fn a_word_object_takes_trailing_space() {
        let buffer = TextBuffer::from_string("foo bar baz");
        let view = ViewState::new();
        let c = ctx(&buffer, &view, 0, 4);
        let r = resolve_object(
            TextObject {
                kind: ObjectKind::Word { big: false },
                inner: false,
            },
            &c,
        )
        .unwrap();
        assert_eq!((r.start.column, r.end.column), (4, 7));
    }

    #[test]
    fn quote_objects_select_inside_and_around() {
        let buffer = TextBuffer::from_string("say \"hi there\" ok");
        let view = ViewState::new();
        let c = ctx(&buffer, &view, 0, 7);
        let inner = resolve_object(
            TextObject {
                kind: ObjectKind::Quote('"'),
                inner: true,
            },
            &c,
        )
        .unwrap();
        assert_eq!((inner.start.column, inner.end.column), (5, 12));
        let around = resolve_object(
            TextObject {
                kind: ObjectKind::Quote('"'),
                inner: false,
            },
            &c,
        )
        .unwrap();
        assert_eq!((around.start.column, around.end.column), (4, 13));
    }

    #[test]
    fn bracket_objects_span_lines() {
        let buffer = TextBuffer::from_string("f({\n  a\n})");
        let view = ViewState::new();
        let c = ctx(&buffer, &view, 1, 2);
        let r = resolve_object(
            TextObject {
                kind: ObjectKind::Pair('{', '}'),
                inner: true,
            },
            &c,
        )
        .unwrap();
        // Just after the `{` on line 0, through the end of line 1 — i.e. the
        // newline plus `  a`, stopping before the `}` that opens line 2.
        assert_eq!((r.start.line, r.start.column), (0, 3));
        assert_eq!((r.end.line, r.end.column), (1, 3));
    }

    #[test]
    fn paragraph_object_is_linewise() {
        let buffer = TextBuffer::from_string("a\nb\n\nc");
        let view = ViewState::new();
        let c = ctx(&buffer, &view, 0, 0);
        let r = resolve_object(
            TextObject {
                kind: ObjectKind::Paragraph,
                inner: true,
            },
            &c,
        )
        .unwrap();
        assert_eq!((r.start.line, r.end.line), (0, 1));
        assert_eq!(r.span, Span::Linewise);
    }
}
