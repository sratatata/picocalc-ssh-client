use embassy_rp::clocks::clk_peri_freq;

// The physical connections in the picocalc schematic are:
// LABEL     PICO      ESP-PSRAM64H
// RAM_CS  - PIN_20    CE                    (pulled up to 3v3 via 10kOhm)
// RAM_SCK - PIN_21    SCLK
// RAM_TX  - PIN_2     SI/SIO0
// RAM_RX  - PIN_3     SO/SIO1
// RAM_IO2 - PIN_4     SIO2     (QPI Mode)
// RAM_IO3 - PIN_5     SIO3     (QPI Mode)
//
// Only the QMI/XIP hardware path below is used to talk to this chip. This
// file used to also have a PIO-driven "slow path" that bit-banged SPI over
// PIN_2/PIN_3/PIN_20/PIN_21 (claiming PIO1/DMA_CH1/DMA_CH2) as a fallback,
// but its detected size was never fed to the heap allocator - only
// `init_qmi_psram_heap` (driven by `init_psram_qmi` below) does that - so it
// was removed as dead weight (see AGENTS.md). Dropping it frees
// PIN_2/PIN_3/PIN_21 for other use (see `src/mic.rs`); PIN_20 (RAM_CS) is
// explicitly held deselected by `main.rs` since nothing here drives it
// anymore, and PIN_4/PIN_5 (QPI IO2/IO3) were never claimed by this file to
// begin with. The QMI/XIP hardware path below uses a separate,
// RP2350-internal chip-select pad (see `detect_psram_qmi`'s `XIP_CS_PIN`),
// not PIN_20, so none of this affects QMI PSRAM access.

#[allow(unused)]
const PSRAM_CMD_QUAD_END: u8 = 0xf5;
#[allow(unused)]
const PSRAM_CMD_READ_ID: u8 = 0x9F;
#[allow(unused)]
const PSRAM_CMD_NOOP: u8 = 0xFF;
#[allow(unused)]
const PSRAM_KNOWN_GOOD_DIE_PASS: u8 = 0x5d;

// The origin of the code in this file is:
// <https://github.com/Altaflux/rp2350-psram-test/blob/ae50a819fef96486f6d962a609984cde4b4dd4cc/src/psram.rs#L1>
// which is MIT/Apache-2 licensed.
#[unsafe(link_section = ".data")]
#[inline(never)]
pub fn detect_psram_qmi(qmi: &embassy_rp::pac::qmi::Qmi) -> u32 {
    const GPIO_FUNC_XIP_CS1: u8 = 9;
    const XIP_CS_PIN: usize = 47;
    embassy_rp::pac::PADS_BANK0.gpio(XIP_CS_PIN).modify(|w| {
        w.set_iso(true);
    });
    embassy_rp::pac::PADS_BANK0.gpio(XIP_CS_PIN).modify(|w| {
        w.set_ie(true);
        w.set_od(false);
    });
    embassy_rp::pac::IO_BANK0
        .gpio(XIP_CS_PIN)
        .ctrl()
        .write(|w| w.set_funcsel(GPIO_FUNC_XIP_CS1));
    embassy_rp::pac::PADS_BANK0.gpio(XIP_CS_PIN).modify(|w| {
        w.set_iso(false);
    });

    critical_section::with(|_cs| {
        // Try and read the PSRAM ID via direct_csr.
        qmi.direct_csr().write(|w| {
            w.set_clkdiv(30);
            w.set_en(true);
        });

        // Need to poll for the cooldown on the last XIP transfer to expire
        // (via direct-mode BUSY flag) before it is safe to perform the first
        // direct-mode operation
        while qmi.direct_csr().read().busy() {
            // rp235x_hal::arch::nop();
        }

        // Exit out of QMI in case we've inited already
        qmi.direct_csr().modify(|w| w.set_assert_cs1n(true));

        // Transmit the command to exit QPI quad mode - read ID as standard SPI
        // Transmit as quad.
        qmi.direct_tx().write(|w| {
            w.set_oe(true);
            w.set_iwidth(embassy_rp::pac::qmi::vals::Iwidth::Q);
            w.set_data(PSRAM_CMD_QUAD_END.into());
        });

        while qmi.direct_csr().read().busy() {
            // rp235x_hal::arch::nop();
        }

        let _ = qmi.direct_rx().read();

        qmi.direct_csr().modify(|w| {
            w.set_assert_cs1n(false);
        });

        // Read the id
        qmi.direct_csr().modify(|w| {
            w.set_assert_cs1n(true);
        });

        // kgd is "known good die"
        let mut kgd: u16 = 0;
        let mut eid: u16 = 0;
        for i in 0usize..7 {
            qmi.direct_tx().write(|w| {
                w.set_data(if i == 0 {
                    PSRAM_CMD_READ_ID.into()
                } else {
                    PSRAM_CMD_NOOP.into()
                })
            });

            while !qmi.direct_csr().read().txempty() {
                // rp235x_hal::arch::nop();
            }

            while qmi.direct_csr().read().busy() {
                // rp235x_hal::arch::nop();
            }

            let value = qmi.direct_rx().read().direct_rx();
            match i {
                5 => {
                    kgd = value;
                }
                6 => {
                    eid = value;
                }
                _ => {}
            }
        }

        qmi.direct_csr().modify(|w| {
            w.set_assert_cs1n(false);
            w.set_en(false);
        });
        let mut param_size: u32 = 0;
        if kgd == PSRAM_KNOWN_GOOD_DIE_PASS as u16 {
            param_size = 1024 * 1024;
            let size_id = eid >> 5;
            if eid == 0x26 || size_id == 2 {
                param_size *= 8;
            } else if size_id == 0 {
                param_size *= 2;
            } else if size_id == 1 {
                param_size *= 4;
            }
        }
        param_size
    })
}

#[unsafe(link_section = ".data")]
#[inline(never)]
pub fn init_psram_qmi(
    qmi: &embassy_rp::pac::qmi::Qmi,
    xip: &embassy_rp::pac::xip_ctrl::XipCtrl,
) -> u32 {
    let psram_size = detect_psram_qmi(qmi);

    if psram_size == 0 {
        return 0;
    }

    // Set PSRAM timing for APS6404
    //
    // Using an rxdelay equal to the divisor isn't enough when running the APS6404 close to 133MHz.
    // So: don't allow running at divisor 1 above 100MHz (because delay of 2 would be too late),
    // and add an extra 1 to the rxdelay if the divided clock is > 100MHz (i.e. sys clock > 200MHz).
    const MAX_PSRAM_FREQ: u32 = 133_000_000;

    let clock_hz = clk_peri_freq();

    let mut divisor: u32 = clock_hz.div_ceil(MAX_PSRAM_FREQ);
    if divisor == 1 && clock_hz > 100_000_000 {
        divisor = 2;
    }
    let mut rxdelay: u32 = divisor;
    if clock_hz / divisor > 100_000_000 {
        rxdelay += 1;
    }

    // - Max select must be <= 8us.  The value is given in multiples of 64 system clocks.
    // - Min deselect must be >= 18ns.  The value is given in system clock cycles - ceil(divisor / 2).
    let clock_period_fs: u64 = 1_000_000_000_000_000_u64 / u64::from(clock_hz);
    let max_select: u8 = ((125 * 1_000_000) / clock_period_fs) as u8;
    let min_deselect: u32 =
        ((18 * 1_000_000u64).div_ceil(clock_period_fs) - u64::from(divisor + 1) / 2) as u32;

    log::info!(
        "clock_period_fs={clock_period_fs} max_select={max_select} min_deselect={min_deselect}"
    );

    qmi.direct_csr().write(|w| {
        w.set_clkdiv(10);
        w.set_en(true);
        w.set_auto_cs1n(true);
    });

    while qmi.direct_csr().read().busy() {
        // rp235x_hal::arch::nop();
    }

    qmi.direct_tx().write(|w| {
        w.set_nopush(true);
        w.0 = 0x35;
    });

    while qmi.direct_csr().read().busy() {
        // rp235x_hal::arch::nop();
    }

    qmi.mem(1).timing().write(|w| {
        w.set_cooldown(1);
        w.set_pagebreak(embassy_rp::pac::qmi::vals::Pagebreak::_1024);
        w.set_max_select(max_select as u8);
        w.set_min_deselect(min_deselect as u8);
        w.set_rxdelay(rxdelay as u8);
        w.set_clkdiv(divisor as u8);
    });

    // // Set PSRAM commands and formats
    qmi.mem(1).rfmt().write(|w| {
        w.set_prefix_width(embassy_rp::pac::qmi::vals::PrefixWidth::Q);
        w.set_addr_width(embassy_rp::pac::qmi::vals::AddrWidth::Q);
        w.set_suffix_width(embassy_rp::pac::qmi::vals::SuffixWidth::Q);
        w.set_dummy_width(embassy_rp::pac::qmi::vals::DummyWidth::Q);
        w.set_data_width(embassy_rp::pac::qmi::vals::DataWidth::Q);
        w.set_prefix_len(embassy_rp::pac::qmi::vals::PrefixLen::_8);
        w.set_dummy_len(embassy_rp::pac::qmi::vals::DummyLen::_24);
    });

    qmi.mem(1).rcmd().write(|w| w.0 = 0xEB);

    qmi.mem(1).wfmt().write(|w| {
        w.set_prefix_width(embassy_rp::pac::qmi::vals::PrefixWidth::Q);
        w.set_addr_width(embassy_rp::pac::qmi::vals::AddrWidth::Q);
        w.set_suffix_width(embassy_rp::pac::qmi::vals::SuffixWidth::Q);
        w.set_dummy_width(embassy_rp::pac::qmi::vals::DummyWidth::Q);
        w.set_data_width(embassy_rp::pac::qmi::vals::DataWidth::Q);
        w.set_prefix_len(embassy_rp::pac::qmi::vals::PrefixLen::_8);
    });

    qmi.mem(1).wcmd().write(|w| w.0 = 0x38);

    // Disable direct mode
    qmi.direct_csr().write(|w| w.0 = 0);

    // Enable writes to PSRAM
    xip.ctrl().modify(|w| w.set_writable_m1(true));
    psram_size
}
