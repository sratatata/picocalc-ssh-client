//! Text codec for the SSH private key's SD-card backup file.
//!
//! The private key is an Ed25519 seed, and the firmware's flash config store
//! keeps it under `ssh_key` as 64 lowercase hex characters (`src/sshkey.rs`).
//! The SD-card backup uses exactly the same representation, so the value is
//! portable and inspectable with any text editor: 64 hex characters, no
//! trailing newline. A trailing newline (or any other surrounding ASCII
//! whitespace) is tolerated when reading, since editors and `echo` add one.
//!
//! This module lives in `terminal-model` only because that is the one crate
//! in this workspace that builds for the host, so it is the only place these
//! parsing edge cases can be unit tested. It has no dependency on the
//! firmware crate.

/// Length of an Ed25519 seed, in bytes.
pub const SEED_LEN: usize = 32;
/// Length of the hex text form of a seed.
pub const HEX_LEN: usize = SEED_LEN * 2;

/// Why a seed's text form isn't usable.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyFileError {
    /// The text is empty (or nothing but whitespace).
    Empty,
    /// After trimming surrounding whitespace, the text isn't exactly
    /// [`HEX_LEN`] characters.
    WrongLength { found: usize },
    /// A character that isn't a hex digit. `offset` is its index in the
    /// trimmed text, so the user can find it.
    InvalidHex { offset: usize },
}

impl core::fmt::Display for KeyFileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "the file is empty"),
            Self::WrongLength { found } => {
                write!(f, "expected {HEX_LEN} hex characters, found {found}")
            }
            Self::InvalidHex { offset } => write!(f, "character {} is not a hex digit", offset + 1),
        }
    }
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

/// Format a seed as the 64-character lowercase hex text used by both the
/// flash config store and the SD-card backup file.
pub fn encode_seed(seed: &[u8; SEED_LEN]) -> [u8; HEX_LEN] {
    let mut out = [0u8; HEX_LEN];
    for (i, byte) in seed.iter().enumerate() {
        out[i * 2] = HEX_CHARS[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX_CHARS[(byte & 0x0f) as usize];
    }
    out
}

/// Decode a seed exactly as the flash config store holds it: exactly
/// [`HEX_LEN`] hex characters and nothing else.
pub fn decode_seed_exact(text: &str) -> Option<[u8; SEED_LEN]> {
    decode_hex(text).ok()
}

/// Decode the contents of the SD-card backup file: the same [`HEX_LEN`] hex
/// characters, with surrounding ASCII whitespace (a trailing newline, CRLF
/// line endings, ...) ignored.
pub fn decode_seed_file(text: &str) -> Result<[u8; SEED_LEN], KeyFileError> {
    decode_hex(text.trim_matches(|c: char| c.is_ascii_whitespace()))
}

fn decode_hex(text: &str) -> Result<[u8; SEED_LEN], KeyFileError> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(KeyFileError::Empty);
    }
    if bytes.len() != HEX_LEN {
        return Err(KeyFileError::WrongLength { found: bytes.len() });
    }
    let mut out = [0u8; SEED_LEN];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let hi = hex_val(pair[0]).ok_or(KeyFileError::InvalidHex { offset: i * 2 })?;
        let lo = hex_val(pair[1]).ok_or(KeyFileError::InvalidHex { offset: i * 2 + 1 })?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; SEED_LEN] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn encode_matches_the_hex_the_config_store_holds() {
        let encoded = encode_seed(&SEED);
        assert_eq!(core::str::from_utf8(&encoded).unwrap(), HEX);
        assert_eq!(encoded.len(), HEX_LEN);
    }

    #[test]
    fn round_trips_through_a_saved_file() {
        let encoded = encode_seed(&SEED);
        let text = core::str::from_utf8(&encoded).unwrap();
        assert_eq!(decode_seed_file(text), Ok(SEED));
        assert_eq!(decode_seed_exact(text), Some(SEED));
    }

    #[test]
    fn file_reader_tolerates_surrounding_whitespace_and_crlf() {
        for text in [
            format!("{HEX}\n"),
            format!("{HEX}\r\n"),
            format!("  {HEX}\n\n"),
            format!("\t{HEX}\r\n"),
            format!("{HEX} "),
        ] {
            assert_eq!(decode_seed_file(&text), Ok(SEED), "should accept {text:?}");
        }
    }

    #[test]
    fn file_reader_accepts_uppercase_hex() {
        let upper = HEX.to_uppercase();
        assert_eq!(decode_seed_file(&upper), Ok(SEED));
        assert_eq!(decode_seed_exact(&upper), Some(SEED));
    }

    #[test]
    fn config_store_reader_stays_strict() {
        // The config store's own parsing must not become more forgiving: a
        // stored value that isn't exactly 64 hex characters is not a key.
        for text in [
            format!("{HEX}\n"),
            format!("{HEX} "),
            HEX[..HEX_LEN - 1].to_string(),
            format!("{HEX}0"),
            String::new(),
        ] {
            assert_eq!(decode_seed_exact(&text), None, "should reject {text:?}");
        }
    }

    #[test]
    fn wrong_length_reports_the_length_it_saw() {
        let short = &HEX[..HEX_LEN - 2];
        assert_eq!(
            decode_seed_file(short),
            Err(KeyFileError::WrongLength { found: HEX_LEN - 2 })
        );
        assert_eq!(
            decode_seed_file(&format!("{HEX}00")),
            Err(KeyFileError::WrongLength { found: HEX_LEN + 2 })
        );
        assert_eq!(
            decode_seed_file("0001 0203"),
            Err(KeyFileError::WrongLength { found: 9 })
        );
    }

    #[test]
    fn empty_and_whitespace_only_files_are_empty() {
        assert_eq!(decode_seed_file(""), Err(KeyFileError::Empty));
        assert_eq!(decode_seed_file("\n\n"), Err(KeyFileError::Empty));
        assert_eq!(decode_seed_file("   "), Err(KeyFileError::Empty));
    }

    #[test]
    fn non_hex_characters_report_their_offset() {
        // 'z' at index 4 of the trimmed text.
        let bad = format!("{}z{}", &HEX[..4], &HEX[5..]);
        assert_eq!(bad.len(), HEX_LEN);
        assert_eq!(
            decode_seed_file(&format!("  {bad}\n")),
            Err(KeyFileError::InvalidHex { offset: 4 })
        );
        // A lone 'g' as the last character.
        assert_eq!(
            decode_seed_file(&format!("{}g", &HEX[..HEX_LEN - 1])),
            Err(KeyFileError::InvalidHex {
                offset: HEX_LEN - 1
            })
        );
    }

    #[test]
    fn error_display_is_human_readable_and_one_based() {
        assert_eq!(
            decode_seed_file("").unwrap_err().to_string(),
            "the file is empty"
        );
        assert_eq!(
            decode_seed_file("00").unwrap_err().to_string(),
            format!("expected {HEX_LEN} hex characters, found 2")
        );
        assert_eq!(
            decode_seed_file(&format!("g{}", &HEX[1..]))
                .unwrap_err()
                .to_string(),
            "character 1 is not a hex digit"
        );
    }
}
