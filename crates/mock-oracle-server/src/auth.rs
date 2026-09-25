//! O5LOGON with the 12c password verifier (PBKDF2-SHA512 + AES-256-CBC), the
//! server side of what thin drivers implement in `encryptDecrypt`.
//!
//! Phase one: the server sends a random session key encrypted with a key derived
//! from the password. Phase two: the client proves it could decrypt it by sending
//! its own session key and the password encrypted with the combined key, and the
//! server proves the same back with `AUTH_SVR_RESPONSE`.

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use rand::RngCore;
use sha2::{Digest, Sha512};

type Enc = cbc::Encryptor<aes::Aes256>;
type Dec = cbc::Decryptor<aes::Aes256>;

pub const VERIFIER_TYPE_12C: u32 = 0x4815;
const VGEN_COUNT: u32 = 4096;
const SDER_COUNT: u32 = 3;

/// Server-side state between the two login phases.
pub struct Challenge {
    csk_salt: [u8; 16],
    password_hash: [u8; 32],
    server_key: [u8; 32],
}

/// The key/value pairs sent to the client in the phase-one response.
pub struct PhaseOne {
    pub sesskey: String,
    pub vfr_data: String,
    pub csk_salt: String,
    pub vgen_count: String,
    pub sder_count: String,
}

pub enum LoginError {
    InvalidCredentials,
    Protocol(&'static str),
}

impl Challenge {
    pub fn new(password: &str) -> (Self, PhaseOne) {
        let mut rng = rand::thread_rng();
        let mut verifier_salt = [0u8; 16];
        let mut csk_salt = [0u8; 16];
        let mut server_key = [0u8; 32];
        rng.fill_bytes(&mut verifier_salt);
        rng.fill_bytes(&mut csk_salt);
        rng.fill_bytes(&mut server_key);

        let mut salt = verifier_salt.to_vec();
        salt.extend_from_slice(b"AUTH_PBKDF2_SPEEDY_KEY");
        let mut password_key = [0u8; 64];
        pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), &salt, VGEN_COUNT, &mut password_key);
        let mut h = Sha512::new();
        h.update(password_key);
        h.update(verifier_salt);
        let password_hash: [u8; 32] = h.finalize()[..32].try_into().unwrap();

        let encrypted_server_key = encrypt(&password_hash, &server_key);
        let reply = PhaseOne {
            sesskey: hex::encode_upper(encrypted_server_key),
            vfr_data: hex::encode_upper(verifier_salt),
            csk_salt: hex::encode_upper(csk_salt),
            vgen_count: VGEN_COUNT.to_string(),
            sder_count: SDER_COUNT.to_string(),
        };
        (
            Self {
                csk_salt,
                password_hash,
                server_key,
            },
            reply,
        )
    }

    /// Checks the client's phase-two values. On success returns the value of
    /// `AUTH_SVR_RESPONSE`.
    pub fn verify(
        &self,
        client_sesskey: &str,
        encrypted_password: &str,
        password: &str,
    ) -> Result<String, LoginError> {
        let client_sesskey = hex::decode(client_sesskey)
            .map_err(|_| LoginError::Protocol("AUTH_SESSKEY is not hex"))?;
        if client_sesskey.len() != 32 {
            return Err(LoginError::Protocol("AUTH_SESSKEY has the wrong length"));
        }
        let client_key = decrypt(&self.password_hash, &client_sesskey);

        let mut mixed = client_key[..32].to_vec();
        mixed.extend_from_slice(&self.server_key);
        let mixed_hex = hex::encode_upper(mixed);
        let mut combo_key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha512>(
            mixed_hex.as_bytes(),
            &self.csk_salt,
            SDER_COUNT,
            &mut combo_key,
        );

        let encrypted_password =
            hex::decode(encrypted_password).map_err(|_| LoginError::InvalidCredentials)?;
        if encrypted_password.len() < 32 || encrypted_password.len() % 16 != 0 {
            return Err(LoginError::InvalidCredentials);
        }
        let plain = decrypt(&combo_key, &encrypted_password);
        let pad = *plain.last().unwrap() as usize;
        if pad == 0 || pad > 16 || plain.len() < 16 + pad {
            return Err(LoginError::InvalidCredentials);
        }
        if &plain[16..plain.len() - pad] != password.as_bytes() {
            return Err(LoginError::InvalidCredentials);
        }

        let mut response = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut response[..16]);
        response[16..].copy_from_slice(b"SERVER_TO_CLIENT");
        Ok(hex::encode_upper(encrypt(&combo_key, &response)))
    }
}

/// AES-256-CBC with a zero IV and no padding; `data` must be block-aligned.
fn encrypt(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut buf = data.to_vec();
    Enc::new(key.into(), &[0u8; 16].into())
        .encrypt_padded_mut::<NoPadding>(&mut buf, data.len())
        .expect("block-aligned input");
    buf
}

fn decrypt(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut buf = data.to_vec();
    Dec::new(key.into(), &[0u8; 16].into())
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .expect("block-aligned input");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays the client side exactly as node-oracledb does, then checks the server accepts it.
    fn client_login(reply: &PhaseOne, password: &str) -> (String, String, [u8; 32]) {
        let vfr = hex::decode(&reply.vfr_data).unwrap();
        let mut salt = vfr.clone();
        salt.extend_from_slice(b"AUTH_PBKDF2_SPEEDY_KEY");
        let mut password_key = [0u8; 64];
        pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), &salt, 4096, &mut password_key);
        let mut h = Sha512::new();
        h.update(password_key);
        h.update(&vfr);
        let password_hash: [u8; 32] = h.finalize()[..32].try_into().unwrap();

        let server_key = decrypt(&password_hash, &hex::decode(&reply.sesskey).unwrap());
        let client_key = [9u8; 32];
        let sesskey = hex::encode_upper(encrypt(&password_hash, &client_key));

        let mut mixed = client_key.to_vec();
        mixed.extend_from_slice(&server_key);
        let mut combo = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha512>(
            hex::encode_upper(mixed).as_bytes(),
            &hex::decode(&reply.csk_salt).unwrap(),
            3,
            &mut combo,
        );
        let mut plain = vec![1u8; 16];
        plain.extend_from_slice(password.as_bytes());
        let n = 16 - plain.len() % 16;
        plain.extend(std::iter::repeat(n as u8).take(n));
        (sesskey, hex::encode_upper(encrypt(&combo, &plain)), combo)
    }

    #[test]
    fn accepts_the_right_password() {
        let (challenge, reply) = Challenge::new("secret");
        let (sesskey, password, combo) = client_login(&reply, "secret");
        let response = challenge
            .verify(&sesskey, &password, "secret")
            .ok()
            .unwrap();
        let decoded = decrypt(&combo, &hex::decode(response).unwrap());
        assert_eq!(&decoded[16..], b"SERVER_TO_CLIENT");
    }

    #[test]
    fn rejects_a_wrong_password() {
        let (challenge, reply) = Challenge::new("secret");
        let (sesskey, password, _) = client_login(&reply, "wrong");
        assert!(matches!(
            challenge.verify(&sesskey, &password, "secret"),
            Err(LoginError::InvalidCredentials)
        ));
    }
}
