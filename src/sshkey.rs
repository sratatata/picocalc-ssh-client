use crate::config::{CONFIG, StrValue};
use crate::storage::STORAGE;
use ed25519_dalek::SigningKey;
use embedded_sdmmc::{Mode, VolumeIdx};
use sunset::{KeyType, SignKey};
use terminal_model::keyfile;

const SSH_KEY_CONFIG: &str = "ssh_key";

/// Name of the private-key backup file in the root directory of the SD card's
/// first volume. FAT short names are upper-cased on the card, so this shows up
/// as `SSH_KEY.HEX` in a card reader (and in `ls`), but lookups here are
/// case-insensitive.
const KEY_FILE_NAME: &str = "ssh_key.hex";

/// Name of the public-key file in the root directory of the SD card's first
/// volume, written in the exact `authorized_keys` line form printed by
/// `print_public_key`. FAT short names are upper-cased on the card, so this
/// shows up as `SSH_KEY.PUB` in a card reader (and in `ls`), but lookups here
/// are case-insensitive. Unlike `KEY_FILE_NAME`, this file is not a secret and
/// is unconditionally (re)written on every `keygen`/`keygen force`/`keygen
/// show`, with no overwrite protection, so it can never go stale relative to
/// the key actually in use.
const PUBLIC_KEY_FILE_NAME: &str = "ssh_key.pub";

/// Loads the ed25519 signing key stored in flash, if one has been generated.
pub async fn load_signing_key() -> Option<SigningKey> {
    let mut config = CONFIG.get().lock().await;
    let stored = config.fetch(SSH_KEY_CONFIG).await.ok()??;
    let seed = keyfile::decode_seed_exact(stored.as_str())?;
    Some(SigningKey::from_bytes(&seed))
}

async fn store_signing_key(key: &SigningKey) -> Result<(), ()> {
    let hex = keyfile::encode_seed(&key.to_bytes());
    // Always ASCII hex, so this conversion never fails.
    let hex = core::str::from_utf8(&hex).map_err(|_| ())?;
    let value: StrValue = hex.try_into().map_err(|_| ())?;
    let mut config = CONFIG.get().lock().await;
    config.store(SSH_KEY_CONFIG, value).await.map_err(|_| ())
}

async fn print_public_key(key: &SigningKey) {
    let blob = pubkey_blob(&key.verifying_key().to_bytes());
    let encoded = base64_encode(&blob);
    // Also log it, so it can be copied exactly from a serial terminal
    // (USB-CDC or the debug UART) instead of hand-transcribed off the LCD.
    log::info!("ssh-ed25519 {encoded} picocalc-ssh-client");
    print!("ssh-ed25519 {encoded} picocalc-ssh-client\r\n");
    write_public_key_to_sd(&encoded).await;
}

/// Best-effort mirror of the public key to `ssh_key.pub` in the SD card's
/// root directory, so pulling the card and reading the file is enough to get
/// the key without transcribing it off the LCD or serial console. Unlike
/// `save_key_command`, this always overwrites: the public key isn't a
/// secret, and a stale mismatched copy on the card would be actively
/// unhelpful. Never fails the caller — no SD card, or any write error, just
/// prints one clear message and returns.
async fn write_public_key_to_sd(encoded: &heapless::String<96>) {
    let mut storage = STORAGE.get().lock().await;
    let Some(mgr) = storage.vol_mgr() else {
        print!(
            "No SD card is present; not mirroring the public key to {PUBLIC_KEY_FILE_NAME}.\r\n"
        );
        return;
    };

    let mut vol = match mgr.open_volume(VolumeIdx(0)) {
        Ok(vol) => vol,
        Err(err) => {
            print!(
                "Failed to open vol0: {err:?}; not mirroring the public key to {PUBLIC_KEY_FILE_NAME}.\r\n"
            );
            return;
        }
    };
    let mut root = match vol.open_root_dir() {
        Ok(root) => root,
        Err(err) => {
            print!(
                "Failed to open the root directory on vol0: {err:?}; not mirroring the public \
                 key to {PUBLIC_KEY_FILE_NAME}.\r\n"
            );
            return;
        }
    };

    let mut file =
        match root.open_file_in_dir(PUBLIC_KEY_FILE_NAME, Mode::ReadWriteCreateOrTruncate) {
            Ok(file) => file,
            Err(err) => {
                print!("Failed to write {PUBLIC_KEY_FILE_NAME} to the SD card: {err:?}\r\n");
                return;
            }
        };

    let write_err = file
        .write(b"ssh-ed25519 ")
        .and_then(|_| file.write(encoded.as_bytes()))
        .and_then(|_| file.write(b" picocalc-ssh-client\n"))
        .err();
    if let Some(err) = write_err {
        print!("Failed to write {PUBLIC_KEY_FILE_NAME} to the SD card: {err:?}\r\n");
        file.close().ok();
        return;
    }
    // `close` flushes the directory entry, so its error is a real write error.
    if let Err(err) = file.close() {
        print!("Failed to write {PUBLIC_KEY_FILE_NAME} to the SD card: {err:?}\r\n");
        return;
    }

    print!(
        "Mirrored the public key to {PUBLIC_KEY_FILE_NAME} in the SD card's root directory.\r\n"
    );
}

/// Handles the local `keygen` shell command: generates and stores a new
/// on-device Ed25519 keypair (the private key never leaves the device unless
/// explicitly exported with `keygen save`), or re-displays the public key of
/// an already generated one.
pub async fn keygen_command(args: &[&str]) {
    match args {
        ["keygen"] | ["keygen", "force"] => {
            let force = args.len() == 2;
            if !force && load_signing_key().await.is_some() {
                print!(
                    "A key already exists. Use `keygen force` to replace it \
                     (servers you've already authorized will stop accepting it).\r\n"
                );
                return;
            }
            let generated = match SignKey::generate(KeyType::Ed25519, None) {
                Ok(k) => k,
                Err(err) => {
                    print!("keygen failed: {err:?}\r\n");
                    return;
                }
            };
            let key = match &generated {
                SignKey::Ed25519(k) => k.clone(),
                _ => unreachable!(),
            };
            if store_signing_key(&key).await.is_err() {
                print!("failed to store key\r\n");
                return;
            }
            print!(
                "New SSH key generated. Add this line to the server's \
                 ~/.ssh/authorized_keys:\r\n\r\n"
            );
            print_public_key(&key).await;
        }
        ["keygen", "show"] => match load_signing_key().await {
            Some(key) => print_public_key(&key).await,
            None => print!("No SSH key configured. Run `keygen` to create one.\r\n"),
        },
        ["keygen", "save"] | ["keygen", "save", "force"] => {
            save_key_command(args.len() == 3).await;
        }
        ["keygen", "load"] | ["keygen", "load", "force"] => {
            load_key_command(args.len() == 3).await;
        }
        _ => {
            print!("Usage: keygen [force|show]\r\n");
            print!(
                "       keygen save [force]   (write the key to {KEY_FILE_NAME} on the SD card)\r\n"
            );
            print!(
                "       keygen load [force]   (restore the key from {KEY_FILE_NAME} on the SD card)\r\n"
            );
            print!(
                "       (the public key is also mirrored to {PUBLIC_KEY_FILE_NAME} on the SD \
                 card whenever it's generated or shown)\r\n"
            );
        }
    }
}

/// `keygen save [force]`: writes the existing on-device private key to
/// `ssh_key.hex` in the SD card's root directory, in the same 64-character
/// hex form the flash config store holds. Refuses to clobber an existing
/// backup file unless `force` is given, matching `keygen`'s refusal to
/// silently replace an existing key.
async fn save_key_command(force: bool) {
    let Some(key) = load_signing_key().await else {
        print!("No SSH key configured. Run `keygen` to create one.\r\n");
        return;
    };
    let hex = keyfile::encode_seed(&key.to_bytes());

    let mut storage = STORAGE.get().lock().await;
    let Some(mgr) = storage.vol_mgr() else {
        print!("No SD card is present\r\n");
        return;
    };

    let mut vol = match mgr.open_volume(VolumeIdx(0)) {
        Ok(vol) => vol,
        Err(err) => {
            print!("Failed to open vol0: {err:?}\r\n");
            return;
        }
    };
    let mut root = match vol.open_root_dir() {
        Ok(root) => root,
        Err(err) => {
            print!("Failed to open the root directory on vol0: {err:?}\r\n");
            return;
        }
    };

    let mode = if force {
        Mode::ReadWriteCreateOrTruncate
    } else {
        Mode::ReadWriteCreate
    };
    let mut file = match root.open_file_in_dir(KEY_FILE_NAME, mode) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::FileAlreadyExists) => {
            print!(
                "{KEY_FILE_NAME} already exists on the SD card. Use `keygen save force` \
                 to overwrite it.\r\n"
            );
            return;
        }
        Err(err) => {
            print!("Failed to create {KEY_FILE_NAME} on the SD card: {err:?}\r\n");
            return;
        }
    };

    if let Err(err) = file.write(&hex) {
        print!("Failed to write {KEY_FILE_NAME}: {err:?}\r\n");
        print!("The SD card may now hold an incomplete copy; do not rely on it.\r\n");
        file.close().ok();
        return;
    }
    // `close` flushes the directory entry, so its error is a real write error.
    if let Err(err) = file.close() {
        print!("Failed to save {KEY_FILE_NAME}: {err:?}\r\n");
        print!("The SD card may now hold an incomplete copy; do not rely on it.\r\n");
        return;
    }

    print!("Saved the SSH private key to {KEY_FILE_NAME} in the SD card's root directory.\r\n");
    print!(
        "That file is the private key in plain text: treat the card as a secret, \
         and don't leave it lying around.\r\n"
    );
}

/// `keygen load [force]`: restores the private key from `ssh_key.hex` in the
/// SD card's root directory. Refuses to replace an existing on-device key
/// unless `force` is given, exactly like `keygen` itself.
async fn load_key_command(force: bool) {
    if !force && load_signing_key().await.is_some() {
        print!(
            "A key already exists. Use `keygen load force` to replace it \
             (servers you've already authorized will stop accepting it).\r\n"
        );
        return;
    }

    let seed = {
        let mut storage = STORAGE.get().lock().await;
        let Some(mgr) = storage.vol_mgr() else {
            print!("No SD card is present\r\n");
            return;
        };

        let mut vol = match mgr.open_volume(VolumeIdx(0)) {
            Ok(vol) => vol,
            Err(err) => {
                print!("Failed to open vol0: {err:?}\r\n");
                return;
            }
        };
        let mut root = match vol.open_root_dir() {
            Ok(root) => root,
            Err(err) => {
                print!("Failed to open the root directory on vol0: {err:?}\r\n");
                return;
            }
        };

        let mut file = match root.open_file_in_dir(KEY_FILE_NAME, Mode::ReadOnly) {
            Ok(file) => file,
            Err(embedded_sdmmc::Error::NotFound) => {
                print!("No {KEY_FILE_NAME} in the SD card's root directory.\r\n");
                return;
            }
            Err(err) => {
                print!("Failed to open {KEY_FILE_NAME} on the SD card: {err:?}\r\n");
                return;
            }
        };

        // The file should hold 64 hex characters, optionally with a trailing
        // newline; anything bigger than this is malformed, not just padded.
        const MAX_KEY_FILE_BYTES: usize = 128;
        let mut buf = [0u8; MAX_KEY_FILE_BYTES];
        let mut len = 0;
        loop {
            match file.read(&mut buf[len..]) {
                Ok(0) => break,
                Ok(n) => {
                    len += n;
                    if len == buf.len() {
                        print!(
                            "{KEY_FILE_NAME} is malformed: it is larger than {MAX_KEY_FILE_BYTES} \
                             bytes, not {} hex characters.\r\n",
                            keyfile::HEX_LEN
                        );
                        file.close().ok();
                        return;
                    }
                }
                Err(err) => {
                    print!("Failed to read {KEY_FILE_NAME}: {err:?}\r\n");
                    file.close().ok();
                    return;
                }
            }
        }
        file.close().ok();

        let text = match core::str::from_utf8(&buf[..len]) {
            Ok(text) => text,
            Err(_) => {
                print!("{KEY_FILE_NAME} is malformed: it is not ASCII text.\r\n");
                return;
            }
        };
        match keyfile::decode_seed_file(text) {
            Ok(seed) => seed,
            Err(err) => {
                print!("{KEY_FILE_NAME} is malformed: {err}.\r\n");
                return;
            }
        }
    };

    // The stored key is only replaced once the file has been read and fully
    // validated, so a bad file can never leave a half-applied key behind.
    let key = SigningKey::from_bytes(&seed);
    if store_signing_key(&key).await.is_err() {
        print!("failed to store key\r\n");
        return;
    }

    print!("Loaded the SSH private key from {KEY_FILE_NAME} on the SD card.\r\n\r\n");
    print_public_key(&key).await;
}

/// SSH wire-format public key blob: string("ssh-ed25519") + string(pubkey).
fn pubkey_blob(pubkey: &[u8; 32]) -> [u8; 51] {
    let mut blob = [0u8; 51];
    let name = b"ssh-ed25519";
    blob[0..4].copy_from_slice(&(name.len() as u32).to_be_bytes());
    blob[4..15].copy_from_slice(name);
    blob[15..19].copy_from_slice(&(pubkey.len() as u32).to_be_bytes());
    blob[19..51].copy_from_slice(pubkey);
    blob
}

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> heapless::String<96> {
    let mut out = heapless::String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char)
            .ok();
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char)
            .ok();
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        })
        .ok();
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        })
        .ok();
    }
    out
}
