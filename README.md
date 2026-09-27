# PicoCalc SSH Client

A standalone SSH client and VT100/ANSI terminal emulator for the [Raspberry Pi Pico 2 W](https://www.raspberrypi.com/products/raspberry-pi-pico-2/) running on the [ClockworkPi PicoCalc](https://www.clockworkpi.com/picocalc).

This project transforms your PicoCalc into a pocket-sized, WiFi-enabled terminal capable of connecting to remote servers via SSH. It is a fork of the [picocalc-wezterm](https://github.com/wez/picocalc-wezterm) project, with improved terminal character support and scrolling. Thus, making the terminal experience more usable.

<p align="center">
  <img src="img/picocalc-ssh-client-gemini.png" width="45%" />
  <img src="img/picocalc-ssh-client-mc.png" width="45%" />
</p>

## Features

*   **Standalone SSH Client**: Connect to any SSH server directly from the device.
*   **Robust Terminal Emulation**: Built on the `vte` crate for accurate ANSI/VT100 parsing.
*   **Extended Character Support**: Custom rendering for box-drawing characters (lines, corners, shades) and common decorative symbols (chevrons, bullets, ellipses, arrows, circles) for TUI applications like `vim`, `gemini-cli`, `claude-code`, `mc`, `htop`, `ollama`, and `tmux`.
*   **Scrolling**: Scroll through the command history, with a heap-budget-derived scrollback limit.
*   **Local Shell**: Built-in commands for device management (WiFi config, battery status, backlight control).
*   **Battery Overlay**: Short-press the power button at any time, even mid-SSH-session, for a brief on-screen battery readout that dismisses itself.
*   **SD Card Key Backup**: Save the SSH private key to the SD card and restore it afterwards, so erasing flash (e.g. `flash_nuke.uf2`) doesn't cost you a freshly generated key and a re-authorisation on every server.
*   **Push-to-Talk Voice Capture**: Hold a button to stream microphone audio to a server-side helper over the SSH session (which transcribes it with Whisper and types it into your tmux pane).
*   **Hardware Accelerated**: Uses the RP2350's capabilities and the ILI9488 display for fast rendering.

## Hardware Requirements

*   **ClockworkPi PicoCalc**
*   **Raspberry Pi Pico 2 W** (RP2350 with WiFi)
    *   *Note: This firmware is specifically designed for the RP2350 architecture.*
*   **Adafruit SPH0645 I2S MEMS microphone breakout** (or compatible digital I2S mic) — *optional*, only needed for push-to-talk voice capture.

## Installing the Released Firmware

1. Download the latest firmware from the [releases](https://github.com/lviss/picocalc-ssh-client/releases) page. Each release has two `.uf2` assets, one per board — grab the one matching your board: `picocalc-ssh-client-pico2w-<version>.uf2` or `picocalc-ssh-client-pimoroni2w-<version>.uf2`.
2. Flash:
    * Hold the BOOTSEL button on your Pico 2 W while plugging it in.
    * Copy the downloaded `.uf2` file to the mounted RP2350 drive.
    * Reboot the Pico 2 W.

## Getting Started with Development

### Prerequisites

You will need a standard Rust toolchain and a few helper tools:

1.  **Install Rust**: [rustup.rs](https://rustup.rs/)
2.  **Install the Nightly Toolchain**:
    ```bash
    rustup toolchain install nightly
    ```
3.  **Add the Compilation Target**:
    ```bash
    rustup target add thumbv8m.main-none-eabihf
    ```
4.  **Install Helper Tools**:
    ```bash
    cargo install flip-link
    # Install picotool (follow instructions at https://github.com/raspberrypi/picotool)
    ```

### Building & Flashing

1.  **Clone the repository**:
    ```bash
    git clone https://github.com/lviss/picocalc-ssh-client.git
    cd picocalc-ssh-client
    ```

2.  **Build the Firmware**:
    ```bash
    # For Pimoroni Pico Plus 2 W (standard PicoCalc upgrade)
    cargo +nightly build --release --features pimoroni2w
    ```

3.  **Generate UF2 File**:
    ```bash
    # Convert the ELF to UF2
    cp target/thumbv8m.main-none-eabihf/release/picocalc-wezterm target/thumbv8m.main-none-eabihf/release/picocalc-ssh-client.elf
    picotool uf2 convert target/thumbv8m.main-none-eabihf/release/picocalc-ssh-client.elf picocalc-ssh-client.uf2
    ```

4.  **Flash**:
    *   Hold the BOOTSEL button on your Pico 2 W while plugging it in.
    *   Copy `picocalc-ssh-client.uf2` to the mounted `RP2350` drive.

## Usage

### Initial Setup (WiFi)

On first boot, you need to configure your WiFi credentials. The device includes a local shell for configuration.

```bash
# Format the config storage (only needed once)
$ config format

# Set WiFi credentials
$ config set wifi_ssid MyNetwork
$ config set wifi_pw MyPassword

# Reboot to apply
$ reboot
```

> [!CAUTION]
> Credentials (and the SSH private key, if you generate one) are stored
> unencrypted in the device's flash memory.

### Connecting via SSH

Once connected to WiFi (you'll see an IP address), you can connect to a remote host:

```bash
$ ssh mymachine
# or
$ ssh 192.168.1.10
```

Connects to port 22 by default. To use a different port, append it after a
colon, and/or a username before an `@`, using the same syntax as regular
`ssh`:

```bash
$ ssh mymachine:2222
$ ssh myuser@mymachine
$ ssh myuser@mymachine:2222
```

You can also save credentials to avoid typing them every time:

```bash
$ config set ssh_user myuser
$ config set ssh_pw mypassword
```

### Remembering Hosts

Rather than retyping a long hostname (and username/port) every time, save it
under a short alias:

```bash
# Save an alias (accepts the same [user@]host[:port] syntax as `ssh`)
$ ssh save home myuser@myserver.example.com:2222

# Connect using the alias
$ ssh home

# List saved aliases
$ ssh list

# Remove one
$ ssh forget home
```

Aliases are stored in the same flash-backed config as other settings, so
they persist across reboots.

### SSH Key Authentication

The device can generate its own Ed25519 keypair and use it to authenticate,
so you don't need to type (or store) a password at all. The private key is
generated on-device and never leaves it on its own; only the public key needs
to be shared with servers.

```bash
# Generate a keypair (refuses to overwrite an existing one)
$ keygen

# Add the printed "ssh-ed25519 AAAA..." line to the *server's*
# ~/.ssh/authorized_keys file

# Re-display the public key at any time
$ keygen show

# Replace the existing keypair (servers using the old public key
# will need to be updated, or they'll stop accepting it)
$ keygen force
```

Once a key is generated, `ssh` tries it automatically before falling back to
`ssh_pw` or an interactive password prompt.

Whenever the key is generated or shown, the public key is also mirrored to
`ssh_key.pub` on an inserted SD card's first (FAT) partition, ready to copy
onto another machine — this is best-effort and never blocks the on-screen
output if no card is present.

#### Backing up and restoring the private key on the SD card

Erasing flash (for example `flash_nuke.uf2` before switching the PicoCalc to
another firmware) also wipes the config sectors that hold the private key.
With an SD card inserted, the key can be backed up and restored:

```bash
# Write the current private key to the card (creates ssh_key.hex;
# refuses if a backup already exists)
$ keygen save
$ keygen save force   # overwrite an existing backup

# Restore a key from the card (refuses if the device already has a key)
$ keygen load
$ keygen load force   # replace the device's current key
```

> [!CAUTION]
> `keygen save` writes the **private key in plain text** to removable media.
> Treat the card as a secret: anyone who can read it can impersonate the
> device on every server that authorises the matching public key. The export
> is always explicit — the firmware never copies the key to the card, or
> loads one from it, on its own.

### Scrolling

You can scroll through the command history using the following key combinations:

*   `Ctrl + UpArrow`: Scroll up
*   `Ctrl + DownArrow`: Scroll down

Typing any character or receiving new output from the server will automatically reset the view to the bottom.

You can configure the number of lines in the scrollback buffer. The maximum
(and default) is derived from the device's available heap and current screen
geometry rather than a fixed number, so it varies by device/font; check the
current value with `config get scroll`:

```bash
$ config get scroll
$ config set scroll 100  # must be <= the heap-budget limit reported on error
$ config rm scroll  # Resets to the heap-budget default
```

### Battery Overlay

A short press of the physical power button shows a bordered "Battery: NN%"
box centered on screen for a few seconds, then it disappears on its own. It
works at any time, including in the middle of an active SSH session, and
never disturbs the underlying screen content.

Holding the power button down instead powers off the device; that's handled
entirely by the keyboard co-processor and doesn't involve this firmware.

### Push-to-Talk Voice Capture

Hold `F1` (plain, no modifiers) and speak: the audio streams down a second
channel on your existing SSH session to a server-side helper, which
transcribes it with Whisper and types the text into your tmux pane. Release
`F1` to stop. With no SSH session up, `F1` shows `no ssh session: not
recording` instead.

**Server setup** needs only `python3` plus either `whisper-cli` (whisper.cpp,
with a model under `~/models/`) or `openai-whisper` — nothing to install by
hand, since the device streams its helper (`tools/picocalc-ptt`) down the
channel itself at the start of every session. The transcript goes to the
active tmux pane, with a trailing space and no Enter unless configured
otherwise. See `~/.config/picocalc-ptt.conf` (created by hand, optional) and
`tools/picocalc-ptt --help` for tuning the target session, whisper command,
and tmux socket.

If the helper can't start (no `python3`/whisper on the server, or it
crashes), only push-to-talk is affected — the SSH session itself keeps
working — and `F1` shows `ptt unavailable`. You can also point the device at
your own script with `config set ptt_ssh_cmd <command>` (empty value
disables push-to-talk entirely); this takes effect on the next SSH session.

#### Microphone wiring

Push-to-talk drives a digital I2S microphone from three expansion-header pins:

| microphone pin | PicoCalc pin | direction |
| --- | --- | --- |
| `BCLK` (bit clock) | `GP2` | driven by the firmware |
| `WS` / `LRCLK` (word select) | `GP3` | driven by the firmware |
| `DOUT` / `SD` (serial data) | `GP21` | driven by the microphone |
| `SEL` (channel select) | left-channel setting | wire it so the mic drives the *left* slot |
| `3V` / `GND` | `3V3` / `GND` | power and ground |

The firmware was brought up against an Adafruit SPH0645 breakout (fixed
32-bit slot per channel, clocked at the default `ptt_bits=32`/`ptt_rate=16000`).
While recording, the overlay shows a realtime input level meter as a quick
"is the mic hearing anything?" check.

For bringing up a different microphone, `ptt_bits`/`ptt_rate`/`ptt_edge` are
runtime `config set` knobs — see `AGENTS.md` for the valid clock window and
tuning details.

> [!NOTE]
> The audio never leaves the SSH session, so it is encrypted end to end by
> the session itself. Transcription happens on the server; the firmware only
> captures and streams raw audio.

### Local Commands

*   `cls`: Clear the screen.
*   `bat`: Show battery status.
*   `bl lcd <percent>`: Set LCD backlight brightness (e.g., `bl lcd 50`).
*   `bl kbd <percent>`: Set keyboard backlight brightness (requires updated keyboard firmware).
*   `free`: Show memory usage.
*   `bootsel`: Reboot into bootloader mode.
*   `keygen [force|show]`: Generate (or re-display) an SSH keypair for public-key authentication. Also mirrors the public key to `ssh_key.pub` on the SD card if one is present.
*   `keygen save [force]`: Write the private key to `ssh_key.hex` on the SD card.
*   `keygen load [force]`: Restore the private key from `ssh_key.hex` on the SD card.

## Credits

*   Forked from [wezterm/picocalc-wezterm](https://github.com/wezterm/picocalc-wezterm).
*   This repo is itself forked from [richcannings/picocalc-ssh-client](https://github.com/richcannings/picocalc-ssh-client).
*   Original SSH implementation using [sunset](https://github.com/wez/sunset).
*   Terminal emulation powered by [vte](https://github.com/alacritty/vte).
