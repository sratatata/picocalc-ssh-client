//! Push-to-talk voice capture: a PIO-driven I2S RX driver for a digital mic
//! wired to the expansion-header pins freed by dropping the slow PSRAM path
//! (see AGENTS.md and `psram.rs`), plus the task that streams captured audio
//! down the SSH session's audio channel (see `crate::net`). With no session up
//! there is nowhere to send audio, so the button says so instead of recording.
//!
//! Audio is staged between the capture and upload tasks in a fixed,
//! allocation-free static ring buffer (`terminal_model::audio_ring`), not on
//! the heap. Capture never blocks and never allocates; when the network side
//! falls behind, the ring drops its oldest samples, so a congested SSH session
//! can at worst lose audio, never stall the I2S clock or exhaust the firmware
//! heap. This holds regardless of whether a PSRAM heap tier is present or
//! working.
//!
//! The DMA side is decoupled from CPU timing the same way: the channel copies
//! the PIO RX FIFO into its own circular buffer ([`DMA_RING`]) in the RP2350's
//! endless transfer mode with the write address wrapped on the ring, and
//! [`capture_task`] only drains that buffer every `DMA_POLL_INTERVAL`. Nothing
//! about the microphone's bit clock depends on how quickly the CPU gets back to
//! it - with a one-shot transfer per chunk, the eight-word FIFO filled while
//! the next transfer was armed, the state machine stalled on `in`, and the
//! clock stopped for as long as the CPU was late (which is what silenced
//! captures under an SSH session's load, and what the old per-chunk
//! plateau-and-glitch artifact was). Being late now only costs whatever audio
//! the ring had to overwrite.
//!
//! The I2S slot width, sample rate and bit-clock edge polarity are runtime
//! settings read from the persisted config store (`ptt_bits`/`ptt_rate`/
//! `ptt_edge`, see [`load_settings`]) at the start of every recording. The PIO
//! program is assembled on the device at that point rather than by `pio_asm!`,
//! so a `config set` takes effect on the next utterance without a reflash or
//! reboot. An out-of-window slot/rate pair is refused at the console and falls
//! back to the default rather than silently mis-clocking the mic.
//!
//! Wire format: see AGENTS.md's push-to-talk entry for the authoritative
//! framing contract a receiving process must speak, and
//! [`terminal_model::ptt_frame`] for the encoder: a 4-byte little-endian
//! length prefix per frame, then mono PCM at `ptt_rate`. The session's channel
//! stays open, so an utterance ends with a zero-length frame rather than by
//! closing a connection.

use crate::Irqs;
use crate::config::{CONFIG, Configuration, StrValue};
use crate::screen::SCREEN;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_rp::PeripheralRef;
use embassy_rp::clocks::clk_sys_freq;
use embassy_rp::dma::Channel as _;
use embassy_rp::pac::dma::vals::{DataSize, TransCountMode, TreqSel};
use embassy_rp::peripherals::{DMA_CH4, PIN_2, PIN_3, PIN_21, PIO2};
use embassy_rp::pio::{
    Config, Direction, FifoJoin, LoadedProgram, Pin, Pio, ShiftConfig, ShiftDirection,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker};
use fixed::traits::ToFixed;
use terminal_model::audio_ring::{AudioRing, utterance_ended};
use terminal_model::i2s_program::build_i2s_rx_program;
use terminal_model::mic_config::{
    BITS_KEY, CHANNELS, DEFAULT_RATE_HZ, EDGE_KEY, MicSettings, RATE_KEY,
    reconcile as reconcile_mic_settings, report_setting as report_mic_setting,
    validate_setting_with_store as validate_mic_setting,
};
use terminal_model::pcm_extract::{ac_rms_level, bit_clock_hz, extract_left_channel_pcm};
use terminal_model::{dma_ring, ptt_frame};

extern crate alloc;

/// 25 ms per frame at the default rate, comfortably inside the brief's
/// 20-50 ms guidance. A chunk is a fixed number of samples, so a non-default
/// `ptt_rate` changes its duration but not the buffer sizes.
const SAMPLES_PER_CHUNK: usize = DEFAULT_RATE_HZ as usize / 40;
/// Bytes of the wire frame one capture chunk is sent as: the length prefix plus
/// the samples, per `terminal_model::ptt_frame`. The SSH session sizes its
/// fixed audio queue with this, so the two cannot drift apart.
pub const WIRE_FRAME_BYTES: usize = ptt_frame::frame_bytes(SAMPLES_PER_CHUNK);
/// Capacity of the shared capture/upload ring, in `i16` samples: 2048 samples
/// at 16 kHz is 128 ms of audio, enough to absorb ordinary scheduling jitter
/// without being a meaningful memory cost. This buffer
/// is a plain `static` compiled into `.bss`, so - unlike the heap-allocated
/// per-chunk `Box`es it replaces - it does not draw on the 64 KiB `DualHeap`
/// the WiFi/TCP/SSH stack and screen scrollback share, and therefore cannot
/// exhaust that heap when the network side falls behind (it drops its oldest
/// samples instead). The behavior does not depend on a PSRAM heap tier
/// existing or working.
const RING_SAMPLES: usize = 2048;
/// Upper bound on a single push-to-talk recording. A `Released` report is the
/// normal way to end one, but the keyboard co-processor link can drop a
/// transition (missed poll, I2C glitch) with no automatic recovery until some
/// other event arrives; this caps worst-case exposure (mic powered, I2S clock
/// running, "recording..." overlay stuck) to a minute, far longer than any
/// normal utterance.
const MAX_RECORDING_DURATION: Duration = Duration::from_secs(60);

/// Words in the capture DMA's circular buffer: the DMA writes the PIO RX FIFO
/// into it in hardware (endless transfer mode, write address wrapped on the
/// ring) so the FIFO is drained with no CPU in the loop. Must be a power of two
/// (the DMA wraps the address on a `1 << DMA_RING_BITS` byte boundary) and even
/// (one L+R frame is two words, so an odd size would flip the left/right parity
/// across a wrap). 2048 words is 1024 frames, i.e. 64 ms of mono audio at
/// 32-bit slots - the slack the consumer gets before the DMA laps it.
const DMA_RING_WORDS: usize = 2048;
/// `log2` of the ring's size in bytes, which is what the DMA's RING_SIZE field
/// wants (`DMA_RING_WORDS * 4 == 1 << DMA_RING_BITS`).
const DMA_RING_BITS: u8 = 13;
/// Words left unread behind the DMA's write head, so a read can never race the
/// word being written right now. Even, which is what keeps a drained run whole
/// L+R frames.
const DMA_RING_MARGIN: usize = 2;
/// How often the consumer drains the DMA ring while a recording runs: often
/// enough to keep the ring nearly empty (it holds 64 ms), cheap enough to cost
/// nothing next to the audio it moves.
const DMA_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A stored `ptt_*` key the final store state still does not resolve to its
/// effective value, with the value the store holds.
struct UnreconciledKey {
    key: &'static str,
    stored: String,
}

/// Outcome of reading and reconciling the stored `ptt_*` keys.
struct StoreOutcome {
    /// The pair the store actually holds after reconciliation (raw values for
    /// any key that could not be rewritten).
    stored: MicSettings,
    /// The resolution of that final store state - exactly what the next
    /// recording uses and what `config get`/`list` report.
    resolved: terminal_model::mic_config::ResolvedSettings,
    unreconciled: Vec<UnreconciledKey>,
}

/// Writes one `ptt_*` setting through the config store, converting the console
/// string into the fixed-size stored value and reporting a write or conversion
/// failure as text.
async fn store_setting(
    config: &mut Configuration,
    key: &'static str,
    value: &str,
) -> Result<(), String> {
    let value: StrValue = value.try_into().map_err(|err| alloc::format!("{err:?}"))?;
    config
        .store(key, value)
        .await
        .map_err(|err| alloc::format!("{err:?}"))
}

/// Reads the `ptt_*` mic keys and resolves them against their defaults via
/// [`terminal_model::mic_config::reconcile`]: a missing or malformed individual
/// value falls back to its own default, and an out-of-window slot/rate pair
/// falls back to the default rather than silently mis-clocking the mic. A
/// stored key that no longer matches its effective value is rewritten, so the
/// store, `config get`/`list`, and the next recording all agree.
///
/// The slot width and sample rate are rewritten as one atomic pair (see
/// [`terminal_model::mic_config::ClockPairFix`]), so a partial clock repair can
/// never leave the pair valid but different from what the console reported. The
/// store is then re-read and re-resolved, so `stored`/`resolved` and the
/// unreconciled list all describe the state the store is actually left in, and
/// a failed write is printed and reported, never discarded.
async fn read_and_reconcile() -> StoreOutcome {
    let mut config = CONFIG.get().lock().await;
    let bits = config.fetch(BITS_KEY).await.ok().flatten();
    let rate = config.fetch(RATE_KEY).await.ok().flatten();
    let edge = config.fetch(EDGE_KEY).await.ok().flatten();
    let (_, fixes, clock_pair) = reconcile_mic_settings(
        bits.as_ref().map(|v| v.as_str()),
        rate.as_ref().map(|v| v.as_str()),
        edge.as_ref().map(|v| v.as_str()),
    );
    let mut write_errors: Vec<(&'static str, String)> = Vec::new();
    if let Some(pair) = &clock_pair {
        // The rate is written first, so any intermediate pair stays out of
        // window. If it fails the slot width is not written at all, and if the
        // slot width write then fails the rate is restored, so neither key of
        // the pair is left changed on a partial repair.
        match store_setting(&mut config, RATE_KEY, &pair.rate).await {
            Ok(()) => {
                if let Err(err) = store_setting(&mut config, BITS_KEY, &pair.bits).await {
                    if let Some(original) = &rate {
                        let _ = store_setting(&mut config, RATE_KEY, original.as_str()).await;
                    }
                    write_errors.push((BITS_KEY, err));
                }
            }
            Err(err) => write_errors.push((RATE_KEY, err)),
        }
    }
    for fix in &fixes {
        if let Err(err) = store_setting(&mut config, fix.key, &fix.value).await {
            write_errors.push((fix.key, err));
        }
    }
    let bits = config.fetch(BITS_KEY).await.ok().flatten();
    let rate = config.fetch(RATE_KEY).await.ok().flatten();
    let edge = config.fetch(EDGE_KEY).await.ok().flatten();
    let (resolved, remaining, remaining_pair) = reconcile_mic_settings(
        bits.as_ref().map(|v| v.as_str()),
        rate.as_ref().map(|v| v.as_str()),
        edge.as_ref().map(|v| v.as_str()),
    );
    let mut unreconciled: Vec<UnreconciledKey> = Vec::new();
    for fix in &remaining {
        let stored = match fix.key {
            BITS_KEY => bits.as_ref(),
            RATE_KEY => rate.as_ref(),
            EDGE_KEY => edge.as_ref(),
            _ => None,
        };
        unreconciled.push(UnreconciledKey {
            key: fix.key,
            stored: stored
                .map(|v| alloc::format!("{v}"))
                .unwrap_or_else(|| fix.value.clone()),
        });
    }
    if let Some(pair) = &remaining_pair {
        unreconciled.push(UnreconciledKey {
            key: BITS_KEY,
            stored: bits
                .as_ref()
                .map(|v| alloc::format!("{v}"))
                .unwrap_or_else(|| pair.bits.clone()),
        });
        unreconciled.push(UnreconciledKey {
            key: RATE_KEY,
            stored: rate
                .as_ref()
                .map(|v| alloc::format!("{v}"))
                .unwrap_or_else(|| pair.rate.clone()),
        });
    }
    drop(config);
    for failed in &unreconciled {
        match write_errors.iter().find(|(key, _)| *key == failed.key) {
            Some((_, err)) => {
                crate::net::ptt_note(&alloc::format!(
                    "failed to reconcile {} ({}) - store still holds {}; console reports flag it",
                    failed.key,
                    err,
                    failed.stored
                ))
                .await
            }
            None => {
                crate::net::ptt_note(&alloc::format!(
                    "{} store is unreconciled (still holds {}); console reports flag it",
                    failed.key,
                    failed.stored
                ))
                .await
            }
        }
    }
    StoreOutcome {
        stored: resolved.raw,
        resolved,
        unreconciled,
    }
}

async fn resolve_settings() -> terminal_model::mic_config::ResolvedSettings {
    read_and_reconcile().await.resolved
}

/// Settings for the recording about to start, logging if a stored value had to
/// be replaced by the default.
pub async fn load_settings() -> MicSettings {
    let resolved = resolve_settings().await;
    if resolved.fell_back {
        crate::net::ptt_note("stored mic clock settings out of range, using defaults").await;
    }
    resolved.settings
}

/// Validates a `config set ptt_*` request against the currently effective
/// settings, so an out-of-window or malformed value is refused at the console
/// rather than stored. A set that would let a stale stored clock value (one a
/// failed reconciliation rewrite could not fix) become effective is also
/// refused, so the reported and effective values cannot diverge silently. Keys
/// this module does not own return `Ok(())`.
pub async fn validate_config_setting(key: &str, value: &str) -> Result<(), String> {
    let outcome = read_and_reconcile().await;
    validate_mic_setting(outcome.stored, outcome.resolved.settings, key, value)
}

/// Effective value of a `ptt_*` setting for `config get`; `None` for keys this
/// module does not own. Shows the default when the key is unset. When a
/// reconciliation rewrite failed, the report names the stale stored value too,
/// so the console cannot claim the store agrees when it does not.
pub async fn effective_setting(key: &str) -> Option<String> {
    let outcome = read_and_reconcile().await;
    let stale = outcome
        .unreconciled
        .iter()
        .find(|failed| failed.key == key)
        .map(|failed| failed.stored.as_str());
    report_mic_setting(key, outcome.resolved.settings, stale)
}

/// Samples captured by `capture_task`, drained by `ptt_upload_task`. A fixed
/// `static` (never heap-allocated, never resized), guarded by an
/// `embassy_sync` mutex rather than held lock-free per `heap.rs`'s CAS
/// caveat. Because `AudioRing::write` never blocks, `capture_task` can deposit
/// samples cooperatively regardless of what the upload task is doing, so there
/// is no upload-vs-capture race to manage. Each sample carries the utterance
/// generation that produced it, so overlapping utterances can share the buffer
/// without one's audio (or end) leaking into the other's upload.
static PCM_RING: Mutex<CriticalSectionRawMutex, AudioRing<RING_SAMPLES>> =
    Mutex::new(AudioRing::new());

/// Highest generation a recording has been armed for; bumped by
/// `start_recording` and read by both tasks. Generations are what distinguish
/// overlapping utterances, so a later recording can never be mistaken for (or
/// consume the end of) an earlier one still being uploaded.
static CURRENT_GEN: AtomicU32 = AtomicU32::new(0);
/// Highest generation whose capture has finished. Monotonic, because only one
/// recording runs at a time.
static ENDED_GEN: AtomicU32 = AtomicU32::new(0);

/// Active recording generation, or 0 when idle. Storing the generation rather
/// than a bool lets the duration cap stop only its own recording.
static RECORDING: AtomicU32 = AtomicU32::new(0);
/// Generation whose capture first overflowed the ring and has not yet been
/// reported, or 0. `capture_task` records this instead of printing, because
/// `print!` awaits the screen lock and would stall the I2S DMA pull.
static OVERFLOW_NOTICE_GEN: AtomicU32 = AtomicU32::new(0);
/// Generation that hit `MAX_RECORDING_DURATION` and has not yet been reported,
/// set for the same reason as `OVERFLOW_NOTICE_GEN`.
static CAP_NOTICE_GEN: AtomicU32 = AtomicU32::new(0);
/// Generation whose capture first outran the DMA ring (the consumer was late by
/// more than the whole ring, so the DMA overwrote unread audio) and has not yet
/// been reported, or 0. Recorded rather than printed for the same reason as
/// `OVERFLOW_NOTICE_GEN`: printing here would take the screen lock on the
/// capture path.
static DMA_OVERRUN_NOTICE_GEN: AtomicU32 = AtomicU32::new(0);

/// Latest drained chunk's AC RMS level (in `i16` counts), for the on-screen
/// level meter. Updated by `ptt_upload_task` from the samples it has just
/// drained (see [`meter_level`]), and reset to 0 when a recording ends. It is
/// deliberately never written by `capture_task`: any per-chunk synchronous work
/// between DMA pulls can starve the PIO RX FIFO and silence the capture (the
/// confirmed level-meter regression), so the capture path only deposits samples
/// in the ring and the meter is computed from them off that path. The screen
/// painter polls this, so it never takes the capture path's lock either.
static PTT_LEVEL: AtomicU32 = AtomicU32::new(0);

/// Current push-to-talk input level for the on-screen meter; 0 when idle.
/// Deliberately the DC-removed (AC) level, so a mic sitting on its noise floor
/// reads near zero instead of being pinned by its DC offset.
pub fn level() -> u32 {
    PTT_LEVEL.load(Ordering::Acquire)
}

/// Level-meter value for a chunk the upload task just drained from the ring.
/// Called from the upload task only, so the cost (a windowed median of integer
/// RMS values) stays off the DMA capture path.
fn meter_level(samples: &[i16]) -> u32 {
    ac_rms_level(samples)
}

static START_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Wakes `ptt_upload_task` when `capture_task` has deposited samples. A
/// `Signal` coalesces, which is exactly the needed contract: one pending wake
/// means "there may be samples to drain", and the drain loop empties
/// everything available before waiting again. Correctness comes from
/// `CURRENT_GEN`/`ENDED_GEN`, not from this signal, so coalescing across
/// utterances is harmless.
static DATA_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Wake-up hint for the same reason as `DATA_READY`; the actual end of an
/// utterance is decided by [`utterance_ended`] against the generation
/// counters.
static STREAM_ENDED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Separate from `START_SIGNAL` (each `Signal` has exactly one waiter:
/// `capture_task` waits on `START_SIGNAL`, `ptt_upload_task` on this one) so
/// the upload task can start serving an utterance as soon as a recording
/// starts, while `capture_task` fills the static ring.
static UPLOAD_START_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Begins push-to-talk capture; a no-op if already recording. Called from
/// `keyboard.rs` on `(KeyState::Pressed, Key::F1)` (with no modifiers held).
pub async fn start_recording() {
    if RECORDING.load(Ordering::Acquire) == 0 {
        // Push-to-talk only exists on the SSH session's audio channel, so
        // without it there is nowhere for the audio to go: say why instead of
        // recording into the void (and without spinning up the microphone, the
        // PIO clock or a ring full of samples nobody will read). The notice is
        // an overlay - painted over the terminal for a couple of seconds,
        // never written into its buffer - so it cannot corrupt the session's
        // output, and the same reason goes to the log for the record.
        if !crate::net::ssh_audio_available() {
            let message = if crate::net::ssh_session_active() {
                "ptt unavailable: the session\'s audio channel is not running"
            } else {
                "no ssh session: not recording"
            };
            crate::net::ptt_note(message).await;
            SCREEN.get().lock().await.show_notice(String::from(message));
            return;
        }
        // A new generation identifies this utterance for the rest of its
        // life. The ring is deliberately *not* cleared here: a previous
        // utterance's undrained audio must stay available to its own upload,
        // and the generation tags keep the two separate.
        let generation = CURRENT_GEN.fetch_add(1, Ordering::AcqRel) + 1;
        RECORDING.store(generation, Ordering::Release);
        SCREEN
            .get()
            .lock()
            .await
            .show_overlay(String::from("recording..."));
        START_SIGNAL.signal(());
        UPLOAD_START_SIGNAL.signal(());
    }
}

/// Whether a push-to-talk recording is currently active. `keyboard.rs` checks
/// this on `(KeyState::Released, Key::F1)` so the matching release always
/// stops capture regardless of which modifiers are held at that instant.
pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Acquire) != 0
}

/// Ends push-to-talk capture; a no-op if not recording. Called from
/// `keyboard.rs` on `(KeyState::Released, Key::F1)` while `is_recording()`.
pub async fn stop_recording() {
    if RECORDING.swap(0, Ordering::AcqRel) != 0 {
        SCREEN.get().lock().await.clear_overlay();
    }
}

/// PIO-backed I2S RX (microphone) driver. This is the mirror image of
/// embassy-rp's `pio_programs::i2s::PioI2sOut` (`in pins, 1` capturing
/// instead of `out pins, 1` emitting, on the same word/bit-clock side-set
/// shape) — embassy-rp ships no I2S RX driver, so this is written directly
/// against the public `embassy_rp::pio` API rather than vendored, following
/// this project's existing `psram.rs` precedent for a custom PIO program.
///
/// It owns the whole `Pio` block plus its pins so [`Self::apply`] can rebuild
/// the runtime-assembled program and swap the loaded one as settings change.
struct Mic {
    pio: Pio<'static, PIO2>,
    dma_ch: PeripheralRef<'static, DMA_CH4>,
    bclk: Pin<'static, PIO2>,
    ws: Pin<'static, PIO2>,
    sd: Pin<'static, PIO2>,
    /// Settings currently loaded into the SM, or `None` before the first
    /// recording; lets `apply` skip redundant reloads.
    applied: Option<MicSettings>,
    /// Instruction memory of the currently loaded program, freed before a new
    /// one is loaded so repeated reconfiguration cannot exhaust PIO RAM.
    loaded: Option<LoadedProgram<'static, PIO2>>,
}

/// The capture DMA's circular buffer, written by the DMA in hardware and read
/// (volatile) by `capture_task`. Power-of-two sized and naturally aligned, so
/// the DMA's write-address wrap covers exactly the buffer once per lap.
#[repr(align(8192))]
struct DmaRingStorage(UnsafeCell<[u32; DMA_RING_WORDS]>);

// SAFETY: the only writer is the DMA channel, which runs only between
// `start_capture_dma` and `stop_capture_dma` inside one recording; the CPU reads
// the buffer through volatile loads and never keeps a reference across an
// await.
unsafe impl Sync for DmaRingStorage {}

static DMA_RING: DmaRingStorage = DmaRingStorage(UnsafeCell::new([0; DMA_RING_WORDS]));

/// Address of the DMA ring's first word.
fn dma_ring_base() -> *mut u32 {
    DMA_RING.0.get() as *mut u32
}

/// The DMA's live write position, in words into [`DMA_RING`].
fn dma_write_index(channel: usize) -> usize {
    let address = embassy_rp::pac::DMA.ch(channel).write_addr().read() as usize;
    address.wrapping_sub(dma_ring_base() as usize) / 4 % DMA_RING_WORDS
}

impl Mic {
    fn set_enabled(&mut self, enabled: bool) {
        self.pio.sm0.set_enable(enabled);
    }

    /// Starts the free-running capture DMA: this channel copies PIO SM0's RX
    /// FIFO into [`DMA_RING`] until it is aborted, wrapping the write address on
    /// the ring (endless transfer mode). With a one-shot transfer per chunk
    /// instead, the eight-word FIFO fills while the next transfer is armed, the
    /// state machine stalls on `in`, and the microphone's bit clock stops for as
    /// long as the CPU was late - which is what silences a capture (and what the
    /// old per-chunk plateau/glitch artifact was). Here the FIFO is drained in
    /// hardware and the CPU only has to keep up with the buffer, not with the
    /// clock.
    ///
    /// Returns the DMA channel number, which the consumer needs to read the
    /// live write position back.
    fn start_capture_dma(&mut self) -> usize {
        let channel = self.dma_ch.number() as usize;
        let ch = embassy_rp::pac::DMA.ch(channel);
        ch.read_addr()
            .write_value(embassy_rp::pac::PIO2.rxf(0).as_ptr() as u32);
        ch.write_addr().write_value(dma_ring_base() as u32);
        ch.trans_count().write(|w| {
            w.set_count(DMA_RING_WORDS as u32);
            // Endless: the count never decrements, so the channel runs until it
            // is aborted, raising no interrupts and triggering no other channel.
            w.set_mode(TransCountMode::ENDLESS);
        });
        ch.ctrl_trig().write(|w| {
            w.set_treq_sel(TreqSel::PIO2_RX0);
            w.set_data_size(DataSize::SIZE_WORD);
            w.set_incr_read(false);
            w.set_incr_write(true);
            // Wrap the write address inside the ring rather than the read one.
            w.set_ring_sel(true);
            w.set_ring_size(DMA_RING_BITS);
            // Chaining to itself is how the field disables chaining.
            w.set_chain_to(channel as u8);
            w.set_en(true);
        });
        channel
    }

    /// Aborts the free-running capture DMA, leaving the state machine and its
    /// pins to `apply`/`set_enabled`.
    fn stop_capture_dma(&mut self) {
        let channel = self.dma_ch.number() as usize;
        let ch = embassy_rp::pac::DMA.ch(channel);
        embassy_rp::pac::DMA
            .chan_abort()
            .write(|w| w.set_chan_abort(1 << channel));
        while ch.ctrl_trig().read().busy() {}
        // A full write (not `modify`) so the read-only status bits cannot be
        // written back, and without `en` so nothing is retriggered.
        ch.ctrl_trig().write(|w| w.set_en(false));
    }

    /// (Re)configures the PIO program and clock for `settings`, freeing the
    /// previously loaded program. The program is rebuilt and reloaded only
    /// when `settings` changed; the state-machine reset and clock/pin config
    /// run on every call.
    fn apply(&mut self, settings: MicSettings) {
        self.pio.sm0.set_enable(false);
        self.pio.sm0.restart();
        self.pio.sm0.clear_fifos();

        if self.applied != Some(settings) {
            let program = build_i2s_rx_program(settings.bits, settings.edge_flip);
            if let Some(old) = self.loaded.take() {
                // SAFETY: the state machine was disabled and restarted above,
                // so it is not executing the instruction memory being freed.
                unsafe { self.pio.common.free_instr(old.used_memory) };
            }
            self.loaded = Some(self.pio.common.load_program(&program));
        }

        let loaded = self
            .loaded
            .take()
            .expect("PIO program must be loaded before applying its config");
        let mut cfg = Config::default();
        cfg.use_program(&loaded, &[&self.bclk, &self.ws]);
        cfg.set_in_pins(&[&self.sd]);
        let clock_frequency = bit_clock_hz(settings.rate, settings.bits, CHANNELS);
        cfg.clock_divider = (clk_sys_freq() as f64 / clock_frequency as f64 / 2.).to_fixed();
        // One autopush per channel slot regardless of width, so FIFO words
        // are always left, right, left, ... and can be sliced by parity.
        cfg.shift_in = ShiftConfig {
            threshold: settings.bits as u8,
            direction: ShiftDirection::Left,
            auto_fill: true,
        };
        // Doubles RX FIFO depth since TX is unused; mirrors PioI2sOut.
        cfg.fifo_join = FifoJoin::RxOnly;

        self.pio.sm0.set_config(&cfg);
        self.pio.sm0.set_pin_dirs(Direction::In, &[&self.sd]);
        self.pio
            .sm0
            .set_pin_dirs(Direction::Out, &[&self.bclk, &self.ws]);
        self.loaded = Some(loaded);
        self.applied = Some(settings);
    }
}

/// Claims PIO2 (unclaimed elsewhere in this codebase; PIO0 is WiFi, PIO1 is
/// now unused since the slow PSRAM path was dropped) and the expansion-header
/// pins that path used to claim, and spawns the capture and network-upload
/// tasks. `bclk`/`ws`/`sd` must be `PIN_2`/`PIN_3`/`PIN_21` per AGENTS.md's
/// pin contract.
pub fn init_mic(
    spawner: &Spawner,
    pio2: PIO2,
    bclk: PIN_2,
    ws: PIN_3,
    sd: PIN_21,
    dma_ch4: DMA_CH4,
) {
    let mut pio = Pio::new(pio2, Irqs);
    let bclk = pio.common.make_pio_pin(bclk);
    let ws = pio.common.make_pio_pin(ws);
    let sd = pio.common.make_pio_pin(sd);

    // Left unconfigured and disabled (no clocks driven, no mic power/noise)
    // until the first recording, when `capture_task` resolves the runtime
    // settings and `Mic::apply` builds the program for them.
    let mic = Mic {
        pio,
        dma_ch: PeripheralRef::new(dma_ch4),
        bclk,
        ws,
        sd,
        applied: None,
        loaded: None,
    };

    spawner.must_spawn(capture_task(mic));
    spawner.must_spawn(ptt_upload_task());
}

#[embassy_executor::task]
async fn capture_task(mut mic: Mic) {
    // One wire chunk's worth of raw RX-FIFO words: one word per channel slot,
    // two slots per L+R frame (see `SAMPLES_PER_CHUNK`).
    let mut chunk = [0u32; SAMPLES_PER_CHUNK * 2];
    loop {
        START_SIGNAL.wait().await;
        let generation = CURRENT_GEN.load(Ordering::Acquire);
        // Resolve and apply the runtime debug configuration before the I2S
        // clock starts, so a `config set ptt_*` affects this very utterance
        // (no rebuild, reflash or reboot).
        let settings = load_settings().await;
        mic.apply(settings);
        mic.set_enabled(true);
        let channel = mic.start_capture_dma();

        let started = Instant::now();
        let mut read_index = 0usize;
        let mut filled = 0usize;
        let mut last_poll = Instant::now();
        let mut overrun_noticed = false;
        let mut ticker = Ticker::every(DMA_POLL_INTERVAL);
        // The DMA writes two words per sample (one per channel slot), which is
        // what tells a *late* poll (see the overrun check below) apart from a
        // quick one: the ring holds `DMA_RING_WORDS / words_per_ms` milliseconds
        // of audio.
        let words_per_ms = ((settings.rate as usize) * 2 / 1000).max(1);
        // Ends when the button is released, when a newer recording supersedes
        // this one, or on the recording-duration cap below.
        while RECORDING.load(Ordering::Acquire) == generation
            && CURRENT_GEN.load(Ordering::Acquire) == generation
        {
            let write_index = dma_write_index(channel);
            // A poll that arrives more than a full ring late cannot tell how far
            // the DMA wrapped, so resynchronise at the write head, drop the
            // audio that was overwritten, and report it once for this recording.
            if (Instant::now() - last_poll).as_millis() as usize * words_per_ms > DMA_RING_WORDS {
                if !overrun_noticed {
                    DMA_OVERRUN_NOTICE_GEN.store(generation, Ordering::Release);
                    overrun_noticed = true;
                }
                // Even index on purpose: the DMA starts this recording at ring
                // index 0 with a left-slot word, so even positions stay the
                // driven (left) slot and the extraction's pairing holds.
                read_index =
                    ((write_index + DMA_RING_WORDS - DMA_RING_MARGIN) % DMA_RING_WORDS) & !1;
                filled = 0;
            }
            last_poll = Instant::now();

            // One FIFO word is one channel slot (see `MicSettings::bits`), not
            // a combined L+R pair, and one wire chunk is
            // `SAMPLES_PER_CHUNK` *frames*: two words each. Take whole frames
            // only, so the left/right parity of the run never shifts, and leave
            // the margin behind the write head untouched.
            let available = dma_ring::available_words(write_index, read_index, DMA_RING_WORDS);
            let take = available
                .saturating_sub(DMA_RING_MARGIN)
                .min(SAMPLES_PER_CHUNK * 2 - filled)
                & !1;
            if take > 0 {
                let mut copied = 0usize;
                for (start, len) in dma_ring::segments(read_index, take, DMA_RING_WORDS) {
                    for offset in 0..len {
                        // SAFETY: `start + offset` is inside the ring (see
                        // `dma_ring::segments`'s bounds test) and the DMA owns
                        // only the region at and beyond the write head.
                        chunk[filled + copied + offset] = unsafe {
                            core::ptr::read_volatile(dma_ring_base().add(start + offset))
                        };
                    }
                    copied += len;
                }
                read_index = (read_index + take) % DMA_RING_WORDS;
                filled += take;
            }

            if filled == chunk.len() {
                // Never blocks and never allocates: if the network side is
                // behind, the audio ring drops its oldest samples instead of
                // stalling this task, which is what keeps the DMA (and with it
                // the I2S clocks) running.
                // Keep only the even-indexed (left-slot) words and drop the
                // odd-indexed (right-slot) ones the mic never drives, matching
                // this driver's left-slot pin/wiring contract in AGENTS.md
                // (swap to odd-indexed words if a captain instead wires the mic
                // to the right slot). ShiftDirection::Left means MSB-first, and
                // `extract_left_channel_pcm` reduces each word to its
                // slot-width-aware 16-bit sample (see its doc for the exact
                // shift).
                let mut pcm = [0i16; SAMPLES_PER_CHUNK];
                extract_left_channel_pcm(&chunk, &mut pcm, settings.bits);
                let result = PCM_RING.lock().await.write(generation, &pcm);
                if result.first_drop {
                    OVERFLOW_NOTICE_GEN.store(generation, Ordering::Release);
                }
                DATA_READY.signal(());
                filled = 0;
            }

            if started.elapsed() >= MAX_RECORDING_DURATION {
                CAP_NOTICE_GEN.store(generation, Ordering::Release);
                let _ =
                    RECORDING.compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire);
                break;
            }
            ticker.next().await;
        }
        mic.set_enabled(false);
        mic.stop_capture_dma();
        PTT_LEVEL.store(0, Ordering::Release);
        ENDED_GEN.fetch_max(generation, Ordering::AcqRel);
        STREAM_ENDED.signal(());
    }
}

/// Emits any one-shot diagnostics that `capture_task` recorded and dismisses
/// the overlay a capped recording left up. Runs on the upload task, never on
/// the DMA capture path, so the screen lock can be held by a repaint without
/// stalling the I2S clocks.
async fn emit_pending_notices() {
    if OVERFLOW_NOTICE_GEN.swap(0, Ordering::AcqRel) != 0 {
        crate::net::ptt_note("upload can't keep up, dropping oldest audio").await;
    }
    if DMA_OVERRUN_NOTICE_GEN.swap(0, Ordering::AcqRel) != 0 {
        crate::net::ptt_note("capture ring overran, dropping some audio").await;
    }
    let capped_generation = CAP_NOTICE_GEN.swap(0, Ordering::AcqRel);
    if capped_generation != 0 {
        crate::net::ptt_note("recording exceeded 60s cap, stopping").await;
        let mut screen = SCREEN.get().lock().await;
        if CURRENT_GEN.load(Ordering::Acquire) == capped_generation {
            screen.clear_overlay();
        }
    }
}

/// Sends one utterance's audio down the session's audio channel: drain every
/// sample tagged `generation` until that generation ends, then mark its end
/// on the channel. Samples belonging to any other
/// generation are left in the ring for their own upload, and the end
/// condition is [`utterance_ended`] on the generation counters - never a
/// shared signal - so a later recording can neither have its audio sent here
/// nor be mistaken for this one's end. When the channel is unavailable the
/// samples are discarded instead, so the ring is still drained and the
/// recording always terminates.
///
/// The destination is the SSH session's audio channel when it is up (see
/// [`crate::net::ssh_audio_available`]); that is the transport that reaches a
/// helper running on the machine the user is typing into. If that channel
/// fails mid-recording the rest of the utterance is drained, so the recording
/// still terminates.
async fn serve_utterance(generation: u32) {
    let mut ssh = crate::net::ssh_audio_available();

    let mut buf = [0i16; SAMPLES_PER_CHUNK];
    loop {
        emit_pending_notices().await;
        // Drain everything this generation has buffered so far. The static
        // ring absorbs (and, when full, drops the oldest of) whatever capture
        // produces meanwhile, so a failed, slow, or congested session only
        // ever costs buffered audio - it never blocks `capture_task`.
        loop {
            let n = {
                let mut ring = PCM_RING.lock().await;
                ring.read(generation, &mut buf)
            };
            if n == 0 {
                break;
            }
            // Meter the chunk here, on the upload task, from the samples just
            // drained - never in `capture_task`, where any per-chunk work
            // would delay the DMA ring and risk the microphone's clock (the
            // confirmed regression).
            PTT_LEVEL.store(meter_level(&buf[..n]), Ordering::Release);
            if ssh {
                // The session's audio channel, carrying this utterance's
                // frames on the connection the interactive session already
                // has. `false` means it stopped taking them (the session ended
                // or its helper went away), which also covers the channel
                // becoming unavailable between chunks.
                if !crate::net::ssh_audio_send(&buf[..n]).await {
                    crate::net::ptt_note("ssh audio channel unavailable, dropping rest").await;
                    ssh = false;
                }
            }
        }

        if utterance_ended(
            CURRENT_GEN.load(Ordering::Acquire),
            ENDED_GEN.load(Ordering::Acquire),
            generation,
        ) {
            break;
        }
        // Nothing more for this generation yet; wait for more audio, for the
        // end of the recording, or for a newer recording that supersedes this
        // one (the counters are re-checked on wake).
        select(DATA_READY.wait(), STREAM_ENDED.wait()).await;
    }
    emit_pending_notices().await;

    if ssh {
        // The SSH channel stays open for the whole session, so the end of this
        // utterance has to be marked in band (a zero-length frame) rather than
        // by closing anything.
        if !crate::net::ssh_audio_end_of_utterance().await {
            crate::net::ptt_note("ssh audio channel unavailable, dropping utterance end").await;
        }
    }
}

#[embassy_executor::task]
async fn ptt_upload_task() {
    let mut next_generation: u32 = 1;

    loop {
        // Serve every utterance exactly once, in order. Generations are
        // contiguous, so once `CURRENT_GEN` has reached one it exists and must
        // be served - even if it was superseded before its upload began.
        // Whether the session's audio channel is up is decided per utterance
        // in `serve_utterance`.
        while next_generation > CURRENT_GEN.load(Ordering::Acquire) {
            UPLOAD_START_SIGNAL.wait().await;
        }
        emit_pending_notices().await;
        serve_utterance(next_generation).await;
        next_generation += 1;
    }
}
