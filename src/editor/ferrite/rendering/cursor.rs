//! Cursor rendering for FerriteEditor.
//!
//! This module handles cursor positioning and rendering, with full support for:
//! - Single-line text (non-wrapped mode with horizontal scrolling)
//! - Word-wrapped text (cursor correctly positions on visual rows)
//! - Cursor blinking (configurable via `cursor_visible` parameter)
//!
//! # Architecture
//!
//! The cursor position is calculated in two stages:
//! 1. **Line Y position**: Calculated in `editor.rs` based on wrap_info
//! 2. **Row-within-line offset**: Calculated here using egui's galley positioning
//!
//! For wrapped text, we use `galley.pos_from_cursor()` which returns the cursor's
//! position relative to the galley origin, automatically accounting for which
//! visual row the cursor is on.
//!
//! # Key Functions
//!
//! - [`render_cursor`] - Main entry point, renders cursor at correct position
//! - [`calculate_wrapped_cursor_position`] - Handles wrapped text cursor positioning
//! - [`get_cursor_position`] - Public API for getting cursor coordinates without rendering

use egui::{Color32, FontId, Pos2, Rect, Vec2};

use super::super::buffer::TextBuffer;
use super::super::cursor::Cursor;
use super::super::view::ViewState;

/// Duration of each cursor blink phase (visible or hidden).
/// Standard is ~500ms, giving a full on/off cycle of ~1 second.
pub const CURSOR_BLINK_INTERVAL_MS: u64 = 500;

/// Width of the insertion-point cursor, in pixels.
const BAR_WIDTH: f32 = 2.0;

/// Fallback block width when there is no character to measure (end of line,
/// empty line). A rough en-width for the font size.
const EMPTY_BLOCK_WIDTH_RATIO: f32 = 0.5;

/// How the cursor is drawn.
///
/// Vim mode makes this visible state: Normal and Visual mode sit *on* a
/// character, so the cursor covers it, whereas Insert mode sits *between*
/// characters and draws a thin bar. Without the distinction there is no way to
/// tell the modes apart while typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// A thin vertical line between characters — insertion point.
    #[default]
    Bar,
    /// A filled block covering the character under the cursor.
    Block,
}

/// Picks a legible text colour to draw over a filled block.
///
/// The block is painted in the cursor colour, so the glyph beneath it has to be
/// repainted in something that contrasts, or the character under the cursor
/// becomes invisible.
fn contrasting_text_color(background: Color32) -> Color32 {
    // Rec. 601 luma, which is good enough for a light/dark decision.
    let [r, g, b, _] = background.to_array();
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luma > 140.0 {
        Color32::from_rgb(20, 20, 20)
    } else {
        Color32::from_rgb(240, 240, 240)
    }
}

/// The character under the cursor, if any. `None` at end of line, where a block
/// cursor has nothing to cover.
fn char_under_cursor(buffer: &TextBuffer, cursor: &Cursor) -> Option<char> {
    let line = buffer.get_line(cursor.line)?;
    line.trim_end_matches(['\r', '\n'])
        .chars()
        .nth(cursor.column)
}

/// Renders the cursor at its current position.
///
/// Handles both wrapped and non-wrapped text modes. For wrapped text, correctly
/// positions the cursor on the appropriate visual row within a logical line.
///
/// # Arguments
/// * `painter` - The egui Painter for drawing
/// * `buffer` - The text buffer containing line content
/// * `cursor` - Current cursor position (line, column)
/// * `view` - View state containing wrap settings and scroll offset
/// * `font_id` - Font used for text measurement
/// * `text_start_x` - X coordinate where text area begins (after gutter)
/// * `line_top_y` - Y coordinate of the cursor's logical line top
/// * `wrap_width` - Width at which text wraps (ignored if wrap disabled)
/// * `cursor_color` - Color for the cursor (should match theme)
/// * `cursor_visible` - Whether the cursor should be drawn (for blink effect)
/// * `shape` - Bar (insertion point) or Block (Vim Normal/Visual mode)
#[allow(clippy::too_many_arguments)]
pub fn render_cursor(
    painter: &egui::Painter,
    buffer: &TextBuffer,
    cursor: &Cursor,
    view: &ViewState,
    font_id: &FontId,
    text_start_x: f32,
    line_top_y: f32,
    wrap_width: f32,
    cursor_color: Color32,
    cursor_visible: bool,
    shape: CursorShape,
) {
    // Skip rendering if cursor is in hidden phase of blink cycle
    if !cursor_visible {
        return;
    }

    let (cursor_x, cursor_y, cursor_height) = if view.is_wrap_enabled() {
        calculate_wrapped_cursor_position(
            painter,
            buffer,
            cursor,
            view,
            font_id,
            text_start_x,
            line_top_y,
            wrap_width,
        )
    } else {
        calculate_unwrapped_cursor_position(
            painter,
            buffer,
            cursor,
            view,
            font_id,
            text_start_x,
            line_top_y,
        )
    };

    match shape {
        CursorShape::Bar => {
            let cursor_rect = Rect::from_min_size(
                Pos2::new(cursor_x, cursor_y),
                Vec2::new(BAR_WIDTH, cursor_height),
            );
            painter.rect_filled(cursor_rect, 0.0, cursor_color);
        }
        CursorShape::Block => {
            let glyph = char_under_cursor(buffer, cursor);

            // The block spans the character it sits on, so it lines up with the
            // text rather than being a fixed width.
            let width = match glyph {
                Some(c) => {
                    let galley =
                        painter.layout_no_wrap(c.to_string(), font_id.clone(), Color32::WHITE);
                    // Zero-width glyphs (combining marks) would give an
                    // invisible cursor.
                    galley.size().x.max(font_id.size * EMPTY_BLOCK_WIDTH_RATIO)
                }
                None => font_id.size * EMPTY_BLOCK_WIDTH_RATIO,
            };

            let cursor_rect = Rect::from_min_size(
                Pos2::new(cursor_x, cursor_y),
                Vec2::new(width, cursor_height),
            );
            painter.rect_filled(cursor_rect, 0.0, cursor_color);

            // Repaint the covered glyph so the character stays readable.
            if let Some(c) = glyph {
                painter.text(
                    Pos2::new(cursor_x, cursor_y),
                    egui::Align2::LEFT_TOP,
                    c,
                    font_id.clone(),
                    contrasting_text_color(cursor_color),
                );
            }
        }
    }
}

/// Calculates cursor position for non-wrapped text.
///
/// In non-wrapped mode, all text stays on a single visual row per logical line.
/// The X position is calculated by measuring text width up to the cursor column,
/// then adjusting for horizontal scroll offset.
///
/// For complex-script lines (Arabic, Bengali, etc.) the HarfRust shaping
/// pipeline is used for accurate advance-width measurement.
fn calculate_unwrapped_cursor_position(
    painter: &egui::Painter,
    buffer: &TextBuffer,
    cursor: &Cursor,
    view: &ViewState,
    font_id: &FontId,
    text_start_x: f32,
    line_top_y: f32,
) -> (f32, f32, f32) {
    let cursor_x = if cursor.column == 0 {
        text_start_x - view.horizontal_scroll()
    } else if let Some(line_content) = buffer.get_line(cursor.line) {
        let display = line_content.trim_end_matches(['\r', '\n']);

        if crate::fonts::needs_complex_script_fonts(display) {
            let font_bytes = crate::fonts::ttf_bytes_for_font_id_shaping(font_id);
            if let Some(x) = super::super::shaping::shaped_column_to_x(
                display,
                font_bytes,
                font_id.size,
                cursor.column,
            ) {
                text_start_x + x - view.horizontal_scroll()
            } else {
                let chars_before: String = display.chars().take(cursor.column).collect();
                let galley = painter.layout_no_wrap(chars_before, font_id.clone(), Color32::WHITE);
                text_start_x + galley.size().x - view.horizontal_scroll()
            }
        } else {
            let chars_before: String = display.chars().take(cursor.column).collect();
            let galley = painter.layout_no_wrap(chars_before, font_id.clone(), Color32::WHITE);
            text_start_x + galley.size().x - view.horizontal_scroll()
        }
    } else {
        text_start_x - view.horizontal_scroll()
    };

    (cursor_x, line_top_y, view.line_height())
}

/// Calculates cursor position for wrapped text.
///
/// When text wraps, a single logical line spans multiple visual rows. This function
/// uses egui's galley cursor positioning to find exactly which visual row the cursor
/// is on and its X position within that row.
///
/// # How it works
///
/// 1. Create a wrapped galley for the cursor's line content
/// 2. Convert the cursor column to a `CCursor` (character cursor)
/// 3. Use `galley.pos_from_cursor()` to get the cursor's rect relative to galley origin
/// 4. Add line_top_y to the rect's Y to get absolute screen position
///
/// # Returns
/// Tuple of (x, y, height) for cursor rendering:
/// - `x`: Horizontal position in screen coordinates
/// - `y`: Vertical position (accounts for which visual row within the wrapped line)
/// - `height`: Height of the cursor (matches the visual row height)
fn calculate_wrapped_cursor_position(
    painter: &egui::Painter,
    buffer: &TextBuffer,
    cursor: &Cursor,
    view: &ViewState,
    font_id: &FontId,
    text_start_x: f32,
    line_top_y: f32,
    wrap_width: f32,
) -> (f32, f32, f32) {
    let effective_wrap_width = if wrap_width > 0.0 {
        wrap_width
    } else {
        f32::INFINITY
    };
    let base_line_height = view.line_height();

    if let Some(line_content) = buffer.get_line(cursor.line) {
        let display_content = line_content.trim_end_matches(['\r', '\n']);

        // Create a wrapped galley matching how text is rendered
        let galley = painter.layout(
            display_content.to_string(),
            font_id.clone(),
            Color32::WHITE,
            effective_wrap_width,
        );

        // Clamp cursor column to valid range
        let char_count = display_content.chars().count();
        let cursor_col = cursor.column.min(char_count);

        // Use egui's built-in cursor positioning - this is the key to correct
        // wrapped text cursor placement. The galley tracks which visual row
        // each character is on, and pos_from_cursor returns a rect whose
        // min.y accounts for the row offset within the galley.
        let ccursor = egui::text::CCursor::new(cursor_col);
        let cursor_rect = galley.pos_from_cursor(ccursor);

        // cursor_rect.min is relative to galley origin:
        // - min.x: X offset within the current visual row
        // - min.y: Y offset from galley top (0 for row 0, ~16 for row 1, etc.)
        let cursor_x = text_start_x + cursor_rect.min.x;
        let cursor_y = line_top_y + cursor_rect.min.y;
        let row_height = cursor_rect.height().max(base_line_height);

        (cursor_x, cursor_y, row_height)
    } else {
        (text_start_x, line_top_y, base_line_height)
    }
}

/// Gets cursor position coordinates without rendering.
///
/// This is useful for other components that need to know cursor position,
/// such as selection rendering or IME positioning.
///
/// # Returns
/// Tuple of (x, y, height) in screen coordinates.
#[allow(dead_code)]
pub fn get_cursor_position(
    painter: &egui::Painter,
    buffer: &TextBuffer,
    cursor: &Cursor,
    view: &ViewState,
    font_id: &FontId,
    text_start_x: f32,
    line_top_y: f32,
    wrap_width: f32,
) -> (f32, f32, f32) {
    if view.is_wrap_enabled() {
        calculate_wrapped_cursor_position(
            painter,
            buffer,
            cursor,
            view,
            font_id,
            text_start_x,
            line_top_y,
            wrap_width,
        )
    } else {
        calculate_unwrapped_cursor_position(
            painter,
            buffer,
            cursor,
            view,
            font_id,
            text_start_x,
            line_top_y,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_is_the_default_shape() {
        // Non-Vim editing must be unaffected by the block cursor work.
        assert_eq!(CursorShape::default(), CursorShape::Bar);
    }

    #[test]
    fn a_light_cursor_gets_dark_text_and_a_dark_cursor_gets_light_text() {
        // The block is painted in the cursor colour, so the glyph under it has
        // to be repainted in something legible or it disappears.
        let on_white = contrasting_text_color(Color32::WHITE);
        let on_black = contrasting_text_color(Color32::BLACK);
        assert_eq!(on_white, Color32::from_rgb(20, 20, 20));
        assert_eq!(on_black, Color32::from_rgb(240, 240, 240));
        assert_ne!(on_white, on_black);
    }

    #[test]
    fn contrast_follows_luma_not_a_single_channel() {
        // Pure green is bright to the eye despite a zero red channel.
        assert_eq!(
            contrasting_text_color(Color32::from_rgb(0, 255, 0)),
            Color32::from_rgb(20, 20, 20)
        );
        // Pure blue is dark.
        assert_eq!(
            contrasting_text_color(Color32::from_rgb(0, 0, 255)),
            Color32::from_rgb(240, 240, 240)
        );
    }

    #[test]
    fn the_character_under_the_cursor_is_found() {
        let buffer = TextBuffer::from_string("abc\ndef");
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(0, 0)), Some('a'));
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(0, 2)), Some('c'));
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(1, 1)), Some('e'));
    }

    #[test]
    fn there_is_no_character_at_the_end_of_a_line() {
        // A block cursor past the last character has nothing to cover, so it
        // falls back to a fixed width rather than measuring nothing.
        let buffer = TextBuffer::from_string("ab\n");
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(0, 2)), None);
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(1, 0)), None);
    }

    #[test]
    fn the_newline_is_never_reported_as_the_character_under_the_cursor() {
        // Otherwise the block would render a stray glyph at end of line.
        let buffer = TextBuffer::from_string("ab\r\ncd");
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(0, 2)), None);
    }

    #[test]
    fn a_missing_line_is_handled() {
        let buffer = TextBuffer::from_string("a");
        assert_eq!(char_under_cursor(&buffer, &Cursor::new(99, 0)), None);
    }
}
