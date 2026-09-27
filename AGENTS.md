# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Build and test

- This is a `no_std`/`no_main` firmware crate for `thumbv8m.main-none-eabihf` (Pico2W/Pimoroni2W).
  `make check` / `make image` (see `Makefile`) select the chip via `--features pico2w` or
  `--features pimoroni2w` — the crate does not build at all with no chip feature selected.
  CI (`.github/workflows/build.yml`) only runs `make image`; there is no `cargo test` step.
- Most of this crate's dependencies (embassy-rp, cyw43, mipidsi, ...) are real hardware/PAC
  bindings and will not compile for a host target — don't try to `cargo test`/`cargo check` the
  root `picocalc-wezterm` package for `x86_64-unknown-linux-gnu`, it fails deep in `embassy-rp`.
- `terminal-model/` is a separate workspace-member crate (path dependency) holding the
  hardware-independent logic the firmware pulls in: terminal buffer/VTE (`screen_model.rs`) and
  vector glyph-drawing (`glyphs.rs`) from `src/screen.rs`, push-to-talk key dispatch
  (`key_dispatch.rs`) from `src/keyboard.rs`, the capture/upload sample ring
  (`audio_ring.rs`), the free-running capture DMA ring's wrap arithmetic (`dma_ring.rs`), the I2S
  bit-clock/PCM-extraction arithmetic (`pcm_extract.rs`), the mic
  runtime-settings resolution and console-validation decision table (`mic_config.rs`) and the
  runtime PIO I2S RX program assembly (`i2s_program.rs`) from `src/mic.rs`, and the SD-card
  SSH-key backup text codec (`keyfile.rs`) from `src/sshkey.rs`. It depends only on
  `vte`, `embedded-graphics`, `pio`, and `profont` — all host-buildable — so it's the place for
  real, runnable unit tests. Run them with
  `cargo test -p terminal-model --target x86_64-unknown-linux-gnu` (must override the default
  target set in `.cargo/config.toml`). If new logic needs a host test and doesn't fit here, prefer
  extending this crate over adding tests to the hardware-coupled root crate.
- `.github/workflows/build.yml` has four jobs. On `pull_request`: `determine-version` (parses the
  prior PR comment's `v<N>` heading, made by the job below, to compute the next per-PR iteration
  number — starts at 1, deliberately not `github.run_number`/`run_attempt` since neither is a
  per-PR counter) → `build-pr` (matrix over `pico2w`/`pimoroni2w`, builds via `make CHIP=<chip>
  image`, uploads each as a workflow artifact named `picocalc-ssh-client-<chip>-pr<N>-v<version>`)
  → `comment-pr` (needs `build-pr`, runs once — not matrixed — and posts/updates a single PR
  comment linking both chips' just-built artifacts, via `actions/github-script` listing this run's
  artifacts over the REST API rather than passing `upload-artifact`'s `artifact-id`/`artifact-url`
  step outputs through matrix job outputs: GitHub Actions matrix job outputs are last-write-wins
  per key across all matrix legs unconditionally, even when the losing leg's value was an empty/
  guard-conditioned string, so a naive "one output key per chip" scheme silently drops one chip's
  link depending on leg completion order — the artifact-listing API sidesteps that race). The
  comment is found and edited in place on later pushes via a stable `<!-- picocalc-uf2-artifacts
  -->` HTML marker, not reposted. `build-release`/`publish-release` (manual `workflow_dispatch`
  with a required `version` input, builds both chips, publishes one GitHub Release tagged
  `version` with both `.uf2` assets) are unrelated to the PR flow. There is no push-to-main
  auto-release anymore.
  This repo is a fork, and GitHub disables Actions by default on forks until the owner opts in from
  the repo's web UI Settings > Actions page (the REST `actions/permissions` endpoint 403s for a
  non-admin token, so this can't be done via `gh api`) — check `gh api
  repos/lviss/picocalc-ssh-client/actions/runs` for a nonzero run count before assuming CI is live.
- `vte::Params` always pushes a parameter slot on `csi_dispatch`, even for a bare sequence with no
  digits (e.g. `CSI C`) — the "no parameter" case arrives as an explicit `0`, not an absent one.
  A plain `params.iter().next().map(|p| p[0]).unwrap_or(1)` therefore silently computes `0` instead
  of the ECMA-48 default of `1` for the extremely common bare-sequence form. Use
  `.unwrap_or(0).max(1)` (see `cursor_move_count` in `terminal-model/src/screen_model.rs`) for any
  CSI parameter that has a nonzero default.
- `ScreenModel::overlay` (`terminal-model/src/screen_model.rs`) is the pattern for any transient
  on-screen banner (the battery readout on a power-button press, and the push-to-talk
  "recording..." indicator): it's a paint-time-only flag that never touches `lines`/`scrollback`,
  composited on top each frame in `src/screen.rs`'s `update_display` / `draw_overlay`.
  `clear_overlay()` forces `full_repaint = true` so dismissal redraws the real, possibly-changed
  cell content underneath from scratch rather than needing a save/restore buffer. The
  overlay/auto-dismiss state machine itself (`overlay`, a persistent-overlay slot to restore,
  and an absolute `u64` millisecond deadline) lives in `ScreenModel` and is host-tested; it takes
  `now_ms` from the caller, so `terminal-model` stays clock-free and `src/screen.rs`'s `Screen`
  wrapper just passes `Instant::now().as_millis()`. Always show overlays through a `Screen`
  helper: `Screen::show_battery_overlay` calls `ScreenModel::show_timed_overlay` (battery readout,
  3 s), while `Screen::show_overlay` (used by `src/mic.rs` for the recording indicator) marks a
  persistent overlay. `ScreenModel::tick_overlay` dismisses only an expired *timed* overlay and
  restores the persistent one it covered (or clears if there was none), forcing a full repaint
  so the wider/taller previous box's pixels are erased; replacing an already-shown overlay with
  different text forces the same repaint (showing one where none was present stays a pure
  paint-time flag). So a power-button press before or during a recording cannot leave the
  "recording..." indicator (and the level meter) cleared or partially overdrawn for the rest of
  the utterance.
- Despite the caution above about the root package not building for the host target: this repo's
  installed toolchain does carry a prebuilt `thumbv8m.main-none-eabihf` std, so
  `cargo check --features pimoroni2w` (or `pico2w`) on the root package works and fully
  type-checks the firmware crate — useful for validating non-`terminal-model` changes without
  hardware. `cargo build --release --features <chip>` (what `make image` runs) also compiles all
  the way through codegen and linking with `flip-link` on `PATH` (in this sandbox it's already
  installed at `/home/ai/.cargo/bin/flip-link`, just not on `PATH` by default — `cargo install
  flip-link` is a no-op confirming this; add `/home/ai/.cargo/bin` to `PATH` rather than
  reinstalling). If `flip-link` is genuinely absent and can't be installed (no network/build
  tools), that's a linker availability gap, not a code problem, if it's the only failure.
- flip-link places static `.bss` directly above the embassy executor stack, so the two compete
  for the same RP2350 SRAM: every byte of new static `.bss` comes off the usable stack, and
  `src/main.rs`'s `DISPLAY_BUFFER_SIZE` (`320 * 3 * 64`, the display SPI interface's batching
  buffer, deliberately a 61 KiB batch rather than a full 307 KiB framebuffer) is the load-bearing
  knob that sets it. This is not cosmetic: the PTT work's ~28 KiB of static `.bss` (the PCM ring
  plus the two embassy task pools) had taken the stack from ~70 KiB on `main`'s 307 KiB-buffer
  build down to ~45 KiB, which is what overflowed the deep SSH connect/KEX path; the 61 KiB batch
  restored it to ~282 KiB (measured as `_stack_start` - 0x20000000 in the linked ELF, i.e. the
  stack region below the statics). Re-measure that region (e.g. read `_stack_start` with `nm` from
  the `.elf` after `make image`) before enlarging the display buffer, adding another task pool, or
  growing any other static `.bss`, and keep the batch at least this generous unless the measurement
  says otherwise. Current readings for the `pimoroni2w` release ELF: `_stack_start`=0x20046948
  (~281.8 KiB) before PTT-over-SSH, 0x20045a88 (~278.6 KiB) after it - that feature's fixed
  `AUDIO_QUEUE` (3 audio frames, `src/net.rs`) plus the larger `ptt_upload_task` future cost ~3.2
  KiB of headroom, which is the whole of its `.bss` footprint. The two chips do differ by a
  little: re-measured after this work, the source gives `_stack_start`=0x20045a88 (~278.6 KiB) on
  `pico2w` and 0x200459f8 (~278.5 KiB) on `pimoroni2w`, so always name the chip with the reading
  (the 0x20045a88 figure above was originally recorded as pimoroni2w's). The free-running capture
  DMA's ring (`src/mic.rs`'s `DMA_RING`, 2048 words = 8 KiB, `#[repr(align(8192))]` so the DMA's
  write-address wrap lands on it) costs another ~14.5 KiB of that region - 8 KiB of buffer plus of
  the order of 8 KiB of alignment padding - taking `_stack_start` to 0x20042000 (~264 KiB) on both
  chips. That is still generous (the crashes this note exists for happened at ~45 KiB), but it is
  the number to re-measure before growing the display buffer or any other static from here on.
- `make image` embeds the image version (reported by `picotool info -a`, and used in the `.uf2`
  filename) from `build.rs`'s `PICOCALC_CI_TAG`, which `build.rs` computes by running `git show -s`
  itself when it runs. `build.rs` asks cargo to `rerun-if-changed=memory.x` only, so an incremental
  build whose only change since the last one is the commit (docs-only commit, or a commit made
  after an earlier build) reuses the old build-script output and embeds the *previous* commit's tag
  - `touch build.rs` (or a clean build) before `make image` whenever the artifact must name the
  current HEAD. CI is unaffected: it always builds from a fresh target directory.
- `terminal-model::screen_model`'s `ScreenModel::max_scrollback` is not a flat literal - it's
  computed by `safe_max_scrollback_for(cols, rows)` against `SCREEN_HEAP_BUDGET_BYTES`
  (`FIRMWARE_HEAP_SIZE_BYTES` minus `NON_SCREEN_HEAP_RESERVE_BYTES`, the heap WiFi/TCP/SSH/SD and
  other boot-time subsystems reliably need per real-hardware `free`-command readings). A correctly
  *capped* scrollback buffer can still exceed the primary heap by 2x+ if the cap is a flat number
  disconnected from actual heap size - see `/ai/firstmate/data/picocalc-crash-display-buffer/report.md`
  and the two host tests next to `safe_max_scrollback_for` in `terminal-model/src/screen_model.rs`
  (`default_scrollback_cap_keeps_full_footprint_within_heap_budget`,
  `heavy_output_scroll_pressure_accelerates_once_visible_area_fills_up` - the latter documents why
  memory pressure from heavy output *accelerates* rather than growing linearly: `scroll_up()` only
  allocates a new `ScreenLine` once the visible `lines` area is already full, so output that still
  fits on-screen is nearly free while output that has to scroll costs one full line-allocation per
  line). `ScreenModel::set_max_scrollback` self-clamps to `max_safe_scrollback()`, so raising the
  user-settable `scroll` config (`src/config.rs`) can never reintroduce this crash even if
  `config.rs`'s own bound-check is ever bypassed or a stale large value is loaded from flash at boot.
  `bytes_per_line()`'s budget counts `size_of::<ScreenLine>()` per slot (not just the two inner
  `chars`/`attrs` allocations) to cover the outer `Vec<ScreenLine>` containers (`lines` and
  `scrollback`) themselves; this only holds because `Default::default()` pre-reserves
  `scrollback`'s capacity to `max_scrollback + 1` up front (`scroll_up`'s push-then-`remove(0)`
  peak) instead of growing it via `Vec::new()` + amortized doubling, which would let its real
  backing capacity overshoot the budgeted count. Any future change to how `scrollback` grows must
  preserve that fixed pre-reserved capacity or the container-overhead accounting goes stale again.
  Re-run the two tests above (and re-derive the budget) if `HEAP_SIZE` (`src/heap.rs`) or the screen
  geometry (font/`SCREEN_WIDTH`/`SCREEN_HEIGHT`) ever changes.
- `keygen save [force]` / `keygen load [force]` (`src/sshkey.rs`) export and restore the private key
  as `ssh_key.hex` in the SD card root, reusing `src/storage.rs`'s `STORAGE`/`VolumeManager` (same
  SPI0 pins as `ls`) rather than a second filesystem stack; the push-to-talk I2S mic is on
  `PIN_2`/`PIN_3`/`PIN_21` and is untouched by this. Restoring is deliberately explicit-only: there
  is NO boot-time auto-restore, because silently adopting a key from whatever card happens to be
  inserted would hand that card the device's identity without an announced action. Keep it that
  way; if a boot restore is ever added it must be announced on the console and must only apply when
  no key is stored at all. The file holds the same 64-char hex the config store keeps, with its
  parser in `terminal-model/src/keyfile.rs` so the edge cases (trailing newline, wrong length,
  non-hex) stay host-tested. Separately, `print_public_key` (called from every `keygen`/`keygen
  force`/`keygen show`) best-effort-mirrors the *public* key to `ssh_key.pub` in the same SD root
  via the same `STORAGE` manager — always overwritten, no `force` gate, since it isn't a secret;
  see README's "Public key on the SD card" for the user-facing behavior and test steps.
- On a NixOS-style agent sandbox where plain `cargo`/`rustc` aren't on `PATH`, the working
  toolchain lives under `$RUSTUP_HOME/toolchains/nightly-x86_64-unknown-linux-gnu/bin` (set
  `RUSTUP_HOME=/home/ai/.rustup` and prepend that dir to `PATH`); building anything host-targeted
  (build scripts, proc-macros, or the `terminal-model` host tests) also needs a C linker, which
  isn't present by default — `nix-shell -p gcc --run '<cargo command>'` supplies one. `cargo check
  --features <chip>` on the root package additionally needs the `embassy` git submodule checked
  out (`git submodule update --init embassy`) because `src/net.rs` embeds cyw43 firmware blobs
  from it via `include_bytes!`; `pico-sdk`/`picotool` are unrelated C build tooling and don't need
  to be initialized for a Rust-only check/build.

- The `embassy/` git submodule is reference material only, NOT what actually gets compiled: every
  `embassy-*` line in `Cargo.toml` is a bare `version = "*"` with no `path`/`git` override, so
  Cargo resolves them from crates.io (check `Cargo.lock` — e.g. `embassy-rp` resolves to a released
  `0.4.0`, which can be well behind the submodule's pinned commit). The two can have materially
  different APIs (e.g. `0.4.0` uses the older `embassy_rp::{Peripheral, PeripheralRef, into_ref!}`
  peripheral-ownership style throughout its `pio` module, while the submodule's HEAD has moved to a
  newer `Peri<'d, T>` style) — always check the actual installed crate source
  (`~/.cargo/registry/src/*/embassy-rp-<version>/`, fetch it with `cargo fetch` first if absent)
  before writing code against any embassy-rp API, rather than trusting the submodule's source.
  `src/mic.rs` is the up-to-date, actually-building example of this project's real PIO/DMA idiom
  (`PeripheralRef`, `PeripheralRef::new`, `Pio::new` + `make_pio_pin` + `StateMachine`) to copy
  from; since the mic's slot width/edge are runtime settings, its program is built at run time by
  `terminal-model/src/i2s_program.rs` with the `pio` crate's `Assembler` instead of `pio_asm!`
  (the macro is still the right tool for a fixed program). `src/psram.rs` no longer has any PIO
  code of its own (see its own entry below) — it only drives PSRAM via raw `embassy_rp::pac`
  register access now.
- Push-to-talk voice capture (`src/mic.rs`) captures mic audio on a held button and streams it to a
  network host; the receiving/transcribing side is a separate, not-yet-built process outside this
  repo. It claims PIO2 (unclaimed elsewhere — PIO0 is WiFi, PIO1 is now unused; see the PSRAM note
  below) for a hand-written I2S RX PIO program (embassy-rp ships no I2S RX driver, only the TX-only
  `pio_programs::i2s`; `mic.rs`'s program is the mirror image of that driver's `pio_asm!` block,
  `in pins, 1` instead of `out pins, 1`) and expansion-header pins
  freed by dropping the slow PSRAM path (see below): `GP2`/`GP3`/`GP21` = I2S `BCLK`/`WS`/`SD`.
  These pins are also wired to the PSRAM chip (see `psram.rs`'s header comment) - that's safe
  because the QMI/XIP hardware path this firmware now uses to reach PSRAM drives a completely
  separate, RP2350-internal chip-select pad, never these pins. GP16/17/18/19/22 (the SD card's
  SPI0 pins) were considered for the mic in an earlier iteration of this feature but the captain's
  own hardware check moved the mic to the PSRAM/expansion-header group instead, keeping SD card
  support intact (see `storage.rs`). The mic is an Adafruit SPH0645 breakout (identified from a real
  capture the captain took with a netcat listener; not INMP441 as this project's earlier
  investigation reports assumed) - a fixed-ratio I2S digital mic whose internal shift-counter is
  hardwired to a 32-bit-per-channel slot (64fs total per L+R frame: confirmed against its documented
  clock table, 1.024 MHz-4.096 MHz BCLK for 16 kHz-64 kHz sample rates, 16 kHz * 64 = 1.024 MHz
  exactly). The PIO program and its clock are now **runtime settings**, resolved from the config
  store at the start of every recording and applied on the device (no reflash or reboot):
  `ptt_bits` (channel slot width, default 32), `ptt_rate` (sample rate Hz, default 16000),
  `ptt_edge` (0 = default BCLK edge, 1 = the inverted-edge experiment; the one remaining knob a
  regular user would never touch). A `ptt_bits`/`ptt_rate` pair
  whose `rate * bits * 2` falls outside the mic's documented 1.024-4.096 MHz window is refused at
  the console and, if a stored value is somehow invalid, falls back to the default pair with a log.
  That decision table (`resolve`/`reconcile`/`validate_setting`, host-tested) lives in
  `terminal-model/src/mic_config.rs`; `src/mic.rs` is only the config-store/console adapter.
  `reconcile` also rewrites any stored key that no longer matches its effective value, so the
  store, `config get`/`config list`, and the next recording cannot diverge - otherwise a
  `ptt_rate`/`ptt_bits` key left stale by `config rm` could be silently re-adopted by a later
  `config set`. The slot-width and sample-rate keys are returned as a single `ClockPairFix` and
  written as one atomic pair (rate first, the slot width skipped if the rate write fails, the rate
  restored if the slot width write fails), so a partial repair can never leave the pair valid but
  different from what `config get` reported. Concurrently, `config list` overlays the effective
  values for the `ptt_*` keys. `ResolvedSettings::raw` exposes the pre-fallback pair. `src/mic.rs`
  re-reads and re-resolves the store after applying its rewrites, so the reported, stored and
  effective values all describe the state the store is actually left in. If a reconcile rewrite
  fails, `src/mic.rs` prints the failure and `config get`/`list` report that key as `unreconciled`
  next to the stale value the store still holds; `validate_setting_with_store` (terminal-model)
  refuses a clock-key `config set` that would make the stale value effective again (a set that
  leaves the pair out of window, or that repairs it to the reported value, is allowed), so reported
  and effective values cannot diverge silently even when the store cannot be rewritten. The program
  itself is built at run time by `terminal-model/src/i2s_program.rs` (the `pio` crate's `Assembler`)
  because `pio_asm!` bakes the loop count and edge in at compile time; `capture_task` keeps only the
  even-indexed/left-slot
  words, and `extract_left_channel_pcm` reduces each to its slot-width-aware 16-bit sample (see that
  function's doc for the exact shift, including the left-justified sub-16-bit case).
  overlay also carries a realtime level meter while recording: `ac_rms_level`
  (host-tested in `pcm_extract.rs`) publishes a windowed median of per-window AC RMS (8 windows,
  each window's own DC removed) through `mic::level()`, reading the same bits
  `extract_left_channel_pcm` puts on the wire. `src/screen.rs`'s
  `draw_overlay` draws it as a bar when `mic::is_recording()` (empty at the noise floor, red when
  pinned). The windowed median is deliberate: a plain DC-removed AC RMS let the known per-chunk
  capture artifact below dominate the bar and swing it full/empty at idle. `src/screen.rs`'s
  `LEVEL_METER_FULL_SCALE` is calibrated against the windowed-median statistic's real measured
  range on four confirmed-good real captures (independently verified transcribable by both
  openai-whisper and whisper.cpp) rather than a synthetic tone: the original 512 was sized against
  `ac_level_ignores_a_spike_confined_to_one_window`'s 100%-duty-cycle `loud` test fixture, which
  reads far higher than real speech ever does at the same peak amplitude (real speech has silence
  between words/syllables, so most 25ms windows sit well under a sustained tone's RMS) - that
  mismatch, not the underlying statistic, is why the meter looked dead on real hardware despite
  carrying real, transcribable audio. Re-derive the constant (see the doc comment on
  `LEVEL_METER_FULL_SCALE`) if the windowed-median formula in `pcm_extract.rs` ever changes.
  The meter is computed on `ptt_upload_task` from the chunks it has already drained from the
  ring (`mic::meter_level`), NOT in `capture_task`. That split was originally forced by the PIO RX
  FIFO's depth (4 words, `pico-sdk` `hardware/pio.h`): with the old one-shot capture DMA, *any*
  per-chunk work between transfers could starve the FIFO and silence capture, which was a
  hardware-confirmed regression - `e9eebf4` (pre-meter) captured real speech, and `f3be4d4` (which
  adds only the level meter) is silent/crickets - so the meter was moved off the hot path.
  `capture_task` now only deposits samples in the ring; the meter is computed from them by the
  upload task, off the capture path.
  THE FIFO STALL IS NOW A SOLVED DEFECT, not a budget to respect: the capture DMA copies the RX
  FIFO into `DMA_RING` in the RP2350's endless transfer mode with the write address wrapped on the
  ring, so it drains the FIFO in hardware and the state machine never stalls between transfers; a
  task only has to poll that ring every 5 ms (64 ms of slack) instead of re-arming a transfer
  within ~250 us. The per-chunk plateau-plus-glitch artifact recorded below was this same stall -
  the FIFO filling while the next transfer was armed, stopping the bit clock - and the same stall
  was what silenced capture for the whole utterance when an SSH session kept the CPU busy: the
  hardware log showed the microphone's data line reading zero for ~90% of an in-session recording
  (~99% non-zero locally) with the transfers arriving ~15 ms late. `capture_task` now owns no
  per-chunk timing requirement at all; the remaining bound is that a poll must arrive inside the
  ring's 64 ms, after which the DMA laps the reader and the audio overwritten is dropped with a
  `ptt: capture ring overran, dropping some audio` line (see `DMA_OVERRUN_NOTICE_GEN`).
  Confirmed on the captain's hardware: with the ring DMA an SSH-session recording moves the level
  meter and whisper transcribes it correctly into the tmux session, where the same build with the
  one-shot transfer was silent.
  A FORMERLY OPEN ARTIFACT, NOW EXPLAINED (found while investigating the meter): every 400-sample
  chunk (one 25 ms DMA transfer) contained ~6 zero samples plus a 1-2 sample glitch (up to
  ~+/-12600 counts) at a stepping offset, with >50% of the chunk a flat plateau; the glitch carried
  ~91% of the chunk's AC energy. That is the RX FIFO stalling between one-shot DMA transfers - the
  state machine stops on `in` while the FIFO is full, so the microphone's bit clock hiccups - and
  the free-running ring DMA described above removes the stall rather than working around it. (The
  old log of the same defect, for reference: in `/ai/ptt-test-4.raw` the peak-deviation index was
  ~7 in the first chunk, ~12 for the next few, then ~59-60 for the rest of the capture - it steps
  once and stays put rather than drifting smoothly.) The artifact was present in the captured
  samples, so it corrupted streamed audio too, not just the meter.
  The earlier "the mic appears to be converting" correction was itself too generous: the
  "~-35 dBFS RMS after DC removal" figure it rested on is glitch-dominated (that chunk-wide
  statistic is ~91% the per-chunk glitch above), and re-analysis of `/ai/ptt-test-4.raw` with the
  glitch excluded puts the windowed level at zero for essentially every chunk (874/874) - that
  capture is a flat/silent negative control, not evidence of signal on its own. The decisive test
  this called for - one fresh capture containing deliberate speech, analysed with the
  glitch-excluded windowed level - has since been passed: the four real captures used to
  calibrate `LEVEL_METER_FULL_SCALE` above are independently verified transcribable by both
  openai-whisper and whisper.cpp, so the mic's audio path is proven to carry real speech for
  those captures. An
  earlier fixed 16-bit slot (a mirror of embassy's
  `PioI2sOut` DAC example's own bit depth, which targets ordinary 16-bit-slot I2S DACs, not this
  mic) clocked 512 kHz - exactly half - and produced a dead line on real hardware (`ppt-test3.raw`:
  every raw word `0x00000000`), confirming this mic will not run below 1.024 MHz. **Still
  unverified without hardware**: the mic's documented rising-edge (non-standard) data-change timing
  versus which BCLK edge this PIO program's `in pins, 1` actually samples on - the RP2040/2350
  datasheet does not document PIO's internal input/output pipeline timing at all (confirmed via
  `raspberrypi/pico-feedback#280`), and this program's BCLK period is only 2 PIO cycles,
  comparable to or shorter than that undocumented pipeline delay. `ptt_edge=1` is the documented
  experiment for that (it inverts the low/bit-clock bit of every side-set value, shifting sampling
  by half a BCLK cycle without changing the loop shape). Capture is 16-bit mono in ~25 ms chunks
  at the default 16 kHz rate (a non-default `ptt_rate` rescales chunk and ring duration, not
  their sample counts), staged between the capture and upload tasks in a fixed 2048-sample
  (`i16`) static ring buffer in `.bss` (`terminal_model::audio_ring::AudioRing`), not on the
  heap, so it does not compete with the `DualHeap` budget and cannot exhaust it; upstream of that,
  the DMA writes into `src/mic.rs`'s own `DMA_RING` and the capture task copies out of it, so
  neither buffer's consumer can hold up the I2S clock. The ring never
  blocks and never grows; when it is full the oldest samples are dropped, logging a single
  `ptt: ...` line per recording, so a congested or stalled SSH session degrades to bounded audio
  loss rather than a stalled I2S clock or a heap-exhaustion abort, independent of whether a PSRAM
  heap tier is present. The ring is guarded by an `embassy_sync` mutex, deliberately not a
  lock-free structure, per `heap.rs`'s CAS-vs-PSRAM `FIXME`. Each sample is tagged with the
  utterance generation that produced it (`CURRENT_GEN`/`ENDED_GEN` atomic counters in `mic.rs`,
  with the pure `utterance_ended` predicate in `audio_ring.rs`), so a recording that starts while
  a previous one is still being uploaded shares the ring without its audio being sent as the older
  utterance's or its own end being consumed by it; the single upload task serves generations in
  order. Button binding is
  plain `Key::F1`. Arming and stopping are independently gated: arming requires `KeyState::Pressed`
  with `Modifiers::NONE` (so Ctrl+F1 still reaches the existing reboot shortcut), while stopping
  fires on `KeyState::Released` whenever `mic::is_recording()` (reusing `mic.rs`'s `RECORDING`
  flag) is set, with no modifier re-check, so a release always ends the recording even if a
  modifier went down mid-hold. The decision table itself lives in
  `terminal-model/src/key_dispatch.rs`'s `ptt_action` (host-tested with
  `cargo test -p terminal-model --target x86_64-unknown-linux-gnu key_dispatch`) and `src/keyboard.rs`
  is only the I2C/`KeyReport`-to-`ptt_action` adapter, so rebinding means changing the single
  `Key::F1` check passed to `ptt_action` there and updating that module's tests.
  `capture_task` also self-stops after `MAX_RECORDING_DURATION` (60s) in case the keyboard
  link drops the `Released` report entirely. `Key::ButtonLeft2`, tried first, turned out to
  correspond to no physical control on real hardware - the PicoCalc has one D-pad and no joystick,
  and `ButtonLeft2` belongs to a `Joy*`/`Button*` group of raw keyboard-protocol codes
  (`src/keyboard.rs`'s `Key` enum and its `From<u8>` impl) that looks like it comes from a
  joystick/gamepad-bearing variant of this same keyboard co-processor protocol, not this device -
  treat that whole code group as suspect for any future key binding on this hardware. That is the
  whole destination story now: the SSH session's audio channel and nothing else. `mic.rs`'s
  `start_recording` asks `net::ssh_audio_available()` when the button is pressed and, when it is
  false, shows `no ssh session: not recording` - or `ptt unavailable: the session's audio channel
  is not running` when `net::ssh_session_active()` says a session *is* up - through
  `Screen::show_notice`, and starts no capture, no PIO clock and no ring traffic. A push-to-talk
  failure must never cost the user their terminal: the helper exits on its own when the server has
  no `python3` or whisper, and `CliEvent::SessionExit` carries no channel number, so the ticker
  decides by the *terminal* channel's lifetime (`TERMINAL_OPEN`): while the terminal is open an exit
  event may be the audio channel's own exit-status message (which arrives ahead of that channel's
  EOF), so the session carries on with push-to-talk unavailable - `pump_audio`'s own channel read
  independently reports that closure via `ptt_note`, and `ssh_session_active()` lets `mic.rs` say
  "ptt unavailable" rather than "no ssh session" - and once the terminal channel is closed nothing
  else can be left, so the ticker ends the session there (the terminal's own EOF ends it too, by
  completing `spawn_session_future`). Deciding by the *audio* branch's liveness instead is a trap:
  its guard clears as soon as `pump_audio` returns, which can happen before the ticker consumes the
  event, and then the ticker reads a dead helper as a dead terminal and drops the session - the
  exact bug reproduced with a host harness (`sunset-embassy` against a real sshd whose helper
  command fails) and fixed by using the terminal's state. For the same reason push-to-talk diagnostics never
  `print!` into a live session's screen - that screen *is* the terminal, so a diagnostic line
  would corrupt the remote output and any transcript being typed into tmux. They go through
  `net::ptt_note` (host-visible): `log::warn!` while a session is up, the device's `print!`
  console otherwise; the only thing drawn on the screen during a session is the transient
  `Screen::show_notice` overlay, which is paint-time-only and never touches the terminal buffer
  (see the overlay note above). There used to be
  a raw-TCP sink (`ptt_host`/`ptt_port`, one connection per utterance) from the capture-only
  stage; it was removed on the captain's call as a stepping stone nobody would use.
- Push-to-talk rides the *existing* SSH session (`src/net.rs`'s `ssh_audio_branch`,
  `ssh_audio_send`, `pump_audio`). The audio goes over a **second SSH
  channel of the same connection** - no new connection, no listening port anywhere, no extra
  credential - which `exec`s the server-side helper. The helper lives in this repo at
  `tools/picocalc-ptt` (Python 3 stdlib only, tests in `tools/test_picocalc_ptt.py`, run with
  `python3 tools/test_picocalc_ptt.py`; nothing else in the repo is Python) and is **embedded in
  the firmware** (`include_bytes!`, `src/net.rs`'s `PTT_HELPER_SCRIPT`), so the server needs only
  `python3` and whisper: at session start the device streams the script down the audio channel
  (`AudioCommand::Embedded`, streamed in `PTT_SCRIPT_CHUNK_BYTES` writes) and the exec'd command
  (`terminal_model::ptt_frame::helper_exec_command`, host-tested) copies exactly that many bytes
  into `$TMPDIR/picocalc-ptt.<pid>` with `dd bs=1` and execs python on it, which leaves the
  channel on python's stdin for the frames that follow - `dd`'s byte-at-a-time reads are what keep
  the script/audio boundary exact. A user-supplied `config set ptt_ssh_cmd <command>` still runs
  instead (nothing is streamed then), and an empty value disables the transport. Its protocol is
  documented in that file's docstring and in `terminal_model/src/ptt_frame.rs`; one helper process
  runs per SSH session and transcribes each utterance on the device's zero-length end marker,
  typing the text into tmux (`tmux send-keys -l`). The tmux hand-off is the one part of the chain
  whose environment the SSH channel does not share with the interactive login: the exec'd helper
  has no `TMUX`/`TMUX_TMPDIR` from the login, so a server started under a non-default socket
  directory (the systemd runtime dir, e.g. `/run/user/1003/tmux-1003/default`, is the common case;
  tmux 3.6a derives `<base>/tmux-<uid>/default` from `TMUX_TMPDIR` then `TMPDIR` then `/tmp`, and
  ignores `XDG_RUNTIME_DIR` itself) is invisible to a bare `tmux` call - the captain hit exactly
  this, with `tmux send-keys failed: error connecting to /tmp/tmux-1003/default`. The helper now
  probes the candidates (`$TMUX`, then each of `TMUX_TMPDIR`/`TMPDIR`/`XDG_RUNTIME_DIR`/`/tmp` as
  a base) with `tmux -S <path> list-sessions` at startup, uses the first that answers (passing no
  `-S` when that is tmux's own default), takes an explicit `tmux_socket`/`--tmux-socket`/
  `$PICOCALC_PTT_TMUX_SOCKET` over all of it, and reports the socket it tried plus the
  `tmux display-message -p '#{socket_path}'` hint whenever a tmux call fails. `tools/test_picocalc_ptt.py`
  covers all three paths, and its `FakeCommands` scrubs the tmux environment so the machine running
  the tests cannot influence them. The device-side half of setting this up has its own sharp edges
  (`config get ptt_ssh_cmd` is the authoritative readout
  (`src/config.rs` special-cases it to print the effective command, `(built-in helper)` when
  nothing is configured, or `(disabled)` for an empty stored value), while `config list` only dumps
  the stored 32-entry map and never resolves this key, so it does not appear there at all at its
  default; the command is read **once per SSH session**
  (`effective_ssh_audio_command` at session start), so a change needs a reconnect; `src/process.rs`
  splits the console line on single spaces with no quote handling, so the value is set unquoted
  (`config set ptt_ssh_cmd TMUX_TMPDIR=/run/user/1003 picocalc-ptt`) and quote characters would be
  stored literally and break the remote shell; and stored values are `FixedString<128>`
  (`src/config.rs`).
  Three sunset (the SSH stack) sharp edges shaped that design and must stay in mind for any future
  channel work: (1) `Channels::open` only reuses a slot that is `None`, and nothing frees a
  client-side channel slot after `channel_done`, so with `MAX_CHANNELS = 4` a client can open only
  a few channels per connection - hence *one* audio channel per session, not one per utterance,
  which is also why the end-of-utterance marker exists at all; (2) `CliEvent::SessionExit` carries
  no channel number, so with two session channels an exit event cannot be attributed - the ticker
  therefore only ends the session on it when `TERMINAL_OPEN` is false (the interactive channel is
  already gone), and otherwise relies on `pump_audio`'s own channel read to report a missing or
  crashing helper (via `ptt_note`) and leave the rest of that utterance unsent instead of tearing
  the user's terminal down. `ssh_audio_branch`
  is an arm of the session's `select` and must never return while the
  session lives: when the audio channel dies it drains `AUDIO_QUEUE` forever instead. (3)
  `PTY_READY`, `AUDIO_EXEC_SENT`, `TERMINAL_OPEN`, and `AUDIO_QUEUE` are
  module-level statics shared across every `ssh_session_task` invocation, and `embassy_sync::Signal`
  keeps a signaled value queued until consumed - so `ssh_session_task` calls
  `reset_audio_session_state()` (`src/net.rs`) once per session, before the audio branch/select
  starts, to clear any of them left over from the previous session; any future per-session channel
  state added here needs the same reset.
  The handoff is a fixed `Channel<CS, AudioFrame, 3>` of encoded frames (never heap):
  `ssh_audio_available()` is set only after the ticker has sent the helper's `exec` request (so
  audio can never be written before the process exists) and is cleared by `AudioReadyGuard` when
  the branch stops pumping; a frame that cannot be queued within `AUDIO_SEND_TIMEOUT` (1 s) makes
  `ssh_audio_send` return false, and the recording's drain loop then drops the rest of that
  utterance while still metering it.
- `src/psram.rs` only drives PSRAM over the RP2350's QMI/XIP hardware path (`init_psram_qmi`) now.
  It used to also have a PIO-driven "slow path" (its own `PsRam` struct, claiming PIO1, DMA_CH1,
  DMA_CH2, and `PIN_2`/`PIN_3`/`PIN_20`/`PIN_21`) as a fallback/self-test, but that path's detected
  size was never fed to the heap allocator — only `init_qmi_psram_heap` (driven by
  `init_psram_qmi`'s result) does that — so it was dropped as dead weight, freeing PIO1,
  DMA_CH1/CH2, and those pins (see the mic note above for where `PIN_2`/`PIN_3`/`PIN_21` went).
  `PIN_20` (`RAM_CS`) is explicitly held deselected in `main.rs` (`Output::new(p.PIN_20,
  Level::High)`, bound for `main`'s whole lifetime) since nothing drives it as PSRAM chip-select
  anymore and it must not float. This is safe regardless of the QMI path's own state: QMI/XIP uses
  a separate, RP2350-internal CS pad (`detect_psram_qmi`'s `XIP_CS_PIN`), never `PIN_20`, so
  deselecting `PIN_20` cannot interfere with QMI PSRAM access.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
