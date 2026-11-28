//! The keeper's signer (`docs/PLAN.md`, piece 7; `docs/DESIGN.md`,
//! "Keys"). A process of its own that holds the keeper's key, decrypted
//! from its keystore with a password the owner types, and signs one kind of
//! transaction: a call to the Safe's roles module on the configured chain,
//! within a gas ceiling. Everything else is refused and logged.
//!
//! - [`tx`]: type-2 transactions, their signing hash, signing, reading back.
//! - [`keystore`]: opening a version-3 keystore.
//! - [`policy`]: the one kind of transaction that is signed.
//! - [`server`] and [`client`]: the Unix socket between the executor and
//!   the signer, one JSON line a request.
//! - [`password`]: the password from the terminal, without echo.
//!
//! The key exists only in this process's memory and is wiped when dropped.
//! Nothing here creates a key: the owner makes the keystore.

pub mod client;
pub mod keystore;
pub mod password;
pub mod policy;
mod rlp;
pub mod server;
pub mod tx;

pub use client::{sign, ClientError, Signed};
pub use keystore::{decrypt, KeystoreError};
pub use password::{read_password, PasswordError};
pub use policy::{roles_selector, Policy, Refusal, ROLES_SIGNATURE};
pub use server::{bind, serve, Answer, Server, ServerError};
pub use tx::{decode_signed, Eip1559Tx, Key, SignedTx, TxError};
