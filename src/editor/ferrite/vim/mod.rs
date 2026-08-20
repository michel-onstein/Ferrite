//! Vim modal editing for FerriteEditor.
//!
//! Structured as the pipeline in `docs/VIM_MODE_DESIGN.md`:
//!
//! ```text
//! egui events → stroke → parse → Command → resolve range → execute
//!                                                            ├→ buffer/selection
//!                                                            └→ VimEffect (app)
//! ```
//!
//! The layering is what lets the keymap compose: `d2w`, `ci"`, `>3j`, `"ayy` and
//! `3.` are all the same rule with different terminals, rather than one match arm
//! each. Activated by the `vim_mode` setting; when off, the editor uses standard
//! non-modal keybindings.

pub mod command;
pub mod ex;
pub mod exec;
pub mod motion;
pub mod registers;
pub mod stroke;

use egui::{Key, Modifiers};

use super::buffer::TextBuffer;
use super::cursor::{Cursor, Selection};
use super::input::InputResult;
use super::view::ViewState;

use command::{
    Command, CommandKind, ModeSwitch, Motion, Operator, ParseMode, ParseResult, Simple, Span,
    Target,
};
use exec::{CaseChange, DEFAULT_SHIFT_WIDTH};
use motion::{FindSpec, MotionCtx};
use registers::{is_clipboard, RegisterValue, Registers};
use stroke::{NamedKey, Stroke};

/// How many strokes may sit in the pending buffer before it is discarded.
/// An unbounded pending buffer is a memory bug a user can trigger by leaning on
/// a key.
const MAX_PENDING: usize = 16;

/// Active Vim editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    /// The `:` / `/` / `?` command line is open.
    CommandLine,
}

impl VimMode {
    /// Display label for the status bar indicator.
    pub fn label(&self) -> &'static str {
        match self {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
            VimMode::VisualLine => "V-LINE",
            VimMode::CommandLine => "COMMAND",
        }
    }

    fn parse_mode(self) -> ParseMode {
        match self {
            VimMode::Visual | VimMode::VisualLine => ParseMode::Visual,
            _ => ParseMode::Normal,
        }
    }

    /// Whether the cursor should be drawn as a block covering the character it
    /// sits on, rather than as a bar between characters.
    ///
    /// Every mode except Insert operates *on* the character under the cursor, so
    /// the cursor covers it — that is what makes the mode visible at a glance.
    pub fn uses_block_cursor(self) -> bool {
        self != VimMode::Insert
    }
}

/// Something only the application can carry out.
///
/// The editor deliberately does not depend on app types: ROADMAP v0.3.x extracts
/// the editor into a standalone crate with a `vim` feature, and `:w` reaching
/// into `ShortcutCommand` would break that. See design §2.6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VimEffect {
    /// `u`
    Undo,
    /// `Ctrl+R`
    Redo,
    /// `:w`, `:wq`, `:x`
    Write { path: Option<String>, quit: bool },
    /// `:q`, `:qa`
    Quit { force: bool, all: bool },
    /// `:e`
    Edit { path: String },
    /// `:set`
    SetOption { option: String, value: ex::SetValue },
    /// `/pattern` or `?pattern`
    Search { pattern: String, forward: bool },
    /// `n` / `N`
    SearchNext { reverse: bool },
    /// `*` / `#`
    SearchWord { reverse: bool },
    /// `:noh`
    ClearSearchHighlight,
    /// `"+y` — the system clipboard is the application's to write.
    ClipboardWrite(String),
    /// `"+p`
    ClipboardPut { before: bool, count: usize },
    /// Feedback for the status bar.
    Message(String),
    /// An error, e.g. an unknown ex command.
    Error(String),
}

/// Result of feeding one event to Vim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimKeyResult {
    /// Vim handled it and the editor should react accordingly.
    Handled(InputResult),
    /// Consumed with no visible change (a pending prefix, a mode switch).
    Consumed,
    /// Not Vim's — the standard input handler should process it.
    Passthrough,
}

/// Mutable editor state the executor needs.
pub struct VimCtx<'a> {
    pub buffer: &'a mut TextBuffer,
    pub selections: &'a mut Vec<Selection>,
    pub primary: usize,
    pub view: &'a mut ViewState,
    /// Lines in the viewport, for `H`/`M`/`L` and page motions.
    pub visible_lines: usize,
}

impl VimCtx<'_> {
    fn selection(&self) -> Selection {
        self.selections
            .get(self.primary)
            .copied()
            .unwrap_or_else(Selection::start)
    }

    fn set_selection(&mut self, sel: Selection) {
        if let Some(s) = self.selections.get_mut(self.primary) {
            *s = sel;
        } else if self.selections.is_empty() {
            self.selections.push(sel);
        }
    }

    fn cursor(&self) -> Cursor {
        self.selection().head
    }

    fn set_cursor(&mut self, c: Cursor) {
        self.set_selection(Selection::collapsed(c));
    }
}

/// What kind of command line is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmdKind {
    Ex,
    Search { forward: bool },
}

/// The change `.` repeats.
#[derive(Debug, Clone)]
struct LastChange {
    command: Command,
    /// Text typed during the insert session this change opened, if any.
    inserted: String,
}

/// Persistent Vim state across frames.
#[derive(Debug, Clone)]
pub struct VimState {
    pub mode: VimMode,
    /// Strokes buffered toward the current command.
    pending: Vec<Stroke>,
    registers: Registers,
    last_find: Option<FindSpec>,
    last_change: Option<LastChange>,
    /// Text typed in the current insert session, for dot-repeat.
    insert_capture: Option<String>,
    /// Selection to restore for `gv`.
    last_visual: Option<Selection>,
    /// `:set shiftwidth`
    shift_width: usize,
    cmdline: Option<(CmdKind, String)>,
    effects: Vec<VimEffect>,
    /// Set while `.` is replaying, so the replay does not re-record itself.
    replaying: bool,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            mode: VimMode::Normal,
            pending: Vec::new(),
            registers: Registers::new(),
            last_find: None,
            last_change: None,
            insert_capture: None,
            last_visual: None,
            shift_width: DEFAULT_SHIFT_WIDTH,
            cmdline: None,
            effects: Vec::new(),
            replaying: false,
        }
    }
}

impl VimState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pending command text, for the status bar (`d2` while typing `d2w`).
    pub fn pending_text(&self) -> String {
        self.pending.iter().filter_map(|s| s.as_char()).collect()
    }

    /// The open command line, if any — `:%s/a/b` or `/pattern`.
    pub fn cmdline_text(&self) -> Option<String> {
        self.cmdline.as_ref().map(|(kind, text)| {
            let prefix = match kind {
                CmdKind::Ex => ':',
                CmdKind::Search { forward: true } => '/',
                CmdKind::Search { forward: false } => '?',
            };
            format!("{prefix}{text}")
        })
    }

    /// Drains queued application effects.
    pub fn take_effects(&mut self) -> Vec<VimEffect> {
        std::mem::take(&mut self.effects)
    }

    /// Applies a `:set shiftwidth=N` locally so `>>` follows it.
    pub fn set_shift_width(&mut self, width: usize) {
        self.shift_width = width.clamp(1, 16);
    }

    /// Hands clipboard contents back for a `"+p`.
    pub fn provide_clipboard(&mut self, text: String) {
        let linewise = text.ends_with('\n');
        self.registers.set_unnamed(RegisterValue::new(
            text.trim_end_matches('\n').to_string(),
            linewise,
        ));
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Entry points
    // ─────────────────────────────────────────────────────────────────────────

    /// Feeds an `Event::Key`.
    pub fn handle_key(
        &mut self,
        key: Key,
        modifiers: &Modifiers,
        ctx: &mut VimCtx,
    ) -> VimKeyResult {
        // In Insert mode only Escape is ours; everything else is normal editing.
        if self.mode == VimMode::Insert {
            if key == Key::Escape && !modifiers.ctrl && !modifiers.command {
                self.leave_insert(ctx);
                return VimKeyResult::Consumed;
            }
            return VimKeyResult::Passthrough;
        }

        let Some(s) = stroke::stroke_from_key(key, modifiers) else {
            // A plain printable key. It reaches the parser as text instead
            // (stroke.rs rule 2), so consume the key event here — otherwise the
            // same press would be processed twice.
            return VimKeyResult::Consumed;
        };

        self.feed(s, ctx)
    }

    /// Feeds an `Event::Text`.
    pub fn handle_text(&mut self, text: &str, ctx: &mut VimCtx) -> VimKeyResult {
        if self.mode == VimMode::Insert {
            // Capture for dot-repeat, then let the editor insert it.
            if let Some(buf) = self.insert_capture.as_mut() {
                buf.push_str(text);
            }
            return VimKeyResult::Passthrough;
        }

        let mut result = VimKeyResult::Consumed;
        for s in stroke::strokes_from_text(text) {
            result = self.feed(s, ctx);
        }
        result
    }

    /// Routes one stroke.
    fn feed(&mut self, s: Stroke, ctx: &mut VimCtx) -> VimKeyResult {
        if self.mode == VimMode::CommandLine {
            return self.feed_cmdline(s, ctx);
        }

        self.pending.push(s);
        if self.pending.len() > MAX_PENDING {
            self.pending.clear();
            return VimKeyResult::Consumed;
        }

        match command::parse(&self.pending, self.mode.parse_mode()) {
            ParseResult::Incomplete => VimKeyResult::Consumed,
            ParseResult::Invalid => {
                self.pending.clear();
                VimKeyResult::Consumed
            }
            ParseResult::Passthrough => {
                self.pending.clear();
                VimKeyResult::Passthrough
            }
            ParseResult::Complete(cmd) => {
                self.pending.clear();
                self.execute(cmd, ctx)
            }
        }
    }

    /// Command-line editing.
    fn feed_cmdline(&mut self, s: Stroke, ctx: &mut VimCtx) -> VimKeyResult {
        let Some((kind, text)) = self.cmdline.as_mut() else {
            self.mode = VimMode::Normal;
            return VimKeyResult::Consumed;
        };
        let kind = *kind;

        match s {
            Stroke::Char(c) => {
                text.push(c);
                VimKeyResult::Consumed
            }
            Stroke::Named(NamedKey::Backspace) => {
                if text.pop().is_none() {
                    self.cmdline = None;
                    self.mode = VimMode::Normal;
                }
                VimKeyResult::Consumed
            }
            Stroke::Named(NamedKey::Escape) => {
                self.cmdline = None;
                self.mode = VimMode::Normal;
                VimKeyResult::Consumed
            }
            Stroke::Ctrl('u') => {
                text.clear();
                VimKeyResult::Consumed
            }
            Stroke::Named(NamedKey::Enter) => {
                let line = text.clone();
                self.cmdline = None;
                self.mode = VimMode::Normal;
                self.submit_cmdline(kind, &line, ctx)
            }
            _ => VimKeyResult::Consumed,
        }
    }

    fn submit_cmdline(&mut self, kind: CmdKind, line: &str, ctx: &mut VimCtx) -> VimKeyResult {
        match kind {
            CmdKind::Search { forward } => {
                if line.is_empty() {
                    return VimKeyResult::Consumed;
                }
                self.effects.push(VimEffect::Search {
                    pattern: line.to_string(),
                    forward,
                });
                VimKeyResult::Consumed
            }
            CmdKind::Ex => match ex::parse(line) {
                Ok(cmd) => self.run_ex(cmd, ctx),
                Err(message) => {
                    self.effects.push(VimEffect::Error(message));
                    VimKeyResult::Consumed
                }
            },
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Ex commands
    // ─────────────────────────────────────────────────────────────────────────

    fn run_ex(&mut self, cmd: ex::ExCommand, ctx: &mut VimCtx) -> VimKeyResult {
        use ex::ExCommand as E;
        let last = ctx.buffer.line_count().saturating_sub(1);
        let cursor_line = ctx.cursor().line;

        match cmd {
            E::Write { path, quit } => {
                self.effects.push(VimEffect::Write { path, quit });
                VimKeyResult::Consumed
            }
            E::Quit { force, all } => {
                self.effects.push(VimEffect::Quit { force, all });
                VimKeyResult::Consumed
            }
            E::Edit { path, .. } => {
                match path {
                    Some(p) => self.effects.push(VimEffect::Edit { path: p }),
                    None => self
                        .effects
                        .push(VimEffect::Error("E32: no file name".into())),
                }
                VimKeyResult::Consumed
            }
            E::Goto(addr) => {
                let line = match addr {
                    ex::Addr::Line(n) => n.saturating_sub(1).min(last),
                    ex::Addr::Current => cursor_line,
                    ex::Addr::Last => last,
                };
                ctx.set_cursor(Cursor::new(line, 0));
                VimKeyResult::Handled(InputResult::CursorMoved)
            }
            E::NoHighlight => {
                self.effects.push(VimEffect::ClearSearchHighlight);
                VimKeyResult::Consumed
            }
            E::Set { option, value } => {
                if option == "shiftwidth" {
                    if let ex::SetValue::Number(n) = value {
                        self.set_shift_width(n);
                    }
                }
                self.effects.push(VimEffect::SetOption { option, value });
                VimKeyResult::Consumed
            }
            E::Delete { range, register } => {
                let (start, end) = range
                    .resolve(cursor_line, last)
                    .unwrap_or((cursor_line, cursor_line));
                let r = exec::edit_range(
                    ctx.buffer,
                    Cursor::new(start, 0),
                    Cursor::new(end, 0),
                    Span::Linewise,
                );
                let value = exec::range_value(ctx.buffer, &r);
                self.write_delete(register, value);
                let c = exec::delete_range(ctx.buffer, &r);
                ctx.set_cursor(c);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            E::Yank { range, register } => {
                let (start, end) = range
                    .resolve(cursor_line, last)
                    .unwrap_or((cursor_line, cursor_line));
                let r = exec::edit_range(
                    ctx.buffer,
                    Cursor::new(start, 0),
                    Cursor::new(end, 0),
                    Span::Linewise,
                );
                let value = exec::range_value(ctx.buffer, &r);
                let n = value.text.lines().count();
                self.write_yank(register, value);
                self.effects
                    .push(VimEffect::Message(format!("{n} lines yanked")));
                VimKeyResult::Consumed
            }
            E::Shift { range, dedent } => {
                let (start, end) = range
                    .resolve(cursor_line, last)
                    .unwrap_or((cursor_line, cursor_line));
                let c = exec::shift_lines(ctx.buffer, start, end, dedent, self.shift_width);
                ctx.set_cursor(c);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            E::Substitute {
                range,
                pattern,
                replacement,
                flags,
            } => self.substitute(range, &pattern, &replacement, flags, ctx),
        }
    }

    /// `:s` / `:%s`, reusing the `regex` crate the project already depends on
    /// rather than growing a second pattern engine.
    fn substitute(
        &mut self,
        range: ex::ExRange,
        pattern: &str,
        replacement: &str,
        flags: ex::SubstFlags,
        ctx: &mut VimCtx,
    ) -> VimKeyResult {
        let last = ctx.buffer.line_count().saturating_sub(1);
        let cursor_line = ctx.cursor().line;
        let (start, end) = range
            .resolve(cursor_line, last)
            .unwrap_or((cursor_line, cursor_line));

        let re = match regex::RegexBuilder::new(pattern)
            .case_insensitive(flags.ignore_case)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                self.effects
                    .push(VimEffect::Error(format!("E486: bad pattern: {e}")));
                return VimKeyResult::Consumed;
            }
        };

        let mut replaced = 0usize;
        let mut lines_changed = 0usize;

        // Walk backwards so earlier edits do not move later line offsets.
        for line in (start..=end.min(last)).rev() {
            let text = motion::line_text(ctx.buffer, line);
            let new_text = if flags.global {
                re.replace_all(&text, replacement).to_string()
            } else {
                re.replace(&text, replacement).to_string()
            };
            if new_text != text {
                let count = if flags.global {
                    re.find_iter(&text).count()
                } else {
                    1
                };
                replaced += count;
                lines_changed += 1;

                let line_start = ctx.buffer.try_line_to_char(line).unwrap_or(0);
                ctx.buffer.remove(line_start, text.chars().count());
                ctx.buffer.insert(line_start, &new_text);
            }
        }

        if replaced == 0 {
            self.effects.push(VimEffect::Error(format!(
                "E486: pattern not found: {pattern}"
            )));
            return VimKeyResult::Consumed;
        }

        self.effects.push(VimEffect::Message(format!(
            "{replaced} substitutions on {lines_changed} lines"
        )));
        let line = start.min(ctx.buffer.line_count().saturating_sub(1));
        ctx.set_cursor(Cursor::new(line, 0));
        VimKeyResult::Handled(InputResult::TextChanged)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Registers, with the clipboard split out to the application
    // ─────────────────────────────────────────────────────────────────────────

    fn write_yank(&mut self, register: Option<char>, value: RegisterValue) {
        if let Some(name) = register {
            if is_clipboard(name) {
                self.effects.push(VimEffect::ClipboardWrite(value.text));
                return;
            }
        }
        self.registers.write_yank(register, value);
    }

    fn write_delete(&mut self, register: Option<char>, value: RegisterValue) {
        if let Some(name) = register {
            if is_clipboard(name) {
                self.effects
                    .push(VimEffect::ClipboardWrite(value.text.clone()));
                self.registers.write_delete(None, value);
                return;
            }
        }
        self.registers.write_delete(register, value);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Execution
    // ─────────────────────────────────────────────────────────────────────────

    fn motion_ctx<'a>(&self, ctx: &'a VimCtx<'a>) -> MotionCtx<'a> {
        MotionCtx {
            buffer: ctx.buffer,
            cursor: ctx.cursor(),
            view: ctx.view,
            last_find: self.last_find,
            visible_lines: ctx.visible_lines,
        }
    }

    fn execute(&mut self, cmd: Command, ctx: &mut VimCtx) -> VimKeyResult {
        // Remember `f`/`t` targets so `;` and `,` can repeat them.
        if let CommandKind::Motion(Motion::FindChar { ch, forward, till })
        | CommandKind::Operator {
            target: Target::Motion(Motion::FindChar { ch, forward, till }),
            ..
        } = cmd.kind
        {
            self.last_find = Some(FindSpec { ch, forward, till });
        }

        match cmd.kind {
            CommandKind::Motion(m) => self.apply_motion(m, cmd.count(), ctx),
            CommandKind::Operator { op, target } => self.apply_operator(&cmd, op, target, ctx),
            CommandKind::Simple(simple) => self.apply_simple(&cmd, simple, ctx),
            CommandKind::Mode(switch) => self.apply_mode_switch(&cmd, switch, ctx),
            CommandKind::ExEntry => {
                self.cmdline = Some((CmdKind::Ex, String::new()));
                self.mode = VimMode::CommandLine;
                VimKeyResult::Consumed
            }
            CommandKind::SearchEntry { forward } => {
                self.cmdline = Some((CmdKind::Search { forward }, String::new()));
                self.mode = VimMode::CommandLine;
                VimKeyResult::Consumed
            }
        }
    }

    fn apply_motion(&mut self, m: Motion, count: usize, ctx: &mut VimCtx) -> VimKeyResult {
        // Search motions need the application's match list.
        if let Motion::SearchNext { reverse } = m {
            self.effects.push(VimEffect::SearchNext { reverse });
            return VimKeyResult::Consumed;
        }

        let resolved = {
            let mctx = self.motion_ctx(ctx);
            motion::resolve(m, count, &mctx)
        };
        let Some(result) = resolved else {
            return VimKeyResult::Consumed; // a motion that cannot move is a no-op
        };

        match self.mode {
            VimMode::Visual => {
                let sel = ctx.selection().with_head(result.target);
                ctx.set_selection(sel);
            }
            VimMode::VisualLine => {
                let sel = ctx.selection().with_head(result.target);
                ctx.set_selection(expand_lines(ctx.buffer, sel));
            }
            _ => ctx.set_cursor(result.target),
        }
        VimKeyResult::Handled(InputResult::CursorMoved)
    }

    /// Resolves an operator's target to a range.
    fn target_range(
        &mut self,
        cmd: &Command,
        target: Target,
        ctx: &mut VimCtx,
    ) -> Option<exec::EditRange> {
        let cursor = ctx.cursor();
        match target {
            Target::Lines => {
                let last = ctx.buffer.line_count().saturating_sub(1);
                let end = (cursor.line + cmd.count() - 1).min(last);
                Some(exec::edit_range(
                    ctx.buffer,
                    Cursor::new(cursor.line, 0),
                    Cursor::new(end, 0),
                    Span::Linewise,
                ))
            }
            Target::Selection => {
                let sel = ctx.selection();
                let span = if self.mode == VimMode::VisualLine {
                    Span::Linewise
                } else {
                    Span::Inclusive
                };
                let (a, b) = sel.ordered();
                Some(exec::edit_range(ctx.buffer, a, b, span))
            }
            Target::Motion(m) => {
                let resolved = {
                    let mctx = self.motion_ctx(ctx);
                    motion::resolve(m, cmd.count(), &mctx)
                }?;
                Some(exec::edit_range(
                    ctx.buffer,
                    cursor,
                    resolved.target,
                    resolved.span,
                ))
            }
            Target::Object(object) => {
                let range = {
                    let mctx = self.motion_ctx(ctx);
                    motion::resolve_object(object, &mctx)
                }?;
                Some(exec::edit_range(
                    ctx.buffer,
                    range.start,
                    range.end,
                    range.span,
                ))
            }
        }
    }

    fn apply_operator(
        &mut self,
        cmd: &Command,
        op: Operator,
        target: Target,
        ctx: &mut VimCtx,
    ) -> VimKeyResult {
        // In Visual mode, `i{obj}`/`a{obj}` extends the selection instead.
        if matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
            && matches!(target, Target::Object(_))
        {
            if let Target::Object(object) = target {
                let range = {
                    let mctx = self.motion_ctx(ctx);
                    motion::resolve_object(object, &mctx)
                };
                if let Some(range) = range {
                    ctx.set_selection(Selection::new(range.start, range.end));
                    return VimKeyResult::Handled(InputResult::CursorMoved);
                }
            }
            return VimKeyResult::Consumed;
        }

        let Some(range) = self.target_range(cmd, target, ctx) else {
            return VimKeyResult::Consumed;
        };
        let was_visual = matches!(self.mode, VimMode::Visual | VimMode::VisualLine);
        if was_visual {
            self.last_visual = Some(ctx.selection());
        }

        let result = match op {
            Operator::Yank => {
                let value = exec::range_value(ctx.buffer, &range);
                self.write_yank(cmd.register, value);
                // Vim leaves the cursor at the start of a yanked range.
                let line = range.start_line;
                let line_start = ctx.buffer.try_line_to_char(line).unwrap_or(0);
                ctx.set_cursor(Cursor::new(line, range.start.saturating_sub(line_start)));
                if was_visual {
                    self.mode = VimMode::Normal;
                }
                return VimKeyResult::Handled(InputResult::CursorMoved);
            }
            Operator::Delete | Operator::Change => {
                let value = exec::range_value(ctx.buffer, &range);
                self.write_delete(cmd.register, value);

                if op == Operator::Change && range.linewise {
                    // `cc` clears the line but keeps it, leaving insert on a
                    // blank line rather than deleting the line outright.
                    let line = range.start_line;
                    let text = motion::line_text(ctx.buffer, line);
                    let line_start = ctx.buffer.try_line_to_char(line).unwrap_or(0);
                    let indent = text.chars().take_while(|c| c.is_whitespace()).count();
                    let len = text.chars().count();
                    if len > indent {
                        ctx.buffer.remove(line_start + indent, len - indent);
                    }
                    // Remove any extra lines the count covered.
                    if range.end_line > range.start_line {
                        let extra = exec::edit_range(
                            ctx.buffer,
                            Cursor::new(line + 1, 0),
                            Cursor::new(range.end_line, 0),
                            Span::Linewise,
                        );
                        exec::delete_range(ctx.buffer, &extra);
                    }
                    ctx.set_cursor(Cursor::new(line, indent));
                } else {
                    let c = exec::delete_range(ctx.buffer, &range);
                    ctx.set_cursor(c);
                }

                if op == Operator::Change {
                    self.enter_insert();
                } else if was_visual {
                    self.mode = VimMode::Normal;
                }
                InputResult::TextChanged
            }
            Operator::Indent | Operator::Dedent => {
                let c = exec::shift_lines(
                    ctx.buffer,
                    range.start_line,
                    range.end_line,
                    op == Operator::Dedent,
                    self.shift_width,
                );
                ctx.set_cursor(c);
                if was_visual {
                    self.mode = VimMode::Normal;
                }
                InputResult::TextChanged
            }
            Operator::Lower | Operator::Upper | Operator::ToggleCase => {
                let how = match op {
                    Operator::Lower => CaseChange::Lower,
                    Operator::Upper => CaseChange::Upper,
                    _ => CaseChange::Toggle,
                };
                exec::change_case(ctx.buffer, &range, how);
                let line = range.start_line;
                let line_start = ctx.buffer.try_line_to_char(line).unwrap_or(0);
                ctx.set_cursor(Cursor::new(line, range.start.saturating_sub(line_start)));
                if was_visual {
                    self.mode = VimMode::Normal;
                }
                InputResult::TextChanged
            }
        };

        if op.is_change() {
            self.record_change(cmd);
        }
        VimKeyResult::Handled(result)
    }

    fn apply_simple(&mut self, cmd: &Command, simple: Simple, ctx: &mut VimCtx) -> VimKeyResult {
        let count = cmd.count();
        let cursor = ctx.cursor();

        match simple {
            Simple::Cancel => {
                self.pending.clear();
                if matches!(self.mode, VimMode::Visual | VimMode::VisualLine) {
                    self.last_visual = Some(ctx.selection());
                    self.mode = VimMode::Normal;
                    ctx.set_cursor(cursor);
                    return VimKeyResult::Handled(InputResult::CursorMoved);
                }
                VimKeyResult::Consumed
            }
            Simple::DeleteChar | Simple::DeleteCharBack => {
                let (from, to) = if simple == Simple::DeleteChar {
                    let len = motion::line_len(ctx.buffer, cursor.line);
                    if cursor.column >= len {
                        return VimKeyResult::Consumed;
                    }
                    (
                        cursor,
                        Cursor::new(cursor.line, (cursor.column + count).min(len)),
                    )
                } else {
                    if cursor.column == 0 {
                        return VimKeyResult::Consumed;
                    }
                    (
                        Cursor::new(cursor.line, cursor.column.saturating_sub(count)),
                        cursor,
                    )
                };
                let range = exec::edit_range(ctx.buffer, from, to, Span::Exclusive);
                let value = exec::range_value(ctx.buffer, &range);
                self.write_delete(cmd.register, value);
                let c = exec::delete_range(ctx.buffer, &range);
                ctx.set_cursor(c);
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            Simple::ReplaceChar(ch) => match exec::replace_chars(ctx.buffer, cursor, ch, count) {
                Some(c) => {
                    ctx.set_cursor(c);
                    self.record_change(cmd);
                    VimKeyResult::Handled(InputResult::TextChanged)
                }
                None => VimKeyResult::Consumed,
            },
            Simple::JoinLines => {
                let c = exec::join_lines(ctx.buffer, cursor.line, count.max(2));
                ctx.set_cursor(c);
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            Simple::ToggleCaseChar => {
                let len = motion::line_len(ctx.buffer, cursor.line);
                if cursor.column >= len {
                    return VimKeyResult::Consumed;
                }
                let end = Cursor::new(cursor.line, (cursor.column + count).min(len));
                let range = exec::edit_range(ctx.buffer, cursor, end, Span::Exclusive);
                exec::change_case(ctx.buffer, &range, CaseChange::Toggle);
                ctx.set_cursor(end);
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            Simple::Put { before } => {
                if let Some(name) = cmd.register {
                    if is_clipboard(name) {
                        self.effects.push(VimEffect::ClipboardPut { before, count });
                        return VimKeyResult::Consumed;
                    }
                }
                let value = self.registers.read(cmd.register);
                if value.is_empty() {
                    return VimKeyResult::Consumed;
                }
                let c = exec::put(ctx.buffer, cursor, &value, before, count);
                ctx.set_cursor(c);
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            Simple::Undo => {
                self.effects.push(VimEffect::Undo);
                VimKeyResult::Consumed
            }
            Simple::Redo => {
                self.effects.push(VimEffect::Redo);
                VimKeyResult::Consumed
            }
            Simple::SearchWord { reverse } => {
                self.effects.push(VimEffect::SearchWord { reverse });
                VimKeyResult::Consumed
            }
            Simple::RepeatChange => self.repeat_change(ctx),
        }
    }

    fn apply_mode_switch(
        &mut self,
        cmd: &Command,
        switch: ModeSwitch,
        ctx: &mut VimCtx,
    ) -> VimKeyResult {
        let cursor = ctx.cursor();

        match switch {
            ModeSwitch::InsertHere => {
                self.enter_insert();
                self.record_change(cmd);
                VimKeyResult::Consumed
            }
            ModeSwitch::InsertAfter => {
                let len = motion::line_len(ctx.buffer, cursor.line);
                ctx.set_cursor(Cursor::new(cursor.line, (cursor.column + 1).min(len)));
                self.enter_insert();
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::CursorMoved)
            }
            ModeSwitch::InsertFirstNonBlank => {
                let text = motion::line_text(ctx.buffer, cursor.line);
                let col = text
                    .chars()
                    .position(|c| !c.is_whitespace())
                    .unwrap_or_else(|| text.chars().count());
                ctx.set_cursor(Cursor::new(cursor.line, col));
                self.enter_insert();
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::CursorMoved)
            }
            ModeSwitch::InsertLineEnd => {
                let len = motion::line_len(ctx.buffer, cursor.line);
                ctx.set_cursor(Cursor::new(cursor.line, len));
                self.enter_insert();
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::CursorMoved)
            }
            ModeSwitch::OpenLine { above } => {
                // Match the indentation of the current line, as Vim does.
                let text = motion::line_text(ctx.buffer, cursor.line);
                let indent: String = text.chars().take_while(|c| c.is_whitespace()).collect();

                if above {
                    let pos = ctx.buffer.try_line_to_char(cursor.line).unwrap_or(0);
                    ctx.buffer.insert(pos, &format!("{indent}\n"));
                    ctx.set_cursor(Cursor::new(cursor.line, indent.chars().count()));
                } else {
                    let len = motion::line_len(ctx.buffer, cursor.line);
                    let pos = exec::char_pos(ctx.buffer, Cursor::new(cursor.line, len));
                    ctx.buffer.insert(pos, &format!("\n{indent}"));
                    ctx.set_cursor(Cursor::new(cursor.line + 1, indent.chars().count()));
                }
                self.enter_insert();
                self.record_change(cmd);
                VimKeyResult::Handled(InputResult::TextChanged)
            }
            ModeSwitch::Visual => {
                self.mode = VimMode::Visual;
                ctx.set_selection(Selection::new(cursor, cursor));
                VimKeyResult::Consumed
            }
            ModeSwitch::VisualLine => {
                self.mode = VimMode::VisualLine;
                let sel = Selection::new(cursor, cursor);
                ctx.set_selection(expand_lines(ctx.buffer, sel));
                VimKeyResult::Handled(InputResult::CursorMoved)
            }
            ModeSwitch::ReselectVisual => match self.last_visual {
                Some(sel) => {
                    self.mode = VimMode::Visual;
                    ctx.set_selection(sel);
                    VimKeyResult::Handled(InputResult::CursorMoved)
                }
                None => VimKeyResult::Consumed,
            },
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Insert sessions and dot-repeat
    // ─────────────────────────────────────────────────────────────────────────

    fn enter_insert(&mut self) {
        self.mode = VimMode::Insert;
        if !self.replaying {
            self.insert_capture = Some(String::new());
        }
    }

    fn leave_insert(&mut self, ctx: &mut VimCtx) {
        self.mode = VimMode::Normal;
        self.pending.clear();

        // Vim steps the cursor back one on leaving insert.
        let cursor = ctx.cursor();
        if cursor.column > 0 {
            ctx.set_cursor(Cursor::new(cursor.line, cursor.column - 1));
        }

        if let Some(text) = self.insert_capture.take() {
            if let Some(change) = self.last_change.as_mut() {
                change.inserted = text;
            }
        }
    }

    /// Records the change `.` will repeat.
    fn record_change(&mut self, cmd: &Command) {
        if self.replaying {
            return;
        }
        self.last_change = Some(LastChange {
            command: *cmd,
            inserted: String::new(),
        });
    }

    /// `.` — re-executes the last change, including any text typed with it.
    fn repeat_change(&mut self, ctx: &mut VimCtx) -> VimKeyResult {
        let Some(change) = self.last_change.clone() else {
            return VimKeyResult::Consumed;
        };

        self.replaying = true;
        let result = self.execute(change.command, ctx);

        // If the change opened an insert session, replay the typed text too.
        if self.mode == VimMode::Insert {
            if !change.inserted.is_empty() {
                let cursor = ctx.cursor();
                let pos = exec::char_pos(ctx.buffer, cursor);
                ctx.buffer.insert(pos, &change.inserted);
                let added = change.inserted.chars().count();
                // Text with newlines moves the cursor down; keep it simple and
                // place it after the inserted run on the same visual line.
                let newlines = change.inserted.matches('\n').count();
                if newlines == 0 {
                    ctx.set_cursor(Cursor::new(cursor.line, cursor.column + added));
                } else {
                    let last_run = change
                        .inserted
                        .rsplit('\n')
                        .next()
                        .map(|s| s.chars().count())
                        .unwrap_or(0);
                    ctx.set_cursor(Cursor::new(cursor.line + newlines, last_run));
                }
            }
            self.mode = VimMode::Normal;
            let cursor = ctx.cursor();
            if cursor.column > 0 {
                ctx.set_cursor(Cursor::new(cursor.line, cursor.column - 1));
            }
        }

        self.replaying = false;
        match result {
            VimKeyResult::Consumed => VimKeyResult::Handled(InputResult::TextChanged),
            other => other,
        }
    }
}

/// Grows a selection to cover whole lines, for Visual Line mode.
fn expand_lines(buffer: &TextBuffer, sel: Selection) -> Selection {
    let (start, end) = sel.ordered();
    let end_len = motion::line_len(buffer, end.line);
    if (sel.anchor.line, sel.anchor.column) <= (sel.head.line, sel.head.column) {
        Selection::new(Cursor::new(start.line, 0), Cursor::new(end.line, end_len))
    } else {
        Selection::new(Cursor::new(end.line, end_len), Cursor::new(start.line, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives real keystrokes through `VimState` against a real buffer.
    struct Vim {
        state: VimState,
        buffer: TextBuffer,
        selections: Vec<Selection>,
        view: ViewState,
    }

    impl Vim {
        fn new(text: &str, line: usize, column: usize) -> Self {
            Self {
                state: VimState::new(),
                buffer: TextBuffer::from_string(text),
                selections: vec![Selection::collapsed(Cursor::new(line, column))],
                view: ViewState::new(),
            }
        }

        /// Types printable characters, as `Event::Text` would deliver them.
        fn keys(&mut self, keys: &str) -> &mut Self {
            for c in keys.chars() {
                let mut ctx = VimCtx {
                    buffer: &mut self.buffer,
                    selections: &mut self.selections,
                    primary: 0,
                    view: &mut self.view,
                    visible_lines: 10,
                };
                self.state.handle_text(&c.to_string(), &mut ctx);
            }
            self
        }

        fn key(&mut self, key: Key) -> &mut Self {
            let mut ctx = VimCtx {
                buffer: &mut self.buffer,
                selections: &mut self.selections,
                primary: 0,
                view: &mut self.view,
                visible_lines: 10,
            };
            self.state.handle_key(key, &Modifiers::NONE, &mut ctx);
            self
        }

        fn ctrl(&mut self, key: Key) -> &mut Self {
            let mut ctx = VimCtx {
                buffer: &mut self.buffer,
                selections: &mut self.selections,
                primary: 0,
                view: &mut self.view,
                visible_lines: 10,
            };
            let modifiers = Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            };
            self.state.handle_key(key, &modifiers, &mut ctx);
            self
        }

        fn esc(&mut self) -> &mut Self {
            self.key(Key::Escape)
        }

        fn enter(&mut self) -> &mut Self {
            self.key(Key::Enter)
        }

        /// Simulates the editor inserting typed text while in Insert mode: Vim
        /// records it for dot-repeat, the editor performs the edit.
        fn insert(&mut self, text: &str) -> &mut Self {
            assert_eq!(self.state.mode, VimMode::Insert, "not in insert mode");
            let mut ctx = VimCtx {
                buffer: &mut self.buffer,
                selections: &mut self.selections,
                primary: 0,
                view: &mut self.view,
                visible_lines: 10,
            };
            self.state.handle_text(text, &mut ctx);
            let cursor = self.selections[0].head;
            let pos = exec::char_pos(&self.buffer, cursor);
            self.buffer.insert(pos, text);
            self.selections[0] = Selection::collapsed(Cursor::new(
                cursor.line,
                cursor.column + text.chars().count(),
            ));
            self
        }

        fn text(&self) -> String {
            self.buffer.to_string()
        }

        fn cursor(&self) -> (usize, usize) {
            let c = self.selections[0].head;
            (c.line, c.column)
        }

        fn effects(&mut self) -> Vec<VimEffect> {
            self.state.take_effects()
        }
    }

    // ── operators × motions ──────────────────────────────────────────────

    #[test]
    fn dw_deletes_a_word_and_de_deletes_through_its_end() {
        assert_eq!(Vim::new("hello world", 0, 0).keys("dw").text(), "world");
        assert_eq!(
            Vim::new("hello world", 0, 0).keys("de").text(),
            " world",
            "de is inclusive where dw is exclusive"
        );
    }

    #[test]
    fn counts_multiply_across_operator_and_motion() {
        assert_eq!(Vim::new("a b c d e", 0, 0).keys("d2w").text(), "c d e");
        assert_eq!(Vim::new("a b c d e", 0, 0).keys("2dw").text(), "c d e");
        assert_eq!(
            Vim::new("a b c d e f g", 0, 0).keys("2d3w").text(),
            "g",
            "2d3w deletes six words"
        );
    }

    #[test]
    fn dollar_and_caret_work_as_operator_targets() {
        assert_eq!(Vim::new("hello world", 0, 5).keys("d$").text(), "hello");
    }

    #[test]
    fn dd_deletes_whole_lines_and_honours_a_count() {
        assert_eq!(Vim::new("a\nb\nc", 0, 0).keys("dd").text(), "b\nc");
        assert_eq!(Vim::new("a\nb\nc\nd", 0, 0).keys("3dd").text(), "d");
    }

    #[test]
    fn cc_clears_the_line_but_keeps_it() {
        let mut v = Vim::new("    foo\nbar", 0, 5);
        v.keys("cc");
        assert_eq!(v.text(), "    \nbar", "indent kept, content gone");
        assert_eq!(v.state.mode, VimMode::Insert);
    }

    #[test]
    fn text_objects_operate_inside_delimiters() {
        assert_eq!(
            Vim::new("say \"hi\" ok", 0, 6).keys("di\"").text(),
            "say \"\" ok"
        );
        assert_eq!(
            Vim::new("say \"hi\" ok", 0, 6).keys("da\"").text(),
            "say  ok"
        );
        assert_eq!(Vim::new("f(a, b)", 0, 3).keys("di(").text(), "f()");
        assert_eq!(Vim::new("foo bar baz", 0, 5).keys("diw").text(), "foo  baz");
        assert_eq!(Vim::new("foo bar baz", 0, 4).keys("daw").text(), "foo baz");
    }

    #[test]
    fn ci_quote_enters_insert_with_the_inside_removed() {
        let mut v = Vim::new("x = \"old\"", 0, 6);
        v.keys("ci\"");
        assert_eq!(v.text(), "x = \"\"");
        assert_eq!(v.state.mode, VimMode::Insert);
        v.insert("new").esc();
        assert_eq!(v.text(), "x = \"new\"");
        assert_eq!(v.state.mode, VimMode::Normal);
    }

    #[test]
    fn ct_changes_up_to_a_character() {
        let mut v = Vim::new("foo bar", 0, 0);
        v.keys("ct ");
        assert_eq!(v.text(), " bar");
    }

    #[test]
    fn percent_jumps_between_brackets() {
        let mut v = Vim::new("if (a) {\n  b\n}", 0, 3);
        v.keys("%");
        assert_eq!(v.cursor(), (0, 5), "( jumps to )");
    }

    #[test]
    fn d_percent_deletes_through_the_matching_bracket() {
        assert_eq!(Vim::new("f(a)g", 0, 1).keys("d%").text(), "fg");
    }

    // ── indent, case, join ───────────────────────────────────────────────

    #[test]
    fn shift_operators_indent_and_dedent() {
        assert_eq!(Vim::new("a\nb", 0, 0).keys(">>").text(), "    a\nb");
        assert_eq!(
            Vim::new("a\nb\nc", 0, 0).keys("3>>").text(),
            "    a\n    b\n    c"
        );
        assert_eq!(Vim::new("        a", 0, 0).keys("<<").text(), "    a");
    }

    #[test]
    fn case_operators_apply_over_a_motion() {
        assert_eq!(Vim::new("hello", 0, 0).keys("gUU").text(), "HELLO");
        assert_eq!(Vim::new("HELLO", 0, 0).keys("guu").text(), "hello");
        assert_eq!(
            Vim::new("hello there", 0, 0).keys("gUw").text(),
            "HELLO there"
        );
        assert_eq!(Vim::new("aBc", 0, 0).keys("3~").text(), "AbC");
    }

    #[test]
    fn join_merges_lines() {
        assert_eq!(Vim::new("foo\n   bar", 0, 0).keys("J").text(), "foo bar");
        assert_eq!(Vim::new("a\nb\nc", 0, 0).keys("3J").text(), "a b c");
    }

    #[test]
    fn x_and_r_edit_single_characters() {
        assert_eq!(Vim::new("abc", 0, 1).keys("x").text(), "ac");
        assert_eq!(Vim::new("abc", 0, 0).keys("2x").text(), "c");
        assert_eq!(Vim::new("abc", 0, 0).keys("rz").text(), "zbc");
        assert_eq!(Vim::new("abc", 0, 1).keys("X").text(), "bc");
    }

    // ── registers ────────────────────────────────────────────────────────

    #[test]
    fn yank_and_put_round_trip_linewise() {
        let mut v = Vim::new("one\ntwo", 0, 0);
        v.keys("yyp");
        assert_eq!(v.text(), "one\none\ntwo");
    }

    #[test]
    fn a_delete_does_not_clobber_the_yank_register() {
        let mut v = Vim::new("keep\ndrop", 0, 0);
        v.keys("yy");
        v.keys("jdd");
        v.keys("\"0p");
        assert!(v.text().contains("keep\nkeep"), "got {:?}", v.text());
    }

    #[test]
    fn the_blackhole_register_discards() {
        let mut v = Vim::new("keep\ndrop", 0, 0);
        v.keys("yy");
        v.keys("j\"_dd");
        v.keys("p");
        assert_eq!(v.text(), "keep\nkeep", "unnamed still holds the yank");
    }

    // ── dot repeat ───────────────────────────────────────────────────────

    #[test]
    fn dot_repeats_the_last_change() {
        let mut v = Vim::new("a a a a", 0, 0);
        v.keys("dw");
        assert_eq!(v.text(), "a a a");
        v.keys(".");
        assert_eq!(v.text(), "a a");
        v.keys(".");
        assert_eq!(v.text(), "a");
    }

    #[test]
    fn dot_repeats_an_insert_session_including_the_typed_text() {
        let mut v = Vim::new("foo\nfoo", 0, 0);
        v.keys("i");
        v.insert("X").esc();
        assert_eq!(v.text(), "Xfoo\nfoo");
        v.keys("j0");
        v.keys(".");
        assert_eq!(
            v.text(),
            "Xfoo\nXfoo",
            "the typed text is part of the repeated change"
        );
    }

    #[test]
    fn dot_does_not_repeat_a_pure_motion() {
        let mut v = Vim::new("abc def ghi", 0, 0);
        v.keys("dw");
        assert_eq!(v.text(), "def ghi");
        v.keys("w"); // a motion — must not become the change `.` repeats
        v.keys(".");
        assert_eq!(v.text(), "def ", "the dw is what repeats, not the w");
    }

    // ── visual mode ──────────────────────────────────────────────────────

    #[test]
    fn visual_mode_selects_and_operates() {
        let mut v = Vim::new("hello world", 0, 0);
        v.keys("vlld");
        assert_eq!(v.text(), "lo world");
        assert_eq!(v.state.mode, VimMode::Normal, "operator leaves visual mode");
    }

    #[test]
    fn visual_line_mode_operates_on_whole_lines() {
        let mut v = Vim::new("a\nb\nc", 0, 0);
        v.keys("Vjd");
        assert_eq!(v.text(), "c");
    }

    #[test]
    fn visual_mode_text_objects_extend_the_selection() {
        let mut v = Vim::new("say \"hi there\" ok", 0, 7);
        v.keys("vi\"");
        v.keys("d");
        assert_eq!(v.text(), "say \"\" ok");
    }

    #[test]
    fn gv_reselects_the_previous_visual_range() {
        let mut v = Vim::new("hello", 0, 0);
        v.keys("vll").esc();
        v.keys("gv");
        assert_eq!(v.state.mode, VimMode::Visual);
        v.keys("d");
        assert_eq!(v.text(), "lo");
    }

    #[test]
    fn escape_leaves_visual_mode_without_editing() {
        let mut v = Vim::new("hello", 0, 0);
        v.keys("vll").esc();
        assert_eq!(v.state.mode, VimMode::Normal);
        assert_eq!(v.text(), "hello");
    }

    // ── insert-mode entry points ─────────────────────────────────────────

    #[test]
    fn insert_entry_points_place_the_cursor_correctly() {
        let mut v = Vim::new("  foo", 0, 3);
        v.keys("I");
        assert_eq!(v.cursor(), (0, 2), "I goes to the first non-blank");

        let mut v = Vim::new("foo", 0, 0);
        v.keys("A");
        assert_eq!(v.cursor(), (0, 3), "A goes to end of line");
    }

    #[test]
    fn open_line_matches_the_current_indent() {
        let mut v = Vim::new("    foo", 0, 0);
        v.keys("o");
        assert_eq!(v.text(), "    foo\n    ");
        assert_eq!(v.cursor(), (1, 4));

        let mut v = Vim::new("  bar", 0, 0);
        v.keys("O");
        assert_eq!(v.text(), "  \n  bar");
        assert_eq!(v.cursor(), (0, 2));
    }

    // ── the passthrough guarantee ────────────────────────────────────────

    #[test]
    fn app_shortcuts_still_reach_the_editor_in_normal_mode() {
        // The bug class that made the arrow keys dead: unrecognised input must
        // not be swallowed by a catch-all.
        for key in [Key::S, Key::F, Key::P, Key::Z, Key::G, Key::O] {
            let mut v = Vim::new("x", 0, 0);
            v.ctrl(key);
            let mut vv = Vim::new("x", 0, 0);
            let mut ctx = VimCtx {
                buffer: &mut vv.buffer,
                selections: &mut vv.selections,
                primary: 0,
                view: &mut vv.view,
                visible_lines: 10,
            };
            let modifiers = Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            };
            let got = vv.state.handle_key(key, &modifiers, &mut ctx);
            assert_eq!(
                got,
                VimKeyResult::Passthrough,
                "Ctrl+{key:?} must reach the app"
            );
            assert_eq!(v.text(), "x");
        }
    }

    #[test]
    fn arrow_keys_navigate_in_normal_mode() {
        let cases = [
            (Key::ArrowLeft, (1, 3)),
            (Key::ArrowRight, (1, 5)),
            (Key::ArrowUp, (0, 4)),
            (Key::ArrowDown, (2, 4)),
        ];
        for (key, want) in cases {
            let mut v = Vim::new("Hello world\nSecond line\nThird", 1, 4);
            v.key(key);
            assert_eq!(v.cursor(), want, "{key:?}");
        }
    }

    // ── cursor shape ─────────────────────────────────────────────────────

    #[test]
    fn normal_and_visual_modes_use_a_block_cursor_and_insert_uses_a_bar() {
        assert!(VimMode::Normal.uses_block_cursor());
        assert!(VimMode::Visual.uses_block_cursor());
        assert!(VimMode::VisualLine.uses_block_cursor());
        assert!(VimMode::CommandLine.uses_block_cursor());
        assert!(
            !VimMode::Insert.uses_block_cursor(),
            "Insert sits between characters, so it draws a bar"
        );
    }

    #[test]
    fn the_cursor_shape_follows_the_mode_as_the_user_switches() {
        let mut v = Vim::new("hello", 0, 0);
        assert!(v.state.mode.uses_block_cursor(), "starts in Normal");

        v.keys("i");
        assert!(!v.state.mode.uses_block_cursor(), "i → bar");

        v.esc();
        assert!(v.state.mode.uses_block_cursor(), "Esc → block");

        v.keys("v");
        assert!(v.state.mode.uses_block_cursor(), "visual → block");

        v.esc();
        v.keys("A");
        assert!(!v.state.mode.uses_block_cursor(), "A → bar");
    }

    #[test]
    fn a_change_operator_leaves_the_cursor_as_a_bar() {
        // `ci"` ends in Insert mode, so the shape must follow.
        let mut v = Vim::new("x = \"old\"", 0, 6);
        v.keys("ci\"");
        assert_eq!(v.state.mode, VimMode::Insert);
        assert!(!v.state.mode.uses_block_cursor());
    }

    #[test]
    fn arrow_keys_pass_through_in_insert_mode() {
        // In Insert mode the arrows belong to the standard editor, which is
        // grapheme-aware; Vim must not intercept them.
        let mut v = Vim::new("hello", 0, 0);
        v.keys("i");
        assert_eq!(v.state.mode, VimMode::Insert);

        for key in [
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
        ] {
            let mut ctx = VimCtx {
                buffer: &mut v.buffer,
                selections: &mut v.selections,
                primary: 0,
                view: &mut v.view,
                visible_lines: 10,
            };
            let got = v.state.handle_key(key, &Modifiers::NONE, &mut ctx);
            assert_eq!(got, VimKeyResult::Passthrough, "{key:?} in Insert mode");
        }
    }

    #[test]
    fn home_and_end_act_as_zero_and_dollar_in_normal_mode() {
        let mut v = Vim::new("    hello world", 0, 7);
        v.key(Key::Home);
        assert_eq!(v.cursor(), (0, 0));
        v.key(Key::End);
        assert_eq!(v.cursor(), (0, 15));
    }

    #[test]
    fn arrow_keys_extend_the_selection_in_visual_mode() {
        let mut v = Vim::new("hello\nworld", 0, 2);
        v.keys("v");
        v.key(Key::ArrowRight).key(Key::ArrowDown);
        assert_eq!(v.selections[0].anchor, Cursor::new(0, 2));
        assert_eq!(v.selections[0].head, Cursor::new(1, 3));
    }

    #[test]
    fn typed_text_does_not_leak_into_the_buffer_in_normal_mode() {
        let mut v = Vim::new("x", 0, 0);
        v.keys("qQzZ");
        assert_eq!(
            v.text(),
            "x",
            "unrecognised characters must not be inserted"
        );
    }

    // ── ex commands ──────────────────────────────────────────────────────

    #[test]
    fn ex_write_and_quit_become_effects() {
        let mut v = Vim::new("x", 0, 0);
        v.keys(":w").enter();
        assert_eq!(
            v.effects(),
            vec![VimEffect::Write {
                path: None,
                quit: false
            }]
        );

        v.keys(":q!").enter();
        assert_eq!(
            v.effects(),
            vec![VimEffect::Quit {
                force: true,
                all: false
            }]
        );
    }

    #[test]
    fn ex_goto_line_moves_the_cursor() {
        let mut v = Vim::new("a\nb\nc\nd", 0, 0);
        v.keys(":3").enter();
        assert_eq!(v.cursor(), (2, 0));
        v.keys(":$").enter();
        assert_eq!(v.cursor(), (3, 0));
    }

    #[test]
    fn ex_substitute_edits_the_buffer() {
        let mut v = Vim::new("foo foo\nfoo", 0, 0);
        v.keys(":s/foo/bar/").enter();
        assert_eq!(v.text(), "bar foo\nfoo", "no g flag: first match only");

        let mut v = Vim::new("foo foo\nfoo", 0, 0);
        v.keys(":%s/foo/bar/g").enter();
        assert_eq!(v.text(), "bar bar\nbar");
    }

    #[test]
    fn ex_substitute_reports_a_pattern_that_matches_nothing() {
        let mut v = Vim::new("abc", 0, 0);
        v.keys(":%s/zzz/x/").enter();
        assert_eq!(v.text(), "abc");
        assert!(
            matches!(v.effects().first(), Some(VimEffect::Error(_))),
            "a failed substitute must say so rather than silently doing nothing"
        );
    }

    #[test]
    fn an_unknown_ex_command_reports_an_error() {
        let mut v = Vim::new("x", 0, 0);
        v.keys(":frobnicate").enter();
        match v.effects().first() {
            Some(VimEffect::Error(msg)) => assert!(msg.contains("not an editor command")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ex_range_delete_removes_the_lines() {
        let mut v = Vim::new("a\nb\nc\nd", 0, 0);
        v.keys(":2,3d").enter();
        assert_eq!(v.text(), "a\nd");
    }

    #[test]
    fn the_command_line_can_be_cancelled() {
        let mut v = Vim::new("x", 0, 0);
        v.keys(":w");
        assert_eq!(v.state.mode, VimMode::CommandLine);
        assert_eq!(v.state.cmdline_text().as_deref(), Some(":w"));
        v.esc();
        assert_eq!(v.state.mode, VimMode::Normal);
        assert!(v.effects().is_empty(), "cancelling runs nothing");
    }

    #[test]
    fn search_entry_emits_a_search_effect() {
        let mut v = Vim::new("x", 0, 0);
        v.keys("/foo").enter();
        assert_eq!(
            v.effects(),
            vec![VimEffect::Search {
                pattern: "foo".into(),
                forward: true
            }]
        );
    }

    #[test]
    fn undo_and_redo_are_effects_for_the_application() {
        let mut v = Vim::new("x", 0, 0);
        v.keys("u");
        assert_eq!(v.effects(), vec![VimEffect::Undo]);
        v.ctrl(Key::R);
        assert_eq!(v.effects(), vec![VimEffect::Redo]);
    }

    #[test]
    fn clipboard_registers_become_effects_rather_than_stored_text() {
        let mut v = Vim::new("hello", 0, 0);
        v.keys("\"+yy");
        assert_eq!(
            v.effects(),
            vec![VimEffect::ClipboardWrite("hello".into())],
            "the app owns the system clipboard"
        );
    }

    #[test]
    fn set_shiftwidth_changes_what_the_shift_operators_do() {
        let mut v = Vim::new("a", 0, 0);
        v.keys(":set shiftwidth=2").enter();
        v.effects();
        v.keys(">>");
        assert_eq!(v.text(), "  a");
    }

    // ── pending state ────────────────────────────────────────────────────

    #[test]
    fn a_partial_command_is_reported_for_the_status_bar() {
        let mut v = Vim::new("x", 0, 0);
        v.keys("2d");
        assert_eq!(v.state.pending_text(), "2d");
        v.esc();
        assert_eq!(v.state.pending_text(), "");
    }

    #[test]
    fn the_pending_buffer_cannot_grow_without_bound() {
        let mut v = Vim::new("x", 0, 0);
        v.keys(&"d".repeat(100));
        assert!(
            v.state.pending_text().chars().count() <= MAX_PENDING,
            "pending buffer is capped"
        );
    }
}
