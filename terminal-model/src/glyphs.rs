use embedded_graphics::Pixel;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::*;

/// Box-drawing block (U+2500-U+259F): already hand-drawn via vector primitives
/// instead of the `profont` bitmap font, which doesn't cover this range.
pub fn is_box_char(c: char) -> bool {
    ('\u{2500}'..='\u{259F}').contains(&c)
}

/// Decorative Unicode symbols claude-code's Ink-based TUI emits for its prompt
/// chrome (chevron, bullets, ellipsis, arrows, dashes, the auto-mode triangle,
/// spinner/status glyphs). `profont` only covers ASCII + Latin-1, so these fall
/// back to its literal `?` glyph unless hand-drawn here, the same way box-drawing
/// characters already are.
pub fn is_symbol_char(c: char) -> bool {
    matches!(
        c,
        '\u{276F}' // ❯ prompt chevron
            | '\u{2022}' // • bullet
            | '\u{25CF}' // ● black circle
            | '\u{25CB}' // ○ white circle
            | '\u{2026}' // … ellipsis
            | '\u{2190}' // ← left arrow
            | '\u{2191}' // ↑ up arrow
            | '\u{2192}' // → right arrow
            | '\u{2193}' // ↓ down arrow
            | '\u{2733}' // ✳ eight spoked asterisk
            | '\u{23F5}' // ⏵ black medium right-pointing triangle (auto-mode indicator)
            | '\u{2014}' // — em dash
            | '\u{2013}' // – en dash
            | '\u{2722}' // ✢ four teardrop-spoked asterisk (spinner frame)
            | '\u{2736}' // ✶ six pointed black star (spinner frame)
            | '\u{273B}' // ✻ teardrop-spoked asterisk (spinner frame)
            | '\u{273D}' // ✽ heavy teardrop-spoked asterisk (spinner frame)
    )
}

pub fn draw_box_char<D>(display: &mut D, c: char, x: i32, y: i32, w: u32, h: u32, color: Rgb565)
where
    D: DrawTarget<Color = Rgb565>,
{
    let cx = x + (w / 2) as i32;
    let cy = y + (h / 2) as i32;
    let stroke = 1; // Line thickness

    // Helper to draw line
    let line = |display: &mut D, x0, y0, x1, y1| {
        Line::new(Point::new(x0, y0), Point::new(x1, y1))
            .into_styled(PrimitiveStyle::with_stroke(color, stroke))
            .draw(display)
            .ok();
    };

    // Helper to draw an arc (used by the rounded-corner glyphs below)
    let arc = |display: &mut D, arc: Arc| {
        arc.into_styled(PrimitiveStyle::with_stroke(color, stroke))
            .draw(display)
            .ok();
    };

    // Helper to draw a heavy (stroke-2) line, used by the bold box-drawing glyphs
    let heavy_line = |display: &mut D, x0, y0, x1, y1| {
        Line::new(Point::new(x0, y0), Point::new(x1, y1))
            .into_styled(PrimitiveStyle::with_stroke(color, 2))
            .draw(display)
            .ok();
    };

    // Half-segments from the glyph center to each edge, used by the light
    // single-line box-drawing junction glyphs below (┌┐└┘├┤┬┴┼).
    let left = |display: &mut D| line(display, x, cy, cx, cy);
    let right = |display: &mut D| line(display, cx, cy, x + w as i32, cy);
    let up = |display: &mut D| line(display, cx, y, cx, cy);
    let down = |display: &mut D| line(display, cx, cy, cx, y + h as i32);

    // Helpers for the double-line corner glyphs below: a double-stroke vertical
    // segment spanning either the top or bottom half, and a double-stroke
    // horizontal segment spanning either the left or right half.
    let vert_double = |display: &mut D, down: bool| {
        let (y0, y1) = if down { (cy, y + h as i32) } else { (y, cy) };
        line(display, cx - 1, y0, cx - 1, y1);
        line(display, cx + 1, y0, cx + 1, y1);
    };
    let horiz_double = |display: &mut D, right: bool| {
        let (x0, x1) = if right { (cx, x + w as i32) } else { (x, cx) };
        line(display, x0, cy - 1, x1, cy - 1);
        line(display, x0, cy + 1, x1, cy + 1);
    };

    match c {
        // Light horizontal
        '\u{2500}' => line(display, x, cy, x + w as i32, cy),
        // Light vertical
        '\u{2502}' => line(display, cx, y, cx, y + h as i32),
        // Light down and right
        '\u{250C}' => {
            right(display);
            down(display);
        }
        // Light down and left
        '\u{2510}' => {
            left(display);
            down(display);
        }
        // Light up and right
        '\u{2514}' => {
            right(display);
            up(display);
        }
        // Light up and left
        '\u{2518}' => {
            left(display);
            up(display);
        }
        // Light vertical and right
        '\u{251C}' => {
            up(display);
            down(display);
            right(display);
        }
        // Light vertical and left
        '\u{2524}' => {
            up(display);
            down(display);
            left(display);
        }
        // Light horizontal and down
        '\u{252C}' => {
            left(display);
            right(display);
            down(display);
        }
        // Light horizontal and up
        '\u{2534}' => {
            left(display);
            right(display);
            up(display);
        }
        // Light vertical and horizontal
        '\u{253C}' => {
            left(display);
            right(display);
            up(display);
            down(display);
        }
        // Heavy horizontal
        '\u{2501}' => heavy_line(display, x, cy, x + w as i32, cy),
        // Heavy vertical
        '\u{2503}' => heavy_line(display, cx, y, cx, y + h as i32),
        // Block
        '\u{2588}' => {
            display
                .fill_solid(&Rectangle::new(Point::new(x, y), Size::new(w, h)), color)
                .ok();
        }
        // Upper half block
        '\u{2580}' => {
            display
                .fill_solid(
                    &Rectangle::new(Point::new(x, y), Size::new(w, h / 2)),
                    color,
                )
                .ok();
        }
        // Lower half block
        '\u{2584}' => {
            display
                .fill_solid(
                    &Rectangle::new(Point::new(x, y + (h / 2) as i32), Size::new(w, h - h / 2)),
                    color,
                )
                .ok();
        }
        // Shades
        '\u{2591}' => draw_shade(display, x, y, w, h, color, 1),
        '\u{2592}' => draw_shade(display, x, y, w, h, color, 2),
        '\u{2593}' => draw_shade(display, x, y, w, h, color, 3),

        // Rounded corners
        '\u{256D}' => {
            // Top-left
            arc(
                display,
                Arc::new(
                    Point::new(x + w as i32 / 2, y + h as i32 / 2),
                    w,
                    Angle::from_degrees(180.0),
                    Angle::from_degrees(90.0),
                ),
            );
            line(display, cx, cy + h as i32 / 2, cx, y + h as i32); // Extend down
            line(display, cx + w as i32 / 2, cy, x + w as i32, cy); // Extend right
        }
        '\u{256E}' => {
            // Top-right
            arc(
                display,
                Arc::new(
                    Point::new(x - w as i32 / 2, y + h as i32 / 2),
                    w,
                    Angle::from_degrees(270.0),
                    Angle::from_degrees(90.0),
                ),
            );
            line(display, cx, cy + h as i32 / 2, cx, y + h as i32); // Extend down
            line(display, x, cy, cx - w as i32 / 2, cy); // Extend left
        }
        '\u{2570}' => {
            // Bottom-left
            arc(
                display,
                Arc::new(
                    Point::new(x + w as i32 / 2, y - h as i32 / 2),
                    w,
                    Angle::from_degrees(90.0),
                    Angle::from_degrees(90.0),
                ),
            );
            line(display, cx, y, cx, cy - h as i32 / 2); // Extend up
            line(display, cx + w as i32 / 2, cy, x + w as i32, cy); // Extend right
        }
        '\u{256F}' => {
            // Bottom-right
            arc(
                display,
                Arc::new(
                    Point::new(x - w as i32 / 2, y - h as i32 / 2),
                    w,
                    Angle::from_degrees(0.0),
                    Angle::from_degrees(90.0),
                ),
            );
            line(display, cx, y, cx, cy - h as i32 / 2); // Extend up
            line(display, x, cy, cx - w as i32 / 2, cy); // Extend left
        }

        // Double lines
        '\u{2550}' => {
            // Horizontal double
            line(display, x, cy - 1, x + w as i32, cy - 1);
            line(display, x, cy + 1, x + w as i32, cy + 1);
        }
        '\u{2551}' => {
            // Vertical double
            line(display, cx - 1, y, cx - 1, y + h as i32);
            line(display, cx + 1, y, cx + 1, y + h as i32);
        }
        // Double corners (simplified as single heavy for now to save space/complexity, or proper implementation)
        '\u{2554}' => {
            // Double down-right
            vert_double(display, true);
            horiz_double(display, true);
        }
        '\u{2557}' => {
            // Double down-left
            vert_double(display, true);
            horiz_double(display, false);
        }
        '\u{255A}' => {
            // Double up-right
            vert_double(display, false);
            horiz_double(display, true);
        }
        '\u{255D}' => {
            // Double up-left
            vert_double(display, false);
            horiz_double(display, false);
        }

        _ => {
            // Fallback for unhandled box chars: draw a small rectangle
            Rectangle::new(Point::new(x + 2, y + 2), Size::new(w - 4, h - 4))
                .into_styled(PrimitiveStyle::with_stroke(color, 1))
                .draw(display)
                .ok();
        }
    }
}

/// Vector-draws the decorative symbols `is_symbol_char` matches, the same way
/// `draw_box_char` already hand-draws the box-drawing block, so these codepoints
/// never fall through to `Text::draw()` and hit `profont`'s `?` fallback glyph.
pub fn draw_symbol_char<D>(display: &mut D, c: char, x: i32, y: i32, w: u32, h: u32, color: Rgb565)
where
    D: DrawTarget<Color = Rgb565>,
{
    let cx = x + (w / 2) as i32;
    let cy = y + (h / 2) as i32;

    let line = |display: &mut D, x0, y0, x1, y1, stroke| {
        Line::new(Point::new(x0, y0), Point::new(x1, y1))
            .into_styled(PrimitiveStyle::with_stroke(color, stroke))
            .draw(display)
            .ok();
    };

    let filled_circle = |display: &mut D, diameter: u32| {
        let d = diameter.max(2);
        let top_left = Point::new(cx - (d / 2) as i32, cy - (d / 2) as i32);
        Circle::new(top_left, d)
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(display)
            .ok();
    };

    let outline_circle = |display: &mut D, diameter: u32| {
        let d = diameter.max(2);
        let top_left = Point::new(cx - (d / 2) as i32, cy - (d / 2) as i32);
        Circle::new(top_left, d)
            .into_styled(PrimitiveStyle::with_stroke(color, 1))
            .draw(display)
            .ok();
    };

    // Draws a shaft from `tail` to `tip` plus a two-line arrowhead at `tip`,
    // shared by the four directional arrow glyphs below.
    let arrow = |display: &mut D, tail: Point, tip: Point, head1: Point, head2: Point| {
        line(display, tail.x, tail.y, tip.x, tip.y, 1);
        line(display, tip.x, tip.y, head1.x, head1.y, 1);
        line(display, tip.x, tip.y, head2.x, head2.y, 1);
    };

    // Draws a plain cross (horizontal + vertical spoke), shared by the
    // four-teardrop-spoked asterisk and as the base of the burst pattern below.
    let cross = |display: &mut D, stroke| {
        line(display, x + 1, cy, x + w as i32 - 1, cy, stroke);
        line(display, cx, y + 1, cx, y + h as i32 - 1, stroke);
    };

    // Draws the 4-spoke asterisk burst (cross + diagonals) shared by the
    // eight-spoked, teardrop-spoked, and heavy-teardrop-spoked asterisk glyphs.
    let burst = |display: &mut D, stroke| {
        cross(display, stroke);
        line(
            display,
            x + 2,
            y + 2,
            x + w as i32 - 2,
            y + h as i32 - 2,
            stroke,
        );
        line(
            display,
            x + w as i32 - 2,
            y + 2,
            x + 2,
            y + h as i32 - 2,
            stroke,
        );
    };

    match c {
        // ❯ prompt chevron: a right-pointing angle bracket
        '\u{276F}' => {
            let x0 = x + w as i32 / 4;
            let x1 = x + (w as i32 * 3) / 4;
            let y0 = y + h as i32 / 4;
            let y1 = y + (h as i32 * 3) / 4;
            line(display, x0, y0, x1, cy, 2);
            line(display, x1, cy, x0, y1, 2);
        }
        // • bullet: small filled circle
        '\u{2022}' => filled_circle(display, h / 3),
        // ● black circle: larger filled circle
        '\u{25CF}' => filled_circle(display, (h * 2) / 3),
        // ○ white circle: outline only
        '\u{25CB}' => outline_circle(display, (h * 2) / 3),
        // … ellipsis: three evenly spaced dots along the baseline
        '\u{2026}' => {
            let dot_y = y + (h as i32 * 4) / 5;
            for i in 0..3i32 {
                let dot_x = x + (w as i32 * (2 * i + 1)) / 6;
                Pixel(Point::new(dot_x, dot_y), color).draw(display).ok();
                Pixel(Point::new(dot_x + 1, dot_y), color)
                    .draw(display)
                    .ok();
            }
        }
        // ← → ↑ ↓ arrows: a shaft plus a small arrowhead
        '\u{2190}' => arrow(
            display,
            Point::new(x + w as i32 - 2, cy),
            Point::new(x + 2, cy),
            Point::new(x + 2 + w as i32 / 3, cy - h as i32 / 4),
            Point::new(x + 2 + w as i32 / 3, cy + h as i32 / 4),
        ),
        '\u{2192}' => arrow(
            display,
            Point::new(x + 2, cy),
            Point::new(x + w as i32 - 2, cy),
            Point::new(x + w as i32 - 2 - w as i32 / 3, cy - h as i32 / 4),
            Point::new(x + w as i32 - 2 - w as i32 / 3, cy + h as i32 / 4),
        ),
        '\u{2191}' => arrow(
            display,
            Point::new(cx, y + h as i32 - 2),
            Point::new(cx, y + 2),
            Point::new(cx - w as i32 / 4, y + 2 + h as i32 / 3),
            Point::new(cx + w as i32 / 4, y + 2 + h as i32 / 3),
        ),
        '\u{2193}' => arrow(
            display,
            Point::new(cx, y + 2),
            Point::new(cx, y + h as i32 - 2),
            Point::new(cx - w as i32 / 4, y + h as i32 - 2 - h as i32 / 3),
            Point::new(cx + w as i32 / 4, y + h as i32 - 2 - h as i32 / 3),
        ),
        // ✳ eight spoked asterisk: burst of lines through the center
        '\u{2733}' => burst(display, 1),
        // ⏵ auto-mode triangle: small filled right-pointing triangle (play-button style)
        '\u{23F5}' => {
            let x0 = x + w as i32 / 4;
            let x1 = x + (w as i32 * 3) / 4;
            let y0 = y + h as i32 / 4;
            let y1 = y + (h as i32 * 3) / 4;
            Triangle::new(Point::new(x0, y0), Point::new(x0, y1), Point::new(x1, cy))
                .into_styled(PrimitiveStyle::with_fill(color))
                .draw(display)
                .ok();
        }
        // — em dash: a long horizontal line spanning most of the cell width
        '\u{2014}' => line(display, x + 1, cy, x + w as i32 - 1, cy, 1),
        // – en dash: a shorter horizontal line, roughly half the cell width
        '\u{2013}' => line(display, x + w as i32 / 4, cy, x + (w as i32 * 3) / 4, cy, 1),
        // ✢ four teardrop-spoked asterisk: a plain cross (4 spokes, no diagonals)
        '\u{2722}' => cross(display, 1),
        // ✶ six pointed black star: three lines through the center, spaced apart
        '\u{2736}' => {
            line(display, x + 1, cy, x + w as i32 - 1, cy, 1);
            line(
                display,
                cx - w as i32 / 4,
                y + 1,
                cx + w as i32 / 4,
                y + h as i32 - 1,
                1,
            );
            line(
                display,
                cx + w as i32 / 4,
                y + 1,
                cx - w as i32 / 4,
                y + h as i32 - 1,
                1,
            );
        }
        // ✻ teardrop-spoked asterisk: same burst treatment as the eight-spoked asterisk
        '\u{273B}' => burst(display, 1),
        // ✽ heavy teardrop-spoked asterisk: the same burst, drawn with a heavier stroke
        '\u{273D}' => burst(display, 2),
        _ => {}
    }
}

fn draw_shade<D>(display: &mut D, x: i32, y: i32, w: u32, h: u32, color: Rgb565, density: u8)
where
    D: DrawTarget<Color = Rgb565>,
{
    for py in 0..h {
        for px in 0..w {
            let on = match density {
                1 => (px % 2 == 0) && (py % 2 == 0),    // 25%
                2 => (px + py) % 2 == 0,                // 50%
                3 => !((px % 2 == 0) && (py % 2 == 0)), // 75%
                _ => false,
            };
            if on {
                Pixel(Point::new(x + px as i32, y + py as i32), color)
                    .draw(display)
                    .ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::mock_display::MockDisplay;

    #[test]
    fn box_drawing_range_is_recognized() {
        assert!(is_box_char('\u{2500}'));
        assert!(is_box_char('\u{259F}'));
        assert!(!is_box_char('A'));
        assert!(!is_box_char(' '));
    }

    #[test]
    fn draw_box_char_actually_paints_pixels() {
        for c in [
            '\u{2500}', '\u{2502}', '\u{250C}', '\u{2510}', '\u{2514}', '\u{2518}', '\u{251C}',
            '\u{2524}', '\u{252C}', '\u{2534}', '\u{253C}', '\u{2501}', '\u{2503}', '\u{2588}',
            '\u{2580}', '\u{2584}', '\u{2591}', '\u{2592}', '\u{2593}',
        ] {
            let mut display: MockDisplay<Rgb565> = MockDisplay::new();
            display.set_allow_out_of_bounds_drawing(true);
            display.set_allow_overdraw(true);
            draw_box_char(&mut display, c, 0, 0, 10, 20, Rgb565::WHITE);
            assert_ne!(
                display,
                MockDisplay::new(),
                "U+{:04X} produced no pixels",
                c as u32
            );
        }
    }

    // Every codepoint the report captured live from claude-code must dispatch to
    // the vector-draw path rather than falling through to Text::draw()'s profont
    // rendering, which is what previously produced the literal `?` fallback glyph.
    #[test]
    fn report_captured_codepoints_are_symbol_chars() {
        for c in [
            '\u{276F}', '\u{2022}', '\u{2026}', '\u{2190}', '\u{25CF}', '\u{2733}',
        ] {
            assert!(
                is_symbol_char(c),
                "U+{:04X} should dispatch to draw_symbol_char",
                c as u32
            );
        }
    }

    // Codepoints live-captured for this follow-up fix: the auto-mode triangle
    // (originally captured but never wired up), em/en dashes (never captured at
    // all), and the spinner-frame asterisk family beyond the single U+2733 frame
    // the original report happened to catch.
    #[test]
    fn followup_captured_codepoints_are_symbol_chars() {
        for c in [
            '\u{23F5}', '\u{2014}', '\u{2013}', '\u{2722}', '\u{2736}', '\u{273B}', '\u{273D}',
        ] {
            assert!(
                is_symbol_char(c),
                "U+{:04X} should dispatch to draw_symbol_char",
                c as u32
            );
        }
    }

    #[test]
    fn plain_ascii_and_box_chars_are_not_symbol_chars() {
        assert!(!is_symbol_char('>'));
        assert!(!is_symbol_char('A'));
        assert!(!is_symbol_char(' '));
        assert!(
            !is_symbol_char('\u{2500}'),
            "box-drawing chars must stay on the draw_box_char path"
        );
        assert!(
            !is_box_char('\u{276F}'),
            "symbol chars must not collide with the box-drawing dispatch"
        );
    }

    #[test]
    fn draw_symbol_char_actually_paints_pixels() {
        // Regression guard for a no-op dispatch: every codepoint in is_symbol_char
        // must produce at least one non-background pixel when drawn, otherwise the
        // fix would silently render nothing instead of fixing the '?' fallback.
        for c in [
            '\u{276F}', '\u{2022}', '\u{25CF}', '\u{25CB}', '\u{2026}', '\u{2190}', '\u{2191}',
            '\u{2192}', '\u{2193}', '\u{2733}', '\u{23F5}', '\u{2014}', '\u{2013}', '\u{2722}',
            '\u{2736}', '\u{273B}', '\u{273D}',
        ] {
            let mut display: MockDisplay<Rgb565> = MockDisplay::new();
            display.set_allow_out_of_bounds_drawing(true);
            // Strokes intentionally cross near the glyph center (e.g. arrowheads,
            // the asterisk spokes), which redraws some pixels more than once.
            display.set_allow_overdraw(true);
            draw_symbol_char(&mut display, c, 0, 0, 10, 20, Rgb565::WHITE);
            assert_ne!(
                display,
                MockDisplay::new(),
                "U+{:04X} produced no pixels",
                c as u32
            );
        }
    }
}
