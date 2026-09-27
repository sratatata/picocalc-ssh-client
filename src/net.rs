use crate::Irqs;
use crate::config::{CONFIG, StrValue};
use crate::keyboard::{Key, KeyReport, Modifiers};
use crate::net::alloc::string::ToString;
use crate::process::{LineEditor, Process, assign_proc, assign_proc_if, erase_prompt_line};
use crate::rng::PicoRng;
use crate::screen::{SCREEN, SCREEN_HEIGHT, SCREEN_WIDTH, Screen};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use cyw43::Control;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_futures::select::*;
use embassy_net::dns::{DnsQueryType, DnsSocket};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Stack};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write as _};
use rand_core::RngCore;
use static_cell::StaticCell;
use sunset::{CliEvent, SessionCommand, SignKey};
use sunset_embassy::{ChanIn, ChanInOut, ProgressHolder, SSHClient};
use terminal_model::ptt_frame;

extern crate alloc;

type CS = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

static WIFI_CONTROL: LazyLock<Mutex<CriticalSectionRawMutex, Option<Control<'static>>>> =
    LazyLock::new(|| Mutex::new(None));
static STACK: LazyLock<Mutex<CriticalSectionRawMutex, Option<Stack<'static>>>> =
    LazyLock::new(|| Mutex::new(None));

#[embassy_executor::task]
pub async fn run_cyw43(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_runner(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

pub async fn setup_wifi(
    spawner: &Spawner,
    pin_23: embassy_rp::peripherals::PIN_23, // WL_ON
    pin_24: embassy_rp::peripherals::PIN_24, // WL_D
    pin_25: embassy_rp::peripherals::PIN_25, // WL_CS
    pin_29: embassy_rp::peripherals::PIN_29, // WL_CLK
    pio_0: embassy_rp::peripherals::PIO0,
    dma_ch0: embassy_rp::peripherals::DMA_CH0,
) {
    let fw = include_bytes!("../embassy/cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../embassy/cyw43-firmware/43439A0_clm.bin");

    // Wireless background task:
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) = {
        let state = STATE.init(cyw43::State::new());
        let wireless_enable = Output::new(pin_23, Level::Low);
        let wireless_spi = {
            let cs = Output::new(pin_25, Level::High);
            let mut pio = Pio::new(pio_0, Irqs);
            PioSpi::new(
                &mut pio.common,
                pio.sm0,
                RM2_CLOCK_DIVIDER,
                pio.irq0,
                cs,
                pin_24,
                pin_29,
                dma_ch0,
            )
        };
        cyw43::new(state, wireless_enable, wireless_spi, fw).await
    };

    spawner.must_spawn(run_cyw43(runner));
    control.init(clm).await;
    use embassy_net::StackResources;
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();

    let config = embassy_net::Config::dhcpv4(Default::default());
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        PicoRng.next_u64(),
    );
    spawner.must_spawn(net_runner(runner));

    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    let (ssid, wifi_pw) = {
        let mut config = CONFIG.get().lock().await;
        let ssid = config.fetch("wifi_ssid").await;
        let wifi_pw = config.fetch("wifi_pw").await;
        (ssid, wifi_pw)
    };
    match (ssid, wifi_pw) {
        (Ok(Some(ssid)), Ok(Some(wifi_pw))) => {
            if !ssid.is_empty() {
                print!("Connecting to \u{1b}[1m{ssid}\u{1b}[0m...\r\n");
                const JOIN_ATTEMPTS: u32 = 5;
                const JOIN_RETRY_DELAY: Duration = Duration::from_millis(500);
                let mut last_err = None;
                for attempt in 1..=JOIN_ATTEMPTS {
                    match control
                        .join(&ssid, cyw43::JoinOptions::new(wifi_pw.as_bytes()))
                        .await
                    {
                        Ok(_) => {
                            last_err = None;
                            break;
                        }
                        Err(err) => {
                            log::warn!(
                                "join attempt {attempt}/{JOIN_ATTEMPTS} failed with status={}",
                                err.status
                            );
                            last_err = Some(err);
                            if attempt < JOIN_ATTEMPTS {
                                Timer::after(JOIN_RETRY_DELAY).await;
                            }
                        }
                    }
                }
                if let Some(err) = last_err {
                    log::error!(
                        "join failed with status={} after {JOIN_ATTEMPTS} attempts",
                        err.status
                    );
                    print!("Failed with status {}\r\n", err.status);
                }
            }
        }
        _ => {
            print!("wifi_ssid and/or wifi_pw are not set\r\n");
        }
    }
    WIFI_CONTROL.get().lock().await.replace(control);

    log::info!("waiting for TCP to be up...");
    const CONFIG_UP_TIMEOUT: Duration = Duration::from_secs(30);
    match with_timeout(CONFIG_UP_TIMEOUT, stack.wait_config_up()).await {
        Ok(()) => log::info!("Stack is up!"),
        Err(_) => {
            log::error!("timed out waiting for network config to come up");
            print!("Failed to bring up network (config timed out)\r\n");
        }
    }
    if let Some(v4) = stack.config_v4() {
        log::info!("{v4:?}");
        print!("IP Address {}\r\n", v4.address);
    }

    spawner.must_spawn(crate::time::time_sync(stack));
    STACK.get().lock().await.replace(stack);
}

const TIMEOUT_DURATION: Duration = Duration::from_secs(10);

async fn send_key_bytes(channel: &mut ChanInOut<'_, '_>, bytes: &[u8]) {
    log::info!(
        "{:?}",
        with_timeout(TIMEOUT_DURATION, channel.write_all(bytes)).await
    );
}

async fn ssh_channel_task(mut channel: ChanInOut<'_, '_>, key_rx: Arc<Channel<CS, KeyReport, 4>>) {
    log::info!("ssh_channel_task waiting for output");

    loop {
        let mut buf = [0u8; 1024];

        let output = channel.read(&mut buf);
        let input = key_rx.receive();

        match select(output, input).await {
            Either::First(read_result) => match read_result {
                Ok(n) => {
                    if n == 0 {
                        log::warn!("ssh_channel_task: EOF on ssh channel");
                        return;
                    }
                    SCREEN.get().lock().await.parse_bytes(&buf[0..n]);
                }
                Err(err) => {
                    print!("\u{1b}[1mssh_channel_task: {err:?}\r\n");
                    return;
                }
            },
            Either::Second(key_report) => {
                // Encode a key with xterm style keyboard encoding.
                // FIXME: woefully incomplete!

                if key_report.modifiers == Modifiers::CTRL
                    && let Key::Char(c) = key_report.key
                    && let Some(mapped) = ctrl_mapping(c)
                {
                    log::info!(
                        "doing mapped ctrl {} -> {}",
                        c.escape_debug(),
                        mapped.escape_debug()
                    );
                    let mut buf = [0u8; 4];
                    send_key_bytes(&mut channel, mapped.encode_utf8(&mut buf).as_bytes()).await;
                    continue;
                }

                if key_report.modifiers == Modifiers::ALT {
                    // Alt sends escape first
                    log::info!("ALT -> send escape first");
                    send_key_bytes(&mut channel, b"\x1b").await;
                }

                if let Key::Char(c) = key_report.key {
                    let mut buf = [0u8; 4];
                    log::info!("just sending {} as-is", c.escape_debug());
                    send_key_bytes(&mut channel, c.encode_utf8(&mut buf).as_bytes()).await;
                } else {
                    let text = match key_report.key {
                        Key::Enter => "\r",
                        Key::BackSpace => "\u{7f}",
                        Key::Tab => "\t",
                        Key::Escape => "\u{1b}",
                        Key::Up => "\u{1b}[A",
                        Key::Down => "\u{1b}[B",
                        Key::Right => "\u{1b}[C",
                        Key::Left => "\u{1b}[D",
                        Key::Home => "\u{1b}[H",
                        Key::End => "\u{1b}[F",
                        Key::PageUp => "\u{1b}[5~",
                        Key::PageDown => "\u{1b}[6~",
                        Key::None | Key::Char(_) => continue,
                        _ => {
                            continue;
                        }
                    };
                    log::info!("{key_report:?} -> {}", text.escape_debug());
                    send_key_bytes(&mut channel, text.as_bytes()).await;
                }
            }
        }
    }
}

/// Config key holding the command the SSH server runs to receive push-to-talk
/// audio, i.e. `config set ptt_ssh_cmd "..."`.
pub const PTT_SSH_CMD_KEY: &str = "ptt_ssh_cmd";
/// The server-side helper this firmware carries with it: `tools/picocalc-ptt`
/// as bytes in flash, streamed down the audio channel on every session so a
/// server needs only `python3` and whisper - nothing pre-installed, no copy
/// step (see [`AudioCommand`]).
const PTT_HELPER_SCRIPT: &[u8] = include_bytes!("../tools/picocalc-ptt");
/// How much of the helper script to hand the channel per write. Small enough
/// to stay out of the session's way while it is being delivered, large enough
/// that a session start costs a few hundred writes rather than thousands.
const PTT_SCRIPT_CHUNK_BYTES: usize = 1024;
/// How long a frame may wait for room in `AUDIO_QUEUE` before the upload task
/// gives up on the SSH sink for the rest of that recording. The queue holds
/// three frames (~75 ms of audio at the default rate) and the channel drains
/// far faster than capture fills it, so this only trips when the session is
/// stalled or its helper has stopped reading.
const AUDIO_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// Depth of `AUDIO_QUEUE`, in frames: enough to absorb scheduling jitter and to
/// have audio ready while the channel is written, shallow enough to stay a few
/// KiB of `.bss` (see AGENTS.md's stack-headroom note).
const AUDIO_QUEUE_DEPTH: usize = 3;

/// One encoded push-to-talk wire frame (`terminal_model::ptt_frame`) waiting
/// for the session's audio channel. Fixed size, so the queue is a plain
/// `static`: it never allocates, never grows, and a stalled channel costs
/// bounded memory rather than heap exhaustion.
struct AudioFrame {
    len: u16,
    bytes: [u8; crate::mic::WIRE_FRAME_BYTES],
}

/// Frames captured audio is waiting to hand to the session's audio channel.
static AUDIO_QUEUE: Channel<CS, AudioFrame, AUDIO_QUEUE_DEPTH> = Channel::new();
/// Whether the current SSH session can carry push-to-talk audio: its audio
/// channel is open and the server has been asked to run the helper on it. Set
/// by the audio branch once the ticker has sent the helper's `exec` request,
/// cleared when that branch stops pumping, so it is never true without a branch
/// draining `AUDIO_QUEUE`.
static AUDIO_READY: AtomicBool = AtomicBool::new(false);
/// Signalled once the interactive terminal channel has been confirmed and its
/// `pty`/`shell` request sent, which is what makes "the next session-open event
/// is the audio channel's" true (see `ssh_audio_branch`).
static PTY_READY: Signal<CS, ()> = Signal::new();
/// Whether the session's interactive terminal channel is still open. Set when
/// its `pty`/`shell` request has been sent, cleared when `ssh_channel_task`
/// returns (its EOF) or the session ends. It answers two questions:
/// `ssh_session_active()` reports it to tell "no session" apart from "session
/// is up but push-to-talk may not be", and it is what decides whether a
/// channel exit event may end the session (see the `SessionExit` arm).
///
/// Deliberately *not* a statement about the audio channel: a channel's
/// exit-status request arrives before that channel closes, and the audio
/// branch's own liveness flips as soon as its pump returns - which can happen
/// before the ticker gets around to consuming the event. Deciding by the
/// terminal's lifetime instead is free of that race, and the audio branch
/// reports its own channel's closure when it happens.
static TERMINAL_OPEN: AtomicBool = AtomicBool::new(false);
/// Signalled by the ticker with the result of the audio channel's `exec`
/// request. The audio branch starts pumping only after that, so audio data can
/// never reach the server before the helper is running.
static AUDIO_EXEC_SENT: Signal<CS, bool> = Signal::new();

/// Whether an SSH session is up at all, regardless of whether its audio
/// channel works. `crate::mic` uses this to tell "there is no session to send
/// audio over" apart from "the session is fine but push-to-talk is not
/// available", which are different things to say to the user.
pub fn ssh_session_active() -> bool {
    TERMINAL_OPEN.load(Ordering::Acquire)
}

/// Report a push-to-talk diagnostic. While an SSH session is up the screen *is*
/// that session's terminal: writing this text into it would corrupt or scroll
/// the remote output, including the transcript the helper is typing into tmux,
/// so the message goes to the log instead. With no session the local console
/// is the only place the user can see it, so it is printed there.
pub async fn ptt_note(message: &str) {
    if ssh_session_active() {
        log::warn!("ptt: {message}");
    } else {
        print!("ptt: {message}\r\n");
    }
}

/// Whether push-to-talk can send audio right now, i.e. whether the session's
/// audio channel is open and the server has been asked to run the helper.
/// `crate::mic` asks this when the button is pressed: without it there is
/// nowhere for a recording to go, so the device says so instead of recording.
///
/// The session is the only transport: it is already authenticated and
/// encrypted, needs no listener anywhere new, and is the only way to hand the
/// audio to a process on the machine the user is typing into (see README.md's
/// push-to-talk section).
pub fn ssh_audio_available() -> bool {
    AUDIO_READY.load(Ordering::Acquire)
}

/// Queues one captured chunk of samples as a wire frame for the session's
/// audio channel, returning `false` if it could not be queued (no session, a
/// chunk too large for one frame, or a channel that stalled for
/// `AUDIO_SEND_TIMEOUT`), which tells the caller to stop sending this
/// recording. Never blocks for longer than that timeout, so a wedged session
/// cannot stall a recording's drain loop.
pub async fn ssh_audio_send(samples: &[i16]) -> bool {
    queue_audio_frame(samples).await
}

/// Marks the end of one push-to-talk utterance on the session's audio channel:
/// a zero-length frame (see `terminal_model::ptt_frame` for why the marker is
/// in band). Returns `false` if the marker could not be queued, leaving the
/// utterance unterminated for the server-side helper.
pub async fn ssh_audio_end_of_utterance() -> bool {
    queue_audio_frame(&[]).await
}

/// Encodes `samples` as one wire frame - an empty slice for the
/// end-of-utterance marker - and queues it for the session's audio channel,
/// bounded by `AUDIO_SEND_TIMEOUT`.
async fn queue_audio_frame(samples: &[i16]) -> bool {
    if !ssh_audio_available() {
        return false;
    }
    // Encoding straight into the message keeps every buffer in the queue's own
    // fixed storage: no temporary, no heap.
    let mut frame = AudioFrame {
        len: 0,
        bytes: [0; crate::mic::WIRE_FRAME_BYTES],
    };
    let Some(len) = ptt_frame::encode_frame(&mut frame.bytes, samples) else {
        log::error!("ptt: a capture chunk does not fit one push-to-talk wire frame");
        return false;
    };
    frame.len = len as u16;
    match with_timeout(AUDIO_SEND_TIMEOUT, AUDIO_QUEUE.send(frame)).await {
        Ok(()) => true,
        Err(_) => {
            log::warn!("ptt: ssh audio channel is not draining");
            false
        }
    }
}

/// Clears any `PTY_READY`/`AUDIO_EXEC_SENT`/`TERMINAL_OPEN`/`AUDIO_QUEUE` state
/// left over from a previous `ssh_session_task` invocation. Those are module-level statics shared between this task and
/// `ssh_audio_branch`/`pump_audio`, and a `Signal` keeps a signaled value
/// until it is consumed - so without this, a session whose `ptt_ssh_cmd` was
/// empty (whose audio branch never waits on `PTY_READY`) can leave a stale
/// `PTY_READY` signal for the *next* session's audio branch to consume
/// immediately, before that session's own interactive channel opens,
/// misattributing which channel is the terminal and which is the audio
/// helper. Must run once per session, before the audio branch/select starts.
fn reset_audio_session_state() {
    PTY_READY.reset();
    AUDIO_EXEC_SENT.reset();
    TERMINAL_OPEN.store(false, Ordering::Release);
    while AUDIO_QUEUE.try_receive().is_ok() {}
}

/// Clears the session's audio availability when its audio branch stops
/// pumping - the helper exited, the server closed the channel, or the whole
/// session is being torn down - so nothing can queue frames for a channel
/// nobody is draining.
struct AudioReadyGuard;

impl Drop for AudioReadyGuard {
    fn drop(&mut self) {
        AUDIO_READY.store(false, Ordering::Release);
    }
}

/// The command `config set ptt_ssh_cmd` selects the session's audio helper
/// with, or `None` when it is set to an empty value, which disables the SSH
/// audio channel (recordings then never start). Reported by
/// `config get` as well, so the console shows what a new session would run
/// rather than the raw store slot.
pub async fn effective_ssh_audio_command() -> Option<AudioCommand> {
    let stored = CONFIG
        .get()
        .lock()
        .await
        .fetch(PTT_SSH_CMD_KEY)
        .await
        .ok()
        .flatten();
    match stored.as_ref().map(|value| value.as_str().trim()) {
        // An empty stored value disables the transport.
        Some("") => None,
        // An explicit command is run as-is; the user has installed whatever it
        // needs on the server.
        Some(command) => Some(AudioCommand::Configured(command.to_string())),
        // Nothing configured: run the helper this firmware carries.
        None => Some(AudioCommand::Embedded),
    }
}

/// The command that receives the embedded helper script: create a private file
/// under `$TMPDIR` (or `/tmp`), copy exactly `script_bytes` bytes off the
/// channel into it with `dd bs=1` - one byte per read, so the audio frames that
/// follow stay on the channel for python to read - and exec the helper. The
/// installed name carries the shell's pid so two sessions cannot collide.
/// The exact string is host-tested in `terminal_model::ptt_frame`.
fn embedded_helper_command(script_bytes: usize) -> alloc::string::String {
    terminal_model::ptt_frame::helper_exec_command(script_bytes)
}

/// What the session's audio channel runs, resolved once per session.
///
/// The default is the helper script embedded in the firmware: the device
/// streams it down the channel right after the `exec`, the command writes it to
/// a private file and runs it with `python3`, and the audio frames follow on
/// the same channel. That means the server needs no copy of `picocalc-ptt` on
/// its `PATH` - only `python3` and whisper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioCommand {
    /// The embedded helper, delivered over the channel.
    Embedded,
    /// A `ptt_ssh_cmd` the user set, run as-is.
    Configured(alloc::string::String),
}

impl AudioCommand {
    /// Whether the helper script is streamed down the channel after the `exec`
    /// request (and therefore whether the command expects it).
    pub fn sends_script(&self) -> bool {
        matches!(self, Self::Embedded)
    }

    /// The command string the server's shell runs.
    fn exec_string(&self) -> alloc::string::String {
        match self {
            Self::Configured(command) => command.clone(),
            Self::Embedded => crate::net::embedded_helper_command(PTT_HELPER_SCRIPT.len()),
        }
    }

    /// What `config get ptt_ssh_cmd` and the log report for this destination.
    pub fn describe(&self) -> alloc::string::String {
        match self {
            Self::Configured(command) => command.clone(),
            // The generated command is long and uninteresting; the console says
            // where the helper comes from instead.
            Self::Embedded => alloc::string::String::from("(built-in helper)"),
        }
    }
}

/// Opens and serves the SSH session's push-to-talk audio channel for as long
/// as the session lasts.
///
/// This is the session's *second* channel: the interactive terminal opened by
/// `spawn_session_future` is the first, and the ticker attributes later
/// session-open events to this branch, so it waits for `PTY_READY` before
/// asking for one. On its channel it asks the server to `exec` the configured
/// helper - a second channel of the connection the device already has, not a
/// new connection - and then writes wire frames to it, so audio needs no
/// listening port on the server and no change to its security posture.
///
/// This future must never complete while the session lives: it is one arm of
/// the session's `select`, so completing would end the session. When the audio
/// channel dies (the helper exited, or the server closed it), the branch keeps
/// draining `AUDIO_QUEUE` forever instead, which leaves the session's audio
/// unavailable so later recordings never start.
async fn ssh_audio_branch(ssh_client: &SSHClient<'_>, command: Option<&AudioCommand>) {
    if let Some(command) = command {
        PTY_READY.wait().await;
        if let Some((channel, stderr)) = open_audio_channel(ssh_client, command).await {
            pump_audio(channel, stderr).await;
        }
    }
    // Not returning: this branch is an arm of the session's `select`, and
    // completing would take the whole session down with it. Draining keeps any
    // send already in flight from blocking the upload task until its timeout.
    loop {
        AUDIO_QUEUE.receive().await;
    }
}

/// Opens the session's audio channel and waits for the ticker to have sent the
/// helper's `exec` request on it. Returns `None`, after reporting why, when
/// the channel cannot carry audio - the session then has none, so later
/// recordings never start.
async fn open_audio_channel<'g, 'a>(
    ssh_client: &'g SSHClient<'a>,
    command: &AudioCommand,
) -> Option<(ChanInOut<'g, 'a>, ChanIn<'g, 'a>)> {
    let (channel, stderr) = match ssh_client.open_session_nopty().await {
        Ok(channel) => channel,
        Err(err) => {
            ptt_note(&alloc::format!(
                "could not open the ssh audio channel: {err:?}"
            ))
            .await;
            return None;
        }
    };
    let described = command.describe();
    log::info!("ptt: ssh audio channel opened for `{described}`");
    // The `exec` request is sent by the ticker, when it sees this channel's
    // session-open event; waiting for its result keeps audio from being
    // written before the helper is running, which would only be discarded.
    if !AUDIO_EXEC_SENT.wait().await {
        ptt_note(&alloc::format!(
            "could not start the ssh audio helper `{described}`"
        ))
        .await;
        return None;
    }
    if command.sends_script() {
        // Deliver the embedded helper before any audio: the server-side command
        // is blocked in `dd` waiting for exactly these bytes, and the frames
        // that follow it are the audio it then reads.
        let mut channel = channel;
        let mut sent = 0usize;
        while sent < PTT_HELPER_SCRIPT.len() {
            let end = (sent + PTT_SCRIPT_CHUNK_BYTES).min(PTT_HELPER_SCRIPT.len());
            if let Err(err) = channel.write_all(&PTT_HELPER_SCRIPT[sent..end]).await {
                ptt_note(&alloc::format!(
                    "could not send the ssh audio helper ({err:?})"
                ))
                .await;
                return None;
            }
            sent = end;
        }
        log::info!("ptt: sent the embedded helper ({} bytes)", sent);
        return Some((channel, stderr));
    }
    Some((channel, stderr))
}

/// Writes queued wire frames to the session's audio channel, keeps the helper's
/// stdout drained, and forwards its stderr to the console, until the channel
/// closes.
async fn pump_audio(mut channel: ChanInOut<'_, '_>, mut stderr: ChanIn<'_, '_>) {
    // From here recordings queue frames for this channel; the guard makes
    // that untrue again the moment this pump stops.
    AUDIO_READY.store(true, Ordering::Release);
    let _ready = AudioReadyGuard;

    // The channel's receive window is small, so anything the helper writes must
    // be read or the helper blocks on it and stops reading audio. stderr is
    // shown (it is the helper's diagnostic channel); stdout is only drained,
    // because that is what a wrapped whisper command tends to print to.
    let mut err_buf = [0u8; 96];
    let mut out_buf = [0u8; 96];
    let mut stderr_done = false;
    loop {
        let read_stderr = async {
            if stderr_done {
                core::future::pending::<Result<usize, sunset::Error>>().await
            } else {
                stderr.read(&mut err_buf).await
            }
        };
        match select3(
            AUDIO_QUEUE.receive(),
            channel.read(&mut out_buf),
            read_stderr,
        )
        .await
        {
            Either3::First(frame) => {
                if let Err(err) = channel.write_all(&frame.bytes[..frame.len as usize]).await {
                    ptt_note(&alloc::format!(
                        "ssh audio helper stopped reading ({err:?})"
                    ))
                    .await;
                    return;
                }
            }
            Either3::Second(read) => match read {
                // EOF on the helper's stdout means the channel is closing:
                // the helper exited, or could not be started at all (a missing
                // `picocalc-ptt` makes the shell report a failure and exit
                // immediately).
                Ok(0) | Err(_) => {
                    ptt_note("ssh audio helper exited").await;
                    return;
                }
                Ok(_) => {}
            },
            Either3::Third(read) => match read {
                // Either end of the helper's stderr means no more diagnostics
                // from it; the channel itself can still be carrying audio.
                Ok(0) | Err(_) => stderr_done = true,
                Ok(n) => {
                    let text = alloc::string::String::from_utf8_lossy(&err_buf[..n]);
                    ptt_note(text.trim_end()).await;
                }
            },
        }
    }
}

#[embassy_executor::task]
async fn ssh_session_task(
    host: String,
    port: u16,
    username: Option<String>,
    command: Option<String>,
) {
    let Some(stack) = STACK.get().lock().await.as_ref().copied() else {
        print!("network is offline\r\n");
        return;
    };

    let command = command.as_deref();
    let username = username.as_deref();

    let dns_client = DnsSocket::new(stack);

    match dns_client.query(&host, DnsQueryType::A).await {
        Ok(addrs) => {
            log::info!("{host} -> {addrs:?}");
            let mut socket_tx_buf = [0u8; 8192];
            let mut socket_rx_buf = [0u8; 8192];
            let mut tcp_socket = TcpSocket::new(stack, &mut socket_tx_buf, &mut socket_rx_buf);

            match tcp_socket
                .connect(IpEndpoint {
                    addr: addrs[0],
                    port,
                })
                .await
            {
                Ok(()) => {
                    use embassy_futures::select::*;

                    let key_channel = Arc::new(Channel::new());
                    let ssh_proc = Arc::new(SshProcess {
                        key_sender: key_channel.clone(),
                    });
                    let prior_proc = assign_proc(ssh_proc).await;

                    print!("Connected to {host} {}:{port}\r\n", addrs[0]);
                    let (mut read, mut write) = tcp_socket.split();
                    let mut ssh_tx_buf = [0u8; 8192];
                    let mut ssh_rx_buf = [0u8; 8192];
                    let ssh_client = match SSHClient::new(&mut ssh_tx_buf, &mut ssh_rx_buf) {
                        Ok(client) => client,
                        Err(err) => {
                            print!("SSHClient::new: {err:?}\r\n");
                            return;
                        }
                    };

                    let session_authd_chan =
                        embassy_sync::channel::Channel::<NoopRawMutex, bool, 1>::new();
                    let wait_for_auth = session_authd_chan.receiver();

                    let spawn_session_future = async {
                        if wait_for_auth.receive().await {
                            let channel = ssh_client.open_session_pty().await?;
                            ssh_channel_task(channel, key_channel).await;
                            // The terminal channel reached EOF (or failed), so
                            // the session is over as far as this arm is
                            // concerned: stop attributing later exit events to
                            // the audio channel.
                            TERMINAL_OPEN.store(false, Ordering::Release);
                        }
                        Ok::<(), sunset::Error>(())
                    };

                    // Resolved once per session: `config set ptt_ssh_cmd`
                    // changes what the *next* session runs, and `None` (an empty
                    // value) disables the audio channel.
                    let audio_command = effective_ssh_audio_command().await;
                    match &audio_command {
                        Some(command) => {
                            log::info!("ptt: ssh audio helper is `{}`", command.describe())
                        }
                        None => log::info!("ptt: ssh audio disabled ({PTT_SSH_CMD_KEY} is empty)"),
                    }
                    reset_audio_session_state();

                    let runner = ssh_client.run(&mut read, &mut write);
                    let mut progress = ProgressHolder::new();
                    let mut pubkey_tried = false;
                    // The interactive terminal is the first channel this
                    // client opens, so the first session-open event is its PTY
                    // and anything later is the audio branch's channel.
                    let mut pty_opened = false;
                    let ssh_ticker = async {
                        loop {
                            match ssh_client.progress(&mut progress).await {
                                Ok(event) => match event {
                                    CliEvent::Hostkey(k) => {
                                        log::info!("host key {:?}", k.hostkey());
                                        k.accept().expect("accept hostkey");
                                    }
                                    CliEvent::Banner(b) => {
                                        if let Ok(b) = b.banner() {
                                            log::info!("banner: {b}");
                                        }
                                    }
                                    CliEvent::Username(req) => {
                                        match username {
                                            Some(user) => req.username(user),
                                            None => match CONFIG
                                                .get()
                                                .lock()
                                                .await
                                                .fetch("ssh_user")
                                                .await
                                            {
                                                Ok(Some(pw)) => req.username(&pw),
                                                _ => {
                                                    let user = prompt_for_input(
                                                        "login: ",
                                                        PromptKind::Text,
                                                    )
                                                    .await;
                                                    match user {
                                                        Some(user) => req.username(&user),
                                                        None => {
                                                            print!("Cancelled\r\n");
                                                            return Ok(());
                                                        }
                                                    }
                                                }
                                            },
                                        }
                                        .expect("set user");
                                    }
                                    CliEvent::Password(req) => {
                                        match CONFIG.get().lock().await.fetch("ssh_pw").await {
                                            Ok(Some(pw)) => req.password(&pw),
                                            _ => {
                                                let user = prompt_for_input(
                                                    "password: ",
                                                    PromptKind::Password,
                                                )
                                                .await;
                                                match user {
                                                    Some(user) => req.password(&user),
                                                    None => req.skip(),
                                                }
                                            }
                                        }
                                        .expect("set pw");
                                    }
                                    CliEvent::Pubkey(req) => {
                                        let key = if pubkey_tried {
                                            None
                                        } else {
                                            crate::sshkey::load_signing_key().await
                                        };
                                        pubkey_tried = true;
                                        match key {
                                            Some(key) => {
                                                req.pubkey(SignKey::Ed25519(key))
                                                    .expect("set pubkey");
                                            }
                                            None => {
                                                req.skip().expect("skip pubkey");
                                            }
                                        }
                                    }
                                    CliEvent::AgentSign(req) => {
                                        req.skip().expect("skip agentsign");
                                    }
                                    CliEvent::Authenticated => {
                                        log::info!("Authenticated!");
                                        session_authd_chan.sender().send(true).await;
                                    }
                                    CliEvent::SessionOpened(mut s) => {
                                        log::info!("session opened channel {}", s.channel());

                                        if pty_opened {
                                            // The only other channel this
                                            // client asks for is the audio
                                            // branch's: run the configured
                                            // server-side helper on it, and tell
                                            // the branch whether that worked.
                                            let sent = match &audio_command {
                                                Some(cmd) => {
                                                    let exec = cmd.exec_string();
                                                    s.cmd(&SessionCommand::Exec(&exec)).is_ok()
                                                }
                                                None => false,
                                            };
                                            if !sent {
                                                ptt_note("could not start the ssh audio helper")
                                                    .await;
                                            }
                                            AUDIO_EXEC_SENT.signal(sent);
                                            continue;
                                        }
                                        pty_opened = true;

                                        use heapless::{String, Vec};

                                        let mut term = String::<32>::new();
                                        term.push_str("xterm").unwrap();

                                        let pty = {
                                            let screen = SCREEN.get().lock().await;
                                            let rows = screen.height();
                                            let cols = screen.width();

                                            sunset::Pty {
                                                term,
                                                rows: rows.into(),
                                                cols: cols.into(),
                                                width: SCREEN_WIDTH as u32,
                                                height: SCREEN_HEIGHT as u32,
                                                modes: Vec::new(),
                                            }
                                        };

                                        log::info!("requesting pty {pty:?}");
                                        if let Err(err) = s.pty(pty) {
                                            log::error!("requesting pty failed {err:?}");
                                            return Err(err);
                                        }
                                        log::info!("setting command");
                                        match &command {
                                            Some(cmd) => {
                                                if let Err(err) = s.cmd(&SessionCommand::Exec(cmd))
                                                {
                                                    log::error!("command failed: {err:?}");
                                                    return Err(err);
                                                }
                                            }
                                            None => {
                                                if let Err(err) = s.shell() {
                                                    print!("shell failed: {err:?}\r\n");
                                                    return Err(err);
                                                }
                                            }
                                        }
                                        log::info!("SessionOpened completed");
                                        // The terminal channel is up, so the
                                        // audio branch may open the session's
                                        // second channel: the next open event
                                        // can only be its own.
                                        TERMINAL_OPEN.store(true, Ordering::Release);
                                        PTY_READY.signal(());
                                    }
                                    CliEvent::SessionExit(status) => {
                                        // sunset does not say which channel an
                                        // exit event belongs to, and it does
                                        // not have to: the terminal channel's
                                        // lifetime answers the only question
                                        // here - may this end the session?
                                        // While the terminal is open, no: the
                                        // event may be the audio channel's
                                        // helper exiting on its own (a server
                                        // without `python3` or whisper, a
                                        // crashed helper), which must not cost
                                        // the user their terminal, and
                                        // `pump_audio` reports that closure
                                        // itself. Once the terminal channel is
                                        // closed, nothing else can be left, so
                                        // this is its exit and the session
                                        // ends. (Its own EOF ends the session
                                        // too, by completing
                                        // `spawn_session_future`.)
                                        log::info!("ssh channel exit event with {status:?}");
                                        if !TERMINAL_OPEN.load(Ordering::Acquire) {
                                            break;
                                        }
                                    }
                                    CliEvent::Defunct => {
                                        log::error!("ssh session terminated");
                                        break;
                                    }
                                },
                                Err(err) => {
                                    log::error!("ssh progress error: {err:?}");
                                    return Err(err);
                                }
                            }
                        }

                        Ok::<(), sunset::Error>(())
                    };

                    let res = select(
                        runner,
                        select(
                            ssh_ticker,
                            select(
                                spawn_session_future,
                                ssh_audio_branch(&ssh_client, audio_command.as_ref()),
                            ),
                        ),
                    )
                    .await;
                    TERMINAL_OPEN.store(false, Ordering::Release);
                    log::info!("ssh result is {res:?}");
                    assign_proc(prior_proc).await;
                }
                Err(err) => {
                    print!("failed to connect to port {port}: {err:?}\r\n");
                }
            }
        }
        Err(err) => {
            print!("failed to resolve {host}: {err:?}\r\n");
        }
    }
}

#[derive(Copy, Clone)]
enum PromptKind {
    Text,
    Password,
}

async fn prompt_for_input(prompt: &str, kind: PromptKind) -> Option<String> {
    use crate::process::{Mutex, ProcHandle};
    use core::fmt::Write;

    let channel = Arc::new(Channel::<CS, Option<String>, 1>::new());

    struct PromptProc {
        prompt: String,
        input: Mutex<LineEditor>,
        channel: Arc<Channel<CS, Option<String>, 1>>,
        kind: PromptKind,
    }

    impl Drop for PromptProc {
        fn drop(&mut self) {
            self.channel.try_send(None).ok();
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Process for PromptProc {
        fn name(&self) -> &str {
            "prompt"
        }
        async fn render(&self) {
            let mut screen = SCREEN.get().lock().await;
            match self.kind {
                PromptKind::Text => {
                    let input = self.input.lock().await;
                    write!(screen, "\r{} {}\u{1b}[K", self.prompt, input.input()).ok();
                }
                PromptKind::Password => {
                    write!(screen, "\r{}\u{1b}[K", self.prompt).ok();
                }
            }
        }

        fn un_prompt(&self, screen: &mut Screen) {
            erase_prompt_line(screen);
        }

        async fn on_key_press(&self, key: KeyReport) {
            use crate::keyboard::Modifiers;
            match (key.modifiers, key.key) {
                (Modifiers::CTRL, Key::Char('c' | 'C' | 'd' | 'D')) | (_, Key::Escape) => {
                    self.channel.send(None).await;
                }
                _ => {
                    if let Some(command) = self.input.lock().await.apply_key(key) {
                        write!(SCREEN.get().lock().await, "\r\n").ok();
                        self.channel.send(Some(command)).await;
                    }
                }
            }
        }
    }

    let prompt_proc: ProcHandle = Arc::new(PromptProc {
        prompt: prompt.to_string(),
        input: Mutex::new(LineEditor::default()),
        channel: channel.clone(),
        kind,
    });

    let prior = assign_proc(prompt_proc.clone()).await;
    let response = channel.receive().await;
    let _ = assign_proc_if(prior, |current| Arc::ptr_eq(current, &prompt_proc)).await;
    response
}

/// Builds the config key under which a host alias is stored, e.g.
/// `alias_key("home")` -> `"host_home"`. Returns `None` if the resulting
/// key wouldn't fit in a config key (32 characters).
fn alias_key(alias: &str) -> Option<heapless::String<32>> {
    let mut key = heapless::String::<32>::new();
    key.push_str("host_").ok()?;
    key.push_str(alias).ok()?;
    Some(key)
}

/// Resolves `name` to a saved alias's destination, if one exists;
/// otherwise returns `name` unchanged, to be parsed as a literal
/// `[user@]host[:port]`.
async fn resolve_target(name: &str) -> String {
    if let Some(key) = alias_key(name)
        && let Ok(Some(value)) = CONFIG.get().lock().await.fetch(key.as_str()).await
    {
        return value.as_str().to_string();
    }
    name.to_string()
}

/// Like `alias_key`, but reports the "alias is too long" error itself when
/// the alias doesn't fit, so callers only need to handle the `None` case.
async fn alias_key_or_report(alias: &str) -> Option<heapless::String<32>> {
    let key = alias_key(alias);
    if key.is_none() {
        print!("alias is too long\r\n");
    }
    key
}

async fn save_alias(alias: &str, dest: &str) {
    if matches!(alias, "save" | "forget" | "list") {
        print!("'{alias}' is a reserved name\r\n");
        return;
    }
    let Some(key) = alias_key_or_report(alias).await else {
        return;
    };
    let value: StrValue = match dest.try_into() {
        Ok(v) => v,
        Err(err) => {
            print!("value `{dest}`: {err:?}\r\n");
            return;
        }
    };
    let mut config = CONFIG.get().lock().await;
    match config.store(key.as_str(), value).await {
        Ok(()) => print!("Saved '{alias}' -> {dest}\r\n"),
        Err(err) => print!("{err:?}\r\n"),
    }
}

async fn forget_alias(alias: &str) {
    let Some(key) = alias_key_or_report(alias).await else {
        return;
    };
    let mut config = CONFIG.get().lock().await;
    match config.remove(key.as_str()).await {
        Ok(()) => print!("Forgot '{alias}'\r\n"),
        Err(err) => print!("{err:?}\r\n"),
    }
}

async fn list_aliases() {
    let mut config = CONFIG.get().lock().await;
    match config.get_all().await {
        Ok(map) => {
            let mut any = false;
            for (k, v) in &map {
                if let Some(alias) = k.as_str().strip_prefix("host_") {
                    print!("{alias} -> {v}\r\n");
                    any = true;
                }
            }
            if !any {
                print!("No saved hosts. Use `ssh save <alias> <[user@]host[:port]>`.\r\n");
            }
        }
        Err(err) => print!("{err:?}\r\n"),
    }
}

/// Resolves and parses a `ssh` command's `[user@]host[:port]` argument
/// (following aliases) into its username, hostname, and port parts.
/// Reports and returns `None` on an invalid port so callers only need to
/// handle that one case.
async fn parse_ssh_target(arg: &str) -> Option<(Option<String>, String, u16)> {
    let target = resolve_target(arg).await;
    let (username, host_port) = match target.split_once('@') {
        Some((user, rest)) => (Some(user.to_string()), rest),
        None => (None, target.as_str()),
    };
    let (hostname, port) = if let Some((host, port_str)) = host_port.rsplit_once(':') {
        match port_str.parse::<u16>() {
            Ok(port) => (host.to_string(), port),
            Err(_) => {
                print!("invalid port `{port_str}`\r\n");
                return None;
            }
        }
    } else {
        (host_port.to_string(), 22)
    };
    Some((username, hostname, port))
}

pub async fn ssh_command(args: &[&str]) {
    match args {
        ["ssh", "save", alias, dest] => {
            save_alias(alias, dest).await;
            return;
        }
        ["ssh", "save", ..] => {
            print!("Usage: ssh save <alias> <[user@]host[:port]>\r\n");
            return;
        }
        ["ssh", "forget", alias] => {
            forget_alias(alias).await;
            return;
        }
        ["ssh", "forget", ..] => {
            print!("Usage: ssh forget <alias>\r\n");
            return;
        }
        ["ssh", "list"] => {
            list_aliases().await;
            return;
        }
        _ => {}
    }

    if args.len() > 1 {
        let Some((username, hostname, port)) = parse_ssh_target(args[1]).await else {
            return;
        };

        let command: Option<String> = if args.len() > 2 {
            Some(args[2..].join(" "))
        } else {
            None
        };
        let spawn_result = {
            let spawner = Spawner::for_current_executor().await;
            spawner.spawn(ssh_session_task(hostname, port, username, command))
        };
        match spawn_result {
            Ok(_) => {}
            Err(err) => {
                print!("failed to start ssh task {err:?}\r\n");
            }
        }
        return;
    }

    print!("Usage: ssh [user@]hostname[:port] [command]\r\n");
    print!("       ssh save <alias> <[user@]host[:port]>\r\n");
    print!("       ssh forget <alias>\r\n");
    print!("       ssh list\r\n");
}

struct SshProcess {
    key_sender: Arc<Channel<CS, KeyReport, 4>>,
}

#[async_trait::async_trait(?Send)]
impl Process for SshProcess {
    fn name(&self) -> &str {
        "ssh"
    }
    async fn render(&self) {}
    async fn on_key_press(&self, key: KeyReport) {
        self.key_sender.send(key).await;
    }
}

/// Map c to its Ctrl equivalent.
/// This mapping translates characters to their control code equivalents.
/// It includes standard alpha mappings (masking with 0x1f) and common
/// aliased mappings for punctuation and digits often found in terminal
/// emulators (e.g., xterm compatibility).
fn ctrl_mapping(c: char) -> Option<char> {
    Some(match c {
        '@' | '`' | ' ' | '2' => '\x00',
        'A' | 'a' => '\x01',
        'B' | 'b' => '\x02',
        'C' | 'c' => '\x03',
        'D' | 'd' => '\x04',
        'E' | 'e' => '\x05',
        'F' | 'f' => '\x06',
        'G' | 'g' => '\x07',
        'H' | 'h' => '\x08',
        'I' | 'i' => '\x09',
        'J' | 'j' => '\x0a',
        'K' | 'k' => '\x0b',
        'L' | 'l' => '\x0c',
        'M' | 'm' => '\x0d',
        'N' | 'n' => '\x0e',
        'O' | 'o' => '\x0f',
        'P' | 'p' => '\x10',
        'Q' | 'q' => '\x11',
        'R' | 'r' => '\x12',
        'S' | 's' => '\x13',
        'T' | 't' => '\x14',
        'U' | 'u' => '\x15',
        'V' | 'v' => '\x16',
        'W' | 'w' => '\x17',
        'X' | 'x' => '\x18',
        'Y' | 'y' => '\x19',
        'Z' | 'z' => '\x1a',
        '[' | '3' | '{' => '\x1b',
        '\\' | '4' | '|' => '\x1c',
        ']' | '5' | '}' => '\x1d',
        '^' | '6' | '~' => '\x1e',
        '_' | '7' | '/' => '\x1f',
        '8' | '?' => '\x7f', // `Delete`
        _ => return None,
    })
}
