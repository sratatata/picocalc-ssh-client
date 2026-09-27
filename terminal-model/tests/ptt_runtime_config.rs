//! End-to-end host model of the captain's push-to-talk runtime-configuration
//! loop: `config set ptt_*`, then hold F1 and record.
//!
//! This is deliberately not a unit test of any single module. It wires the
//! same public entry points the firmware calls, in the same order, so the
//! captain's actual workflow is exercised as a whole:
//!
//! * `validate_setting_with_store` is `src/config.rs`'s `config set` guard;
//! * `resolve` is `mic::load_settings`, which runs at the start of *every*
//!   recording so a change takes effect on the next utterance;
//! * `build_i2s_rx_program` is what `Mic::apply` assembles for the settings;
//! * `extract_left_channel_pcm` is `capture_task`'s processing of one captured
//!   FIFO chunk into the `i16` stream that goes on the wire.
//!
//! The only stand-in is the persisted string store itself: the firmware keeps
//! these keys in `sequential_storage` via `CONFIG`, which needs flash and so
//! cannot run on the host. Every decision around it is the real code.

use pio::{Instruction, InstructionOperands, SetDestination};
use terminal_model::i2s_program::{PROGRAM_SIZE, build_i2s_rx_program};
use terminal_model::mic_config::{
    BITS_KEY, DEFAULT_BITS, DEFAULT_RATE_HZ, EDGE_KEY, MicSettings, RATE_KEY,
    reconcile as reconcile_settings, report_setting, resolve, validate_setting_with_store,
};
use terminal_model::pcm_extract::extract_left_channel_pcm;

/// The firmware's persisted `config` store, modeled as the string map the mic
/// keys live in. `set`/`get` reproduce the console's observable behavior.
#[derive(Default)]
struct ConfigStore {
    entries: Vec<(String, String)>,
    /// Simulates every reconciliation write failing, as a full
    /// `sequential_storage` region would.
    fail_reconcile: bool,
    /// Simulates a write failure for specific keys only, so a partial
    /// reconciliation (some rewrites land, others do not) can be exercised.
    fail_writes: Vec<&'static str>,
}

impl ConfigStore {
    fn fetch(&self, key: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    fn remove(&mut self, key: &str) {
        self.entries.retain(|(k, _)| k != key);
    }

    /// Whether a reconciliation write for `key` succeeds in this simulated
    /// store.
    fn can_write(&self, key: &str) -> bool {
        !self.fail_reconcile && !self.fail_writes.contains(&key)
    }

    fn put(&mut self, key: &str, value: &str) {
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value.to_string(),
            None => self.entries.push((key.to_string(), value.to_string())),
        }
    }

    /// The settings the stored values resolve to before the pair fallback -
    /// i.e. the pair the store actually holds.
    fn stored_settings(&self) -> MicSettings {
        resolve(
            self.fetch(BITS_KEY).as_deref(),
            self.fetch(RATE_KEY).as_deref(),
            self.fetch(EDGE_KEY).as_deref(),
        )
        .raw
    }

    /// Mirrors `mic::read_and_reconcile`: applies the rewrites it can, then
    /// re-resolves the store it is actually left holding and returns the keys
    /// that still diverge (with the value the store holds). The slot width and
    /// sample rate are applied as one atomic pair - rate first, the slot width
    /// skipped if the rate write failed, the rate restored if the slot width
    /// write failed - so a partial repair cannot change the effective pair.
    fn reconcile(&mut self) -> Vec<(String, String)> {
        let (_, fixes, clock_pair) = reconcile_settings(
            self.fetch(BITS_KEY).as_deref(),
            self.fetch(RATE_KEY).as_deref(),
            self.fetch(EDGE_KEY).as_deref(),
        );
        if let Some(pair) = clock_pair {
            if self.can_write(RATE_KEY) {
                let original_rate = self.fetch(RATE_KEY);
                self.put(RATE_KEY, &pair.rate);
                if self.can_write(BITS_KEY) {
                    self.put(BITS_KEY, &pair.bits);
                } else {
                    match original_rate {
                        Some(value) => self.put(RATE_KEY, &value),
                        None => self.remove(RATE_KEY),
                    }
                }
            }
        }
        for fix in fixes {
            if self.can_write(fix.key) {
                self.put(fix.key, &fix.value);
            }
        }
        let (_, remaining, remaining_pair) = reconcile_settings(
            self.fetch(BITS_KEY).as_deref(),
            self.fetch(RATE_KEY).as_deref(),
            self.fetch(EDGE_KEY).as_deref(),
        );
        let mut unreconciled: Vec<(String, String)> = remaining
            .iter()
            .map(|fix| {
                (
                    fix.key.to_string(),
                    self.fetch(fix.key).unwrap_or_else(|| fix.value.clone()),
                )
            })
            .collect();
        if let Some(pair) = remaining_pair {
            unreconciled.push((
                BITS_KEY.to_string(),
                self.fetch(BITS_KEY).unwrap_or(pair.bits),
            ));
            unreconciled.push((
                RATE_KEY.to_string(),
                self.fetch(RATE_KEY).unwrap_or(pair.rate),
            ));
        }
        unreconciled
    }

    /// The effective settings a recording started right now would use, exactly
    /// as `mic::resolve_settings` computes them from the store.
    fn effective(&self) -> MicSettings {
        resolve(
            self.fetch(BITS_KEY).as_deref(),
            self.fetch(RATE_KEY).as_deref(),
            self.fetch(EDGE_KEY).as_deref(),
        )
        .settings
    }

    /// The settings a recording started right now would use.
    fn settings(&mut self) -> MicSettings {
        self.reconcile();
        self.effective()
    }

    /// `config set`: refuse an invalid mic value before it reaches the store,
    /// and otherwise persist it. Mirrors `mic::validate_config_setting`, which
    /// refuses a set that would let an unreconciled stale clock key become
    /// effective (validating against the effective pair, the value the console
    /// reported).
    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.reconcile();
        validate_setting_with_store(self.stored_settings(), self.effective(), key, value)?;
        self.put(key, value);
        Ok(())
    }

    /// `config get`: the value the next recording will actually use, with any
    /// stale stored value a failed reconcile left behind named alongside it.
    fn get(&mut self, key: &str) -> Option<String> {
        let unreconciled = self.reconcile();
        let stale = unreconciled
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str());
        report_setting(key, self.effective(), stale)
    }
}

/// The `set x, N` loop counter of the program `Mic::apply` would load for the
/// settings a recording resolves to.
fn loop_count(program: &pio::Program<PROGRAM_SIZE>) -> u32 {
    match Instruction::decode(program.code[0], program.side_set)
        .expect("assembled instruction decodes")
        .operands
    {
        InstructionOperands::SET {
            destination: SetDestination::X,
            data,
        } => data as u32,
        other => panic!("first instruction was {other:?}, expected `set x, N`"),
    }
}

/// One recording's worth of processing: resolve settings, build the PIO
/// program the SM would run, and turn a captured alternating left/right-slot
/// FIFO chunk into the `i16` stream that goes on the wire.
fn process_chunk(store: &mut ConfigStore, chunk: &[u32]) -> Vec<i16> {
    let settings = store.settings();
    // `Mic::apply` assembles this before enabling the state machine.
    let program = build_i2s_rx_program(settings.bits, settings.edge_flip);
    assert_eq!(program.code.len(), PROGRAM_SIZE, "program shape is fixed");
    let _ = loop_count(&program);

    let mut pcm = vec![0i16; chunk.len() / 2];
    extract_left_channel_pcm(chunk, &mut pcm, settings.bits);
    pcm
}

/// A chunk whose left slots carry `samples` in the top 16 bits (the 32-bit
/// slot's signal field) and whose right (undriven) slots carry a sentinel that
/// must never leak into the left PCM.
fn chunk_with_samples(samples: &[i16]) -> Vec<u32> {
    let mut chunk = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        chunk.push((sample as u16 as u32) << 16);
        chunk.push(0x0000_BEEFu32);
    }
    chunk
}

/// The same shape, but with the signal in the slot's low 16 bits, which is
/// where a narrow (16-bit) slot's samples live after extraction.
fn chunk_with_low16_samples(samples: &[i16]) -> Vec<u32> {
    let mut chunk = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        chunk.push(sample as u16 as u32);
        chunk.push(0x0000_BEEFu32);
    }
    chunk
}

#[test]
fn unconfigured_device_records_todays_proven_defaults() {
    let mut store = ConfigStore::default();
    let settings = store.settings();

    // No `config set` at all: every key reports the documented default and the
    // recording resolves to today's proven 32-bit/16 kHz pair.
    assert_eq!(settings, MicSettings::default());
    assert_eq!(store.get(BITS_KEY).as_deref(), Some("32"));
    assert_eq!(store.get(RATE_KEY).as_deref(), Some("16000"));
    assert_eq!(store.get(EDGE_KEY).as_deref(), Some("0"));
    assert_eq!(settings.bits, 32);
    assert_eq!(settings.rate, DEFAULT_RATE_HZ);
    assert_eq!(settings.bclk_hz(), 1_024_000);

    // 32-bit slot => `set x, bits - 2` = 30, matching the shipped program.
    let program = build_i2s_rx_program(settings.bits, settings.edge_flip);
    assert_eq!(loop_count(&program), 30);

    // The extracted samples are exactly the top 16 bits of each left slot,
    // with the undriven slots ignored.
    let chunk = chunk_with_samples(&[1000, 1002, 998, 1004]);
    let stream = process_chunk(&mut store, &chunk);
    assert_eq!(stream, [1000, 1002, 998, 1004]);
}

#[test]
fn captain_config_set_loop_changes_the_very_next_recording() {
    let mut store = ConfigStore::default();
    // The same captured words, processed before and after the reconfiguration.
    let chunk = chunk_with_samples(&[0x1234, 0x5678]);
    let before = process_chunk(&mut store, &chunk);

    // A slot width that would under-clock the mic at the current rate is
    // refused at the console and leaves the store untouched...
    let refused = store.set(BITS_KEY, "16");
    assert!(refused.is_err(), "16-bit slot at 16 kHz must be refused");
    assert_eq!(store.get(BITS_KEY).as_deref(), Some("32"));
    assert_eq!(store.fetch(BITS_KEY), None, "nothing was persisted");

    // ...but the captain's loop works when the pair stays inside the window:
    // raise the rate first, then narrow the slot, then flip the edge.
    store
        .set(RATE_KEY, "32000")
        .expect("32-bit @ 32 kHz is legal");
    store.set(BITS_KEY, "16").expect("16-bit @ 32 kHz is legal");
    store.set(EDGE_KEY, "1").expect("ptt_edge is 0/1");

    assert_eq!(store.get(BITS_KEY).as_deref(), Some("16"));
    assert_eq!(store.get(RATE_KEY).as_deref(), Some("32000"));
    assert_eq!(store.get(EDGE_KEY).as_deref(), Some("1"));

    let after_settings = store.settings();
    assert_eq!(after_settings.bits, 16);
    assert_eq!(after_settings.rate, 32_000);
    assert!(after_settings.edge_flip);
    // Same documented BCLK, different shape.
    assert_eq!(after_settings.bclk_hz(), 1_024_000);

    // The runtime-assembled program now has a 16-bit loop count...
    let program = build_i2s_rx_program(after_settings.bits, after_settings.edge_flip);
    assert_eq!(loop_count(&program), 14);
    // ...and the edge flip really changes the sampled side-set.
    let normal = build_i2s_rx_program(after_settings.bits, false);
    assert_eq!(
        Instruction::decode(program.code[0], program.side_set)
            .unwrap()
            .side_set
            .unwrap()
            ^ 0b01,
        Instruction::decode(normal.code[0], normal.side_set)
            .unwrap()
            .side_set
            .unwrap()
    );

    // The next recording uses the new slot width: the low 16 bits are now the
    // signal field, so the same captured words extract differently than they
    // did on the unconfigured device.
    let after = process_chunk(&mut store, &chunk);
    // At 32 bits the extractor took the top 16 bits (`0x1234`/`0x5678`); at
    // 16 bits those sentinel left words have a zero low half, proving the
    // runtime slot width reached the extraction, not just the console.
    assert_eq!(before, [0x1234, 0x5678]);
    assert_eq!(after, [0, 0]);
}

#[test]
fn config_remove_restores_the_default_for_the_next_recording() {
    let mut store = ConfigStore::default();
    store.set(RATE_KEY, "32000").expect("legal");
    store.set(BITS_KEY, "16").expect("legal");
    assert_eq!(store.settings().bits, 16);
    assert_eq!(store.settings().rate, 32_000);
    assert_eq!(store.get(RATE_KEY).as_deref(), Some("32000"));

    // `config rm ptt_rate`: the malformed/incomplete stored pair is now
    // out-of-window (16-bit @ 16 kHz), which must resolve back to the default
    // rather than mis-clock the mic.
    store.remove(RATE_KEY);
    assert_eq!(store.get(RATE_KEY).as_deref(), Some("16000"));
    assert_eq!(store.settings(), MicSettings::default());

    // `config rm` of every mic key returns the device to the unconfigured
    // defaults, and the next recording is byte-identical to the fresh device.
    store.remove(BITS_KEY);
    store.remove(EDGE_KEY);
    assert_eq!(store.settings(), MicSettings::default());
    assert_eq!(store.fetch(BITS_KEY), None);
    assert_eq!(store.fetch(RATE_KEY), None);
    assert_eq!(store.fetch(EDGE_KEY), None);
    assert_eq!(
        process_chunk(&mut store, &chunk_with_samples(&[7, -7])),
        [7, -7]
    );
}

/// Regression for the console/store divergence: after `config rm` leaves a
/// lone slot-width key whose pair is out of window, the console reports (and a
/// recording uses) the default, so a later `config set ptt_rate` must not
/// silently re-adopt the abandoned stored value.
#[test]
fn removed_pair_key_is_not_silently_re_adopted_by_a_later_set() {
    let mut store = ConfigStore::default();

    store.set(RATE_KEY, "32000").expect("legal");
    store.set(BITS_KEY, "16").expect("legal");
    assert_eq!(store.settings().bits, 16);

    store.remove(RATE_KEY);

    // `config get` must report the value the next recording would use: the
    // out-of-window lone 16-bit key falls back to the default.
    assert_eq!(store.get(BITS_KEY).as_deref(), Some("32"));
    assert_eq!(store.get(RATE_KEY).as_deref(), Some("16000"));
    // ...and reconciliation must rewrite the stale stored key rather than just
    // masking it, so the set below cannot re-adopt it.
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("32"));

    store
        .set(RATE_KEY, "32000")
        .expect("32-bit @ 32 kHz is legal");
    assert_eq!(store.get(BITS_KEY).as_deref(), Some("32"));
    let settings = store.settings();
    assert_eq!(settings.bits, DEFAULT_BITS);
    assert_eq!(settings.rate, 32_000);
}

/// The reporting invariant the captain's no-reflash loop relies on: every
/// stored `ptt_*` key a `config get`/`config list` would print is exactly the
/// value the next recording uses, even when the store started out stale with
/// an out-of-window pair and a malformed value.
#[test]
fn store_reported_and_effective_values_agree_after_a_stale_store_is_reconciled() {
    let mut store = ConfigStore::default();
    // A store left stale (out-of-window lone slot width, malformed edge) as an
    // older firmware or an interrupted probe could have left it.
    store.entries.push((BITS_KEY.to_string(), "16".to_string()));
    store
        .entries
        .push((EDGE_KEY.to_string(), "maybe".to_string()));

    // Any mic resolution (get/list/recording) reconciles before reporting.
    let settings = store.settings();
    assert_eq!(settings, MicSettings::default());
    for key in [BITS_KEY, RATE_KEY, EDGE_KEY] {
        if let Some(stored) = store.fetch(key) {
            let effective = report_setting(key, settings, None).unwrap();
            assert_eq!(stored, effective, "{key}: stored and reported disagree");
        }
    }
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("32"));
    assert_eq!(store.fetch(EDGE_KEY).as_deref(), Some("0"));
}

/// A reconciliation write can fail (e.g. `sequential_storage` reports full
/// storage). The console must then report the effective value the next
/// recording uses *and* flag the stale stored value, and a `config set` must
/// never let that stale value become effective.
#[test]
fn failed_reconcile_write_is_reported_and_cannot_be_silently_reactivated() {
    let mut store = ConfigStore::default();
    // A stale, out-of-window lone 16-bit slot that reconciliation would rewrite
    // to the 32-bit default...
    store.entries.push((BITS_KEY.to_string(), "16".to_string()));
    // ...but the write fails, so the stale key stays in the store.
    store.fail_reconcile = true;

    // `config get` reports the effective value the next recording uses (32)
    // but names the stale stored 16 rather than pretending the store agreed.
    let reported = store.get(BITS_KEY).expect("ptt_bits is owned");
    assert!(reported.starts_with("32"), "{reported}");
    assert!(reported.contains("unreconciled"), "{reported}");
    assert!(reported.contains("16"), "{reported}");

    // The exact divergence the earlier fix missed: setting ptt_rate to 32000
    // would make 16 @ 32000 a legal pair, silently changing the effective slot
    // width from the reported 32 to 16. It must be refused, and the store must
    // be unchanged.
    let refused = store
        .set(RATE_KEY, "32000")
        .expect_err("activating the stale 16-bit slot must be refused");
    assert!(refused.contains(RATE_KEY), "{refused}");
    assert_eq!(store.fetch(RATE_KEY), None);
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("16"));
    assert_eq!(store.settings().bits, DEFAULT_BITS);

    // Repairing ptt_bits to the reported value is allowed, and makes the store
    // consistent again so the console no longer has to flag it.
    store
        .set(BITS_KEY, "32")
        .expect("repair to the reported value");
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("32"));
    assert_eq!(store.get(BITS_KEY).as_deref(), Some("32"));
    // The recovered store can now be reconfigured normally.
    store.set(RATE_KEY, "32000").expect("legal once reconciled");
    assert_eq!(store.get(RATE_KEY).as_deref(), Some("32000"));
    assert_eq!(store.settings().bits, DEFAULT_BITS);
}

/// The slot width and sample rate are one clock decision and must be repaired
/// atomically: a partial repair cannot leave the pair valid but different from
/// what the console reported. Trace: an out-of-window 8-bit/32 kHz pair, then a
/// read where the slot-width rewrite would land but the rate rewrite does not -
/// the slot width must not be written, so the pair stays out of window and the
/// effective pair stays the 32/16000 default the console reported.
#[test]
fn clock_pair_repair_is_atomic_and_cannot_flip_the_effective_pair() {
    let mut store = ConfigStore::default();
    // An out-of-window pair: 8 * 32000 * 2 = 512 kHz, below the 1.024 MHz floor.
    store.entries.push((BITS_KEY.to_string(), "8".to_string()));
    store
        .entries
        .push((RATE_KEY.to_string(), "32000".to_string()));

    // Both clock rewrites fail on the first reconcile: the console reports the
    // fallback pair and the stale pair stays stored.
    store.fail_writes.push(BITS_KEY);
    store.fail_writes.push(RATE_KEY);
    assert!(store.get(BITS_KEY).unwrap().starts_with("32"));
    assert!(store.get(RATE_KEY).unwrap().starts_with("16000"));
    assert_eq!(store.settings().bits, DEFAULT_BITS);
    assert_eq!(store.settings().rate, DEFAULT_RATE_HZ);
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("8"));
    assert_eq!(store.fetch(RATE_KEY).as_deref(), Some("32000"));

    // On the next attempt the slot-width rewrite would land but the rate
    // rewrite still fails. Because the pair is written rate-first and
    // atomically, the slot width must not be written: the pair's validity, and
    // therefore the effective pair the console reports and the next recording
    // uses, cannot change as a side effect.
    store.fail_writes.clear();
    store.fail_writes.push(RATE_KEY);
    let settings = store.settings();
    assert_eq!(settings.bits, DEFAULT_BITS);
    assert_eq!(settings.rate, DEFAULT_RATE_HZ);
    assert_eq!(settings.bclk_hz(), 1_024_000);
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("8"));
    assert_eq!(store.fetch(RATE_KEY).as_deref(), Some("32000"));
    assert!(
        store.get(BITS_KEY).unwrap().starts_with("32"),
        "console changed the effective slot width"
    );

    // Once the rate write is allowed, both keys are written and the store
    // resolves to the repaired default pair.
    store.fail_writes.clear();
    let settings = store.settings();
    assert_eq!(settings.bits, DEFAULT_BITS);
    assert_eq!(settings.rate, DEFAULT_RATE_HZ);
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("32"));
    assert_eq!(store.fetch(RATE_KEY).as_deref(), Some("16000"));
}

/// If the slot-width write fails after the rate write landed, the rate is
/// restored, so neither key of the pair is left changed by the partial repair.
#[test]
fn clock_pair_rolls_back_a_partial_repair() {
    let mut store = ConfigStore::default();
    store.entries.push((BITS_KEY.to_string(), "8".to_string()));
    store
        .entries
        .push((RATE_KEY.to_string(), "32000".to_string()));
    store.fail_writes.push(BITS_KEY);

    let settings = store.settings();
    assert_eq!(settings.bits, DEFAULT_BITS);
    assert_eq!(settings.rate, DEFAULT_RATE_HZ);
    assert_eq!(store.fetch(BITS_KEY).as_deref(), Some("8"));
    assert_eq!(store.fetch(RATE_KEY).as_deref(), Some("32000"));
}

/// Prints a transcript of the captain's real workflow - `config set`, then
/// hold F1 - using the same decision functions the console and capture path
/// call. Assertions pin the transcript's substance so this is a real test,
/// not just logging; run with `--nocapture` (see the evidence artifact) to
/// read the session.
#[test]
fn console_session_transcript_config_set_then_record() {
    let mut store = ConfigStore::default();
    let mut transcript = String::new();

    fn show_get(store: &mut ConfigStore, key: &str, transcript: &mut String) {
        transcript.push_str(&format!(
            "$ config get {key}\n{}\n",
            store.get(key).unwrap_or_default()
        ));
    }
    show_get(&mut store, BITS_KEY, &mut transcript);
    show_get(&mut store, RATE_KEY, &mut transcript);
    show_get(&mut store, EDGE_KEY, &mut transcript);

    // A value that would under-clock the mic is refused with the real message
    // the console prints, and is not stored.
    match store.set(BITS_KEY, "16") {
        Err(message) => transcript.push_str(&format!("$ config set ptt_bits 16\n{message}\n")),
        Ok(()) => panic!("16-bit at 16 kHz must be refused"),
    }
    assert_eq!(store.fetch(BITS_KEY), None);

    // The captain's ordered probe: raise the rate, then narrow the slot, then
    // flip the edge.
    for (key, value) in [(RATE_KEY, "32000"), (BITS_KEY, "16"), (EDGE_KEY, "1")] {
        match store.set(key, value) {
            Ok(()) => transcript.push_str(&format!("$ config set {key} {value}\nOK\n")),
            Err(message) => panic!("{key}={value} should be accepted: {message}"),
        }
    }
    for key in [BITS_KEY, RATE_KEY, EDGE_KEY] {
        show_get(&mut store, key, &mut transcript);
    }

    // Now "hold F1": the next recording resolves the stored settings, builds
    // the runtime PIO program, and processes one captured chunk.
    let settings = store.settings();
    let program = build_i2s_rx_program(settings.bits, settings.edge_flip);
    let chunk = chunk_with_low16_samples(&[1000, 1002, 998, 1004]);
    let stream = process_chunk(&mut store, &chunk);

    transcript.push_str(&format!(
        "--- hold F1: next recording ---\n\
         resolved: bits={} rate={} edge={} bclk={}\n\
         PIO `set x` loop count: {}\n\
         captured left-slot samples: {:?}\n\
         streamed PCM: {:?}\n",
        settings.bits,
        settings.rate,
        settings.edge_flip as u8,
        settings.bclk_hz(),
        loop_count(&program),
        [1000, 1002, 998, 1004],
        stream,
    ));

    assert_eq!(settings.bits, 16);
    assert_eq!(settings.rate, 32_000);
    assert_eq!(loop_count(&program), 14);
    assert_eq!(stream, [1000, 1002, 998, 1004]);

    print!("{transcript}");
}
