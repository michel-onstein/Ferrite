# Vim Mode

## Overview

Optional modal editing mode that adds Vim-style keybindings to FerriteEditor. Disabled by default to preserve standard editing behavior. When enabled, provides Normal/Insert/Visual/Visual Line modes with essential Vim commands.

## Key Files

- `src/config/settings.rs` - `vim_mode: bool` setting (default: `false`)
- `src/editor/ferrite/vim.rs` - `VimState` struct, `VimMode` enum, `handle_key()` dispatcher
- `src/editor/ferrite/editor.rs` - Event loop interception, `set_vim_mode()`/`vim_mode()` methods
- `src/editor/widget.rs` - `vim_mode` builder method, `vim_mode_label` on `EditorOutput`
- `src/app/central_panel.rs` - Propagates setting to EditorWidget, surfaces mode to UiState
- `src/app/status_bar.rs` - Renders `[NORMAL]`/`[INSERT]`/`[VISUAL]`/`[V-LINE]` indicator
- `src/ui/settings.rs` - Vim Mode checkbox in Editor settings section
- `src/state.rs` - `vim_mode_indicator: Option<&'static str>` on `UiState`

## Architecture

### Data Flow

```
Settings.vim_mode ──► EditorWidget.vim_mode() ──► FerriteEditor.set_vim_mode()
                                                        │
                                                   VimState.handle_key()
                                                        │
                                              VimMode (label: &'static str)
                                                        │
                                              EditorOutput.vim_mode_label
                                                        │
                                              UiState.vim_mode_indicator
                                                        │
                                              status_bar.rs (renders indicator)
```

### Modal State Machine

`VimState` manages the current mode and pending operations:

- **Normal**: Default mode. Keys are interpreted as commands (motions, operators, mode switches).
- **Insert**: Text input mode. All keys pass through to normal editor handling. `Esc` returns to Normal.
- **Visual**: Character-wise selection. Motions extend selection. `d`/`y` operate on selection.
- **Visual Line**: Line-wise selection. Similar to Visual but selects full lines.

### Event Loop Integration

In `FerriteEditor::ui()`, when `vim_mode_enabled` is true:

1. `Event::Key` events are intercepted by `VimState::handle_key()` before normal processing.
2. Returns `VimKeyResult::Handled(result)` if Vim consumed the key, `Passthrough` if not.
3. `Event::Text` events are suppressed in Normal/Visual modes via `should_insert_text()`.
4. Standard egui shortcuts (Ctrl+C, Ctrl+V, etc.) are not intercepted by Vim.

> **Careful with the catch-all.** Normal and Visual mode both end in
> `_ => VimKeyResult::Consumed`, which silently swallows every key without an
> explicit arm — the key reaches neither Vim nor the standard input handler. That
> is what made the arrow keys dead in Normal mode until v0.3.1.
> A key that should keep its standard behaviour needs an explicit
> `VimKeyResult::Passthrough` arm.

## Implemented Commands

### Normal Mode

| Key | Action |
|-----|--------|
| `h`/`j`/`k`/`l` | Left/down/up/right movement |
| `←`/`↓`/`↑`/`→` | Aliases for `h`/`j`/`k`/`l` (no line wrapping, matching Vim's default `whichwrap`) |
| `w`/`b` | Word forward/backward |
| `0`/`Home` | Line start |
| `End` | Line end (`$` itself is **not** bound — `egui::Key` has no `$` variant) |
| `PageUp`/`PageDown` | Move a page (delegated to the standard input handler) |
| `G` | Last line |
| `i`/`a` | Insert before/after cursor |
| `I`/`A` | Insert at line start/end |
| `o`/`O` | Open line below/above |
| `x` | Delete character |
| `dd` | Delete line |
| `yy` | Yank line |
| `D` | Delete to end of line |
| `p`/`P` | Paste after/before |
| `v`/`V` | Enter Visual/Visual Line mode |
| `{count}{motion}` | Repeat count (e.g., `3j` or `3↓` = move down 3) |

### Not yet implemented

Documented here so the gaps aren't rediscovered as bugs:

- `e` (word end), `gg` (file start), `f`/`t` (find char), `%` (matching bracket)
- `C` (change to end of line) — `Key::C` has no shift branch, so `C` sets a pending
  *change operator* instead of changing to end of line.
- `$`, `^`, `%`, `*`, `~`, `>`, `<` — `egui::Key` has no variant for these characters, and
  `Event::Text` is discarded in Normal/Visual mode, so they cannot be bound at all without
  the restructuring in [`docs/VIM_MODE_DESIGN.md`](../../VIM_MODE_DESIGN.md).
- Text objects (`ci"`, `dap`), registers (`"ayy`), and `.` repeat — the single
  `Option<PendingOperator>` state cannot represent them.
- `u` / `Ctrl+R` — `u` is currently swallowed in Normal mode; undo/redo is not wired
  into `VimState`, which only receives the buffer, selection, and view (not `EditHistory`).
  Vim-mode edits therefore also bypass the undo history.

> A design for lifting these limits — a keystroke/parser pipeline plus an ex command
> subset — is in [`docs/VIM_MODE_DESIGN.md`](../../VIM_MODE_DESIGN.md) (Status: Proposed).

### Visual/Visual Line Mode

| Key | Action |
|-----|--------|
| Motions (`hjkl` or arrow keys) | Extend selection |
| `Home`/`End` | Extend selection to line start/end |
| `d` | Delete selection |
| `y` | Yank selection |
| `Esc` | Return to Normal |

## Dependencies Used

No additional crates. Built entirely on existing `TextBuffer`, `Cursor`, and `Selection` types.

## Usage

1. Open Settings (gear icon or Ctrl+,)
2. In the Editor section, check "Vim Mode"
3. Status bar shows `[NORMAL]` when active
4. Press `i` to enter Insert mode, `Esc` to return to Normal
5. Disable the checkbox to return to standard editing
