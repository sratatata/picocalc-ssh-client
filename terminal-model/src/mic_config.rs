//! Hardware-independent resolution of the push-to-talk microphone's runtime
//! settings from persisted config strings.
//!
//! The firmware stores the I2S debug knobs as plain strings in its existing
//! config store (`config set ptt_bits|ptt_rate|ptt_edge`, see
//! `src/mic.rs` for the store access). This module holds the pure part - the
//! defaults, the string parsing, the out-of-window fallback, the effective
//! value each key reports, and the console-time validation - so that host
//! tests can exercise the exact decision table the firmware runs, per
//! AGENTS.md's "host-testable logic lives in terminal-model" guidance.
//!
//! The binding contract from the captain's brief:
//!
//! * an unconfigured device behaves exactly as before, i.e. the defaults are
//!   today's proven values (32-bit slots at 16 kHz, default BCLK edge);
//! * a value that is absent, malformed, or outside the mic's documented range
//!   is either refused at the console ([`validate_setting`]) or, if it is
//!   already in the store, replaced by the default ([`resolve`]) - never
//!   silently mis-clocked;
//! * a change takes effect on the next recording because the caller re-reads
//!   and re-applies these settings before every capture, assembling the PIO
//!   program at runtime.

use crate::pcm_extract::{MAX_MIC_BCLK_HZ, MIN_MIC_BCLK_HZ, bit_clock_hz, mic_settings_valid};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// I2S frames a left+right pair per word-select cycle; fixed at two, as the
/// captain settled the channel framing.
pub const CHANNELS: u32 = 2;
/// Default sample rate, in Hz. Matches Whisper's internal resampling target,
/// so there's no benefit to capturing at a higher rate.
pub const DEFAULT_RATE_HZ: u32 = 16_000;
/// Default bits per channel slot. The SPH0645 has a fixed 32-bit slot; the
/// captain's proven-working configuration is 32-bit at 16 kHz (1.024 MHz
/// BCLK, the bottom of the mic's documented window).
pub const DEFAULT_BITS: u32 = 32;
/// Default bit-clock edge polarity: `false` keeps the historical side-set
/// values; `true` inverts the low (bit-clock) bit, shifting sampling by half
/// a BCLK cycle.
pub const DEFAULT_EDGE_FLIP: bool = false;
/// Config keys for the mic's runtime settings.
pub const BITS_KEY: &str = "ptt_bits";
pub const RATE_KEY: &str = "ptt_rate";
pub const EDGE_KEY: &str = "ptt_edge";

/// Microphone settings resolved from persisted config. Re-read and re-applied
/// at the start of every recording, so a `config set ptt_*` takes effect on
/// the next utterance without a reflash or reboot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MicSettings {
    /// Bits per channel slot; the PIO loop counter is `bits - 2`.
    pub bits: u32,
    /// I2S sample rate in Hz; the bit clock is `rate * bits * CHANNELS`.
    pub rate: u32,
    /// `true` inverts the BCLK edge the PIO program samples on.
    pub edge_flip: bool,
}

impl Default for MicSettings {
    fn default() -> Self {
        Self {
            bits: DEFAULT_BITS,
            rate: DEFAULT_RATE_HZ,
            edge_flip: DEFAULT_EDGE_FLIP,
        }
    }
}

impl MicSettings {
    /// The bit clock these settings produce, in Hz.
    pub fn bclk_hz(&self) -> u32 {
        bit_clock_hz(self.rate, self.bits, CHANNELS)
    }
}

/// Effective settings plus whether an out-of-window stored pair had to be
/// replaced by the default (so the caller can log it once per recording), and
/// the raw settings the individual stored values resolve to before that pair
/// fallback (`raw.bits`/`raw.rate` are what the store actually holds, so a
/// caller can validate a `config set` against the pair it will create even if
/// a reconciliation write does not land).
pub struct ResolvedSettings {
    pub settings: MicSettings,
    pub raw: MicSettings,
    pub fell_back: bool,
}

/// Parse an unsigned integer setting, tolerating surrounding whitespace.
pub fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

/// Parse a boolean setting in the forms the console accepts.
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Whether `key` is one of this module's mic settings.
pub fn owns(key: &str) -> bool {
    matches!(key, BITS_KEY | RATE_KEY | EDGE_KEY)
}

/// Resolves the raw stored values (each optional; `None` when the key is
/// absent) against the defaults. A missing or malformed individual value falls
/// back to its own default; if the resulting slot-width/rate pair is outside
/// the mic's documented clock window, the whole pair falls back to the default
/// so the firmware can never silently mis-clock the mic.
pub fn resolve(bits: Option<&str>, rate: Option<&str>, edge: Option<&str>) -> ResolvedSettings {
    let mut settings = MicSettings {
        bits: bits.and_then(parse_u32).unwrap_or(DEFAULT_BITS),
        rate: rate.and_then(parse_u32).unwrap_or(DEFAULT_RATE_HZ),
        edge_flip: edge.and_then(parse_bool).unwrap_or(DEFAULT_EDGE_FLIP),
    };
    let raw = settings;
    let mut fell_back = false;
    if !mic_settings_valid(settings.bits, settings.rate) {
        settings.bits = DEFAULT_BITS;
        settings.rate = DEFAULT_RATE_HZ;
        fell_back = true;
    }
    ResolvedSettings {
        settings,
        raw,
        fell_back,
    }
}

/// A stored `ptt_*` value the next recording will not actually use, together
/// with the canonical value to write back so the store, `config get`/`list`,
/// and the effective settings all agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredValueFix {
    pub key: &'static str,
    pub value: String,
}

fn stored_fix(key: &'static str, settings: MicSettings) -> StoredValueFix {
    StoredValueFix {
        key,
        value: effective_setting(key, settings).expect("owned key has an effective form"),
    }
}

/// The `ptt_bits`/`ptt_rate` repair, which must be applied as one unit: the
/// two keys decide the mic clock jointly, so writing only one can leave the
/// pair valid but different from the effective pair the console reported. A
/// caller writes `rate` first and then `bits`, and if either write fails must
/// leave the pair as the store already held it.
///
/// Writing `rate` first is what keeps the pair safe mid-repair: whenever both
/// keys need rewriting the effective pair is the 32-bit/16 kHz default, and the
/// intermediate `{stored_bits, 16000}` pair either resolves to that same
/// default or stays out of window (a stored slot width below 32-bit cannot
/// clock legally at 16 kHz), so a failed second write cannot change what the
/// next recording resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockPairFix {
    pub bits: String,
    pub rate: String,
}

/// Resolves the raw stored values and reports every stored `ptt_*` key whose
/// value differs from the effective setting, with the canonical value to write
/// back. An out-of-window slot/rate pair makes both stored keys "used"
/// defaults rather than the stale stored value, so a later `config set` cannot
/// silently re-adopt a key the console already reported as default. Applying
/// the returned fixes leaves store, reports and effective settings in
/// agreement.
///
/// The slot-width and sample-rate keys are returned as a single [`ClockPairFix`]
/// whenever both need rewriting, because they are one clock decision: applying
/// them independently could leave the pair valid but different from what was
/// reported (e.g. repairing `ptt_bits` to 32 while a stored 32 kHz rate is
/// still legal clocks the mic at 32 kHz instead of the reported 16 kHz).
pub fn reconcile(
    bits: Option<&str>,
    rate: Option<&str>,
    edge: Option<&str>,
) -> (ResolvedSettings, Vec<StoredValueFix>, Option<ClockPairFix>) {
    let resolved = resolve(bits, rate, edge);
    let effective = resolved.settings;
    let mut fixes = Vec::new();
    let bits_stale = bits.is_some() && bits.and_then(parse_u32) != Some(effective.bits);
    let rate_stale = rate.is_some() && rate.and_then(parse_u32) != Some(effective.rate);
    let clock_pair = if bits_stale && rate_stale {
        Some(ClockPairFix {
            bits: format!("{}", effective.bits),
            rate: format!("{}", effective.rate),
        })
    } else {
        if bits_stale {
            fixes.push(stored_fix(BITS_KEY, effective));
        }
        if rate_stale {
            fixes.push(stored_fix(RATE_KEY, effective));
        }
        None
    };
    if edge.is_some() && edge.and_then(parse_bool) != Some(effective.edge_flip) {
        fixes.push(stored_fix(EDGE_KEY, effective));
    }
    (resolved, fixes, clock_pair)
}

/// Console report for a `ptt_*` key, used by `config get`/`config list`.
/// Normally this is the effective setting (the value the next recording uses).
/// When a reconciliation rewrite could not land, `unreconciled_stored` is the
/// value the store still holds for that key, and the report names both: the
/// console never presents the fallback as if the store agreed, so a later
/// accepted `config set` cannot change the reported value without the user
/// having been shown the stale one.
pub fn report_setting(
    key: &str,
    settings: MicSettings,
    unreconciled_stored: Option<&str>,
) -> Option<String> {
    let effective = effective_setting(key, settings)?;
    Some(match unreconciled_stored {
        Some(stored) => format!("{effective} (unreconciled: stored {stored})"),
        None => effective,
    })
}

/// Effective value of a `ptt_*` setting for `config get`, so an unset key
/// reports the value the next recording would actually use. `None` for keys
/// this module does not own.
pub fn effective_setting(key: &str, settings: MicSettings) -> Option<String> {
    match key {
        BITS_KEY => Some(format!("{}", settings.bits)),
        RATE_KEY => Some(format!("{}", settings.rate)),
        EDGE_KEY => Some(format!("{}", settings.edge_flip as u8)),
        _ => None,
    }
}

/// Validates a `config set ptt_*` request against the currently effective
/// settings, so an out-of-window or malformed value is refused at the console
/// rather than stored. Keys this module does not own return `Ok(())`.
pub fn validate_setting(current: MicSettings, key: &str, value: &str) -> Result<(), String> {
    if !owns(key) {
        return Ok(());
    }
    let mut candidate = current;
    match key {
        BITS_KEY => {
            candidate.bits = parse_u32(value)
                .ok_or_else(|| String::from("ptt_bits must be a number (bits per channel slot)"))?;
        }
        RATE_KEY => {
            candidate.rate =
                parse_u32(value).ok_or_else(|| String::from("ptt_rate must be a number (Hz)"))?;
        }
        EDGE_KEY => {
            candidate.edge_flip =
                parse_bool(value).ok_or_else(|| String::from("ptt_edge must be 0 or 1"))?;
        }
        _ => return Ok(()),
    }
    if !mic_settings_valid(candidate.bits, candidate.rate) {
        return Err(format!(
            "invalid mic clock: ptt_bits={} ptt_rate={} -> BCLK {} Hz, outside the mic's documented {}-{} Hz window",
            candidate.bits,
            candidate.rate,
            candidate.bclk_hz(),
            MIN_MIC_BCLK_HZ,
            MAX_MIC_BCLK_HZ,
        ));
    }
    Ok(())
}

/// Whether setting `key` to `value` would change the *other* clock key's
/// effective value, i.e. let a stored clock value that reconciliation could
/// not rewrite (and that `config get` therefore did not report) become live
/// again. A malformed value never counts (it is refused by
/// [`validate_setting`]), and neither does a value that leaves the pair out of
/// window or that repairs it. This is the exact condition that let a stale
/// `ptt_bits=16` reappear when `ptt_rate` was later set to 32000.
fn set_changes_other_clock_key(
    store: MicSettings,
    effective: MicSettings,
    key: &str,
    value: &str,
) -> bool {
    let mut candidate = store;
    match key {
        BITS_KEY => match parse_u32(value) {
            Some(bits) => candidate.bits = bits,
            None => return false,
        },
        RATE_KEY => match parse_u32(value) {
            Some(rate) => candidate.rate = rate,
            None => return false,
        },
        _ => return false,
    }
    let (bits, rate) = if mic_settings_valid(candidate.bits, candidate.rate) {
        (candidate.bits, candidate.rate)
    } else {
        (DEFAULT_BITS, DEFAULT_RATE_HZ)
    };
    match key {
        BITS_KEY => rate != effective.rate,
        RATE_KEY => bits != effective.bits,
        _ => false,
    }
}

/// `config set` guard that keeps the reported, stored and effective values
/// from diverging silently when a reconciliation rewrite failed. `store` is the
/// pair the store actually holds (raw values for any key that could not be
/// rewritten) and `effective` is what the next recording uses and what
/// `config get` reported. A normal set (store and effective already agree) is
/// unaffected; a repair set (`config set ptt_bits <reported>`) is allowed; only
/// a set that would re-activate the stale clock value is refused, with the
/// stale key named.
pub fn validate_setting_with_store(
    store: MicSettings,
    effective: MicSettings,
    key: &str,
    value: &str,
) -> Result<(), String> {
    validate_setting(effective, key, value)?;
    if set_changes_other_clock_key(store, effective, key, value) {
        return Err(format!(
            "{key} refused: the stored mic clock is unreconciled (a rewrite failed), and this change would re-activate a stored value the console did not report; set ptt_bits/ptt_rate to their reported values first"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_device_uses_todays_proven_defaults() {
        // Nothing stored: every key resolves to the value the captain has been
        // testing with (32-bit slots at 16 kHz, default edge), and that pair
        // sits exactly at the bottom of the mic's clock window.
        let resolved = resolve(None, None, None);
        assert_eq!(resolved.settings, MicSettings::default());
        assert!(!resolved.fell_back);
        assert_eq!(resolved.settings.bits, 32);
        assert_eq!(resolved.settings.rate, 16_000);
        assert!(!resolved.settings.edge_flip);
        assert_eq!(resolved.settings.bclk_hz(), 1_024_000);
        assert!(mic_settings_valid(DEFAULT_BITS, DEFAULT_RATE_HZ));
        assert_eq!(
            bit_clock_hz(DEFAULT_RATE_HZ, DEFAULT_BITS, CHANNELS),
            1_024_000
        );
    }

    #[test]
    fn a_valid_stored_change_is_used_without_falling_back() {
        // 16-bit slots at 32 kHz is still exactly 1.024 MHz, so it is a legal
        // probe combination and must be taken as-is.
        let resolved = resolve(Some("16"), Some("32000"), Some("1"));
        assert!(!resolved.fell_back);
        assert_eq!(resolved.settings.bits, 16);
        assert_eq!(resolved.settings.rate, 32_000);
        assert!(resolved.settings.edge_flip);
        assert_eq!(resolved.settings.bclk_hz(), 1_024_000);
    }

    #[test]
    fn malformed_individual_values_fall_back_to_their_own_default() {
        // A malformed value must not be taken literally, and must not disturb
        // the other keys.
        let resolved = resolve(Some("nonsense"), Some("32000"), Some("maybe"));
        assert_eq!(resolved.settings.bits, DEFAULT_BITS);
        assert_eq!(resolved.settings.rate, 32_000);
        assert_eq!(resolved.settings.edge_flip, DEFAULT_EDGE_FLIP);
        assert!(!resolved.fell_back);
    }

    #[test]
    fn whitespace_and_the_documented_bool_spellings_are_accepted() {
        let resolved = resolve(Some(" 16 "), Some("32000\n"), Some("on"));
        assert_eq!(resolved.settings.bits, 16);
        assert_eq!(resolved.settings.rate, 32_000);
        assert!(resolved.settings.edge_flip);
        assert!(!resolved.fell_back);
    }

    #[test]
    fn out_of_window_stored_pair_falls_back_to_default_rather_than_mis_clocking() {
        // The 16-bit-slot-at-16-kHz pair that produced a dead line on real
        // hardware, and an absurd stored rate, must both be rejected as a
        // pair and replaced by the default - never handed to the PIO clock.
        for (bits, rate) in [
            (Some("16"), Some("16000")),
            (Some("4000000000"), Some("16000")),
        ] {
            let resolved = resolve(bits, rate, Some("1"));
            assert!(resolved.fell_back, "bits={bits:?} rate={rate:?}");
            assert_eq!(resolved.settings.bits, DEFAULT_BITS);
            assert_eq!(resolved.settings.rate, DEFAULT_RATE_HZ);
            // The edge setting is unaffected by the pair fallback.
            assert!(resolved.settings.edge_flip);
            assert!(mic_settings_valid(
                resolved.settings.bits,
                resolved.settings.rate
            ));
        }
    }

    #[test]
    fn console_accepts_valid_settings_and_reports_them_as_effective() {
        let current = MicSettings::default();
        assert!(validate_setting(current, RATE_KEY, "32000").is_ok());
        assert!(validate_setting(current, BITS_KEY, "16").is_err()); // 16@16k is out of window
        // Applying both changes in a legal order leaves a valid pair.
        let changed = MicSettings {
            bits: 16,
            rate: 32_000,
            edge_flip: true,
        };
        assert!(validate_setting(changed, EDGE_KEY, "1").is_ok());
        assert_eq!(effective_setting(BITS_KEY, changed).as_deref(), Some("16"));
        assert_eq!(
            effective_setting(RATE_KEY, changed).as_deref(),
            Some("32000")
        );
        assert_eq!(effective_setting(EDGE_KEY, changed).as_deref(), Some("1"));
        assert_eq!(effective_setting("ptt_ssh_cmd", changed), None);
    }

    #[test]
    fn console_refuses_malformed_and_out_of_window_values() {
        let current = MicSettings::default();
        for (key, value) in [
            (BITS_KEY, "nonsense"),
            (RATE_KEY, "not-a-rate"),
            (EDGE_KEY, "2"),
            (BITS_KEY, "16"),         // 512 kHz, below the window
            (RATE_KEY, "128000"),     // 8.192 MHz, above the window
            (BITS_KEY, "7"),          // below the PIO shift-register minimum
            (BITS_KEY, "33"),         // above the PIO shift-register maximum
            (RATE_KEY, "4000000000"), // huge; checked arithmetic must reject, not wrap
        ] {
            assert!(
                validate_setting(current, key, value).is_err(),
                "{key}={value} should be refused"
            );
        }
    }

    #[test]
    fn unowned_keys_are_left_alone_by_validation() {
        // `ptt_ssh_cmd` and any other config key must still store through the
        // normal path.
        assert!(validate_setting(MicSettings::default(), "ptt_ssh_cmd", "picocalc-ptt").is_ok());
        assert!(validate_setting(MicSettings::default(), "scroll", "200").is_ok());
        assert!(!owns("ptt_ssh_cmd"));
    }

    #[test]
    fn a_reconfigured_device_reads_back_its_new_settings_on_the_next_recording() {
        // This is the captain's loop: `config set`, then record again. The
        // resolver is the code that runs at capture start, so feeding it the
        // stored strings must reproduce the set values.
        let stored_bits = "24";
        let stored_rate = "21334"; // 24-bit * 21334 * 2 = 1.024032 MHz, just inside the window
        let resolved = resolve(Some(stored_bits), Some(stored_rate), None);
        assert!(!resolved.fell_back);
        assert_eq!(resolved.settings.bits, 24);
        assert_eq!(resolved.settings.rate, 21_334);
        assert!(mic_settings_valid(
            resolved.settings.bits,
            resolved.settings.rate
        ));
    }

    #[test]
    fn reconcile_reports_every_stored_key_that_diverges_from_the_effective_value() {
        // An out-of-window pair (8-bit @ 32 kHz) plus a malformed edge: both
        // stored keys are not what the next recording uses, so each must be
        // reported for rewriting.
        let (resolved, fixes, clock_pair) = reconcile(Some("8"), Some("32000"), Some("maybe"));
        assert!(resolved.fell_back);
        assert_eq!(resolved.settings.bits, DEFAULT_BITS);
        assert_eq!(resolved.settings.rate, DEFAULT_RATE_HZ);
        assert!(!resolved.settings.edge_flip);
        // Both clock keys are stale, so they come back as one atomic unit
        // rather than as two independently-applied keys.
        let pair = clock_pair.expect("an out-of-window pair needs a clock repair");
        assert_eq!(pair.bits, "32");
        assert_eq!(pair.rate, "16000");
        let keys: Vec<&str> = fixes.iter().map(|f| f.key).collect();
        assert!(!keys.contains(&BITS_KEY));
        assert!(!keys.contains(&RATE_KEY));
        assert!(keys.contains(&EDGE_KEY));
        for fix in &fixes {
            assert_eq!(
                effective_setting(fix.key, resolved.settings).as_deref(),
                Some(fix.value.as_str())
            );
        }
    }

    #[test]
    fn only_one_stale_clock_key_is_a_single_fix() {
        // A lone 8-bit slot at the default rate: writing the one key lands on
        // the valid default pair, so it does not need the atomic pair repair.
        let (resolved, fixes, clock_pair) = reconcile(Some("8"), None, None);
        assert!(resolved.fell_back);
        assert!(clock_pair.is_none());
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].key, BITS_KEY);
        assert_eq!(fixes[0].value, "32");
    }

    #[test]
    fn reconcile_leaves_a_consistent_store_untouched() {
        // A legal stored pair with a legal edge is exactly what the next
        // recording uses, so nothing needs rewriting.
        let (resolved, fixes, clock_pair) = reconcile(Some("16"), Some("32000"), Some("1"));
        assert!(!resolved.fell_back);
        assert_eq!(resolved.settings.bits, 16);
        assert_eq!(resolved.settings.rate, 32_000);
        assert!(fixes.is_empty());
        assert!(clock_pair.is_none());

        // A consistent store reports exactly its effective values.
        assert_eq!(
            report_setting(BITS_KEY, resolved.settings, None).as_deref(),
            Some("16")
        );
        assert_eq!(
            report_setting(RATE_KEY, resolved.settings, None).as_deref(),
            Some("32000")
        );
    }

    #[test]
    fn report_names_both_the_effective_value_and_the_unreconciled_stored_one() {
        // A stale 16-bit slot that reconciliation could not rewrite: the
        // recording uses the 32-bit default, but the console must also show
        // the stale stored value so a later set cannot surprise the user.
        let (resolved, fixes, clock_pair) = reconcile(Some("16"), None, None);
        assert!(resolved.fell_back);
        assert_eq!(resolved.settings.bits, DEFAULT_BITS);
        assert!(clock_pair.is_none());
        assert_eq!(fixes.len(), 1);
        let report = report_setting(BITS_KEY, resolved.settings, Some("16")).unwrap();
        assert!(report.starts_with("32"), "{report}");
        assert!(report.contains("unreconciled"), "{report}");
        assert!(report.contains("16"), "{report}");
        // Keys this module does not own have no report.
        assert_eq!(report_setting("ptt_ssh_cmd", resolved.settings, None), None);
    }

    #[test]
    fn a_set_that_would_reactivate_a_stale_clock_key_is_refused() {
        // The store holds a stale 16-bit slot (no rate); the effective pair is
        // the 32/16000 default the console reported.
        let store = MicSettings {
            bits: 16,
            rate: DEFAULT_RATE_HZ,
            ..MicSettings::default()
        };
        let effective = MicSettings::default();

        // Setting ptt_rate to 32000 would make 16 @ 32000 legal and silently
        // change the effective slot width from 32 to 16: refuse, naming the
        // key, rather than resurrecting the stale value.
        let err = validate_setting_with_store(store, effective, RATE_KEY, "32000")
            .expect_err("must refuse the resurrection");
        assert!(err.contains(RATE_KEY), "{err}");

        // A value that leaves the pair out of window does not activate the
        // stale key and is allowed (16 @ 16 kHz is still out of window).
        assert!(validate_setting_with_store(store, effective, RATE_KEY, "16000").is_ok());

        // Repairing ptt_bits to the reported value is also allowed.
        assert!(validate_setting_with_store(store, effective, BITS_KEY, "32").is_ok());

        // A consistent store is unaffected by the guard: the captain's normal
        // ordered probe still works.
        let consistent = MicSettings::default();
        assert!(validate_setting_with_store(consistent, consistent, RATE_KEY, "32000").is_ok());
        let changed = MicSettings {
            rate: 32_000,
            ..MicSettings::default()
        };
        assert!(validate_setting_with_store(changed, changed, BITS_KEY, "16").is_ok());
    }

    #[test]
    fn a_set_that_changes_the_reporters_own_key_is_still_governed_by_the_window() {
        // Setting a clock key on a consistent store is a normal parameter
        // change, not a resurrection, and must keep working.
        let current = MicSettings::default();
        assert!(validate_setting_with_store(current, current, RATE_KEY, "32000").is_ok());
        assert!(validate_setting_with_store(current, current, BITS_KEY, "32").is_ok());
        // An out-of-window value is still refused by the normal path.
        assert!(validate_setting_with_store(current, current, BITS_KEY, "16").is_err());
    }
}
