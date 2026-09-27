use embedded_graphics::mono_font::MonoFont;
use embedded_graphics::pixelcolor::{Rgb565, Rgb888};
use embedded_graphics::prelude::*;
use vte::Perform;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const SCREEN_HEIGHT: u16 = 320;
pub const SCREEN_WIDTH: u16 = 320;

/// Total primary heap available to the firmware (mirrors `HEAP_SIZE` in
/// `src/heap.rs`). Duplicated here, not imported, because `terminal-model` is
/// a host-portable crate with no dependency back on the firmware crate - keep
/// this in sync if `HEAP_SIZE` ever changes.
const FIRMWARE_HEAP_SIZE_BYTES: usize = 64 * 1024;

/// Heap the firmware's own non-screen subsystems (WiFi/TCP stack, SSH crypto,
/// SD card driver, flash-backed config, USB/logging, etc.) reliably need.
/// Real-hardware-measured via the `free` command: idle with an active SSH
/// session already uses ~32 KiB of the 64 KiB primary heap before any screen
/// output happens. Reserved so the screen's own buffers never assume they own
/// the whole heap - see
/// /ai/firstmate/data/picocalc-crash-display-buffer/report.md for the
/// investigation that measured the screen-side footprint, and the launch
/// brief for the real-hardware `free` readings this reserve is based on.
/// Re-check this if `HEAP_SIZE` or the boot-time subsystem set changes.
const NON_SCREEN_HEAP_RESERVE_BYTES: usize = 32 * 1024;

/// What's left in the primary heap for the screen itself (visible `lines` +
/// `scrollback`) once `NON_SCREEN_HEAP_RESERVE_BYTES` is set aside.
const SCREEN_HEAP_BUDGET_BYTES: usize = FIRMWARE_HEAP_SIZE_BYTES - NON_SCREEN_HEAP_RESERVE_BYTES;

/// Conservative per-`ScreenLine` allowance for allocator bookkeeping. Each
/// `ScreenLine` is 2 independent heap allocations (`chars`, `attrs`); the
/// real embedded allocator's per-allocation overhead (alignment padding,
/// minimum block size, fragmentation) can't be measured without hardware, so
/// this pads the logical byte count rather than sizing the budget exactly on
/// the edge of what the raw `size_of` math predicts.
const PER_LINE_ALLOC_OVERHEAD_BYTES: usize = 32;

static FONTS: &[&MonoFont] = &[
    &profont::PROFONT_7_POINT,
    &profont::PROFONT_9_POINT,
    &profont::PROFONT_10_POINT,
    &profont::PROFONT_12_POINT,
    &profont::PROFONT_14_POINT,
    &profont::PROFONT_18_POINT,
    &profont::PROFONT_24_POINT,
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    DefaultFg,
    DefaultBg,
    Rgb(u8, u8, u8),
    Indexed(u8),
}

impl Color {
    pub fn to_rgb565(self, is_bg: bool) -> Rgb565 {
        match self {
            Color::Black => Rgb565::BLACK,
            Color::Red => Rgb565::RED,
            Color::Green => Rgb565::GREEN,
            Color::Yellow => Rgb565::YELLOW,
            Color::Blue => Rgb565::BLUE,
            Color::Magenta => Rgb565::MAGENTA,
            Color::Cyan => Rgb565::CYAN,
            Color::White => Rgb565::WHITE,
            Color::BrightBlack => Rgb565::new(10, 20, 10), // Approx
            Color::BrightRed => Rgb565::new(31, 20, 20),
            Color::BrightGreen => Rgb565::new(20, 63, 20),
            Color::BrightYellow => Rgb565::new(31, 63, 20),
            Color::BrightBlue => Rgb565::new(20, 20, 31),
            Color::BrightMagenta => Rgb565::new(31, 20, 31),
            Color::BrightCyan => Rgb565::new(20, 63, 31),
            Color::BrightWhite => Rgb565::WHITE,
            Color::DefaultFg => Rgb565::CSS_LIGHT_GRAY,
            Color::DefaultBg => Rgb565::BLACK,
            Color::Rgb(r, g, b) => Rgb888::new(r, g, b).into(),
            // Simple mapping for the first 16 colors (the standard ANSI palette
            // plus its bright variants), else default. Delegates to the named
            // variants above rather than repeating their Rgb565 values here.
            Color::Indexed(0) => Color::Black.to_rgb565(is_bg),
            Color::Indexed(1) => Color::Red.to_rgb565(is_bg),
            Color::Indexed(2) => Color::Green.to_rgb565(is_bg),
            Color::Indexed(3) => Color::Yellow.to_rgb565(is_bg),
            Color::Indexed(4) => Color::Blue.to_rgb565(is_bg),
            Color::Indexed(5) => Color::Magenta.to_rgb565(is_bg),
            Color::Indexed(6) => Color::Cyan.to_rgb565(is_bg),
            Color::Indexed(7) => Rgb565::CSS_LIGHT_GRAY,
            Color::Indexed(8) => Color::BrightBlack.to_rgb565(is_bg),
            Color::Indexed(9) => Color::BrightRed.to_rgb565(is_bg),
            Color::Indexed(10) => Color::BrightGreen.to_rgb565(is_bg),
            Color::Indexed(11) => Color::BrightYellow.to_rgb565(is_bg),
            Color::Indexed(12) => Color::BrightBlue.to_rgb565(is_bg),
            Color::Indexed(13) => Color::BrightMagenta.to_rgb565(is_bg),
            Color::Indexed(14) => Color::BrightCyan.to_rgb565(is_bg),
            Color::Indexed(15) => Color::BrightWhite.to_rgb565(is_bg),
            Color::Indexed(_) => {
                if is_bg {
                    Rgb565::BLACK
                } else {
                    Rgb565::WHITE
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: Color::DefaultFg,
            bg: Color::DefaultBg,
            bold: false,
            underline: false,
            reverse: false,
        }
    }
}

#[derive(Clone)]
pub struct ScreenLine {
    pub chars: Vec<char>,
    pub attrs: Vec<Attrs>,
    pub dirty: bool,
}

impl ScreenLine {
    fn new(width: usize) -> Self {
        Self {
            chars: vec![' '; width],
            attrs: vec![Attrs::default(); width],
            dirty: true,
        }
    }

    fn clear(&mut self) {
        for c in self.chars.iter_mut() {
            *c = ' ';
        }
        for a in self.attrs.iter_mut() {
            *a = Attrs::default();
        }
        self.dirty = true;
    }

    /// Blanks `[start, end)` with `attrs` and marks the line dirty.
    fn erase(&mut self, start: usize, end: usize, attrs: Attrs) {
        for i in start..end {
            self.chars[i] = ' ';
            self.attrs[i] = attrs;
        }
        self.dirty = true;
    }
}

pub struct ScreenModel {
    pub lines: Vec<ScreenLine>,
    pub scrollback: Vec<ScreenLine>,
    pub viewport_offset: usize,
    max_scrollback: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    current_attrs: Attrs,
    pub font: &'static MonoFont<'static>,
    pub rows: usize,
    cols: usize,
    pub full_repaint: bool,
    /// Transient text shown on top of the terminal contents (e.g. the battery
    /// overlay). Never mutates `lines`/`scrollback` - the painter composites it
    /// over the framebuffer each frame while set, and `clear_overlay` forces a
    /// full repaint from the untouched cell data so dismissal is invisible.
    pub overlay: Option<String>,
    /// A non-timed overlay (e.g. the recording indicator) that a timed overlay
    /// may temporarily cover, to be restored when that timed overlay expires.
    overlay_restore: Option<String>,
    /// Absolute millisecond deadline of the active timed overlay, if any. The
    /// caller supplies the clock (see `tick_overlay`), keeping this model
    /// host-portable.
    overlay_deadline_ms: Option<u64>,
    /// Counts `scroll_up` invocations directly. Test-only: with `max_scrollback`
    /// now correctly sized to the real heap budget (including outer-container
    /// overhead) it can be smaller than a test's total feed count, so inferring
    /// scroll count from `scrollback.len()` deltas is unreliable once the cap
    /// is reached mid-test - this counter isn't capped and stays exact.
    #[cfg(test)]
    scroll_up_calls: usize,
}

impl Default for ScreenModel {
    fn default() -> Self {
        let font = FONTS[2];
        let cols =
            ((SCREEN_WIDTH as u32) / (font.character_size.width + font.character_spacing)) as usize;
        let rows = ((SCREEN_HEIGHT as u32) / font.character_size.height) as usize;

        // Initialize lines
        let mut lines = Vec::with_capacity(rows);
        for _ in 0..rows {
            lines.push(ScreenLine::new(cols));
        }

        let max_scrollback = ScreenModel::safe_max_scrollback_for(cols, rows);
        // Reserve the exact capacity `scroll_up` ever needs (it pushes, then
        // trims back down to `max_scrollback` via `remove(0)`, so length
        // transiently peaks at `max_scrollback + 1`) up front. Without this,
        // Rust's amortized-growth doubling would let `scrollback`'s backing
        // array overshoot `max_scrollback` slots - real heap `ScreenModel`
        // container overhead the budget below has no way to account for
        // otherwise. `set_max_scrollback` only ever lowers `max_scrollback`
        // toward this same ceiling (`max_safe_scrollback`), so this capacity
        // is never exceeded later.
        let scrollback = Vec::with_capacity(max_scrollback + 1);

        Self {
            lines,
            scrollback,
            viewport_offset: 0,
            max_scrollback,
            cursor_x: 0,
            cursor_y: 0,
            current_attrs: Attrs::default(),
            font,
            rows,
            cols,
            full_repaint: true,
            overlay: None,
            overlay_restore: None,
            overlay_deadline_ms: None,
            #[cfg(test)]
            scroll_up_calls: 0,
        }
    }
}

impl ScreenModel {
    pub fn width(&self) -> u16 {
        self.cols as u16
    }

    pub fn height(&self) -> u16 {
        self.rows as u16
    }

    pub fn clear(&mut self) {
        for line in self.lines.iter_mut() {
            line.clear();
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.full_repaint = true;
    }

    pub fn increase_font(&mut self) {
        // TODO: implement font resizing
    }

    pub fn decrease_font(&mut self) {
        // TODO: implement font resizing
    }

    fn scroll_up(&mut self) {
        // Remove first line, add new line at end
        if !self.lines.is_empty() {
            let line = self.lines.remove(0);
            self.scrollback.push(line);
            if self.scrollback.len() > self.max_scrollback {
                self.scrollback.remove(0);
            }
            self.lines.push(ScreenLine::new(self.cols));
            self.full_repaint = true;
            #[cfg(test)]
            {
                self.scroll_up_calls += 1;
            }
        }
    }

    /// Test-only accessor for [`Self::scroll_up_calls`] - see its doc comment
    /// for why tests must count scroll invocations directly instead of
    /// inferring them from `scrollback.len()` deltas.
    #[cfg(test)]
    fn scroll_up_calls(&self) -> usize {
        self.scroll_up_calls
    }

    /// Scrolls up by one line if the cursor has moved past the last row,
    /// clamping it back onto the newly revealed bottom row.
    fn scroll_if_cursor_past_bottom(&mut self) {
        if self.cursor_y >= self.rows {
            self.scroll_up();
            self.cursor_y = self.rows - 1;
        }
    }

    pub fn scroll_view_up(&mut self, n: usize) {
        self.viewport_offset = (self.viewport_offset + n).min(self.scrollback.len());
        self.full_repaint = true;
    }

    pub fn scroll_view_down(&mut self, n: usize) {
        self.viewport_offset = self.viewport_offset.saturating_sub(n);
        self.full_repaint = true;
    }

    pub fn reset_view(&mut self) {
        if self.viewport_offset != 0 {
            self.viewport_offset = 0;
            self.full_repaint = true;
        }
    }

    /// Show `text` as an overlay that stays until it is explicitly cleared or
    /// covered by a timed overlay. Purely a paint-time overlay flag - the cell
    /// buffer is never touched, so any host output that lands underneath while
    /// it's showing is preserved untouched.
    pub fn show_overlay(&mut self, text: String) {
        self.overlay_restore = Some(text.clone());
        self.replace_overlay(text);
        self.overlay_deadline_ms = None;
    }

    /// Show `text` as a timed overlay (e.g. the battery readout) that
    /// auto-dismisses at `now_ms + duration_ms`, restoring whatever non-timed
    /// overlay it covered (or clearing entirely if there was none).
    pub fn show_timed_overlay(&mut self, text: String, now_ms: u64, duration_ms: u64) {
        self.replace_overlay(text);
        self.overlay_deadline_ms = Some(now_ms.saturating_add(duration_ms));
    }

    /// Installs `text` as the composited overlay. Replacing an already-shown
    /// overlay with different text forces a full repaint, since the previous
    /// box may be wider/taller and its pixels would otherwise stay around it.
    fn replace_overlay(&mut self, text: String) {
        if self.overlay.is_some() && self.overlay.as_deref() != Some(text.as_str()) {
            self.full_repaint = true;
        }
        self.overlay = Some(text);
    }

    /// Applies the active timed overlay's deadline against `now_ms`: on expiry
    /// the non-timed overlay it covered is restored, or the overlay is cleared
    /// if there was none. Restoring forces a full repaint, like dismissal, so
    /// the wider/taller timed overlay's pixels are erased from the frame. A
    /// no-op while no timed overlay is active.
    pub fn tick_overlay(&mut self, now_ms: u64) {
        let Some(deadline) = self.overlay_deadline_ms else {
            return;
        };
        if now_ms < deadline {
            return;
        }
        self.overlay_deadline_ms = None;
        match self.overlay_restore.clone() {
            Some(text) => {
                self.overlay = Some(text);
                self.full_repaint = true;
            }
            None => self.clear_overlay(),
        }
    }

    /// Hide the overlay and force a full repaint so the real, possibly-changed
    /// cell contents underneath it are redrawn exactly as they'd look had the
    /// overlay never been shown.
    pub fn clear_overlay(&mut self) {
        self.overlay_restore = None;
        self.overlay_deadline_ms = None;
        if self.overlay.take().is_some() {
            self.full_repaint = true;
        }
    }

    /// Sets the scrollback cap, silently clamped to [`Self::max_safe_scrollback`]
    /// so no caller - the `scroll` config command, a stale value loaded from
    /// flash at boot, or any future code - can push the screen's logical
    /// memory footprint back over its heap budget.
    pub fn set_max_scrollback(&mut self, max: usize) {
        let max = max.min(self.max_safe_scrollback());
        self.max_scrollback = max;
        if self.scrollback.len() > max {
            let remove_count = self.scrollback.len() - max;
            self.scrollback.drain(0..remove_count);
            // Adjust viewport offset if it's now out of bounds
            if self.viewport_offset > self.scrollback.len() {
                self.viewport_offset = self.scrollback.len();
            }
        }
    }

    pub fn max_scrollback(&self) -> usize {
        self.max_scrollback
    }

    /// The largest `max_scrollback` value that keeps the screen's total
    /// logical footprint (visible `lines` + `scrollback`) within
    /// [`SCREEN_HEAP_BUDGET_BYTES`] at the current screen geometry. This is
    /// also the value `Default::default()` picks, so "the default" and "the
    /// heap-safe maximum" are the same number by construction.
    pub fn max_safe_scrollback(&self) -> usize {
        Self::safe_max_scrollback_for(self.cols, self.rows)
    }

    fn bytes_per_line(cols: usize) -> usize {
        (core::mem::size_of::<char>() + core::mem::size_of::<Attrs>()) * cols
            + PER_LINE_ALLOC_OVERHEAD_BYTES
            + core::mem::size_of::<ScreenLine>()
    }

    fn safe_max_scrollback_for(cols: usize, rows: usize) -> usize {
        let per_line = Self::bytes_per_line(cols);
        let visible_budget = per_line * rows;
        let scrollback_budget = SCREEN_HEAP_BUDGET_BYTES.saturating_sub(visible_budget);
        (scrollback_budget / per_line).max(1)
    }
}

// vte always pushes a parameter slot on CSI dispatch, even when the sequence had no
// digits (e.g. bare `CSI C`) - the missing parameter comes through as an explicit 0,
// not an absent one. ECMA-48 treats both "absent" and "0" as "use the default value",
// so `.unwrap_or(1)` alone never fires for the common bare-sequence case; this maps
// both cases to the default of 1.
fn cursor_move_count(params: &vte::Params) -> usize {
    params.iter().next().map(|p| p[0]).unwrap_or(0).max(1) as usize
}

/// Reads a 1-based CSI position parameter (defaulting to 1) and converts it
/// to a 0-based index, as used by cursor-positioning sequences like CUP/VPA.
fn cursor_pos_index(param: Option<&[u16]>) -> usize {
    param.map(|p| p[0]).unwrap_or(1).max(1) as usize - 1
}

/// Reads the mode parameter shared by the Erase in Display (J) and Erase in
/// Line (K) CSI sequences, defaulting to 0 when absent.
fn erase_mode(params: &vte::Params) -> u16 {
    params.iter().next().map(|p| p[0]).unwrap_or(0)
}

impl Perform for ScreenModel {
    fn print(&mut self, c: char) {
        self.reset_view();
        self.scroll_if_cursor_past_bottom();
        if self.cursor_x >= self.cols {
            self.cursor_x = 0;
            self.cursor_y += 1;
            self.scroll_if_cursor_past_bottom();
        }

        let line = &mut self.lines[self.cursor_y];
        if self.cursor_x < line.chars.len() {
            line.chars[self.cursor_x] = c;
            line.attrs[self.cursor_x] = self.current_attrs;
            line.dirty = true;
            self.cursor_x += 1;
        }
    }

    fn execute(&mut self, byte: u8) {
        self.reset_view();
        match byte {
            b'\n' => {
                // LF
                self.cursor_y += 1;
                self.scroll_if_cursor_past_bottom();
            }
            b'\r' => {
                // CR
                self.cursor_x = 0;
            }
            // BS
            b'\x08' if self.cursor_x > 0 => {
                self.cursor_x -= 1;
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore || !intermediates.is_empty() {
            return;
        }

        match action {
            'A' => {
                // Cursor Up
                let n = cursor_move_count(params);
                self.cursor_y = self.cursor_y.saturating_sub(n);
            }
            'B' => {
                // Cursor Down
                let n = cursor_move_count(params);
                self.cursor_y = (self.cursor_y + n).min(self.rows - 1);
            }
            'C' => {
                // Cursor Forward
                let n = cursor_move_count(params);
                self.cursor_x = (self.cursor_x + n).min(self.cols - 1);
            }
            'D' => {
                // Cursor Backward
                let n = cursor_move_count(params);
                self.cursor_x = self.cursor_x.saturating_sub(n);
            }
            'H' | 'f' => {
                // Cursor Position
                let mut iter = params.iter();
                let row = cursor_pos_index(iter.next());
                let col = cursor_pos_index(iter.next());
                self.cursor_y = row.min(self.rows - 1);
                self.cursor_x = col.min(self.cols - 1);
            }
            'd' => {
                // Line Position Absolute (VPA): move to row Pn, column unchanged.
                let row = cursor_pos_index(params.iter().next());
                self.cursor_y = row.min(self.rows - 1);
            }
            'J' => {
                // Erase in Display
                let n = erase_mode(params);
                match n {
                    0 => {
                        // Cursor to end
                        // Clear current line from cursor
                        let attrs = self.current_attrs;
                        self.lines[self.cursor_y].erase(self.cursor_x, self.cols, attrs);
                        // Clear lines below
                        for i in (self.cursor_y + 1)..self.rows {
                            self.lines[i].clear();
                        }
                    }
                    1 => {
                        // Beginning to cursor
                        // Clear lines above
                        for i in 0..self.cursor_y {
                            self.lines[i].clear();
                        }
                        // Clear current line up to cursor
                        let attrs = self.current_attrs;
                        self.lines[self.cursor_y].erase(0, self.cursor_x + 1, attrs);
                    }
                    2 => {
                        // Entire screen
                        self.clear();
                    }
                    _ => {}
                }
            }
            'K' => {
                // Erase in Line
                let n = erase_mode(params);
                let attrs = self.current_attrs;
                let line = &mut self.lines[self.cursor_y];
                match n {
                    0 => line.erase(self.cursor_x, self.cols, attrs), // Cursor to end
                    1 => line.erase(0, self.cursor_x + 1, attrs),     // Beginning to cursor
                    2 => line.erase(0, self.cols, attrs),             // Entire line
                    _ => {}
                }
                line.dirty = true;
            }
            'm' => {
                // SGR
                for param in params.iter() {
                    let p = param[0];
                    match p {
                        0 => self.current_attrs = Attrs::default(),
                        1 => self.current_attrs.bold = true,
                        4 => self.current_attrs.underline = true,
                        7 => self.current_attrs.reverse = true,
                        22 => self.current_attrs.bold = false,
                        24 => self.current_attrs.underline = false,
                        27 => self.current_attrs.reverse = false,
                        30..=37 => self.current_attrs.fg = Color::Indexed((p - 30) as u8),
                        39 => self.current_attrs.fg = Color::DefaultFg,
                        40..=47 => self.current_attrs.bg = Color::Indexed((p - 40) as u8),
                        49 => self.current_attrs.bg = Color::DefaultBg,
                        90..=97 => self.current_attrs.fg = Color::Indexed((p - 90 + 8) as u8),
                        100..=107 => self.current_attrs.bg = Color::Indexed((p - 100 + 8) as u8),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(model: &mut ScreenModel, bytes: &[u8]) {
        let mut parser = vte::Parser::new();
        for byte in bytes {
            parser.advance(model, *byte);
        }
    }

    // Cursor Forward and Cursor Backward are both content-preserving, strict ECMA-48
    // semantics: CUF/CUB must move the cursor without touching cell content.
    #[test]
    fn cursor_forward_and_backward_do_not_touch_cell_content() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"AB");
        model.lines[0].dirty = false;

        feed(&mut model, b"\x1b[2D");
        assert_eq!(model.lines[0].chars[0], 'A');
        assert_eq!(model.lines[0].chars[1], 'B');
        assert!(!model.lines[0].dirty);
        assert_eq!(model.cursor_x, 0);

        feed(&mut model, b"\x1b[2C");
        assert_eq!(model.lines[0].chars[0], 'A');
        assert_eq!(model.lines[0].chars[1], 'B');
        assert!(!model.lines[0].dirty);
        assert_eq!(model.cursor_x, 2);
    }

    // Reproduces the regression from /ai/firstmate/data/picocalc-random-blanked-chars/report.md:
    // Ink uses the exact same bare `CSI C` idiom to skip over a column whose character
    // is unchanged from the previous frame during a redraw - including a real mid-word
    // letter - as it does for runs of typed spaces it believes are already blank. This
    // replays the report's own live-captured byte sequence ("Captain, s" + bare CSI C
    // skipping an already-correct 'h' + "ip" + "shape.") and asserts the skipped letter
    // survives untouched, matching content-preserving CUF semantics. Relies on
    // cursor_move_count treating the bare form as count 1, not 0.
    #[test]
    fn cursor_forward_over_unchanged_midword_letter_does_not_blank_it() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"Captain, shipshape.\r");

        feed(&mut model, b"Captain, s\x1b[Cip");

        let line: alloc::string::String = model.lines[0].chars[0..19].iter().collect();
        assert_eq!(line, "Captain, shipshape.");
    }

    // vte always pushes a parameter (0, not absent) even for a bare CSI, so a naive
    // `.unwrap_or(1)` on the parsed value would silently compute n=0 for the extremely
    // common bare `ESC[C` form (no digit) claude-code's Ink renderer actually emits.
    #[test]
    fn cursor_forward_default_count_is_one() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"XYZ\r");

        feed(&mut model, b"\x1b[C");

        assert_eq!(model.cursor_x, 1);
    }

    #[test]
    fn cursor_forward_clamps_to_line_width() {
        let mut model = ScreenModel::default();
        let cols = model.width() as usize;
        feed(&mut model, format!("\x1b[{}C", cols + 50).as_bytes());
        assert_eq!(model.cursor_x, cols - 1);
    }

    // ECMA-48 treats "0" and "absent" as the same "use the default value" case, so an
    // explicit zero parameter ("\x1b[0C"/"\x1b[0D") must move by 1, exactly like the
    // bare form - cursor_move_count handles both the same way.
    #[test]
    fn cursor_forward_and_backward_move_by_one_on_explicit_zero_param() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"AB");
        assert_eq!(model.cursor_x, 2);

        feed(&mut model, b"\x1b[0D");
        assert_eq!(model.cursor_x, 1);

        feed(&mut model, b"\x1b[0C");
        assert_eq!(model.cursor_x, 2);
    }

    #[test]
    fn erase_in_line_still_blanks_and_marks_dirty() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"ABCDE\r");
        model.lines[0].dirty = false;

        feed(&mut model, b"\x1b[2K");

        assert!(model.lines[0].chars.iter().all(|&c| c == ' '));
        assert!(model.lines[0].dirty);
    }

    // Reproduces the regression from
    // /ai/firstmate/data/picocalc-overlapping-text-corruption/report.md: a terminal
    // multiplexer's "CUP-then-cheap-VPA-restore" idiom - an absolute cursor position
    // (e.g. for its own status-line redraw) immediately followed by a bare `CSI Pn d`
    // (Line Position Absolute) to restore just the row, leaving the column as the
    // multiplexer's own redraw left it - previously left cursor_y stale since VPA fell
    // through the catch-all. That caused the next print() to land on the wrong row,
    // smashing unrelated content together mid-word.
    #[test]
    fn vpa_line_position_absolute_restores_cursor_row() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"cong"); // row 0: live line so far
        feed(&mut model, b"\x1b[6;1Hunacknowledged data can be"); // row 5: unrelated content
        feed(&mut model, b"\x1b[6;5H\x1b[1d"); // CUP then VPA-restore idiom
        feed(&mut model, b"estion window begins"); // should land on row 0

        let row0: alloc::string::String = model.lines[0].chars[0..25].iter().collect();
        let row5: alloc::string::String = model.lines[5].chars[0..27].iter().collect();
        assert_eq!(row0.trim_end(), "congestion window begins");
        assert_eq!(row5.trim_end(), "unacknowledged data can be");
    }

    // The overlay (e.g. the battery readout shown on a power-button press) must
    // never touch the cell buffer - it's a paint-time compositing flag only, so
    // showing it can't perturb full_repaint/dirty state used by other logic.
    #[test]
    fn show_overlay_does_not_touch_cell_buffer_or_repaint_state() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"hello\r");
        model.full_repaint = false;
        model.lines[0].dirty = false;
        let before: alloc::string::String = model.lines[0].chars[0..5].iter().collect();

        model.show_overlay(alloc::string::String::from("Battery: 87%"));

        assert_eq!(model.overlay.as_deref(), Some("Battery: 87%"));
        assert!(!model.full_repaint);
        assert!(!model.lines[0].dirty);
        let after: alloc::string::String = model.lines[0].chars[0..5].iter().collect();
        assert_eq!(before, after);
    }

    // Reproduces the core requirement from the battery-overlay feature: content
    // the remote host writes while the overlay is showing must not be lost, and
    // once the overlay is dismissed the screen must reflect that content exactly
    // as if the overlay had never appeared - i.e. dismissal is just "repaint from
    // the (untouched, up to date) cell buffer", not a snapshot/restore.
    #[test]
    fn overlay_composited_then_dismissed_reverts_to_current_host_content() {
        let mut model = ScreenModel::default();
        feed(&mut model, b"before\r");

        model.show_overlay(alloc::string::String::from("Battery: 87%"));

        // Remote host output arrives while the overlay is up.
        feed(&mut model, b"after-overlay-output\r");

        assert!(model.overlay.is_some());
        model.clear_overlay();

        assert!(model.overlay.is_none());
        assert!(model.full_repaint);
        let row0: alloc::string::String = model.lines[0].chars[0..20].iter().collect();
        assert_eq!(row0, "after-overlay-output");
    }

    #[test]
    fn clear_overlay_is_a_noop_when_nothing_is_showing() {
        let mut model = ScreenModel::default();
        model.full_repaint = false;

        model.clear_overlay();

        assert!(model.overlay.is_none());
        assert!(!model.full_repaint);
    }

    // Recording-then-battery ordering: a power-button battery readout taken
    // while the button is still held must cover the recording overlay for its
    // own 3 s and then hand the screen back to it (and therefore the level
    // meter) rather than dismissing the recording indicator mid-utterance.
    #[test]
    fn timed_overlay_restores_the_recording_overlay_on_expiry() {
        let mut model = ScreenModel::default();
        model.show_overlay(alloc::string::String::from("recording..."));
        assert_eq!(model.overlay.as_deref(), Some("recording..."));

        model.show_timed_overlay(alloc::string::String::from("Battery: 87%"), 1_000, 3_000);
        assert_eq!(model.overlay.as_deref(), Some("Battery: 87%"));
        model.tick_overlay(3_999);
        assert_eq!(model.overlay.as_deref(), Some("Battery: 87%"));

        model.tick_overlay(4_000);
        assert_eq!(model.overlay.as_deref(), Some("recording..."));

        // A second battery readout while the recording is still live restores
        // it again.
        model.show_timed_overlay(alloc::string::String::from("Battery: 86%"), 5_000, 3_000);
        model.tick_overlay(8_000);
        assert_eq!(model.overlay.as_deref(), Some("recording..."));

        // Ending the recording clears it for good.
        model.clear_overlay();
        assert!(model.overlay.is_none());
    }

    // A restored persistent overlay can be narrower than the timed one it
    // replaced, so the expiry must force a full repaint or the wider timed
    // box's edges stay on the framebuffer for the rest of the utterance.
    #[test]
    fn timed_overlay_expiry_forces_a_repaint_when_it_restores_a_persistent_overlay() {
        let mut model = ScreenModel::default();
        model.show_overlay(alloc::string::String::from("recording..."));
        model.show_timed_overlay(
            alloc::string::String::from("Battery: 100% (charging)"),
            0,
            3_000,
        );
        model.full_repaint = false;

        model.tick_overlay(3_000);

        assert_eq!(model.overlay.as_deref(), Some("recording..."));
        assert!(model.full_repaint);
    }

    // The replacement direction is the same class: a narrower overlay taking
    // over a wider one must repaint the frame so the wider box's edges are
    // erased, while showing an overlay where none was present stays a pure
    // paint-time flag (see `show_overlay_does_not_touch_cell_buffer_or_...`).
    #[test]
    fn replacing_an_overlay_forces_a_repaint_to_erase_the_old_box() {
        let mut model = ScreenModel::default();
        model.show_timed_overlay(
            alloc::string::String::from("Battery: 100% (charging)"),
            0,
            3_000,
        );
        model.full_repaint = false;

        model.show_overlay(alloc::string::String::from("recording..."));

        assert_eq!(model.overlay.as_deref(), Some("recording..."));
        assert!(model.full_repaint);
    }

    #[test]
    fn lone_timed_overlay_clears_on_expiry() {
        let mut model = ScreenModel::default();
        model.full_repaint = false;
        model.show_timed_overlay(alloc::string::String::from("Battery: 50%"), 0, 3_000);
        assert_eq!(model.overlay.as_deref(), Some("Battery: 50%"));

        model.tick_overlay(2_999);
        assert_eq!(model.overlay.as_deref(), Some("Battery: 50%"));

        model.tick_overlay(3_000);
        assert!(model.overlay.is_none());
        assert!(model.full_repaint);
    }

    #[test]
    fn persistent_overlay_cancels_a_pending_timer() {
        let mut model = ScreenModel::default();
        model.show_timed_overlay(alloc::string::String::from("Battery: 87%"), 0, 3_000);

        // A recording starting after the battery readout replaces it for good:
        // the stale battery deadline must not clear the recording indicator.
        model.show_overlay(alloc::string::String::from("recording..."));
        model.tick_overlay(3_000);
        assert_eq!(model.overlay.as_deref(), Some("recording..."));
    }

    // Regression test for the heap-exhaustion crash investigated in
    // /ai/firstmate/data/picocalc-crash-display-buffer/report.md: a correctly
    // *capped* scrollback buffer still exceeded the firmware's 64 KiB primary
    // heap by over 2x at the old default of 200 lines (measured: 152,550
    // bytes logical footprint). This asserts the real capped steady-state
    // footprint - measured via actual `Vec::capacity()`, not assumed sizes -
    // fits within `SCREEN_HEAP_BUDGET_BYTES` (64 KiB minus
    // `NON_SCREEN_HEAP_RESERVE_BYTES`, the heap the firmware's WiFi/TCP/SSH/SD
    // subsystems reliably need, per the launch brief's real `free`-command
    // readings). Re-run this after changing `HEAP_SIZE`
    // (`src/heap.rs`), the screen geometry (font/`SCREEN_WIDTH`/
    // `SCREEN_HEIGHT`), or `NON_SCREEN_HEAP_RESERVE_BYTES`.
    #[test]
    fn default_scrollback_cap_keeps_full_footprint_within_heap_budget() {
        let mut model = ScreenModel::default();

        // Heavy-output session: far more lines than any plausible scrollback
        // cap, so scrollback reaches and stays at its capped steady state
        // (like repeated `help`/`ls`/`cat`-over-ssh in the real crash).
        for i in 0..2000 {
            feed(&mut model, format!("line {i}\r\n").as_bytes());
        }

        assert_eq!(
            model.scrollback.len(),
            model.max_scrollback(),
            "scrollback should be capped at steady state"
        );

        let resident_bytes: usize = model
            .scrollback
            .iter()
            .chain(model.lines.iter())
            .map(|line| {
                line.chars.capacity() * core::mem::size_of::<char>()
                    + line.attrs.capacity() * core::mem::size_of::<Attrs>()
            })
            .sum::<usize>()
            + (model.scrollback.capacity() + model.lines.capacity())
                * core::mem::size_of::<ScreenLine>();

        assert!(
            resident_bytes <= SCREEN_HEAP_BUDGET_BYTES,
            "capped steady-state resident bytes {resident_bytes} exceeds the \
             {SCREEN_HEAP_BUDGET_BYTES}-byte screen heap budget \
             (max_scrollback={}, rows={})",
            model.max_scrollback(),
            model.rows,
        );
    }

    // Reproduces the real-hardware growth pattern from the crash
    // investigation's `free`-command readings: two back-to-back ~15-line
    // `help` calls (identical output each time) where the *second* call
    // costs far more heap than the first. The mechanism is not a nonlinear
    // per-line cost inside `ScreenLine` - its `chars`/`attrs` are right-sized
    // in one allocation each via `vec![elem; width]`, not grown through
    // repeated small `push`es (see `ScreenLine::new`). It's that `scroll_up`
    // only allocates a brand-new `ScreenLine` (the line that scrolled off is
    // *moved* into `scrollback`, never freed and reused) once the visible
    // `lines` area is already full. A round of output that still fits in
    // still-blank visible rows costs zero new allocations; once the screen is
    // completely full, every single output line forces exactly one
    // scroll_up, i.e. one new full-line allocation - hence the sudden jump.
    #[test]
    fn heavy_output_scroll_pressure_accelerates_once_visible_area_fills_up() {
        let mut model = ScreenModel::default();
        let rows = model.rows;

        let scrolls_in_round = |model: &mut ScreenModel, lines: usize| -> usize {
            let before = model.scroll_up_calls();
            for i in 0..lines {
                feed(model, format!("help output line {i}\r\n").as_bytes());
            }
            model.scroll_up_calls() - before
        };

        let round1 = scrolls_in_round(&mut model, 15);
        let round2 = scrolls_in_round(&mut model, 15);
        let round3 = scrolls_in_round(&mut model, 15);

        assert!(
            round1 < round2,
            "round1={round1} round2={round2}: expected round 2 (visible area \
             already full of round 1's output) to scroll - and therefore \
             allocate - more than round 1, matching the real free-command \
             readings where the second identical `help` call cost ~4x the first"
        );
        assert_eq!(
            round3, 15,
            "once fully in steady-state scrolling (visible area permanently \
             full, rows={rows}), every output line should force exactly one \
             scroll_up/allocation - no more, no less"
        );
    }
}
