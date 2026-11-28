//! The keeper's key at rest: a Web3 Secret Storage (version 3) keystore,
//! the JSON file `geth`, `cast wallet` and most wallets write. The password
//! is typed by the owner when the signer starts (`password`); the key is
//! decrypted into memory and nowhere else.
//!
//! Both key derivations of the format are read (scrypt, and PBKDF2 with
//! HMAC-SHA256), with AES-128-CTR, the one cipher it defines. The MAC is
//! checked before anything is decrypted, so a wrong password is told as
//! such and nothing else.

use aes::cipher::{KeyIvInit, StreamCipher};
use alloy_primitives::keccak256;
use serde::Deserialize;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::tx::Key;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Why a keystore could not be opened. None of them carries key material.
#[derive(Debug, Error)]
pub enum KeystoreError {
    #[error("the keystore is not version-3 JSON as wallets write it: {0}")]
    Format(String),
    #[error("the keystore uses {0}, which the signer does not read; export it as a version-3 keystore with scrypt or pbkdf2 and aes-128-ctr")]
    Unsupported(String),
    #[error("wrong password for this keystore: start the signer again and type it again")]
    WrongPassword,
    #[error("the keystore's key is not a valid secp256k1 key")]
    BadKey,
}

#[derive(Deserialize)]
struct File {
    #[serde(alias = "Crypto")]
    crypto: Crypto,
    version: u32,
}

#[derive(Deserialize)]
struct Crypto {
    cipher: String,
    cipherparams: CipherParams,
    ciphertext: String,
    kdf: String,
    kdfparams: serde_json::Value,
    mac: String,
}

#[derive(Deserialize)]
struct CipherParams {
    iv: String,
}

#[derive(Deserialize)]
struct ScryptParams {
    dklen: usize,
    n: u64,
    r: u32,
    p: u32,
    salt: String,
}

#[derive(Deserialize)]
struct Pbkdf2Params {
    c: u32,
    dklen: usize,
    prf: String,
    salt: String,
}

fn unhex(field: &str, text: &str) -> Result<Vec<u8>, KeystoreError> {
    hex::decode(text.trim_start_matches("0x"))
        .map_err(|_| KeystoreError::Format(format!("`{field}` is not hex")))
}

fn params<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, KeystoreError> {
    serde_json::from_value(value.clone()).map_err(|e| KeystoreError::Format(e.to_string()))
}

/// The derived key, 32 bytes: the first half is the AES key, the second
/// goes into the MAC.
fn derive(crypto: &Crypto, password: &[u8]) -> Result<Zeroizing<[u8; 32]>, KeystoreError> {
    let mut key = Zeroizing::new([0u8; 32]);
    match crypto.kdf.as_str() {
        "scrypt" => {
            let p: ScryptParams = params(&crypto.kdfparams)?;
            if p.dklen != 32 {
                return Err(KeystoreError::Unsupported(format!(
                    "a {}-byte key",
                    p.dklen
                )));
            }
            if !p.n.is_power_of_two() || p.n < 2 {
                return Err(KeystoreError::Format(format!(
                    "scrypt n is {}, which is not a power of two",
                    p.n
                )));
            }
            let log_n = p.n.trailing_zeros() as u8;
            // The scrypt standard needs n below 2^(16·r); geth, cast and
            // wallets write r = 8, which allows any n they use.
            if u32::from(log_n) >= p.r.saturating_mul(16) {
                return Err(KeystoreError::Unsupported(format!(
                    "scrypt with n = {} and r = {}, outside what the scrypt standard allows \
                     (n below 2^(16·r)); keystores from geth, cast or a wallet use r = 8",
                    p.n, p.r
                )));
            }
            let scrypt = scrypt::Params::new(log_n, p.r, p.p, 32)
                .map_err(|e| KeystoreError::Format(format!("scrypt parameters: {e}")))?;
            scrypt::scrypt(password, &unhex("salt", &p.salt)?, &scrypt, key.as_mut())
                .map_err(|e| KeystoreError::Format(format!("scrypt: {e}")))?;
        }
        "pbkdf2" => {
            let p: Pbkdf2Params = params(&crypto.kdfparams)?;
            if p.prf != "hmac-sha256" {
                return Err(KeystoreError::Unsupported(format!("pbkdf2 with {}", p.prf)));
            }
            if p.dklen != 32 {
                return Err(KeystoreError::Unsupported(format!(
                    "a {}-byte key",
                    p.dklen
                )));
            }
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
                password,
                &unhex("salt", &p.salt)?,
                p.c,
                key.as_mut(),
            );
        }
        other => {
            return Err(KeystoreError::Unsupported(format!(
                "the {other} key derivation"
            )))
        }
    }
    Ok(key)
}

/// Opens a keystore with its password.
pub fn decrypt(json: &str, password: &str) -> Result<Key, KeystoreError> {
    let file: File =
        serde_json::from_str(json).map_err(|e| KeystoreError::Format(e.to_string()))?;
    if file.version != 3 {
        return Err(KeystoreError::Unsupported(format!(
            "keystore version {}",
            file.version
        )));
    }
    let crypto = &file.crypto;
    if crypto.cipher != "aes-128-ctr" {
        return Err(KeystoreError::Unsupported(format!(
            "the {} cipher",
            crypto.cipher
        )));
    }
    let iv = unhex("iv", &crypto.cipherparams.iv)?;
    if iv.len() != 16 {
        return Err(KeystoreError::Format("the iv is not 16 bytes".into()));
    }
    let ciphertext = unhex("ciphertext", &crypto.ciphertext)?;
    let mac = unhex("mac", &crypto.mac)?;
    let derived = derive(crypto, password.as_bytes())?;

    let mut mac_input = Zeroizing::new(Vec::with_capacity(16 + ciphertext.len()));
    mac_input.extend_from_slice(&derived[16..32]);
    mac_input.extend_from_slice(&ciphertext);
    let expected = keccak256(mac_input.as_slice());
    // Compared in full whatever differs first.
    let differs = mac.len() != 32
        || expected
            .iter()
            .zip(&mac)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0;
    if differs {
        return Err(KeystoreError::WrongPassword);
    }

    let mut secret = Zeroizing::new(ciphertext);
    let mut cipher = Aes128Ctr::new(derived[..16].into(), iv.as_slice().into());
    cipher.apply_keystream(secret.as_mut_slice());
    if secret.len() != 32 {
        return Err(KeystoreError::BadKey);
    }
    Key::from_secret(&secret).map_err(|_| KeystoreError::BadKey)
}

/// Writes a keystore, for tests only: the signer never makes keys.
#[cfg(test)]
pub(crate) fn encrypt_for_test(secret: &[u8; 32], password: &str, log_n: u8) -> String {
    let salt = [7u8; 32];
    let iv = [9u8; 16];
    let mut derived = [0u8; 32];
    let p = scrypt::Params::new(log_n, 8, 1, 32).unwrap();
    scrypt::scrypt(password.as_bytes(), &salt, &p, &mut derived).unwrap();
    let mut ciphertext = secret.to_vec();
    Aes128Ctr::new(derived[..16].into(), iv.as_slice().into()).apply_keystream(&mut ciphertext);
    let mut mac_input = derived[16..32].to_vec();
    mac_input.extend_from_slice(&ciphertext);
    serde_json::json!({
        "address": "",
        "crypto": {
            "cipher": "aes-128-ctr",
            "cipherparams": { "iv": hex::encode(iv) },
            "ciphertext": hex::encode(&ciphertext),
            "kdf": "scrypt",
            "kdfparams": { "dklen": 32, "n": 1u64 << log_n, "r": 8, "p": 1, "salt": hex::encode(salt) },
            "mac": hex::encode(keccak256(&mac_input)),
        },
        "id": "00000000-0000-0000-0000-000000000000",
        "version": 3,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::tests::{DEV_ADDRESS, DEV_KEY};
    use alloy_primitives::{address, Address};

    /// The secret of both published test vectors of the Web3 Secret Storage
    /// definition, password `testpassword`.
    const VECTOR_ADDRESS_OF: [u8; 32] =
        alloy_primitives::hex!("7a28b5ba57c53603b0b07b56bba752f7784bf506fa95edc395f5cf6c7514fe9d");

    const PBKDF2_VECTOR: &str = r#"{
        "crypto" : {
            "cipher" : "aes-128-ctr",
            "cipherparams" : { "iv" : "6087dab2f9fdbbfaddc31a909735c1e6" },
            "ciphertext" : "5318b4d5bcd28de64ee5559e671353e16f075ecae9f99c7a79a38af5f869aa46",
            "kdf" : "pbkdf2",
            "kdfparams" : {
                "c" : 262144,
                "dklen" : 32,
                "prf" : "hmac-sha256",
                "salt" : "ae3cd4e7013836a3df6bd7241b12db061dbe2c6785853cce422d148a624ce0bd"
            },
            "mac" : "517ead924a9d0dc3124507e3393d175ce3ff7c1e96529c6c555ce9e51205e9b2"
        },
        "id" : "3198bc9c-6672-5ab3-d995-4942343ae5b6",
        "version" : 3
    }"#;

    const SCRYPT_VECTOR: &str = r#"{
        "crypto" : {
            "cipher" : "aes-128-ctr",
            "cipherparams" : { "iv" : "83dbcc02d8ccb40e466191a123791e0e" },
            "ciphertext" : "d172bf743a674da9cdad04534d56926ef8358534d458fffccd4e6ad2fbde479c",
            "kdf" : "scrypt",
            "kdfparams" : {
                "dklen" : 32,
                "n" : 262144,
                "p" : 8,
                "r" : 1,
                "salt" : "ab0c7876052600dd703518d6fc3fe8984592145b591fc8fb5c6d43190334ba19"
            },
            "mac" : "2103ac29920d71da29f15d75b4a16dbe95cfd7ff8faea1056c33131d846e3097"
        },
        "id" : "3198bc9c-6672-5ab3-d995-4942343ae5b6",
        "version" : 3
    }"#;

    fn vector_address() -> Address {
        Key::from_secret(&VECTOR_ADDRESS_OF).unwrap().address()
    }

    #[test]
    fn the_published_pbkdf2_vector_opens_with_its_password_only() {
        let key = decrypt(PBKDF2_VECTOR, "testpassword").unwrap();
        assert_eq!(key.address(), vector_address());
        assert!(matches!(
            decrypt(PBKDF2_VECTOR, "testpassworD"),
            Err(KeystoreError::WrongPassword)
        ));
    }

    #[test]
    fn the_published_scrypt_vector_is_refused_for_its_r_of_1() {
        // The definition's scrypt vector has n = 2^18 with r = 1, which the
        // scrypt standard (RFC 7914) does not allow and the `scrypt` crate
        // refuses. Keystores from geth, cast and wallets use r = 8: see the
        // next test, and the ignored one against a keystore cast wrote.
        let refused = decrypt(SCRYPT_VECTOR, "testpassword").unwrap_err();
        assert!(
            matches!(&refused, KeystoreError::Unsupported(m) if m.contains("r = 8")),
            "{refused}"
        );
    }

    /// `LPBOT_KEYSTORE_CHECK=<path>|<password>|<address>`: a keystore some
    /// other tool wrote opens to its address. Run by hand with a throwaway
    /// keystore (`cast wallet new <dir> --unsafe-password <p>`); none is kept
    /// in the repository.
    #[test]
    #[ignore = "needs a keystore written by another tool"]
    fn a_keystore_another_tool_wrote_opens_to_its_address() {
        let check = std::env::var("LPBOT_KEYSTORE_CHECK")
            .expect("set LPBOT_KEYSTORE_CHECK=<path>|<password>|<address>");
        let parts: Vec<&str> = check.split('|').collect();
        let json = std::fs::read_to_string(parts[0]).unwrap();
        let key = decrypt(&json, parts[1]).unwrap();
        assert_eq!(key.address(), parts[2].parse::<Address>().unwrap());
        assert!(matches!(
            decrypt(&json, "not the password"),
            Err(KeystoreError::WrongPassword)
        ));
    }

    #[test]
    fn a_keystore_written_for_the_development_key_opens_to_it() {
        let json = encrypt_for_test(&DEV_KEY, "a long passphrase", 10);
        assert_eq!(
            decrypt(&json, "a long passphrase").unwrap().address(),
            DEV_ADDRESS
        );
        assert!(matches!(
            decrypt(&json, "a long passphrasf"),
            Err(KeystoreError::WrongPassword)
        ));
        // `Crypto`, as some wallets write it, reads the same.
        let capital = json.replace("\"crypto\"", "\"Crypto\"");
        assert_eq!(
            decrypt(&capital, "a long passphrase").unwrap().address(),
            DEV_ADDRESS
        );
        assert_ne!(
            DEV_ADDRESS,
            address!("0000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn keystores_the_signer_does_not_read_are_refused_by_name() {
        let json = encrypt_for_test(&DEV_KEY, "x", 4);
        let cipher = json.replace("aes-128-ctr", "aes-128-cbc");
        assert!(
            matches!(decrypt(&cipher, "x"), Err(KeystoreError::Unsupported(m)) if m.contains("aes-128-cbc"))
        );
        let version = json.replace("\"version\":3", "\"version\":4");
        assert!(matches!(
            decrypt(&version, "x"),
            Err(KeystoreError::Unsupported(_))
        ));
        let kdf = json.replace("\"scrypt\"", "\"argon2\"");
        assert!(
            matches!(decrypt(&kdf, "x"), Err(KeystoreError::Unsupported(m)) if m.contains("argon2"))
        );
        assert!(matches!(decrypt("{}", "x"), Err(KeystoreError::Format(_))));
        let n = json.replace("\"n\":16", "\"n\":15");
        assert!(matches!(decrypt(&n, "x"), Err(KeystoreError::Format(_))));
    }
}
