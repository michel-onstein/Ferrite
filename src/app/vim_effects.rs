//! Applies Vim commands that the editor cannot carry out itself.
//!
//! The editor never calls into application types — `:w` reaching into
//! `ShortcutCommand` would break the planned `ferrite-editor` crate extraction.
//! Instead it reports intent as a [`VimEffect`] and this module maps each one
//! onto the handler the ribbon and keyboard shortcuts already use, so `:w` and
//! Ctrl+S take exactly the same path.
//!
//! See `docs/VIM_MODE_DESIGN.md` §2.6.

use std::path::PathBuf;

use eframe::egui;

use crate::editor::VimEffect;
use crate::editor::VimSetValue;

use super::FerriteApp;

impl FerriteApp {
    /// Carries out the Vim effects produced during this frame.
    pub(crate) fn apply_vim_effects(&mut self, ctx: &egui::Context, effects: Vec<VimEffect>) {
        for effect in effects {
            self.apply_vim_effect(ctx, effect);
        }
    }

    fn apply_vim_effect(&mut self, ctx: &egui::Context, effect: VimEffect) {
        let app_time = self.get_app_time();

        match effect {
            // `u` / `Ctrl+R` — the undo stack lives on the Tab, not the editor.
            VimEffect::Undo => self.handle_undo(),
            VimEffect::Redo => self.handle_redo(),

            // `:w`, `:w file`, `:wq`, `:x`
            VimEffect::Write { path, quit } => {
                match path {
                    Some(_) => {
                        // `:w {file}` — route through Save As so the existing
                        // path/encoding handling applies.
                        self.handle_save_as_file();
                    }
                    None => self.handle_save_file(),
                }
                if quit {
                    self.handle_close_current_tab(ctx);
                }
            }

            // `:q`, `:q!`, `:qa`
            VimEffect::Quit { all, .. } => {
                if all {
                    // `:qa` closes the window, which runs the app's normal
                    // on-close flow (including the unsaved-changes prompt).
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    self.handle_close_current_tab(ctx);
                }
            }

            // `:e {file}`
            VimEffect::Edit { path } => {
                let path = PathBuf::from(path);
                if let Err(e) = self.open_file_smart(path.clone(), true, Some(app_time)) {
                    self.state.show_toast(
                        format!("E484: can't open {}: {e}", path.display()),
                        app_time,
                        4.0,
                    );
                }
            }

            // `/pattern` and `?pattern` feed the existing find panel, so Vim
            // search and Ctrl+F share one match list and one highlight state.
            VimEffect::Search { pattern, forward } => {
                self.state.ui.find_state.search_term = pattern;
                self.handle_open_find(false);
                if !forward {
                    self.handle_find_prev();
                }
            }
            VimEffect::SearchNext { reverse } => {
                if reverse {
                    self.handle_find_prev();
                } else {
                    self.handle_find_next();
                }
            }
            // `*` / `#` — search for the word under the cursor.
            VimEffect::SearchWord { reverse } => match self.word_under_cursor() {
                Some(word) => {
                    self.state.ui.find_state.search_term = word;
                    self.handle_open_find(false);
                    if reverse {
                        self.handle_find_prev();
                    }
                }
                None => {
                    self.state
                        .show_toast("E348: no string under cursor".to_string(), app_time, 2.0)
                }
            },
            VimEffect::ClearSearchHighlight => {
                self.state.ui.find_state.search_term.clear();
                self.state.ui.show_find_replace = false;
            }

            // `"+y` — the system clipboard belongs to the application.
            VimEffect::ClipboardWrite(text) => {
                ctx.copy_text(text);
            }
            // `"+p` — reading the clipboard is not wired up; Ctrl+V still works.
            VimEffect::ClipboardPut { .. } => {
                self.state.show_toast(
                    "\"+p is not supported yet — use Ctrl+V".to_string(),
                    app_time,
                    3.0,
                );
            }

            // `:set`
            VimEffect::SetOption { option, value } => {
                self.apply_vim_set_option(&option, value, app_time);
            }

            VimEffect::Message(msg) => self.state.show_toast(msg, app_time, 2.0),
            VimEffect::Error(msg) => self.state.show_toast(msg, app_time, 4.0),
        }
    }

    /// Maps a `:set` option onto a real setting. An unknown option reports an
    /// error rather than silently doing nothing.
    fn apply_vim_set_option(&mut self, option: &str, value: VimSetValue, app_time: f64) {
        /// Resolves on/off/toggle against the current value.
        fn resolve(current: bool, value: &VimSetValue) -> Option<bool> {
            match value {
                VimSetValue::On => Some(true),
                VimSetValue::Off => Some(false),
                VimSetValue::Toggle => Some(!current),
                VimSetValue::Number(_) => None,
            }
        }

        let settings = &mut self.state.settings;

        let applied = match option {
            "number" | "nu" => resolve(settings.show_line_numbers, &value).map(|v| {
                settings.show_line_numbers = v;
                format!("number={v}")
            }),
            "wrap" => resolve(settings.word_wrap, &value).map(|v| {
                settings.word_wrap = v;
                format!("wrap={v}")
            }),
            "ignorecase" | "ic" => {
                // Vim's `ignorecase` is the inverse of the find panel's flag.
                let current = !self.state.ui.find_state.case_sensitive;
                resolve(current, &value).map(|v| {
                    self.state.ui.find_state.case_sensitive = !v;
                    format!("ignorecase={v}")
                })
            }
            "expandtab" | "et" => resolve(settings.use_spaces, &value).map(|v| {
                settings.use_spaces = v;
                format!("expandtab={v}")
            }),
            "tabstop" | "ts" | "shiftwidth" | "sw" => match value {
                VimSetValue::Number(n) => {
                    let n = n.clamp(1, 16);
                    settings.tab_size = n as u8;
                    Some(format!("tabstop={n}"))
                }
                _ => None,
            },
            _ => {
                self.state
                    .show_toast(format!("E518: unknown option: {option}"), app_time, 3.0);
                return;
            }
        };

        match applied {
            Some(msg) => self.state.show_toast(msg, app_time, 1.5),
            None => self
                .state
                .show_toast(format!("E521: wrong value for {option}"), app_time, 3.0),
        }
    }

    /// The word under the cursor, for `*` and `#`.
    fn word_under_cursor(&self) -> Option<String> {
        let tab = self.state.active_tab()?;
        let (line_idx, column) = tab.cursor_position;
        let line = tab.content.lines().nth(line_idx)?;
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            return None;
        }

        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let col = column.min(chars.len() - 1);
        if !is_word(chars[col]) {
            return None;
        }

        let mut start = col;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < chars.len() && is_word(chars[end + 1]) {
            end += 1;
        }

        Some(chars[start..=end].iter().collect())
    }
}
