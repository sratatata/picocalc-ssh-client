//! Hardware-independent PCM extraction arithmetic for the I2S mic capture.
//!
//! The mic (an Adafruit SPH0645) has a fixed 32-bit-per-channel I2S slot
//! (64fs total per L+R frame - see `mic.rs`'s settings for the full
//! derivation against its documented clock table). The PIO program's RX FIFO
//! therefore delivers one 32-bit word per channel slot, alternating left and
//! right, rather than one word per combined L+R frame. This module holds the
//! arithmetic that doesn't need any hardware to verify: the bit-clock
//! frequency the firmware must generate, whether a candidate slot width /
//! sample-rate pair keeps that clock inside the mic's documented range, and
//! how a mono PCM stream is recovered from the alternating raw words.
//! Keeping this here (free of embassy/`crate` dependencies) lets host tests
//! exercise the exact arithmetic the firmware runs, per AGENTS.md's
//! "host-testable logic lives in terminal-model" guidance.
//!
//! Slot width, sample rate and BCLK edge polarity are runtime-configurable on
//! the device (see `mic.rs`) so the microphone can be probed without a reflash.
//! The default remains the documented-correct 32-bit slot at 16 kHz, i.e. a
//! 1.024 MHz BCLK.

/// Bit-clock (BCLK) frequency, in Hz, this mic requires for a given sample
/// rate: `sample_rate_hz * bits_per_channel_slot * channels`. For the
/// SPH0645's fixed 32-bit slot at 16 kHz this is 1.024 MHz, the bottom of its
/// documented supported-clock table.
pub const fn bit_clock_hz(sample_rate_hz: u32, bits_per_channel_slot: u32, channels: u32) -> u32 {
    sample_rate_hz
        .saturating_mul(bits_per_channel_slot)
        .saturating_mul(channels)
}

/// Lowest BCLK the SPH0645's documented clock table supports (1.024 MHz).
pub const MIN_MIC_BCLK_HZ: u32 = 1_024_000;
/// Highest BCLK the SPH0645's documented clock table supports (4.096 MHz).
pub const MAX_MIC_BCLK_HZ: u32 = 4_096_000;
/// Smallest configurable channel slot width, in bits. Below this the
/// extraction arithmetic stops being meaningful; the PIO loop counter needs
/// at least `bits - 2`.
pub const MIN_SLOT_BITS: u32 = 8;
/// Largest channel slot width, in bits (the PIO shift register's width).
pub const MAX_SLOT_BITS: u32 = 32;

/// Whether `bits_per_channel_slot` and `sample_rate_hz` describe a
/// configuration this mic can actually run, i.e. the resulting BCLK falls
/// inside its documented `MIN_MIC_BCLK_HZ..=MAX_MIC_BCLK_HZ` window and both
/// inputs are in range. The firmware refuses to clock the mic outside this
/// window rather than silently mis-clocking it (see `mic.rs`'s config
/// handling).
pub fn mic_settings_valid(bits_per_channel_slot: u32, sample_rate_hz: u32) -> bool {
    if sample_rate_hz == 0 || !(MIN_SLOT_BITS..=MAX_SLOT_BITS).contains(&bits_per_channel_slot) {
        return false;
    }
    // `checked_mul` keeps an absurd stored sample rate from wrapping the
    // BCLK computation into a value that happens to sit in range.
    match sample_rate_hz
        .checked_mul(bits_per_channel_slot)
        .and_then(|bclk| bclk.checked_mul(2))
    {
        Some(bclk) => (MIN_MIC_BCLK_HZ..=MAX_MIC_BCLK_HZ).contains(&bclk),
        None => false,
    }
}

/// Recovers mono 16-bit PCM samples from raw I2S RX FIFO words, where each
/// word is one channel's full slot (MSB-first, alternating left then right)
/// rather than a combined L+R pair.
///
/// Keeps only the even-indexed (left-slot) words - swap to odd-indexed if a
/// mic is instead wired to the right slot - and takes each kept word's most
/// significant 16 bits as the PCM sample. The PIO `ShiftDirection::Left`
/// autopush leaves a slot's `bits_per_channel_slot` captured bits in the low
/// `bits_per_channel_slot` bits of the word, so the top 16 of the slot are
/// `word >> (bits - 16)` for widths >= 16, or left-justified by
/// `16 - bits` for narrower slots. `pcm` is filled from `raw.len() / 2`
/// words; any extra `pcm` entries beyond that are left untouched.
pub fn extract_left_channel_pcm(raw: &[u32], pcm: &mut [i16], bits_per_channel_slot: u32) {
    for (dst, word) in pcm.iter_mut().zip(raw.iter().step_by(2)) {
        *dst = slot_sample(*word, bits_per_channel_slot);
    }
}

/// The 16-bit PCM sample a single captured slot word carries, using the same
/// shift the production extraction applies: the slot's most significant 16
/// bits for widths >= 16, or the captured bits left-justified for narrower
/// slots.
pub fn slot_sample(word: u32, bits_per_channel_slot: u32) -> i16 {
    let bits = bits_per_channel_slot.clamp(1, 32);
    if bits >= 16 {
        (word >> (bits - 16)) as i16
    } else {
        (word << (16 - bits)) as u16 as i16
    }
}

fn integer_sqrt(value: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    let mut x = value;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + value / x) / 2;
    }
    x
}

/// Number of equal windows the level meter splits a chunk into. Eight is
/// enough that the known per-chunk capture artifact (a handful of extreme
/// samples once per DMA transfer) falls inside one window and is out-voted by
/// the median, while still tracking a sustained signal.
const LEVEL_WINDOWS: usize = 8;

fn median(values: &mut [u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

/// Robust DC-removed level over `len` samples reached through `sample`.
///
/// Splits the chunk into [`LEVEL_WINDOWS`] equal windows, takes each window's
/// RMS around that window's own mean, and returns the median of those values.
/// Because each window removes its own DC, the mic's large offset does not bias
/// the result; because the result is a median across windows, an isolated
/// spike or dropout confined to one window cannot throw it. This is what makes
/// the on-screen meter honest: a microphone sitting on its noise floor reads
/// empty, while sustained speech raises every window and therefore the median.
fn windowed_ac_level(len: usize, sample: impl Fn(usize) -> i32) -> u32 {
    if len == 0 {
        return 0;
    }
    let mut levels = [0u32; LEVEL_WINDOWS];
    let mut count = 0usize;
    for window in 0..LEVEL_WINDOWS {
        let start = window * len / LEVEL_WINDOWS;
        let end = (window + 1) * len / LEVEL_WINDOWS;
        if end <= start {
            continue;
        }
        let n = (end - start) as i64;
        let mean = (start..end).map(|i| sample(i) as i64).sum::<i64>() / n;
        let sum_sq: i64 = (start..end)
            .map(|i| {
                let d = sample(i) as i64 - mean;
                d * d
            })
            .sum();
        levels[count] = integer_sqrt((sum_sq / n) as u64) as u32;
        count += 1;
    }
    median(&mut levels[..count])
}

/// Level-meter value for a chunk of extracted mono samples: a windowed median
/// of per-window AC RMS (see [`windowed_ac_level`]). Integer-only and linear,
/// so a mic on its noise floor reads near zero while speech drives it up; the
/// raw value is the number of `i16` counts, which the overlay maps to a bar.
/// The firmware computes it on the upload task from the samples it has already
/// drained from the audio ring, never on the DMA capture path.
pub fn ac_rms_level(samples: &[i16]) -> u32 {
    windowed_ac_level(samples.len(), |i| samples[i] as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sph0645_bit_clock_matches_documented_table_at_16khz() {
        // 16 kHz * 32-bit slot * 2 channels = 1.024 MHz, exactly the clock
        // this mic's own datasheet-derived table requires at this sample
        // rate - see mic.rs's settings comment for the source. This guards
        // against an accidental regression back to a 16-bit slot (512 kHz,
        // exactly half), which is what originally shipped and produced
        // garbage on real hardware.
        assert_eq!(bit_clock_hz(16_000, 32, 2), 1_024_000);
    }

    #[test]
    fn default_settings_are_inside_the_documented_clock_window() {
        assert!(mic_settings_valid(32, 16_000));
        // 32-bit slots at 32/64 kHz sit at the top of the window.
        assert!(mic_settings_valid(32, 64_000));
        assert_eq!(bit_clock_hz(64_000, 32, 2), 4_096_000);
    }

    #[test]
    fn bit_clock_saturates_instead_of_wrapping_for_absurd_rates() {
        // A `config set ptt_rate` far above the mic's range is correctly
        // rejected by `mic_settings_valid`, but `bclk_hz()` still formats the
        // refusal message. A wrapping multiply would print a plausible
        // in-window number (or panic with overflow checks on); saturating
        // keeps the displayed clock honest.
        assert_eq!(bit_clock_hz(u32::MAX, 32, 2), u32::MAX);
        assert_eq!(bit_clock_hz(4_000_000_000, 32, 2), u32::MAX);
    }

    #[test]
    fn out_of_window_settings_are_rejected_not_mis_clocked() {
        // 16-bit slots at 16 kHz is the 512 kHz configuration that produced a
        // dead line on real hardware; it must be refused.
        assert!(!mic_settings_valid(16, 16_000));
        assert!(!mic_settings_valid(32, 8_000)); // 512 kHz
        assert!(!mic_settings_valid(32, 128_000)); // 8.192 MHz
        assert!(!mic_settings_valid(7, 16_000)); // below MIN_SLOT_BITS
        assert!(!mic_settings_valid(33, 16_000)); // above MAX_SLOT_BITS
        assert!(!mic_settings_valid(32, 0));
        // A rate so large the multiply would wrap must also be rejected
        // rather than wrapping into an in-window value.
        assert!(!mic_settings_valid(32, u32::MAX / 8));
    }

    #[test]
    fn a_narrower_slot_is_valid_at_a_proportionally_higher_rate() {
        // 16-bit slots are legal at 32 kHz (still exactly 1.024 MHz), which
        // is how the probe can test narrow slots without under-clocking.
        assert!(mic_settings_valid(16, 32_000));
        assert_eq!(bit_clock_hz(32_000, 16, 2), 1_024_000);
    }

    #[test]
    fn extract_left_channel_pcm_keeps_only_even_indexed_words() {
        // Alternating left/right words: left words carry a distinct pattern
        // in their top 16 bits so we can tell them apart from right words if
        // the extraction ever accidentally picks the wrong parity.
        let raw = [
            0x1111_0000u32, // left  (index 0)
            0x2222_0000u32, // right (index 1) - must be skipped
            0x3333_0000u32, // left  (index 2)
            0x4444_0000u32, // right (index 3) - must be skipped
        ];
        let mut pcm = [0i16; 2];
        extract_left_channel_pcm(&raw, &mut pcm, 32);
        assert_eq!(pcm, [0x1111u16 as i16, 0x3333u16 as i16]);
    }

    #[test]
    fn extract_left_channel_pcm_takes_top_16_of_a_32_bit_slot() {
        // MSB-first (ShiftDirection::Left) with a 32-bit slot: the top 16
        // bits of a captured word are this mic's most significant 16 (of its
        // 18 significant) bits. A word with a nonzero low half must not leak
        // into the extracted sample.
        let raw = [0xABCD_FFFFu32];
        let mut pcm = [0i16; 1];
        extract_left_channel_pcm(&raw, &mut pcm, 32);
        assert_eq!(pcm[0], 0xABCDu16 as i16);
    }

    #[test]
    fn extract_left_channel_pcm_uses_the_slot_width() {
        // A 24-bit slot leaves its captured bits in 23..0, so the top 16 are
        // bits 23..8 (a shift of 8), and a 16-bit slot needs no shift at all.
        // 0x123456 in a 24-bit slot yields 0x1234; 0xABCD in a 16-bit slot
        // stays 0xABCD.
        let mut pcm = [0i16; 1];
        extract_left_channel_pcm(&[0x0012_3456u32], &mut pcm, 24);
        assert_eq!(pcm[0], 0x1234u16 as i16);
        extract_left_channel_pcm(&[0x0000_ABCDu32], &mut pcm, 16);
        assert_eq!(pcm[0], 0xABCDu16 as i16);
    }

    #[test]
    fn extract_left_channel_pcm_left_justifies_a_sub_16_bit_slot() {
        // Below 16 bits there is no "top 16 of the slot"; left-justify the
        // captured bits instead of losing them to a zero shift.
        let mut pcm = [0i16; 1];
        extract_left_channel_pcm(&[0x0000_00ABu32], &mut pcm, 8);
        assert_eq!(pcm[0], 0xAB00u16 as i16);
    }

    #[test]
    fn extract_left_channel_pcm_stops_at_the_shorter_of_the_two_buffers() {
        // Fewer raw words than pcm slots: only as many samples as available
        // word-pairs are written, matching Iterator::zip's short-circuit -
        // this is what lets capture_task always pass a full-size raw buffer
        // without needing to track a partial-chunk count itself.
        let raw = [0x1111_0000u32, 0x2222_0000u32];
        let mut pcm = [0x7fffi16; 3];
        extract_left_channel_pcm(&raw, &mut pcm, 32);
        assert_eq!(pcm, [0x1111u16 as i16, 0x7fff, 0x7fff]);
    }

    #[test]
    fn ac_level_ignores_dc_and_measures_the_deviation() {
        // A flat DC chunk is silence no matter how large the offset.
        assert_eq!(ac_rms_level(&[1234i16; 80]), 0);
        // A sustained +/-10 square wave has RMS 10 in every window -> median 10.
        let square: [i16; 80] = core::array::from_fn(|i| if i % 2 == 0 { 10 } else { -10 });
        assert_eq!(ac_rms_level(&square), 10);
        // The same deviation on top of a large DC offset is unchanged.
        let offset: [i16; 80] = core::array::from_fn(|i| if i % 2 == 0 { 1010 } else { 990 });
        assert_eq!(ac_rms_level(&offset), 10);
        assert_eq!(ac_rms_level(&[]), 0);
    }

    #[test]
    fn ac_level_ignores_a_spike_confined_to_one_window() {
        // The real per-chunk capture artifact: a flat chunk with a few extreme
        // samples in one region. A plain chunk-mean AC RMS reads hundreds; the
        // windowed median must stay near the true (tiny) level so the meter
        // does not swing at idle.
        let mut chunk = [5i16; 400];
        chunk[..8].fill(8000);
        assert!(
            ac_rms_level(&chunk) < 20,
            "spike leaked into the level: {}",
            ac_rms_level(&chunk)
        );
        // A sustained signal raises every window and therefore the median.
        let loud: [i16; 400] = core::array::from_fn(|i| if i % 2 == 0 { 300 } else { -300 });
        assert!(
            ac_rms_level(&loud) > 250,
            "sustained signal read too low: {}",
            ac_rms_level(&loud)
        );
    }
}
