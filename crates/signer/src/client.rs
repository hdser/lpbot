//! The executor's side of the signer's socket. It sends one transaction,
//! and checks that what comes back is that transaction, signed: the same
//! fields, a hash that matches, and the signer's address recovered from the
//! signature.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use alloy_primitives::{keccak256, Address};
use thiserror::Error;

use crate::server::Answer;
use crate::tx::{decode_signed, Eip1559Tx, SignedTx};

/// How long the client waits for the signer's answer.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// A transaction the signer signed, and who signed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub tx: SignedTx,
    pub from: Address,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("cannot reach the signer at {path}: {source}. Is it running? The owner starts it with `lpbot signer` in a terminal")]
    Connect {
        path: String,
        source: std::io::Error,
    },
    #[error("the signer's socket failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the signer refused: {0}")]
    Refused(String),
    #[error("the signer could not read the request: {0}")]
    Malformed(String),
    #[error("the signer's answer is not what was asked: {0}. Do not send it; check the signer")]
    Mismatch(String),
}

/// Asks the signer on `socket` to sign `tx`.
pub fn sign(socket: &Path, tx: &Eip1559Tx) -> Result<Signed, ClientError> {
    let mut stream = UnixStream::connect(socket).map_err(|source| ClientError::Connect {
        path: socket.display().to_string(),
        source,
    })?;
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    let mut line = serde_json::to_string(tx).expect("transactions serialize");
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    let answer: Answer = serde_json::from_str(reply.trim()).map_err(|e| {
        ClientError::Mismatch(format!("the answer is not JSON the signer writes: {e}"))
    })?;
    match answer {
        Answer::Signed(signed) => check(tx, signed),
        Answer::Refused(reason) => Err(ClientError::Refused(reason)),
        Answer::Malformed(reason) => Err(ClientError::Malformed(reason)),
    }
}

/// Checks a signed transaction against the one asked for.
pub fn check(asked: &Eip1559Tx, signed: SignedTx) -> Result<Signed, ClientError> {
    if keccak256(&signed.raw) != signed.hash {
        return Err(ClientError::Mismatch(
            "its hash is not the hash of its bytes".into(),
        ));
    }
    let (tx, from) =
        decode_signed(&signed.raw).map_err(|e| ClientError::Mismatch(e.to_string()))?;
    if &tx != asked {
        return Err(ClientError::Mismatch(
            "the signed fields differ from the request".into(),
        ));
    }
    Ok(Signed { tx: signed, from })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::policy;
    use crate::server::{bind, tests::TempDir};
    use crate::tx::tests::{roles_call, transfer, DEV_ADDRESS, DEV_KEY};
    use crate::tx::Key;

    #[test]
    fn a_round_trip_over_the_socket_signs_what_was_asked_and_refuses_the_rest() {
        let dir = TempDir::new("round");
        let path = dir.0.join("s.sock");
        let server = bind(&path).unwrap();
        let key = Key::from_secret(&DEV_KEY).unwrap();
        let p = policy();
        let mut log = Vec::new();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                // One connection per request below.
                for _ in 0..2 {
                    server.handle_next(&key, &p, &mut log).unwrap();
                }
            });
            let signed = sign(&path, &roles_call()).unwrap();
            assert_eq!(signed.from, DEV_ADDRESS);
            assert_eq!(signed.tx, key.sign(&roles_call()).unwrap());
            let refused = sign(&path, &transfer()).unwrap_err();
            assert!(matches!(refused, ClientError::Refused(ref r) if r.contains("roles module")));
            handle.join().unwrap();
        });
        let log = String::from_utf8(log).unwrap();
        assert_eq!(log.lines().count(), 2, "{log}");
        assert!(!log.contains(&hex::encode(DEV_KEY)));
    }

    #[test]
    fn an_answer_for_another_transaction_is_not_accepted() {
        let key = Key::from_secret(&DEV_KEY).unwrap();
        let other = key
            .sign(&Eip1559Tx {
                nonce: 8,
                ..roles_call()
            })
            .unwrap();
        assert!(matches!(
            check(&roles_call(), other.clone()),
            Err(ClientError::Mismatch(m)) if m.contains("differ")
        ));
        let mut wrong_hash = key.sign(&roles_call()).unwrap();
        wrong_hash.hash = other.hash;
        assert!(matches!(
            check(&roles_call(), wrong_hash),
            Err(ClientError::Mismatch(m)) if m.contains("hash")
        ));
        assert!(check(&roles_call(), key.sign(&roles_call()).unwrap()).is_ok());
    }

    #[test]
    fn no_signer_on_the_path_says_how_to_start_one() {
        let dir = TempDir::new("none");
        let error = sign(&dir.0.join("missing.sock"), &roles_call()).unwrap_err();
        assert!(error.to_string().contains("lpbot signer"), "{error}");
    }
}
