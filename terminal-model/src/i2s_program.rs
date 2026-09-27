//! Runtime assembly of the PIO I2S RX (microphone) program.
//!
//! The firmware used to bake this program in with `pio_asm!`, which fixes the
//! per-channel shift count and the BCLK edge polarity at compile time. Since
//! those are now runtime debug settings (`ptt_bits`/`ptt_edge`, resolved in
//! `crate::mic_config`), the program is assembled on the device from the `pio`
//! crate's public `Assembler` each time the settings change.
//!
//! Keeping the assembly here (rather than in the hardware-coupled root crate)
//! lets host tests decode the produced instructions and assert the loop count,
//! the jump targets, the word-select/bit-clock side-set values, and that the
//! edge flip touches only the bit-clock bit - the exact arithmetic that a
//! silent regression would otherwise turn into an untestable hardware bug.

use pio::{
    Assembler, InSource, Instruction, InstructionOperands, JmpCondition, Program, SetDestination,
    SideSet,
};

/// Instructions in the fixed-shape program. `set x, N`, left-phase
/// `in`/`jmp x--`/`in`, `set x, N`, right-phase `in`/`jmp x--`/`in`.
pub const PROGRAM_SIZE: usize = 8;

/// Assembles the I2S RX program for a channel slot width and BCLK edge
/// polarity.
///
/// * `bits_per_channel_slot` sets the loop count: `set x, bits - 2` gives
///   `(bits - 2) + 2 = bits` shifts per channel (the loop's `in`, plus the
///   trailing `in` outside it).
/// * `edge_flip` inverts the low (bit-clock) bit of every side-set value,
///   shifting sampling half a BCLK cycle without changing the loop shape or
///   the word-select polarity.
///
/// The shape mirrors embassy-rp's TX `PioI2sOut` program, with `in pins, 1`
/// capturing instead of `out pins, 1` emitting. The `set x, N` between the two
/// phases doubles as the standard word-select-to-first-bit delay slot.
///
/// `bits_per_channel_slot` must be at least 2 (so the 5-bit `set x` immediate
/// is `bits - 2`); the firmware only ever calls this with values validated
/// against `crate::pcm_extract::mic_settings_valid`.
pub fn build_i2s_rx_program(bits_per_channel_slot: u32, edge_flip: bool) -> Program<PROGRAM_SIZE> {
    let count = (bits_per_channel_slot - 2) as u8;
    let side = |value: u8| if edge_flip { value ^ 0b01 } else { value };

    let mut assembler = Assembler::<PROGRAM_SIZE>::new_with_side_set(SideSet::new(false, 2, false));
    {
        // side 0bWB: bit1 = word clock (WS), bit0 = bit clock (BCLK).
        let mut emit = |operands, value: u8| {
            assembler.instructions.push(Instruction {
                operands,
                delay: 0,
                side_set: Some(side(value)),
            });
        };
        emit(
            InstructionOperands::SET {
                destination: SetDestination::X,
                data: count,
            },
            0b01,
        );
        emit(
            InstructionOperands::IN {
                source: InSource::PINS,
                bit_count: 1,
            },
            0b00,
        ); // left_data
        emit(
            InstructionOperands::JMP {
                condition: JmpCondition::XDecNonZero,
                address: 1,
            },
            0b01,
        );
        emit(
            InstructionOperands::IN {
                source: InSource::PINS,
                bit_count: 1,
            },
            0b10,
        );
        emit(
            InstructionOperands::SET {
                destination: SetDestination::X,
                data: count,
            },
            0b11,
        );
        emit(
            InstructionOperands::IN {
                source: InSource::PINS,
                bit_count: 1,
            },
            0b10,
        ); // right_data
        emit(
            InstructionOperands::JMP {
                condition: JmpCondition::XDecNonZero,
                address: 5,
            },
            0b11,
        );
        emit(
            InstructionOperands::IN {
                source: InSource::PINS,
                bit_count: 1,
            },
            0b00,
        );
    }
    assembler.assemble_program()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(program: &Program<PROGRAM_SIZE>, index: usize) -> Instruction {
        Instruction::decode(program.code[index], program.side_set)
            .expect("assembled instruction must decode")
    }

    #[test]
    fn program_has_expected_shape_and_wrap() {
        let program = build_i2s_rx_program(32, false);
        assert_eq!(program.code.len(), PROGRAM_SIZE);
        // Full-program wrap: after the last shift, jump back to instruction 0.
        assert_eq!(
            program.wrap,
            pio::Wrap {
                source: 7,
                target: 0
            }
        );

        // Both channel phases reset the loop counter with the same value and
        // their `jmp x--` targets the phase's own first `in`.
        for (set_index, jump_index, target) in [(0usize, 2usize, 1u8), (4, 6, 5)] {
            match decoded(&program, set_index).operands {
                InstructionOperands::SET {
                    destination: SetDestination::X,
                    data,
                } => assert_eq!(data, 30),
                other => panic!("instruction {set_index} was {other:?}"),
            }
            match decoded(&program, jump_index).operands {
                InstructionOperands::JMP {
                    condition: JmpCondition::XDecNonZero,
                    address,
                } => assert_eq!(address, target, "jump {jump_index} target"),
                other => panic!("instruction {jump_index} was {other:?}"),
            }
        }
        // Every instruction carries a 2-bit side-set value; the two that
        // start a phase assert the word clock, the rest toggle the bit clock.
        for index in 0..PROGRAM_SIZE {
            assert!(decoded(&program, index).side_set.is_some());
        }
        assert_eq!(decoded(&program, 0).side_set, Some(0b01));
        assert_eq!(decoded(&program, 4).side_set, Some(0b11));
    }

    #[test]
    fn loop_count_tracks_the_slot_width() {
        // `set x, N` gives N + 2 shifts per channel, so the immediate is
        // always two less than the configured slot width.
        for bits in [8u32, 16, 24, 32] {
            let program = build_i2s_rx_program(bits, false);
            match decoded(&program, 0).operands {
                InstructionOperands::SET { data, .. } => assert_eq!(data as u32, bits - 2),
                other => panic!("instruction 0 was {other:?}"),
            }
            match decoded(&program, 4).operands {
                InstructionOperands::SET { data, .. } => assert_eq!(data as u32, bits - 2),
                other => panic!("instruction 4 was {other:?}"),
            }
        }
    }

    #[test]
    fn edge_flip_only_inverts_the_bit_clock_bit() {
        let normal = build_i2s_rx_program(32, false);
        let flipped = build_i2s_rx_program(32, true);
        assert_eq!(normal.code.len(), flipped.code.len());
        for index in 0..PROGRAM_SIZE {
            let a = decoded(&normal, index);
            let b = decoded(&flipped, index);
            // Operands are untouched...
            assert_eq!(a.operands.encode(), b.operands.encode());
            // ...and only the low (bit-clock) side-set bit differs.
            assert_eq!(a.side_set.unwrap() ^ 0b01, b.side_set.unwrap());
        }
    }
}
