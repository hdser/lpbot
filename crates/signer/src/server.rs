//! The signer's side of its socket: one JSON request a line, each a
//! transaction to sign, each answered with the signed transaction or the
//! rule it broke. The key never leaves the process; the log gets what was
//! asked and what was answered, never key material.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::policy::Policy;
use crate::tx::{Eip1559Tx, Key, SignedTx};

/// How long a client may take to send its line before it is dropped, so a
/// stalled connection cannot hold the signer.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The answer to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Signed(SignedTx),
    /// The policy's rule that failed.
    Refused(String),
    /// The line was not a transaction.
    Malformed(String),
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("{0} exists and is not a socket; move it away, or give the signer another path")]
    NotASocket(PathBuf),
    #[error("a signer is already listening on {0}; stop it first, or give this one another path")]
    AlreadyRunning(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The listening socket. Dropping it removes the socket file.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Creates the socket at `path`, readable and writable by its owner only.
/// A socket left there by a signer that stopped is replaced; anything else
/// at the path, or a signer still answering on it, is refused.
pub fn bind(path: &Path) -> Result<Server, ServerError> {
    let io = |source| ServerError::Io {
        path: path.to_path_buf(),
        source,
    };
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(ServerError::AlreadyRunning(path.to_path_buf()));
            }
            std::fs::remove_file(path).map_err(io)?;
        }
        Ok(_) => return Err(ServerError::NotASocket(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let listener = UnixListener::bind(path).map_err(io)?;
    let server = Server {
        listener,
        path: path.to_path_buf(),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    Ok(server)
}

/// What a request was, for the log: never the key, never the full data.
fn describe(tx: &Eip1559Tx) -> String {
    format!(
        "chain={} to={} selector={} nonce={} gas_limit={} max_fee={} priority_fee={} value={}",
        tx.chain_id,
        tx.to,
        tx.selector()
            .map_or_else(|| "none".into(), |s| format!("0x{}", hex::encode(s))),
        tx.nonce,
        tx.gas_limit,
        tx.max_fee_per_gas,
        tx.max_priority_fee_per_gas,
        tx.value
    )
}

/// The answer to one request line, and the line to log for it.
pub fn answer(key: &Key, policy: &Policy, line: &str) -> (Answer, String) {
    let tx: Eip1559Tx = match serde_json::from_str(line) {
        Ok(tx) => tx,
        Err(e) => {
            let reason = format!("not a transaction: {e}");
            return (
                Answer::Malformed(reason.clone()),
                format!("malformed: {reason}"),
            );
        }
    };
    let what = describe(&tx);
    if let Err(refusal) = policy.check(&tx) {
        let reason = refusal.to_string();
        return (
            Answer::Refused(reason.clone()),
            format!("{what}: refused: {reason}"),
        );
    }
    match key.sign(&tx) {
        Ok(signed) => {
            let line = format!("{what}: signed {}", signed.hash);
            (Answer::Signed(signed), line)
        }
        Err(e) => {
            let reason = format!("could not sign: {e}");
            (Answer::Refused(reason.clone()), format!("{what}: {reason}"))
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Server {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Answers one connection, line by line, until the client closes it or
    /// stalls past [`READ_TIMEOUT`].
    pub fn handle_next(
        &self,
        key: &Key,
        policy: &Policy,
        log: &mut dyn Write,
    ) -> Result<(), ServerError> {
        let io = |source| ServerError::Io {
            path: self.path.clone(),
            source,
        };
        let (stream, _) = self.listener.accept().map_err(io)?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(io)?;
        let mut writer = stream.try_clone().map_err(io)?;
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                // A timeout or a broken connection ends it.
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let (answer, logged) = self::answer(key, policy, &line);
            let _ = writeln!(log, "{} {logged}", now());
            let _ = log.flush();
            let mut text = serde_json::to_string(&answer).expect("answers serialize");
            text.push('\n');
            if writer.write_all(text.as_bytes()).is_err() {
                break;
            }
        }
        Ok(())
    }
}

/// Listens on `socket` and answers every connection, one at a time, until
/// the process is stopped.
pub fn serve(
    socket: &Path,
    key: &Key,
    policy: &Policy,
    log: &mut dyn Write,
) -> Result<(), ServerError> {
    let server = bind(socket)?;
    let _ = writeln!(
        log,
        "{} listening on {} for chain {}, roles module {}, signing as {}",
        now(),
        socket.display(),
        policy.chain_id,
        policy.roles_module,
        key.address()
    );
    loop {
        server.handle_next(key, policy, log)?;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::policy::tests::policy;
    use crate::tx::tests::{roles_call, transfer, DEV_KEY};

    /// A fresh folder for a test's socket, removed with it.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(label: &str) -> Self {
            // Unix socket paths are short (104 bytes on macOS): stay near /tmp.
            let path = PathBuf::from(format!("/tmp/lpbot-signer-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn answers_sign_a_roles_call_and_refuse_the_rest_with_a_log_line_each() {
        let key = Key::from_secret(&DEV_KEY).unwrap();
        let p = policy();
        let line = serde_json::to_string(&roles_call()).unwrap();
        let (answer, logged) = super::answer(&key, &p, &line);
        let Answer::Signed(signed) = answer else {
            panic!("{answer:?}")
        };
        assert_eq!(signed, key.sign(&roles_call()).unwrap());
        assert!(logged.contains("selector=0xc6fe8747"), "{logged}");
        assert!(
            logged.contains(&format!("signed {}", signed.hash)),
            "{logged}"
        );

        let (answer, logged) =
            super::answer(&key, &p, &serde_json::to_string(&transfer()).unwrap());
        assert!(matches!(answer, Answer::Refused(ref r) if r.contains("roles module")));
        assert!(logged.contains("refused"), "{logged}");

        let (answer, logged) = super::answer(&key, &p, "{\"nonce\": 1}");
        assert!(matches!(answer, Answer::Malformed(_)));
        assert!(logged.starts_with("malformed"));

        // No log line carries the key.
        let secret = hex::encode(DEV_KEY);
        assert!(!logged.contains(&secret));
    }

    #[test]
    fn the_socket_is_its_owners_only_and_a_path_that_is_not_a_socket_is_refused() {
        let dir = TempDir::new("bind");
        let path = dir.0.join("signer.sock");
        let server = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A second signer on the same path while the first listens.
        assert!(matches!(bind(&path), Err(ServerError::AlreadyRunning(_))));
        drop(server);
        assert!(!path.exists(), "dropping the server removes its socket");

        // A stale socket file is replaced.
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());
        let again = bind(&path).unwrap();
        drop(again);

        // Anything else at the path is left alone.
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(matches!(bind(&path), Err(ServerError::NotASocket(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    }
}
