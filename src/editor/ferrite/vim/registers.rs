//! Vim registers.
//!
//! Replaces the single `yank_register: String` with the set Vim users actually
//! reach for: the unnamed register, `"a`–`"z` (uppercase appends), the yank
//! register `"0`, the small-delete register `"-`, the numbered shift register
//! `"1`–`"9`, and the blackhole `"_`.
//!
//! `"+` and `"*` are *not* stored here — they are the system clipboard, and the
//! executor turns a read or write of them into an effect for the application to
//! carry out. See `docs/VIM_MODE_DESIGN.md` §5.

/// A register's contents. The linewise flag travels *with* the text, because
/// whether `p` opens a new line is a property of what was yanked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisterValue {
    pub text: String,
    pub linewise: bool,
}

impl RegisterValue {
    pub fn new(text: impl Into<String>, linewise: bool) -> Self {
        Self {
            text: text.into(),
            linewise,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The register file.
#[derive(Debug, Clone, Default)]
pub struct Registers {
    unnamed: RegisterValue,
    named: Vec<(char, RegisterValue)>,
    yank: RegisterValue,
    small_delete: RegisterValue,
    numbered: Vec<RegisterValue>,
}

/// Whether a register name refers to the system clipboard.
pub fn is_clipboard(name: char) -> bool {
    name == '+' || name == '*'
}

impl Registers {
    pub fn new() -> Self {
        Self::default()
    }

    fn named_slot(&mut self, name: char) -> &mut RegisterValue {
        let lower = name.to_ascii_lowercase();
        if let Some(idx) = self.named.iter().position(|(c, _)| *c == lower) {
            &mut self.named[idx].1
        } else {
            self.named.push((lower, RegisterValue::default()));
            let last = self.named.len() - 1;
            &mut self.named[last].1
        }
    }

    /// Records a yank. Goes to the unnamed register and to `"0`, or to an
    /// explicitly named register instead.
    pub fn write_yank(&mut self, register: Option<char>, value: RegisterValue) {
        match register {
            Some('_') => {}
            Some(name) if name.is_ascii_alphabetic() => {
                if name.is_ascii_uppercase() {
                    let slot = self.named_slot(name);
                    slot.text.push_str(&value.text);
                    slot.linewise = slot.linewise || value.linewise;
                    let appended = slot.clone();
                    self.unnamed = appended;
                } else {
                    *self.named_slot(name) = value.clone();
                    self.unnamed = value;
                }
            }
            _ => {
                self.yank = value.clone();
                self.unnamed = value;
            }
        }
    }

    /// Records a delete. Line-wise deletes shift through `"1`–`"9`; small
    /// (within-line) deletes go to `"-`. Both land in the unnamed register.
    pub fn write_delete(&mut self, register: Option<char>, value: RegisterValue) {
        match register {
            Some('_') => {}
            Some(name) if name.is_ascii_alphabetic() => {
                if name.is_ascii_uppercase() {
                    let slot = self.named_slot(name);
                    slot.text.push_str(&value.text);
                    slot.linewise = slot.linewise || value.linewise;
                    let appended = slot.clone();
                    self.unnamed = appended;
                } else {
                    *self.named_slot(name) = value.clone();
                    self.unnamed = value;
                }
            }
            _ => {
                if value.linewise {
                    self.numbered.insert(0, value.clone());
                    self.numbered.truncate(9);
                } else {
                    self.small_delete = value.clone();
                }
                self.unnamed = value;
            }
        }
    }

    /// Reads a register for `p`/`P`. `None` means the unnamed register.
    pub fn read(&self, register: Option<char>) -> RegisterValue {
        match register {
            None | Some('"') => self.unnamed.clone(),
            Some('_') => RegisterValue::default(),
            Some('0') => self.yank.clone(),
            Some('-') => self.small_delete.clone(),
            Some(d) if d.is_ascii_digit() => {
                let idx = d.to_digit(10).unwrap_or(0) as usize;
                self.numbered
                    .get(idx.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default()
            }
            Some(name) if name.is_ascii_alphabetic() => {
                let lower = name.to_ascii_lowercase();
                self.named
                    .iter()
                    .find(|(c, _)| *c == lower)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            }
            _ => RegisterValue::default(),
        }
    }

    /// Sets a register directly — used when the application hands back clipboard
    /// contents for `"+p`.
    pub fn set_unnamed(&mut self, value: RegisterValue) {
        self.unnamed = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_yank_lands_in_the_unnamed_and_yank_registers() {
        let mut r = Registers::new();
        r.write_yank(None, RegisterValue::new("hello", false));
        assert_eq!(r.read(None).text, "hello");
        assert_eq!(r.read(Some('0')).text, "hello");
    }

    #[test]
    fn a_delete_does_not_clobber_the_yank_register() {
        // This is the point of "0: yanked text survives an intervening delete.
        let mut r = Registers::new();
        r.write_yank(None, RegisterValue::new("yanked", false));
        r.write_delete(None, RegisterValue::new("deleted", false));
        assert_eq!(r.read(Some('0')).text, "yanked");
        assert_eq!(r.read(None).text, "deleted");
    }

    #[test]
    fn named_registers_round_trip() {
        let mut r = Registers::new();
        r.write_yank(Some('a'), RegisterValue::new("first", true));
        let got = r.read(Some('a'));
        assert_eq!(got.text, "first");
        assert!(got.linewise, "the linewise flag travels with the value");
    }

    #[test]
    fn an_uppercase_register_appends() {
        let mut r = Registers::new();
        r.write_yank(Some('a'), RegisterValue::new("one", false));
        r.write_yank(Some('A'), RegisterValue::new("two", false));
        assert_eq!(r.read(Some('a')).text, "onetwo");
    }

    #[test]
    fn the_blackhole_discards() {
        let mut r = Registers::new();
        r.write_yank(None, RegisterValue::new("keep", false));
        r.write_delete(Some('_'), RegisterValue::new("gone", false));
        assert_eq!(r.read(None).text, "keep");
        assert!(r.read(Some('_')).is_empty());
    }

    #[test]
    fn linewise_deletes_shift_through_the_numbered_registers() {
        let mut r = Registers::new();
        r.write_delete(None, RegisterValue::new("first", true));
        r.write_delete(None, RegisterValue::new("second", true));
        assert_eq!(r.read(Some('1')).text, "second", "most recent is \"1");
        assert_eq!(r.read(Some('2')).text, "first");
    }

    #[test]
    fn small_deletes_go_to_the_dash_register() {
        let mut r = Registers::new();
        r.write_delete(None, RegisterValue::new("x", false));
        assert_eq!(r.read(Some('-')).text, "x");
        assert!(
            r.read(Some('1')).is_empty(),
            "a within-line delete is not a numbered shift"
        );
    }

    #[test]
    fn clipboard_registers_are_recognised_but_not_stored() {
        assert!(is_clipboard('+'));
        assert!(is_clipboard('*'));
        assert!(!is_clipboard('a'));
    }
}
