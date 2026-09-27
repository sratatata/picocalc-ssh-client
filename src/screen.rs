extern crate alloc;

use alloc::string::String;
use core::fmt;
use core::ops::{Deref, DerefMut};
use embassy_embedded_hal::shared_bus::blocking::spi::SpiDeviceWithConfig;
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::SPI1;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
use embassy_time::{Duration, Instant, Ticker};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle, MonoTextStyleBuilder};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::*;
use embedded_graphics::text::Text;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9488Rgb565;

use terminal_model::glyphs::{draw_box_char, draw_symbol_char, is_box_char, is_symbol_char};
use terminal_model::screen_model::ScreenModel;
pub use terminal_model::screen_model::{SCREEN_HEIGHT, SCREEN_WIDTH};

// Define PicoCalcDisplay here so it can be used in main.rs and here
pub type PicoCalcDisplay<'a> = mipidsi::Display<
    SpiInterface<
        'a,
        SpiDeviceWithConfig<
            'a,
            NoopRawMutex,
            embassy_rp::spi::Spi<'a, SPI1, embassy_rp::spi::Blocking>,
            Output<'a>,
        >,
        Output<'a>,
    >,
    ILI9488Rgb565,
    Output<'a>,
>;

pub static SCREEN: LazyLock<AsyncMutex<CriticalSectionRawMutex, Screen>> =
    LazyLock::new(|| AsyncMutex::new(Screen::new()));

/// How long the battery overlay stays on screen before it auto-dismisses.
const OVERLAY_DURATION: Duration = Duration::from_secs(3);
/// How long a transient notice (e.g. "no SSH session") stays on screen.
const NOTICE_DURATION: Duration = Duration::from_secs(2);

pub struct Screen {
    model: ScreenModel,
    parser: vte::Parser,
}

impl Deref for Screen {
    type Target = ScreenModel;
    fn deref(&self) -> &ScreenModel {
        &self.model
    }
}

impl DerefMut for Screen {
    fn deref_mut(&mut self) -> &mut ScreenModel {
        &mut self.model
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            model: ScreenModel::default(),
            parser: vte::Parser::new(),
        }
    }

    pub fn parse_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.parser.advance(&mut self.model, *byte);
        }
    }

    pub fn print(&mut self, text: &str) {
        self.parse_bytes(text.as_bytes())
    }

    pub fn clear(&mut self) {
        self.model.clear();
    }

    /// Show `text` as a transient overlay on top of whatever is currently on
    /// screen; it auto-dismisses after `OVERLAY_DURATION` without corrupting the
    /// underlying terminal buffer, restoring any overlay it covered.
    pub fn show_battery_overlay(&mut self, text: String) {
        self.model.show_timed_overlay(
            text,
            Instant::now().as_millis(),
            OVERLAY_DURATION.as_millis(),
        );
    }

    /// Show `text` as a transient notice that clears itself after
    /// `NOTICE_DURATION`, for a button press that had nothing to do (e.g.
    /// push-to-talk with no SSH session up).
    pub fn show_notice(&mut self, text: String) {
        self.model.show_timed_overlay(
            text,
            Instant::now().as_millis(),
            NOTICE_DURATION.as_millis(),
        );
    }

    /// Show `text` as an overlay that stays until it is explicitly cleared,
    /// cancelling any auto-dismiss left over from a previous overlay.
    pub fn show_overlay(&mut self, text: String) {
        self.model.show_overlay(text);
    }

    pub fn update_display(&mut self, display: &mut PicoCalcDisplay) {
        self.model.tick_overlay(Instant::now().as_millis());
        update_display(&mut self.model, display);
    }
}

impl fmt::Write for Screen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.print(s);
        Ok(())
    }
}

fn text_style(
    font: &'static MonoFont<'static>,
    fg: Rgb565,
    bg: Rgb565,
) -> MonoTextStyle<'static, Rgb565> {
    MonoTextStyleBuilder::new()
        .font(font)
        .text_color(fg)
        .background_color(bg)
        .build()
}

fn fill_rect<'a>(
    display: &mut PicoCalcDisplay<'a>,
    point: Point,
    size: Size,
    color: Rgb565,
) -> Result<(), <PicoCalcDisplay<'a> as DrawTarget>::Error> {
    display.fill_solid(&Rectangle::new(point, size), color)
}

fn update_display(model: &mut ScreenModel, display: &mut PicoCalcDisplay) {
    if model.full_repaint {
        display.clear(Rgb565::BLACK).unwrap();
    }

    let font = model.font;
    let cell_width = font.character_size.width + font.character_spacing;
    let cell_height = font.character_size.height;

    for y in 0..model.rows {
        let line_idx = if model.viewport_offset > 0 {
            // Calculate absolute index in history + lines
            // Total lines = scrollback.len() + lines.len() (which is rows)
            // View start = Total lines - rows - viewport_offset
            // Current row abs index = View start + y
            let total_len = model.scrollback.len() + model.rows;
            let view_start = total_len
                .saturating_sub(model.rows)
                .saturating_sub(model.viewport_offset);
            let abs_idx = view_start + y;

            if abs_idx < model.scrollback.len() {
                Some(&mut model.scrollback[abs_idx])
            } else {
                Some(&mut model.lines[abs_idx - model.scrollback.len()])
            }
        } else {
            Some(&mut model.lines[y])
        };

        let line = match line_idx {
            Some(l) => l,
            None => continue,
        };

        if !line.dirty && !model.full_repaint {
            continue;
        }

        let row_y = y as u32 * cell_height;
        if row_y >= SCREEN_HEIGHT as u32 {
            break;
        }

        for (x, (char, attr)) in line.chars.iter().zip(line.attrs.iter()).enumerate() {
            let col_x = x as u32 * cell_width;
            if col_x >= SCREEN_WIDTH as u32 {
                break;
            }

            let mut fg = attr.fg.to_rgb565(false);
            let mut bg = attr.bg.to_rgb565(true);

            if attr.reverse {
                core::mem::swap(&mut fg, &mut bg);
            }

            if attr.bold {
                // Brighten fg?
                if fg == Rgb565::CSS_LIGHT_GRAY {
                    fg = Rgb565::WHITE;
                }
            }

            // Draw background
            fill_rect(
                display,
                Point::new(col_x as i32, row_y as i32),
                Size::new(cell_width, cell_height),
                bg,
            )
            .unwrap();

            // Draw text
            if *char != ' ' {
                let style = text_style(font, fg, bg);

                // We need to handle char string
                let mut buf = [0u8; 4];
                let s = char.encode_utf8(&mut buf);

                if is_box_char(*char) {
                    draw_box_char(
                        display,
                        *char,
                        col_x as i32,
                        row_y as i32,
                        cell_width,
                        cell_height,
                        fg,
                    );
                } else if is_symbol_char(*char) {
                    draw_symbol_char(
                        display,
                        *char,
                        col_x as i32,
                        row_y as i32,
                        cell_width,
                        cell_height,
                        fg,
                    );
                } else {
                    Text::new(
                        s,
                        Point::new(col_x as i32, row_y as i32 + font.baseline as i32),
                        style,
                    )
                    .draw(display)
                    .ok(); // Ignore errors for missing glyphs
                }
            }

            if attr.underline {
                fill_rect(
                    display,
                    Point::new(col_x as i32, (row_y + cell_height - 1) as i32),
                    Size::new(cell_width, 1),
                    fg,
                )
                .unwrap();
            }
        }
        line.dirty = false;
    }
    model.full_repaint = false;

    // Draw cursor
    let cx = model.cursor_x as u32 * cell_width;
    let cy = model.cursor_y as u32 * cell_height;
    if cx < SCREEN_WIDTH as u32 && cy < SCREEN_HEIGHT as u32 {
        fill_rect(
            display,
            Point::new(cx as i32, cy as i32),
            Size::new(cell_width, cell_height),
            Rgb565::WHITE,
        )
        .ok();
    }

    // Composite the overlay (if any) on top of whatever was just painted. This
    // never touches `model.lines`/`scrollback`, so once `overlay` goes back to
    // `None` the forced full repaint in `ScreenModel::clear_overlay` redraws the
    // real, possibly-changed content underneath exactly as if the overlay had
    // never been shown.
    if let Some(text) = &model.overlay {
        draw_overlay(model.font, text, display);
    }
}

/// Height of the push-to-talk level-meter bar, in pixels.
const LEVEL_METER_HEIGHT: u32 = 8;
/// AC RMS level (`i16` counts) that fills the meter. Calibrated against the
/// windowed-median statistic's actual measured range on four confirmed-good
/// real captures (independently verified transcribable by both openai-whisper
/// and whisper.cpp): per-recording median levels of 5-10, 90th-percentile
/// levels of 32-79, and peaks of 118-307. The original value of 512 was
/// calibrated against a synthetic 100%-duty-cycle test tone (see
/// `ac_level_ignores_a_spike_confined_to_one_window`'s `loud` fixture in
/// `pcm_extract.rs`) rather than real speech; real speech has silence between
/// words and syllables, so a 25ms window's own median-of-8-sub-window RMS
/// reads far lower than a sustained tone at the same peak amplitude ever
/// would. That mismatch - not the underlying statistic, which is correctly
/// glitch-robust - is why the meter stayed visually near-empty on real
/// hardware despite carrying real, transcribable audio.
const LEVEL_METER_FULL_SCALE: u32 = 150;

fn draw_overlay(font: &'static MonoFont<'static>, text: &str, display: &mut PicoCalcDisplay) {
    const PADDING_X: i32 = 10;
    const PADDING_Y: i32 = 8;

    // While recording, reserve a strip under the text for the level meter. Any
    // other overlay (e.g. the battery readout) is unchanged.
    let recording = crate::mic::is_recording();
    let meter_space = if recording {
        LEVEL_METER_HEIGHT as i32 + 6
    } else {
        0
    };

    let char_count = text.chars().count() as u32;
    let text_width = char_count * (font.character_size.width + font.character_spacing);
    let text_height = font.character_size.height;

    let box_w = text_width + (PADDING_X as u32) * 2;
    let box_h = text_height + (PADDING_Y as u32) * 2 + meter_space as u32;

    let x = (SCREEN_WIDTH as i32 - box_w as i32) / 2;
    let y = (SCREEN_HEIGHT as i32 - box_h as i32) / 2;

    let bg = Rgb565::CSS_DARK_SLATE_GRAY;
    let fg = Rgb565::WHITE;

    let rect = Rectangle::new(Point::new(x, y), Size::new(box_w, box_h));
    display.fill_solid(&rect, bg).ok();
    rect.into_styled(
        PrimitiveStyleBuilder::new()
            .stroke_color(fg)
            .stroke_width(1)
            .build(),
    )
    .draw(display)
    .ok();

    let style = text_style(font, fg, bg);
    Text::new(
        text,
        Point::new(x + PADDING_X, y + PADDING_Y + font.baseline as i32),
        style,
    )
    .draw(display)
    .ok();

    if recording {
        draw_level_meter(
            display,
            x + PADDING_X,
            y + PADDING_Y + text_height as i32 + 6,
            box_w - (PADDING_X as u32) * 2,
            LEVEL_METER_HEIGHT,
            crate::mic::level(),
        );
    }
}

/// Draws the realtime push-to-talk input meter: an empty track that fills from
/// the left with the current AC level, turning red when pinned at full scale so
/// a loud (or railed) input is obvious. The whole strip is repainted each
/// overlay pass, so the bar both grows and shrinks rather than smearing.
fn draw_level_meter(
    display: &mut PicoCalcDisplay,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    level: u32,
) {
    fill_rect(
        display,
        Point::new(x, y),
        Size::new(width, height),
        Rgb565::BLACK,
    )
    .ok();
    let filled = (width * level.min(LEVEL_METER_FULL_SCALE)) / LEVEL_METER_FULL_SCALE;
    if filled > 0 {
        let color = if level >= LEVEL_METER_FULL_SCALE {
            Rgb565::RED
        } else {
            Rgb565::GREEN
        };
        fill_rect(display, Point::new(x, y), Size::new(filled, height), color).ok();
    }
}

#[embassy_executor::task]
pub async fn screen_painter(mut display: PicoCalcDisplay<'static>) {
    display.clear(Rgb565::BLACK).unwrap();
    let _ = display.set_vertical_scroll_region(0, 0);

    let mut ticker = Ticker::every(Duration::from_millis(200));
    loop {
        SCREEN.get().lock().await.update_display(&mut display);
        ticker.next().await;
    }
}

pub async fn cls_command(_args: &[&str]) {
    SCREEN.get().lock().await.clear();
}
