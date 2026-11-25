//! Password protection with the standard security handler, revision 6:
//! AES-256 (ISO 32000-2, §7.6.4), which every current reader opens.
//!
//! Everything a reader decodes is encrypted with one random 256-bit file
//! key: each stream and each string, with AES-256-CBC, a random IV in front
//! and PKCS#7 padding. The key is stored twice: wrapped by the user's
//! password (`/U`, `/UE`) and by a random owner password nobody knows (`/O`,
//! `/OE`). Every permission is granted, so the password only guards opening
//! the file and there is nothing extra an owner password would unlock.

use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use pdf_writer::{Dict, Name, Str};
use sha2::{Digest, Sha256, Sha384, Sha512};

/// Passwords are compared as at most this many UTF-8 bytes (ISO 32000-2).
pub(crate) const MAX_PASSWORD_BYTES: usize = 127;
/// Every permission (print, copy, edit…); bits 1–2 must be zero.
const PERMISSIONS: i32 = -4;

pub(crate) struct Encryption {
    key: [u8; 32],
    owner: Vec<u8>,
    user: Vec<u8>,
    owner_key: Vec<u8>,
    user_key: Vec<u8>,
    perms: [u8; 16],
}

impl Encryption {
    /// Sets up encryption for `password`, already validated as 1 to
    /// [`MAX_PASSWORD_BYTES`] bytes.
    pub(crate) fn new(password: &str) -> Result<Self, getrandom::Error> {
        let user_password = password.as_bytes();
        let mut owner_password = [0u8; 32];
        let mut key = [0u8; 32];
        // User validation and key salts, then the owner's (Algorithms 8 and 9).
        let mut salts = [0u8; 32];
        getrandom::fill(&mut owner_password)?;
        getrandom::fill(&mut key)?;
        getrandom::fill(&mut salts)?;
        let (user_salts, owner_salts) = salts.split_at(16);
        let (user_validation, user_key_salt) = user_salts.split_at(8);
        let (owner_validation, owner_key_salt) = owner_salts.split_at(8);

        let mut user = hash(user_password, user_validation, &[]).to_vec();
        user.extend_from_slice(user_salts);
        let user_key = wrap_key(&hash(user_password, user_key_salt, &[]), &key);

        let mut owner = hash(&owner_password, owner_validation, &user).to_vec();
        owner.extend_from_slice(owner_salts);
        let owner_key = wrap_key(&hash(&owner_password, owner_key_salt, &user), &key);

        // Algorithm 10: the permissions, encrypted so they can't be altered.
        let mut perms = [0u8; 16];
        perms[..4].copy_from_slice(&PERMISSIONS.to_le_bytes());
        perms[4..8].fill(0xFF);
        perms[8] = b'T'; // metadata encrypted too
        perms[9..12].copy_from_slice(b"adb");
        getrandom::fill(&mut perms[12..])?;
        Aes256::new(&key.into()).encrypt_block((&mut perms).into());

        Ok(Self {
            key,
            owner,
            user,
            owner_key,
            user_key,
            perms,
        })
    }

    /// `data` encrypted for a stream or string: a random IV, then the
    /// AES-256-CBC ciphertext of `data` padded with PKCS#7.
    pub(crate) fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, getrandom::Error> {
        let mut iv = [0u8; 16];
        getrandom::fill(&mut iv)?;
        let padding = 16 - data.len() % 16;
        let mut out = Vec::with_capacity(16 + data.len() + padding);
        out.extend_from_slice(&iv);
        out.extend_from_slice(data);
        out.resize(16 + data.len() + padding, padding as u8);
        let cipher = Aes256::new(&self.key.into());
        cbc(&mut out[16..], iv, |block| {
            cipher.encrypt_block(block.into())
        });
        Ok(out)
    }

    /// Writes the encryption dictionary's entries into `dict`.
    pub(crate) fn write_dict(&self, dict: &mut Dict) {
        dict.pair(Name(b"Filter"), Name(b"Standard"));
        dict.pair(Name(b"V"), 5);
        dict.pair(Name(b"R"), 6);
        dict.pair(Name(b"Length"), 256);
        {
            let mut filters = dict.insert(Name(b"CF")).dict();
            let mut std = filters.insert(Name(b"StdCF")).dict();
            std.pair(Name(b"Type"), Name(b"CryptFilter"));
            std.pair(Name(b"CFM"), Name(b"AESV3"));
            std.pair(Name(b"AuthEvent"), Name(b"DocOpen"));
            std.pair(Name(b"Length"), 32);
        }
        dict.pair(Name(b"StmF"), Name(b"StdCF"));
        dict.pair(Name(b"StrF"), Name(b"StdCF"));
        dict.pair(Name(b"O"), Str(&self.owner));
        dict.pair(Name(b"U"), Str(&self.user));
        dict.pair(Name(b"OE"), Str(&self.owner_key));
        dict.pair(Name(b"UE"), Str(&self.user_key));
        dict.pair(Name(b"P"), PERMISSIONS);
        dict.pair(Name(b"Perms"), Str(&self.perms));
        dict.pair(Name(b"EncryptMetadata"), true);
    }
}

/// Algorithm 2.B: the hash of a password with a salt (and, for the owner,
/// the user's `/U`), hardened by at least 64 rounds of AES and SHA-2.
fn hash(password: &[u8], salt: &[u8], user: &[u8]) -> [u8; 32] {
    let mut k = Sha256::new()
        .chain_update(password)
        .chain_update(salt)
        .chain_update(user)
        .finalize()
        .to_vec();
    let mut round: u32 = 0;
    loop {
        // 64 copies: always a whole number of AES blocks.
        let mut e = Vec::with_capacity(64 * (password.len() + k.len() + user.len()));
        for _ in 0..64 {
            e.extend_from_slice(password);
            e.extend_from_slice(&k);
            e.extend_from_slice(user);
        }
        let (key, iv) = k.split_at(16);
        let mut iv_block = [0u8; 16];
        iv_block.copy_from_slice(&iv[..16]);
        let mut key_block = [0u8; 16];
        key_block.copy_from_slice(key);
        let cipher = Aes128::new(&key_block.into());
        cbc(&mut e, iv_block, |block| cipher.encrypt_block(block.into()));

        k = match e[..16].iter().map(|&b| u32::from(b)).sum::<u32>() % 3 {
            0 => Sha256::digest(&e).to_vec(),
            1 => Sha384::digest(&e).to_vec(),
            _ => Sha512::digest(&e).to_vec(),
        };
        round += 1;
        let last = u32::from(e.last().copied().unwrap_or_default());
        if round >= 64 && last + 32 <= round {
            break;
        }
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&k[..32]);
    out
}

/// The file key encrypted with `wrapping_key`: AES-256-CBC, zero IV, no
/// padding (`/UE` and `/OE`).
fn wrap_key(wrapping_key: &[u8; 32], file_key: &[u8; 32]) -> Vec<u8> {
    let mut out = file_key.to_vec();
    let cipher = Aes256::new(&(*wrapping_key).into());
    cbc(&mut out, [0; 16], |block| {
        cipher.encrypt_block(block.into())
    });
    out
}

/// CBC encryption in place of `data`, a whole number of 16-byte blocks.
fn cbc(data: &mut [u8], iv: [u8; 16], mut encrypt_block: impl FnMut(&mut [u8; 16])) {
    let mut previous = iv;
    for chunk in data.chunks_exact_mut(16) {
        let mut block = [0u8; 16];
        for ((b, &c), &p) in block.iter_mut().zip(chunk.iter()).zip(previous.iter()) {
            *b = c ^ p;
        }
        encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
        previous = block;
    }
}

/// A text string as PDF stores it before encryption: UTF-16BE with a BOM.
pub(crate) fn text_string(text: &str) -> Vec<u8> {
    let mut out = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

/// Adds `/Encrypt <id> 0 R` to the trailer of a finished PDF. The trailer
/// comes after the cross-reference table, so no object moves.
pub(crate) fn link_trailer(pdf: &mut Vec<u8>, encrypt_id: i32) -> bool {
    const MARKER: &[u8] = b"trailer\n<<";
    let Some(at) = pdf.windows(MARKER.len()).rposition(|w| w == MARKER) else {
        return false;
    };
    let entry = format!("\n  /Encrypt {encrypt_id} 0 R");
    let insert_at = at + MARKER.len();
    pdf.splice(insert_at..insert_at, entry.into_bytes());
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A PDF reader's check of the user password (Algorithm 11) and of the
    /// file key it recovers (Algorithms 2.A and 13).
    #[test]
    fn the_user_password_unlocks_the_file_key() {
        let enc = Encryption::new("clave secreta").unwrap();
        let (validation, key_salt) = (&enc.user[32..40], &enc.user[40..48]);
        assert_eq!(hash(b"clave secreta", validation, &[]), enc.user[..32]);
        assert_ne!(hash(b"otra", validation, &[]), enc.user[..32]);

        // Decrypt /UE with the intermediate key: CBC decrypt by hand.
        use aes::cipher::BlockCipherDecrypt;
        let intermediate = hash(b"clave secreta", key_salt, &[]);
        let cipher = Aes256::new(&intermediate.into());
        let mut recovered = [0u8; 32];
        let mut previous = [0u8; 16];
        for (i, chunk) in enc.user_key.chunks_exact(16).enumerate() {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            cipher.decrypt_block((&mut block).into());
            for (j, b) in block.iter().enumerate() {
                recovered[i * 16 + j] = b ^ previous[j];
            }
            previous.copy_from_slice(chunk);
        }
        assert_eq!(recovered, enc.key);

        let mut perms = enc.perms;
        Aes256::new(&enc.key.into()).decrypt_block((&mut perms).into());
        assert_eq!(&perms[9..12], b"adb");
        assert_eq!(
            i32::from_le_bytes(perms[..4].try_into().unwrap()),
            PERMISSIONS
        );
    }

    #[test]
    fn encrypted_data_is_padded_and_never_repeats() {
        let enc = Encryption::new("x").unwrap();
        assert_eq!(
            enc.encrypt(b"").unwrap().len(),
            32,
            "IV + one block of padding"
        );
        assert_eq!(enc.encrypt(&[7; 16]).unwrap().len(), 48);
        assert_ne!(enc.encrypt(b"hola").unwrap(), enc.encrypt(b"hola").unwrap());
    }

    #[test]
    fn text_strings_are_utf16_with_a_bom() {
        assert_eq!(text_string("Añ"), [0xFE, 0xFF, 0x00, b'A', 0x00, 0xF1]);
    }
}
