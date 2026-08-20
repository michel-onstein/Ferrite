//! The `:` command line — a command line, deliberately not a command language.
//!
//! **No vimscript.** No `:if`, no `:function`, no `:map`, no expression
//! evaluation. That boundary is the design's, and it is stated here so the scope
//! cannot creep. See `docs/VIM_MODE_DESIGN.md` §4.
//!
//! ```text
//! ex     := [range] name ['!'] [args]
//! range  := addr [',' addr] | '%'
//! addr   := number | '.' | '$'
//! ```

/// One end of a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addr {
    /// An absolute, 1-based line number.
    Line(usize),
    /// `.` — the cursor line.
    Current,
    /// `$` — the last line.
    Last,
}

/// A parsed line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExRange {
    /// No range given.
    None,
    /// A single address.
    One(Addr),
    /// `a,b`
    Two(Addr, Addr),
}

impl ExRange {
    /// Resolves to inclusive 0-based line indices.
    pub fn resolve(self, cursor_line: usize, last_line: usize) -> Option<(usize, usize)> {
        let one = |a: Addr| match a {
            Addr::Line(n) => n.saturating_sub(1).min(last_line),
            Addr::Current => cursor_line,
            Addr::Last => last_line,
        };
        match self {
            ExRange::None => None,
            ExRange::One(a) => {
                let l = one(a);
                Some((l, l))
            }
            ExRange::Two(a, b) => {
                let (x, y) = (one(a), one(b));
                Some((x.min(y), x.max(y)))
            }
        }
    }
}

/// Flags on `:s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubstFlags {
    /// `g` — every match on the line, not just the first.
    pub global: bool,
    /// `i` — case-insensitive.
    pub ignore_case: bool,
}

/// The value `:set` is assigning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetValue {
    On,
    Off,
    Toggle,
    Number(usize),
}

/// A parsed ex command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExCommand {
    /// `:w`, `:w file`, `:wq`, `:x`
    Write { path: Option<String>, quit: bool },
    /// `:q`, `:q!`, `:qa`
    Quit { force: bool, all: bool },
    /// `:e file`, `:e!`
    Edit { path: Option<String>, force: bool },
    /// `:42`, `:$`
    Goto(Addr),
    /// `:{range}d [reg]`
    Delete {
        range: ExRange,
        register: Option<char>,
    },
    /// `:{range}y [reg]`
    Yank {
        range: ExRange,
        register: Option<char>,
    },
    /// `:{range}s/pat/rep/flags`
    Substitute {
        range: ExRange,
        pattern: String,
        replacement: String,
        flags: SubstFlags,
    },
    /// `:noh`
    NoHighlight,
    /// `:set opt`
    Set { option: String, value: SetValue },
    /// `:{range}>` / `:{range}<`
    Shift { range: ExRange, dedent: bool },
}

/// Parses an address at the start of `s`, returning it and the rest.
fn parse_addr(s: &str) -> Option<(Addr, &str)> {
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    match first {
        '.' => Some((Addr::Current, &s[1..])),
        '$' => Some((Addr::Last, &s[1..])),
        d if d.is_ascii_digit() => {
            let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            let n = s[..end].parse().ok()?;
            Some((Addr::Line(n), &s[end..]))
        }
        _ => None,
    }
}

/// Parses the leading range, returning it and the rest of the line.
fn parse_range(s: &str) -> (ExRange, &str) {
    if let Some(rest) = s.strip_prefix('%') {
        return (ExRange::Two(Addr::Line(1), Addr::Last), rest);
    }
    match parse_addr(s) {
        Some((first, rest)) => {
            if let Some(rest2) = rest.strip_prefix(',') {
                match parse_addr(rest2) {
                    Some((second, rest3)) => (ExRange::Two(first, second), rest3),
                    None => (ExRange::Two(first, Addr::Current), rest2),
                }
            } else {
                (ExRange::One(first), rest)
            }
        }
        None => (ExRange::None, s),
    }
}

/// Splits `s/pat/rep/flags` on its delimiter, honouring backslash escapes.
fn split_subst(body: &str) -> Option<(String, String, &str)> {
    let mut chars = body.char_indices();
    let (_, delim) = chars.next()?;
    if delim.is_alphanumeric() || delim.is_whitespace() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    let mut consumed = delim.len_utf8();

    for (idx, c) in body.char_indices().skip(1) {
        consumed = idx + c.len_utf8();
        if escaped {
            // Keep the escape for the regex engine unless it escapes the delimiter.
            if c != delim {
                current.push('\\');
            }
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            _ if c == delim => {
                parts.push(std::mem::take(&mut current));
                if parts.len() == 2 {
                    break;
                }
            }
            _ => current.push(c),
        }
    }

    if parts.is_empty() {
        // `:s/pat` with no closing delimiter.
        parts.push(std::mem::take(&mut current));
    }
    if parts.len() == 1 {
        parts.push(String::new());
        return Some((parts[0].clone(), parts[1].clone(), ""));
    }

    let flags = body.get(consumed..).unwrap_or("");
    Some((parts[0].clone(), parts[1].clone(), flags))
}

fn parse_subst_flags(s: &str) -> SubstFlags {
    let mut flags = SubstFlags::default();
    for c in s.chars() {
        match c {
            'g' => flags.global = true,
            'i' => flags.ignore_case = true,
            _ => {}
        }
    }
    flags
}

/// Parses a `:set` argument.
fn parse_set(arg: &str) -> Result<ExCommand, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err("E518: set: option required".into());
    }
    if let Some((name, value)) = arg.split_once('=') {
        let n: usize = value
            .trim()
            .parse()
            .map_err(|_| format!("E521: {name}= expects a number"))?;
        return Ok(ExCommand::Set {
            option: name.trim().to_string(),
            value: SetValue::Number(n),
        });
    }
    if let Some(name) = arg.strip_suffix('!') {
        return Ok(ExCommand::Set {
            option: name.to_string(),
            value: SetValue::Toggle,
        });
    }
    if let Some(name) = arg.strip_prefix("no") {
        return Ok(ExCommand::Set {
            option: name.to_string(),
            value: SetValue::Off,
        });
    }
    Ok(ExCommand::Set {
        option: arg.to_string(),
        value: SetValue::On,
    })
}

/// Parses a command line (without the leading `:`).
///
/// An unknown command is an error, never a silent no-op — a silent no-op is how
/// users conclude the feature is broken.
pub fn parse(line: &str) -> Result<ExCommand, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("E471: argument required".into());
    }

    let (range, rest) = parse_range(line);
    let rest = rest.trim_start();

    // A bare range is "go to that line".
    if rest.is_empty() {
        return match range {
            ExRange::One(a) => Ok(ExCommand::Goto(a)),
            ExRange::Two(_, b) => Ok(ExCommand::Goto(b)),
            ExRange::None => Err("E471: argument required".into()),
        };
    }

    // `:>` and `:<` carry no name.
    if let Some(stripped) = rest.strip_prefix('>') {
        if stripped.trim().is_empty() {
            return Ok(ExCommand::Shift {
                range,
                dedent: false,
            });
        }
    }
    if let Some(stripped) = rest.strip_prefix('<') {
        if stripped.trim().is_empty() {
            return Ok(ExCommand::Shift {
                range,
                dedent: true,
            });
        }
    }

    // Split the name from its arguments. `!` binds to the name.
    let name_end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (name, tail) = rest.split_at(name_end);
    let force = tail.starts_with('!');
    let args = if force { &tail[1..] } else { tail };
    let args_trimmed = args.trim();
    let arg_opt = (!args_trimmed.is_empty()).then(|| args_trimmed.to_string());

    match name {
        "w" | "write" => Ok(ExCommand::Write {
            path: arg_opt,
            quit: false,
        }),
        "wq" | "x" | "xit" => Ok(ExCommand::Write {
            path: arg_opt,
            quit: true,
        }),
        "q" | "quit" => Ok(ExCommand::Quit { force, all: false }),
        "qa" | "qall" | "quita" | "quitall" => Ok(ExCommand::Quit { force, all: true }),
        "wqa" | "xa" => Ok(ExCommand::Write {
            path: None,
            quit: true,
        }),
        "e" | "edit" => Ok(ExCommand::Edit {
            path: arg_opt,
            force,
        }),
        "d" | "delete" => Ok(ExCommand::Delete {
            range,
            register: args_trimmed.chars().next(),
        }),
        "y" | "yank" => Ok(ExCommand::Yank {
            range,
            register: args_trimmed.chars().next(),
        }),
        "noh" | "nohl" | "nohlsearch" => Ok(ExCommand::NoHighlight),
        "set" | "se" => parse_set(args),
        "s" | "substitute" => {
            let (pattern, replacement, flags) = split_subst(args)
                .ok_or_else(|| "E486: :s needs a pattern, e.g. :s/old/new/".to_string())?;
            if pattern.is_empty() {
                return Err("E35: no previous regular expression".into());
            }
            Ok(ExCommand::Substitute {
                range,
                pattern,
                replacement,
                flags: parse_subst_flags(flags),
            })
        }
        other => Err(format!("E492: not an editor command: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_quit_variants() {
        assert_eq!(
            parse("w").unwrap(),
            ExCommand::Write {
                path: None,
                quit: false
            }
        );
        assert_eq!(
            parse("w notes.md").unwrap(),
            ExCommand::Write {
                path: Some("notes.md".into()),
                quit: false
            }
        );
        assert_eq!(
            parse("wq").unwrap(),
            ExCommand::Write {
                path: None,
                quit: true
            }
        );
        assert_eq!(
            parse("x").unwrap(),
            ExCommand::Write {
                path: None,
                quit: true
            }
        );
        assert_eq!(
            parse("q").unwrap(),
            ExCommand::Quit {
                force: false,
                all: false
            }
        );
        assert_eq!(
            parse("q!").unwrap(),
            ExCommand::Quit {
                force: true,
                all: false
            }
        );
        assert_eq!(
            parse("qa").unwrap(),
            ExCommand::Quit {
                force: false,
                all: true
            }
        );
    }

    #[test]
    fn a_bare_number_is_a_goto() {
        assert_eq!(parse("42").unwrap(), ExCommand::Goto(Addr::Line(42)));
        assert_eq!(parse("$").unwrap(), ExCommand::Goto(Addr::Last));
    }

    #[test]
    fn substitute_parses_pattern_replacement_and_flags() {
        match parse("%s/foo/bar/g").unwrap() {
            ExCommand::Substitute {
                range,
                pattern,
                replacement,
                flags,
            } => {
                assert_eq!(range, ExRange::Two(Addr::Line(1), Addr::Last));
                assert_eq!(pattern, "foo");
                assert_eq!(replacement, "bar");
                assert!(flags.global);
                assert!(!flags.ignore_case);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn substitute_accepts_an_alternate_delimiter_and_escapes() {
        match parse("s#a/b#c#").unwrap() {
            ExCommand::Substitute {
                pattern,
                replacement,
                ..
            } => {
                assert_eq!(pattern, "a/b");
                assert_eq!(replacement, "c");
            }
            other => panic!("{other:?}"),
        }
        match parse(r"s/a\/b/c/").unwrap() {
            ExCommand::Substitute { pattern, .. } => assert_eq!(pattern, "a/b"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn substitute_without_a_closing_delimiter_still_parses() {
        match parse("s/foo").unwrap() {
            ExCommand::Substitute {
                pattern,
                replacement,
                ..
            } => {
                assert_eq!(pattern, "foo");
                assert_eq!(replacement, "");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ranges_resolve_to_zero_based_inclusive_lines() {
        assert_eq!(
            ExRange::Two(Addr::Line(1), Addr::Last).resolve(5, 9),
            Some((0, 9))
        );
        assert_eq!(ExRange::One(Addr::Current).resolve(5, 9), Some((5, 5)));
        assert_eq!(
            ExRange::Two(Addr::Line(8), Addr::Line(3)).resolve(0, 20),
            Some((2, 7)),
            "a reversed range is normalised"
        );
        assert_eq!(ExRange::None.resolve(3, 9), None);
        assert_eq!(
            ExRange::One(Addr::Line(500)).resolve(0, 9),
            Some((9, 9)),
            "past the end clamps"
        );
    }

    #[test]
    fn set_handles_on_off_toggle_and_numbers() {
        assert_eq!(
            parse("set number").unwrap(),
            ExCommand::Set {
                option: "number".into(),
                value: SetValue::On
            }
        );
        assert_eq!(
            parse("set nonumber").unwrap(),
            ExCommand::Set {
                option: "number".into(),
                value: SetValue::Off
            }
        );
        assert_eq!(
            parse("set wrap!").unwrap(),
            ExCommand::Set {
                option: "wrap".into(),
                value: SetValue::Toggle
            }
        );
        assert_eq!(
            parse("set tabstop=4").unwrap(),
            ExCommand::Set {
                option: "tabstop".into(),
                value: SetValue::Number(4)
            }
        );
    }

    #[test]
    fn range_delete_and_yank() {
        assert_eq!(
            parse("1,3d").unwrap(),
            ExCommand::Delete {
                range: ExRange::Two(Addr::Line(1), Addr::Line(3)),
                register: None
            }
        );
        assert_eq!(
            parse("%y").unwrap(),
            ExCommand::Yank {
                range: ExRange::Two(Addr::Line(1), Addr::Last),
                register: None
            }
        );
    }

    #[test]
    fn shift_commands_parse() {
        assert_eq!(
            parse("1,5>").unwrap(),
            ExCommand::Shift {
                range: ExRange::Two(Addr::Line(1), Addr::Line(5)),
                dedent: false
            }
        );
        assert_eq!(
            parse("<").unwrap(),
            ExCommand::Shift {
                range: ExRange::None,
                dedent: true
            }
        );
    }

    #[test]
    fn nohighlight_aliases() {
        for s in ["noh", "nohl", "nohlsearch"] {
            assert_eq!(parse(s).unwrap(), ExCommand::NoHighlight, "{s}");
        }
    }

    #[test]
    fn an_unknown_command_is_an_error_not_a_silent_noop() {
        let err = parse("frobnicate").unwrap_err();
        assert!(err.contains("not an editor command"), "{err}");
        assert!(parse("").is_err());
    }

    #[test]
    fn vimscript_is_not_supported_and_says_so() {
        // The boundary from §4: these must fail loudly rather than half-work.
        for s in ["if x", "function Foo()", "map j k"] {
            assert!(parse(s).is_err(), "{s} must be rejected");
        }
    }
}
