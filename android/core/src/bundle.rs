//! Connections sealed into a file to share: AES-256-GCM under a key drawn
//! from a passphrase with PBKDF2-HMAC-SHA256.
//!
//! The file is `DBDELVE1`, a 16-byte salt, the iteration count (u32, big
//! endian), a 12-byte nonce, then the ciphertext and its tag. The whole
//! header is authenticated with it, so a changed byte anywhere, or a wrong
//! passphrase, fails to open instead of opening as something else. What is
//! sealed is the app's own JSON; this module neither reads nor writes it.

use std::num::NonZeroU32;

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};

use crate::CoreError;

const MAGIC: &[u8; 8] = b"DBDELVE1";
const SALT: usize = 16;
const NONCE: usize = 12;
const HEADER: usize = MAGIC.len() + SALT + 4 + NONCE;
/// OWASP's figure for PBKDF2-HMAC-SHA256: under a second on a phone, and
/// slow enough per guess that a passphrase worth the name holds.
const ITERATIONS: u32 = 600_000;
/// A file asking for more would hang the phone opening it.
const MOST_ITERATIONS: u32 = 10_000_000;
/// Short passphrases fall to guessing however slow each guess is.
pub const SHORTEST_PASSPHRASE: usize = 8;

fn error(reason: &str) -> CoreError {
    CoreError::Query {
        reason: reason.to_owned(),
    }
}

fn key(passphrase: &str, salt: &[u8], iterations: u32) -> LessSafeKey {
    let mut bytes = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations).expect("iterations are checked to be positive"),
        salt,
        passphrase.as_bytes(),
        &mut bytes,
    );
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &bytes).expect("32 bytes is an AES-256 key"))
}

pub fn seal(plaintext: &str, passphrase: &str) -> Result<Vec<u8>, CoreError> {
    seal_with(plaintext, passphrase, ITERATIONS)
}

fn seal_with(plaintext: &str, passphrase: &str, iterations: u32) -> Result<Vec<u8>, CoreError> {
    if passphrase.chars().count() < SHORTEST_PASSPHRASE {
        return Err(error(&format!(
            "Use a passphrase of at least {SHORTEST_PASSPHRASE} characters."
        )));
    }
    let random = SystemRandom::new();
    let mut header = Vec::with_capacity(HEADER + plaintext.len() + 16);
    header.extend_from_slice(MAGIC);
    let mut salt = [0u8; SALT];
    let mut nonce = [0u8; NONCE];
    random
        .fill(&mut salt)
        .and_then(|()| random.fill(&mut nonce))
        .map_err(|_| error("No secure random numbers available."))?;
    header.extend_from_slice(&salt);
    header.extend_from_slice(&iterations.to_be_bytes());
    header.extend_from_slice(&nonce);
    let mut body = plaintext.as_bytes().to_vec();
    key(passphrase, &salt, iterations)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(&header),
            &mut body,
        )
        .map_err(|_| error("Could not encrypt."))?;
    header.extend_from_slice(&body);
    Ok(header)
}

pub fn open(sealed: &[u8], passphrase: &str) -> Result<String, CoreError> {
    if sealed.len() < HEADER + 16 || &sealed[..MAGIC.len()] != MAGIC {
        return Err(error("This is not a DBDelve connections file."));
    }
    let (header, body) = sealed.split_at(HEADER);
    let salt = &header[MAGIC.len()..MAGIC.len() + SALT];
    let iterations = u32::from_be_bytes(
        header[MAGIC.len() + SALT..MAGIC.len() + SALT + 4]
            .try_into()
            .expect("four bytes"),
    );
    if iterations == 0 || iterations > MOST_ITERATIONS {
        return Err(error(
            "This file asks for an unreasonable amount of work to open.",
        ));
    }
    let nonce: [u8; NONCE] = header[HEADER - NONCE..].try_into().expect("twelve bytes");
    let mut body = body.to_vec();
    let plaintext = key(passphrase, salt, iterations)
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(header),
            &mut body,
        )
        .map_err(|_| error("Wrong passphrase, or the file was changed."))?;
    String::from_utf8(plaintext.to_vec()).map_err(|_| error("The file holds no text."))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A low count keeps the tests fast; the format carries whatever was used.
    fn sealed(text: &str) -> Vec<u8> {
        seal_with(text, "correct horse", 1_000).unwrap()
    }

    #[test]
    fn opens_with_the_passphrase_only() {
        let file = sealed(r#"[{"name":"prod","password":"s3cret"}]"#);
        assert_eq!(
            open(&file, "correct horse").unwrap(),
            r#"[{"name":"prod","password":"s3cret"}]"#
        );
        assert!(open(&file, "correct horsE").is_err());
        assert!(
            !String::from_utf8_lossy(&file).contains("s3cret"),
            "nothing in the clear"
        );
    }

    #[test]
    fn any_changed_byte_refuses_to_open() {
        let file = sealed("connections");
        for at in [
            0,
            9,
            MAGIC.len() + SALT + 2,
            HEADER - 1,
            HEADER,
            file.len() - 1,
        ] {
            let mut changed = file.clone();
            changed[at] ^= 1;
            assert!(open(&changed, "correct horse").is_err(), "byte {at}");
        }
        assert!(open(&file[..file.len() - 1], "correct horse").is_err());
        assert!(open(b"DBDELVE1", "correct horse").is_err());
    }

    #[test]
    fn each_file_is_salted_and_needs_a_real_passphrase() {
        assert_ne!(sealed("same"), sealed("same"));
        assert!(seal("x", "short").is_err());
        let mut greedy = sealed("x");
        greedy[MAGIC.len() + SALT..MAGIC.len() + SALT + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(open(&greedy, "correct horse").is_err());
    }
}
