//! Command types and the grammar parser — stages 2 and 3 of the Vim pipeline.
//!
//! Vim's keymap is not a list of bindings, it is a small language:
//!
//! ```text
//! [register] [count] operator [count] (motion | text-object)
//! ```
//!
//! Parsing it, rather than enumerating operator/motion pairs, is what makes
//! `d2w`, `"ay$`, `ci"`, `>3j` and `3dd` fall out of one rule instead of costing
//! one match arm each. See `docs/VIM_MODE_DESIGN.md` §2.2–2.3.

use super::stroke::{NamedKey, Stroke};

/// Which mode the parser is reading for. Visual mode applies operators to the
/// existing selection, so it accepts `d` with no following motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    Normal,
    Visual,
}

/// How an operator should interpret the range a motion produces.
///
/// This distinction *is* the difference between `dw` and `de`, and between `dj`
/// deleting two whole lines and deleting a character range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// Up to but not including the target position.
    Exclusive,
    /// Up to and including the character at the target position.
    Inclusive,
    /// Whole lines.
    Linewise,
}

/// A cursor movement. Resolved to a range by `motion::resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    /// `w` / `W`
    WordFwd {
        big: bool,
    },
    /// `b` / `B`
    WordBack {
        big: bool,
    },
    /// `e` / `E`
    WordEnd {
        big: bool,
    },
    /// `ge`
    WordEndBack {
        big: bool,
    },
    /// `0`
    LineStart,
    /// `^`
    FirstNonBlank,
    /// `$`
    LineEnd,
    /// `g_`
    LastNonBlank,
    /// `gg` with no count, `G` with none, or either with one.
    GotoLine {
        first: bool,
    },
    /// `f` `F` `t` `T`
    FindChar {
        ch: char,
        forward: bool,
        till: bool,
    },
    /// `;` and `,`
    RepeatFind {
        reverse: bool,
    },
    /// `%`
    MatchPair,
    /// `}` and `{`
    Paragraph {
        forward: bool,
    },
    /// `H` `M` `L`
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    /// The PageUp/PageDown keys. Vim's Ctrl+D/U/F/B chords are deliberately not
    /// bound — see the note in `parse_motion`.
    Scroll {
        down: bool,
        half: bool,
    },
    /// `n` / `N` — jump to the next/previous search match.
    SearchNext {
        reverse: bool,
    },
    /// `+` / `<CR>` — first non-blank of the next line; `-` for the previous.
    LineFirstNonBlank {
        forward: bool,
    },
}

/// What an `i`/`a` text object selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Word {
        big: bool,
    },
    Paragraph,
    /// A quote character, e.g. `i"`.
    Quote(char),
    /// A bracket pair, e.g. `i(`.
    Pair(char, char),
}

/// `iw`, `ap`, `i"`, `a(` …
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextObject {
    pub kind: ObjectKind,
    /// `i` selects the inside, `a` includes the delimiters/trailing space.
    pub inner: bool,
}

/// An operator, which needs a range to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Yank,
    Change,
    Indent,
    Dedent,
    Lower,
    Upper,
    ToggleCase,
}

impl Operator {
    /// The character that doubles this operator into its linewise form
    /// (`dd`, `yy`, `>>`, `guu`).
    fn double_char(self) -> char {
        match self {
            Operator::Delete => 'd',
            Operator::Yank => 'y',
            Operator::Change => 'c',
            Operator::Indent => '>',
            Operator::Dedent => '<',
            Operator::Lower => 'u',
            Operator::Upper => 'U',
            Operator::ToggleCase => '~',
        }
    }

    /// Whether the operator changes text (drives dot-repeat recording).
    pub fn is_change(self) -> bool {
        !matches!(self, Operator::Yank)
    }
}

/// What an operator acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Motion(Motion),
    Object(TextObject),
    /// The doubled form — `count` whole lines from the cursor.
    Lines,
    /// Visual mode: the current selection.
    Selection,
}

/// A command that acts without needing a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Simple {
    /// `x`
    DeleteChar,
    /// `X`
    DeleteCharBack,
    /// `r{char}`
    ReplaceChar(char),
    /// `J`
    JoinLines,
    /// `~`
    ToggleCaseChar,
    /// `p` / `P`
    Put { before: bool },
    /// `u`
    Undo,
    /// `Ctrl+R`
    Redo,
    /// `.`
    RepeatChange,
    /// `*` / `#`
    SearchWord { reverse: bool },
    /// `Esc` — cancel pending state, or leave Visual mode.
    Cancel,
}

/// A command that changes mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSwitch {
    /// `i`
    InsertHere,
    /// `a`
    InsertAfter,
    /// `I`
    InsertFirstNonBlank,
    /// `A`
    InsertLineEnd,
    /// `o` / `O`
    OpenLine { above: bool },
    /// `v`
    Visual,
    /// `V`
    VisualLine,
    /// `gv`
    ReselectVisual,
}

/// The parsed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Motion(Motion),
    Operator {
        op: Operator,
        target: Target,
    },
    Simple(Simple),
    Mode(ModeSwitch),
    /// `:` — open the ex command line.
    ExEntry,
    /// `/` or `?` — open the search command line.
    SearchEntry {
        forward: bool,
    },
}

/// A complete command: what to do, how many times, and through which register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub register: Option<char>,
    pub count: Option<usize>,
    pub kind: CommandKind,
}

impl Command {
    /// The count, defaulting to 1.
    pub fn count(&self) -> usize {
        self.count.unwrap_or(1).max(1)
    }
}

/// The parser's verdict on the strokes buffered so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    /// A valid prefix — buffer more strokes.
    Incomplete,
    /// Ready to execute.
    Complete(Command),
    /// Not a command; discard the buffer.
    Invalid,
    /// Not ours. Let the standard editor handler have it.
    ///
    /// Making this the default for unrecognised chords and keys — rather than a
    /// catch-all that consumes them — is what keeps app shortcuts alive in
    /// Normal mode.
    Passthrough,
}

/// Registers Vim accepts after `"`.
fn is_register(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '*' | '_' | '-' | '"')
}

/// Parses a count. Returns `(count, next_index)`; a leading `0` is not a count,
/// it is the line-start motion.
fn parse_count(strokes: &[Stroke], mut i: usize) -> (Option<usize>, usize) {
    let mut count: Option<usize> = None;
    while let Some(Stroke::Char(c)) = strokes.get(i) {
        if let Some(d) = c.to_digit(10) {
            if count.is_none() && d == 0 {
                break;
            }
            // Cap the accumulator: an unbounded count is a memory bug a user can
            // trigger by leaning on a digit key.
            count = Some((count.unwrap_or(0) * 10 + d as usize).min(1_000_000));
            i += 1;
        } else {
            break;
        }
    }
    (count, i)
}

/// The bracket pair a text-object character selects.
fn pair_for(c: char) -> Option<(char, char)> {
    Some(match c {
        '(' | ')' | 'b' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' | 'B' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        _ => return None,
    })
}

/// Parses a text object after `i` or `a`.
fn parse_object(inner: bool, c: char) -> Option<TextObject> {
    let kind = match c {
        'w' => ObjectKind::Word { big: false },
        'W' => ObjectKind::Word { big: true },
        'p' => ObjectKind::Paragraph,
        '"' | '\'' | '`' => ObjectKind::Quote(c),
        other => {
            let (open, close) = pair_for(other)?;
            ObjectKind::Pair(open, close)
        }
    };
    Some(TextObject { kind, inner })
}

/// Outcome of trying to read a motion.
enum MotionParse {
    Got(Motion),
    Incomplete,
    No,
}

/// Reads a motion starting at `i`.
fn parse_motion(strokes: &[Stroke], i: usize) -> MotionParse {
    let Some(stroke) = strokes.get(i) else {
        return MotionParse::Incomplete;
    };

    let m = match stroke {
        Stroke::Named(NamedKey::Left) => Motion::Left,
        Stroke::Named(NamedKey::Right) => Motion::Right,
        Stroke::Named(NamedKey::Up) => Motion::Up,
        Stroke::Named(NamedKey::Down) => Motion::Down,
        Stroke::Named(NamedKey::Home) => Motion::LineStart,
        Stroke::Named(NamedKey::End) => Motion::LineEnd,
        Stroke::Named(NamedKey::PageUp) => Motion::Scroll {
            down: false,
            half: false,
        },
        Stroke::Named(NamedKey::PageDown) => Motion::Scroll {
            down: true,
            half: false,
        },
        Stroke::Named(NamedKey::Enter) => Motion::LineFirstNonBlank { forward: true },
        // Vim's Ctrl+D/U/F/B scroll chords are deliberately NOT bound: Ferrite
        // already uses Ctrl+D for Delete Line, Ctrl+F for Find and Ctrl+B for
        // Bold, and silently stealing them would break three working shortcuts
        // for anyone who turns Vim mode on. Paging is available on the
        // PageUp/PageDown keys instead.
        Stroke::Ctrl(_) => return MotionParse::No,
        Stroke::Char(c) => match c {
            'h' => Motion::Left,
            'l' | ' ' => Motion::Right,
            'j' => Motion::Down,
            'k' => Motion::Up,
            'w' => Motion::WordFwd { big: false },
            'W' => Motion::WordFwd { big: true },
            'b' => Motion::WordBack { big: false },
            'B' => Motion::WordBack { big: true },
            'e' => Motion::WordEnd { big: false },
            'E' => Motion::WordEnd { big: true },
            '0' => Motion::LineStart,
            '^' => Motion::FirstNonBlank,
            '$' => Motion::LineEnd,
            '%' => Motion::MatchPair,
            '}' => Motion::Paragraph { forward: true },
            '{' => Motion::Paragraph { forward: false },
            'H' => Motion::ScreenTop,
            'M' => Motion::ScreenMiddle,
            'L' => Motion::ScreenBottom,
            'G' => Motion::GotoLine { first: false },
            'n' => Motion::SearchNext { reverse: false },
            'N' => Motion::SearchNext { reverse: true },
            '+' => Motion::LineFirstNonBlank { forward: true },
            '-' => Motion::LineFirstNonBlank { forward: false },
            ';' => Motion::RepeatFind { reverse: false },
            ',' => Motion::RepeatFind { reverse: true },
            'f' | 'F' | 't' | 'T' => {
                let forward = *c == 'f' || *c == 't';
                let till = *c == 't' || *c == 'T';
                return match strokes.get(i + 1) {
                    None => MotionParse::Incomplete,
                    Some(Stroke::Char(target)) => MotionParse::Got(Motion::FindChar {
                        ch: *target,
                        forward,
                        till,
                    }),
                    Some(_) => MotionParse::No,
                };
            }
            'g' => {
                return match strokes.get(i + 1) {
                    None => MotionParse::Incomplete,
                    Some(Stroke::Char('g')) => MotionParse::Got(Motion::GotoLine { first: true }),
                    Some(Stroke::Char('_')) => MotionParse::Got(Motion::LastNonBlank),
                    Some(Stroke::Char('e')) => MotionParse::Got(Motion::WordEndBack { big: false }),
                    Some(Stroke::Char('E')) => MotionParse::Got(Motion::WordEndBack { big: true }),
                    Some(_) => MotionParse::No,
                };
            }
            _ => return MotionParse::No,
        },
        _ => return MotionParse::No,
    };

    MotionParse::Got(m)
}

/// Reads the operator at `i`, if there is one.
fn parse_operator(strokes: &[Stroke], i: usize) -> Option<(Operator, usize)> {
    match strokes.get(i)? {
        Stroke::Char('d') => Some((Operator::Delete, i + 1)),
        Stroke::Char('y') => Some((Operator::Yank, i + 1)),
        Stroke::Char('c') => Some((Operator::Change, i + 1)),
        Stroke::Char('>') => Some((Operator::Indent, i + 1)),
        Stroke::Char('<') => Some((Operator::Dedent, i + 1)),
        Stroke::Char('g') => match strokes.get(i + 1)? {
            Stroke::Char('u') => Some((Operator::Lower, i + 2)),
            Stroke::Char('U') => Some((Operator::Upper, i + 2)),
            Stroke::Char('~') => Some((Operator::ToggleCase, i + 2)),
            _ => None,
        },
        _ => None,
    }
}

/// Multiplies the operator count by the motion count, as Vim does for `2d3w`.
fn combine(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(1) * y.unwrap_or(1)),
    }
}

/// Parses the buffered strokes.
pub fn parse(strokes: &[Stroke], mode: ParseMode) -> ParseResult {
    if strokes.is_empty() {
        return ParseResult::Incomplete;
    }

    // Escape always resolves, whatever is pending.
    if strokes[0] == Stroke::Named(NamedKey::Escape) {
        return ParseResult::Complete(Command {
            register: None,
            count: None,
            kind: CommandKind::Simple(Simple::Cancel),
        });
    }

    let mut i = 0;

    // ── register ──────────────────────────────────────────────────────────
    let mut register = None;
    if strokes.first() == Some(&Stroke::Char('"')) {
        match strokes.get(1) {
            None => return ParseResult::Incomplete,
            Some(Stroke::Char(c)) if is_register(*c) => {
                register = Some(*c);
                i = 2;
            }
            Some(_) => return ParseResult::Invalid,
        }
    }

    // ── count ─────────────────────────────────────────────────────────────
    let (count, next) = parse_count(strokes, i);
    i = next;

    let Some(stroke) = strokes.get(i) else {
        return ParseResult::Incomplete;
    };

    let finish = |kind: CommandKind| {
        ParseResult::Complete(Command {
            register,
            count,
            kind,
        })
    };

    // ── operators ─────────────────────────────────────────────────────────
    // In Visual mode an operator applies to the selection immediately.
    if let Some((op, after_op)) = parse_operator(strokes, i) {
        if mode == ParseMode::Visual {
            return ParseResult::Complete(Command {
                register,
                count,
                kind: CommandKind::Operator {
                    op,
                    target: Target::Selection,
                },
            });
        }

        let (op_count, after_count) = parse_count(strokes, after_op);
        let total = combine(count, op_count);

        match strokes.get(after_count) {
            None => return ParseResult::Incomplete,
            // Doubled operator → linewise over `count` lines.
            Some(Stroke::Char(c)) if *c == op.double_char() => {
                return ParseResult::Complete(Command {
                    register,
                    count: total,
                    kind: CommandKind::Operator {
                        op,
                        target: Target::Lines,
                    },
                });
            }
            // Text object.
            Some(Stroke::Char(c @ ('i' | 'a'))) => {
                let inner = *c == 'i';
                return match strokes.get(after_count + 1) {
                    None => ParseResult::Incomplete,
                    Some(Stroke::Char(obj)) => match parse_object(inner, *obj) {
                        Some(object) => ParseResult::Complete(Command {
                            register,
                            count: total,
                            kind: CommandKind::Operator {
                                op,
                                target: Target::Object(object),
                            },
                        }),
                        None => ParseResult::Invalid,
                    },
                    Some(_) => ParseResult::Invalid,
                };
            }
            _ => {}
        }

        return match parse_motion(strokes, after_count) {
            MotionParse::Got(m) => ParseResult::Complete(Command {
                register,
                count: total,
                kind: CommandKind::Operator {
                    op,
                    target: Target::Motion(m),
                },
            }),
            MotionParse::Incomplete => ParseResult::Incomplete,
            MotionParse::No => ParseResult::Invalid,
        };
    }

    // ── everything else ───────────────────────────────────────────────────
    match stroke {
        Stroke::Char(c) => match c {
            'i' if mode == ParseMode::Normal => finish(CommandKind::Mode(ModeSwitch::InsertHere)),
            'a' if mode == ParseMode::Normal => finish(CommandKind::Mode(ModeSwitch::InsertAfter)),
            'I' => finish(CommandKind::Mode(ModeSwitch::InsertFirstNonBlank)),
            'A' => finish(CommandKind::Mode(ModeSwitch::InsertLineEnd)),
            'o' => finish(CommandKind::Mode(ModeSwitch::OpenLine { above: false })),
            'O' => finish(CommandKind::Mode(ModeSwitch::OpenLine { above: true })),
            'v' => finish(CommandKind::Mode(ModeSwitch::Visual)),
            'V' => finish(CommandKind::Mode(ModeSwitch::VisualLine)),

            // Visual-mode text objects extend the selection.
            'i' | 'a' if mode == ParseMode::Visual => {
                let inner = *c == 'i';
                match strokes.get(i + 1) {
                    None => ParseResult::Incomplete,
                    Some(Stroke::Char(obj)) => match parse_object(inner, *obj) {
                        Some(object) => finish(CommandKind::Operator {
                            op: Operator::Yank, // placeholder; exec only uses the target
                            target: Target::Object(object),
                        }),
                        None => ParseResult::Invalid,
                    },
                    Some(_) => ParseResult::Invalid,
                }
            }

            'x' => finish(CommandKind::Simple(Simple::DeleteChar)),
            'X' => finish(CommandKind::Simple(Simple::DeleteCharBack)),
            'J' => finish(CommandKind::Simple(Simple::JoinLines)),
            '~' => finish(CommandKind::Simple(Simple::ToggleCaseChar)),
            'p' => finish(CommandKind::Simple(Simple::Put { before: false })),
            'P' => finish(CommandKind::Simple(Simple::Put { before: true })),
            'u' if mode == ParseMode::Normal => finish(CommandKind::Simple(Simple::Undo)),
            '.' => finish(CommandKind::Simple(Simple::RepeatChange)),
            '*' => finish(CommandKind::Simple(Simple::SearchWord { reverse: false })),
            '#' => finish(CommandKind::Simple(Simple::SearchWord { reverse: true })),

            // Shorthands that are operators in disguise.
            'D' => finish(CommandKind::Operator {
                op: Operator::Delete,
                target: Target::Motion(Motion::LineEnd),
            }),
            'C' => finish(CommandKind::Operator {
                op: Operator::Change,
                target: Target::Motion(Motion::LineEnd),
            }),
            'Y' => finish(CommandKind::Operator {
                op: Operator::Yank,
                target: Target::Lines,
            }),
            'S' => finish(CommandKind::Operator {
                op: Operator::Change,
                target: Target::Lines,
            }),
            's' => finish(CommandKind::Operator {
                op: Operator::Change,
                target: Target::Motion(Motion::Right),
            }),

            'r' => match strokes.get(i + 1) {
                None => ParseResult::Incomplete,
                Some(Stroke::Char(ch)) => finish(CommandKind::Simple(Simple::ReplaceChar(*ch))),
                Some(_) => ParseResult::Invalid,
            },

            ':' => finish(CommandKind::ExEntry),
            '/' => finish(CommandKind::SearchEntry { forward: true }),
            '?' => finish(CommandKind::SearchEntry { forward: false }),

            'g' => match strokes.get(i + 1) {
                None => ParseResult::Incomplete,
                Some(Stroke::Char('v')) => finish(CommandKind::Mode(ModeSwitch::ReselectVisual)),
                _ => match parse_motion(strokes, i) {
                    MotionParse::Got(m) => finish(CommandKind::Motion(m)),
                    MotionParse::Incomplete => ParseResult::Incomplete,
                    MotionParse::No => ParseResult::Invalid,
                },
            },

            _ => match parse_motion(strokes, i) {
                MotionParse::Got(m) => finish(CommandKind::Motion(m)),
                MotionParse::Incomplete => ParseResult::Incomplete,
                MotionParse::No => ParseResult::Invalid,
            },
        },

        Stroke::Ctrl(c) => match c {
            'r' => finish(CommandKind::Simple(Simple::Redo)),
            // Ctrl+S, Ctrl+F, Ctrl+D, Ctrl+B, Ctrl+P … belong to the
            // application. Only Ctrl+R is claimed, because nothing else binds it.
            _ => ParseResult::Passthrough,
        },

        Stroke::Named(_) => match parse_motion(strokes, i) {
            MotionParse::Got(m) => finish(CommandKind::Motion(m)),
            MotionParse::Incomplete => ParseResult::Incomplete,
            MotionParse::No => ParseResult::Passthrough,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Vec<Stroke> {
        text.chars().map(Stroke::Char).collect()
    }

    fn parse_normal(text: &str) -> ParseResult {
        parse(&s(text), ParseMode::Normal)
    }

    #[test]
    fn bare_motions_parse() {
        for (keys, want) in [
            ("h", Motion::Left),
            ("l", Motion::Right),
            ("j", Motion::Down),
            ("k", Motion::Up),
            ("w", Motion::WordFwd { big: false }),
            ("W", Motion::WordFwd { big: true }),
            ("e", Motion::WordEnd { big: false }),
            ("0", Motion::LineStart),
            ("^", Motion::FirstNonBlank),
            ("$", Motion::LineEnd),
            ("%", Motion::MatchPair),
            ("}", Motion::Paragraph { forward: true }),
            ("G", Motion::GotoLine { first: false }),
            ("gg", Motion::GotoLine { first: true }),
            ("g_", Motion::LastNonBlank),
        ] {
            match parse_normal(keys) {
                ParseResult::Complete(Command {
                    kind: CommandKind::Motion(got),
                    ..
                }) => assert_eq!(got, want, "{keys}"),
                other => panic!("{keys} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn the_characters_egui_key_cannot_spell_now_parse() {
        // Every one of these was unreachable under `egui::Key` dispatch.
        for keys in ["$", "^", "%", "~", ">>", "<<", "guu", "gUU"] {
            assert!(
                matches!(parse_normal(keys), ParseResult::Complete(_)),
                "{keys} should parse"
            );
        }
    }

    #[test]
    fn operator_plus_motion_parses_with_counts_multiplied() {
        match parse_normal("2d3w") {
            ParseResult::Complete(cmd) => {
                assert_eq!(cmd.count, Some(6), "2d3w deletes 6 words");
                assert_eq!(
                    cmd.kind,
                    CommandKind::Operator {
                        op: Operator::Delete,
                        target: Target::Motion(Motion::WordFwd { big: false }),
                    }
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn doubled_operators_are_linewise() {
        for (keys, op, count) in [
            ("dd", Operator::Delete, None),
            ("3dd", Operator::Delete, Some(3)),
            ("yy", Operator::Yank, None),
            ("cc", Operator::Change, None),
            (">>", Operator::Indent, None),
        ] {
            match parse_normal(keys) {
                ParseResult::Complete(cmd) => {
                    assert_eq!(
                        cmd.kind,
                        CommandKind::Operator {
                            op,
                            target: Target::Lines
                        },
                        "{keys}"
                    );
                    assert_eq!(cmd.count, count, "{keys}");
                }
                other => panic!("{keys} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn text_objects_parse() {
        match parse_normal("ci\"") {
            ParseResult::Complete(cmd) => assert_eq!(
                cmd.kind,
                CommandKind::Operator {
                    op: Operator::Change,
                    target: Target::Object(TextObject {
                        kind: ObjectKind::Quote('"'),
                        inner: true
                    }),
                }
            ),
            other => panic!("{other:?}"),
        }
        match parse_normal("dap") {
            ParseResult::Complete(cmd) => assert_eq!(
                cmd.kind,
                CommandKind::Operator {
                    op: Operator::Delete,
                    target: Target::Object(TextObject {
                        kind: ObjectKind::Paragraph,
                        inner: false
                    }),
                }
            ),
            other => panic!("{other:?}"),
        }
        match parse_normal("yi{") {
            ParseResult::Complete(cmd) => assert_eq!(
                cmd.kind,
                CommandKind::Operator {
                    op: Operator::Yank,
                    target: Target::Object(TextObject {
                        kind: ObjectKind::Pair('{', '}'),
                        inner: true
                    }),
                }
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn registers_parse() {
        match parse_normal("\"ayy") {
            ParseResult::Complete(cmd) => {
                assert_eq!(cmd.register, Some('a'));
                assert_eq!(
                    cmd.kind,
                    CommandKind::Operator {
                        op: Operator::Yank,
                        target: Target::Lines
                    }
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn find_char_waits_for_its_target() {
        assert_eq!(parse_normal("f"), ParseResult::Incomplete);
        match parse_normal("fx") {
            ParseResult::Complete(cmd) => assert_eq!(
                cmd.kind,
                CommandKind::Motion(Motion::FindChar {
                    ch: 'x',
                    forward: true,
                    till: false
                })
            ),
            other => panic!("{other:?}"),
        }
        match parse_normal("dt,") {
            ParseResult::Complete(cmd) => assert_eq!(
                cmd.kind,
                CommandKind::Operator {
                    op: Operator::Delete,
                    target: Target::Motion(Motion::FindChar {
                        ch: ',',
                        forward: true,
                        till: true
                    }),
                }
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_proper_prefix_is_incomplete() {
        for prefix in [
            "2", "d", "2d", "2d3", "c", "ci", "\"", "\"a", "g", "f", "r", "y2",
        ] {
            assert_eq!(
                parse_normal(prefix),
                ParseResult::Incomplete,
                "{prefix} is a valid prefix"
            );
        }
    }

    #[test]
    fn garbage_is_invalid_not_swallowed_silently() {
        for keys in ["dQ", "ciZ", "\"!"] {
            assert_eq!(parse_normal(keys), ParseResult::Invalid, "{keys}");
        }
    }

    #[test]
    fn app_shortcuts_pass_through_to_the_standard_handler() {
        // This is the arrow-key bug class, closed structurally: anything the
        // grammar does not claim defaults to the application.
        // Ctrl+D (Delete Line), Ctrl+F (Find) and Ctrl+B (Bold) are Ferrite
        // shortcuts, so Vim must not claim them. Ctrl+R is the one exception.
        for c in [
            's', 'f', 'p', 'z', 'a', 'o', 'n', 'g', 'k', 'w', 'd', 'b', 'u', 'v',
        ] {
            if c == 'r' {
                continue; // Ctrl+R is redo
            }
            assert_eq!(
                parse(&[Stroke::Ctrl(c)], ParseMode::Normal),
                ParseResult::Passthrough,
                "Ctrl+{c} belongs to the app"
            );
        }
    }

    #[test]
    fn arrow_keys_are_motions_in_both_modes() {
        for (named, want) in [
            (NamedKey::Left, Motion::Left),
            (NamedKey::Right, Motion::Right),
            (NamedKey::Up, Motion::Up),
            (NamedKey::Down, Motion::Down),
        ] {
            for mode in [ParseMode::Normal, ParseMode::Visual] {
                match parse(&[Stroke::Named(named)], mode) {
                    ParseResult::Complete(Command {
                        kind: CommandKind::Motion(got),
                        ..
                    }) => assert_eq!(got, want, "{named:?} in {mode:?}"),
                    other => panic!("{named:?} in {mode:?} parsed as {other:?}"),
                }
            }
        }
    }

    #[test]
    fn counts_apply_to_arrow_keys_too() {
        match parse(
            &[Stroke::Char('3'), Stroke::Named(NamedKey::Down)],
            ParseMode::Normal,
        ) {
            ParseResult::Complete(cmd) => {
                assert_eq!(cmd.count, Some(3));
                assert_eq!(cmd.kind, CommandKind::Motion(Motion::Down));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn escape_resolves_whatever_is_pending() {
        let strokes = vec![Stroke::Char('2'), Stroke::Named(NamedKey::Escape)];
        // Escape is checked first only when it leads; mid-command it ends the
        // sequence as a cancel via the pending buffer being reset by the caller.
        assert_eq!(
            parse(&[Stroke::Named(NamedKey::Escape)], ParseMode::Normal),
            ParseResult::Complete(Command {
                register: None,
                count: None,
                kind: CommandKind::Simple(Simple::Cancel),
            })
        );
        assert!(matches!(
            parse(&strokes, ParseMode::Normal),
            ParseResult::Invalid | ParseResult::Passthrough
        ));
    }

    #[test]
    fn visual_mode_operators_act_on_the_selection() {
        match parse(&s("d"), ParseMode::Visual) {
            ParseResult::Complete(cmd) => assert_eq!(
                cmd.kind,
                CommandKind::Operator {
                    op: Operator::Delete,
                    target: Target::Selection
                }
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_count_is_capped_so_a_held_key_cannot_grow_it_without_bound() {
        let long: String = "9".repeat(40);
        let (count, _) = parse_count(&s(&long), 0);
        assert_eq!(count, Some(1_000_000));
    }
}
