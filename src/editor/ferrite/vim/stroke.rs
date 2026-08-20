//! Keystroke normalisation — stage 1 of the Vim pipeline.
//!
//! egui delivers a keypress as *two* events: an [`egui::Event::Key`] carrying a
//! logical [`egui::Key`], and an [`egui::Event::Text`] carrying the character the
//! active keyboard layout (and any IME) actually produced. Dispatching on `Key`
//! alone cannot express most of Vim's command set — `egui::Key` has no variant for
//! `$ ^ % * # ~ > < ( ) _` — and what variants it does have depend on the layout.
//!
//! So the pipeline works on one canonical stream instead:
//!
//! 1. Printable characters come from `Event::Text` only.
//! 2. `Event::Key` for a plain printable key is **ignored** in Normal/Visual mode,
//!    otherwise every key would be processed twice.
//! 3. Non-printable keys and Ctrl chords come from `Event::Key` only —
//!    `Event::Text` is never emitted for Enter, nor for chords.
//! 4. IME output arrives as text, so it lands in [`Stroke::Char`] like any
//!    other character.
//!
//! See `docs/VIM_MODE_DESIGN.md` §2.1.

use egui::{Key, Modifiers};

/// A non-printable key that produces no `Event::Text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
}

/// A Vim-level keystroke — the only thing the parser ever sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stroke {
    /// A printable character, layout- and IME-resolved.
    Char(char),
    /// A non-printable key.
    Named(NamedKey),
    /// A Ctrl chord over a character, e.g. `Ctrl+R`.
    Ctrl(char),
}

impl Stroke {
    /// The character this stroke stands for, if any.
    pub fn as_char(self) -> Option<char> {
        match self {
            Stroke::Char(c) => Some(c),
            _ => None,
        }
    }
}

/// Maps an `egui::Key` to the character it stands for in a Ctrl chord.
///
/// Only letters and digits are needed: Vim's Ctrl chords are all
/// `Ctrl+<letter>` (`Ctrl+R`, `Ctrl+V`, `Ctrl+D`, …).
fn chord_char(key: Key) -> Option<char> {
    Some(match key {
        Key::A => 'a',
        Key::B => 'b',
        Key::C => 'c',
        Key::D => 'd',
        Key::E => 'e',
        Key::F => 'f',
        Key::G => 'g',
        Key::H => 'h',
        Key::I => 'i',
        Key::J => 'j',
        Key::K => 'k',
        Key::L => 'l',
        Key::M => 'm',
        Key::N => 'n',
        Key::O => 'o',
        Key::P => 'p',
        Key::Q => 'q',
        Key::R => 'r',
        Key::S => 's',
        Key::T => 't',
        Key::U => 'u',
        Key::V => 'v',
        Key::W => 'w',
        Key::X => 'x',
        Key::Y => 'y',
        Key::Z => 'z',
        _ => return None,
    })
}

/// Maps an `egui::Key` to a [`NamedKey`], for keys that emit no text.
fn named(key: Key) -> Option<NamedKey> {
    Some(match key {
        Key::Escape => NamedKey::Escape,
        Key::Enter => NamedKey::Enter,
        Key::Tab => NamedKey::Tab,
        Key::Backspace => NamedKey::Backspace,
        Key::Delete => NamedKey::Delete,
        Key::Insert => NamedKey::Insert,
        Key::Home => NamedKey::Home,
        Key::End => NamedKey::End,
        Key::PageUp => NamedKey::PageUp,
        Key::PageDown => NamedKey::PageDown,
        Key::ArrowLeft => NamedKey::Left,
        Key::ArrowRight => NamedKey::Right,
        Key::ArrowUp => NamedKey::Up,
        Key::ArrowDown => NamedKey::Down,
        _ => return None,
    })
}

/// Converts an `Event::Key` into a [`Stroke`], or `None` when the key carries no
/// Vim meaning of its own.
///
/// Returning `None` for plain printable keys is deliberate — rule 2 above. Those
/// reach the parser through [`strokes_from_text`] instead, so that `$` works on
/// every keyboard layout rather than only the ones where egui happens to expose a
/// matching `Key` variant.
pub fn stroke_from_key(key: Key, modifiers: &Modifiers) -> Option<Stroke> {
    // Alt chords are never Vim commands here; let the app have them.
    if modifiers.alt {
        return None;
    }

    if modifiers.ctrl || modifiers.command {
        return chord_char(key).map(Stroke::Ctrl);
    }

    named(key).map(Stroke::Named)
}

/// Converts an `Event::Text` payload into strokes — one per character.
///
/// A single text event can carry more than one character (IME commits, pasted
/// keyboard input), and each is a separate Vim keystroke.
pub fn strokes_from_text(text: &str) -> impl Iterator<Item = Stroke> + '_ {
    text.chars().filter(|c| !c.is_control()).map(Stroke::Char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_keys_are_not_strokes_so_they_are_not_handled_twice() {
        // egui emits both Key::D and Text("d") for one press of `d`. Only the
        // text event may reach the parser.
        for key in [Key::D, Key::W, Key::Num4, Key::Semicolon, Key::Slash] {
            assert_eq!(
                stroke_from_key(key, &Modifiers::NONE),
                None,
                "{key:?} must arrive as text, not as a key"
            );
        }
    }

    #[test]
    fn non_printable_keys_become_named_strokes() {
        let cases = [
            (Key::Escape, NamedKey::Escape),
            (Key::Enter, NamedKey::Enter),
            (Key::ArrowLeft, NamedKey::Left),
            (Key::ArrowDown, NamedKey::Down),
            (Key::Home, NamedKey::Home),
            (Key::PageUp, NamedKey::PageUp),
        ];
        for (key, want) in cases {
            assert_eq!(
                stroke_from_key(key, &Modifiers::NONE),
                Some(Stroke::Named(want)),
                "{key:?}"
            );
        }
    }

    #[test]
    fn ctrl_chords_become_ctrl_strokes() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            stroke_from_key(Key::R, &ctrl),
            Some(Stroke::Ctrl('r')),
            "Ctrl+R is redo"
        );
        assert_eq!(stroke_from_key(Key::V, &ctrl), Some(Stroke::Ctrl('v')));
    }

    #[test]
    fn alt_chords_are_left_to_the_application() {
        let alt = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        assert_eq!(stroke_from_key(Key::ArrowUp, &alt), None);
    }

    #[test]
    fn text_events_yield_one_stroke_per_character() {
        let got: Vec<_> = strokes_from_text("d$").collect();
        assert_eq!(got, vec![Stroke::Char('d'), Stroke::Char('$')]);
    }

    #[test]
    fn the_command_alphabet_egui_key_cannot_spell_survives_as_text() {
        // The whole point of stage 1: these have no `egui::Key` variant at all.
        for c in ['$', '^', '%', '*', '#', '~', '>', '<', '(', ')', '_'] {
            let got: Vec<_> = strokes_from_text(&c.to_string()).collect();
            assert_eq!(got, vec![Stroke::Char(c)], "{c} must reach the parser");
        }
    }

    #[test]
    fn control_characters_in_text_are_dropped() {
        let got: Vec<_> = strokes_from_text("\u{1b}").collect();
        assert!(got.is_empty(), "escape arrives as a Named key, not text");
    }
}
