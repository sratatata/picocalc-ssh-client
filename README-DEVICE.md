# Connections

## Serial
* UART0 (`GP0`/`GP1`) - connected to mux on CH340C on picocalc
* UART1 (`GP8`/`GP9`) - connected to `M_UART3` aka `Serial1` on picocalc mcu. Default mcu firmware writes pmu debug logs to this.

## I2C
* I2C1 (`GP6`/`GP7`) - I2C bus connected to picocalc keyboard/pmu mcu `M_I2C1`

## LCD
* `GP10` - `SPI1_SCK`
* `GP11` - `SPI1_TX`
* `GP12` - `SPI1_RX`
* `GP13` - `SPI1_CS`
* `GP14` - `LCD_DC`
* `GP15` - `LCD_RST`

## Audio
GP26/GP27 - `PWM_L`/`PWM_R` on picocalc audio circuitry

## TF Card reader
* `GP16` - `SPI0_RX`
* `GP17` - `SPI0_CS`
* `GP18` - `SPI0_SCK`
* `GP19` - `SPI0_TX`
* `GP22` - `SD_DET`

## Microphone (I2S, push-to-talk)
* `GP2`  - I2S `BCLK` (bit clock, firmware drives this)
* `GP3`  - I2S `WS`/`LRCLK` (word select, firmware drives this)
* `GP21` - I2S `SD`/`DOUT` (serial data, mic drives this - the only actual input line)

The mic's I2S slot width, sample rate and BCLK edge polarity are runtime
settings (`config set ptt_bits`/`ptt_rate`/`ptt_edge`) applied at the start of
each recording, so a new configuration can be probed without reflashing. The
defaults (`ptt_bits=32`, `ptt_rate=16000`, `ptt_edge=0`) are the SPH0645's;
see `README.md`'s Push-to-Talk section and `AGENTS.md`.

These are exposed expansion/jumper block pins also wired to the PSRAM chip
(see below); they're free for the mic because this firmware only talks to
PSRAM over the QMI/XIP hardware path now, which uses a separate,
RP2350-internal chip-select pad and never drives these pins (see
`src/psram.rs`'s header comment and AGENTS.md). `GP20` (PSRAM `RAM_CS`) is
explicitly held deselected by firmware since nothing drives it as PSRAM
chip-select anymore.

## PSRAM
* `GP2`  - `RAM_TX` (repurposed for the mic's I2S `BCLK`; see above)
* `GP3`  - `RAM_RX` (repurposed for the mic's I2S `WS`/`LRCLK`; see above)
* `GP4`  - `RAM_IO2`  - quad mode (unclaimed by this firmware)
* `GP5`  - `RAM_IO3`  - quad mode (unclaimed by this firmware)
* `GP20` - `RAM_CS` (held explicitly deselected by firmware; see above)
* `GP21` - `RAM_SCK` (repurposed for the mic's I2S `SD`/`DOUT`; see above)

Note that all except the CS are exposed to expansion/jumper block. Only the
QMI/XIP hardware path (`init_psram_qmi` in `src/psram.rs`) is used to reach
this chip; the old PIO-driven "slow path" that bit-banged SPI over these
pins was removed (see AGENTS.md).

## Expansion Port/Jumper block
* `GP2`  - Also connected to PSRAM; used by this firmware for the mic's I2S `BCLK`
* `GP3`  - Also connected to PSRAM; used by this firmware for the mic's I2S `WS`/`LRCLK`
* `GP4`  - Also connected to PSRAM
* `GP5`  - Also connected to PSRAM
* `GP21` - Also connected to PSRAM; used by this firmware for the mic's I2S `SD`/`DOUT`
* `GP28`

