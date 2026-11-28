//! EIP-1559 (type 2) transactions: encoding, the hash that is signed,
//! signing with the keeper's key, and reading a signed one back.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::rlp::{self, Item, RlpError};

/// The byte that starts every type-2 transaction.
pub const TYPE_EIP1559: u8 = 0x02;

/// A type-2 transaction with an empty access list. `to` is always set: the
/// signer never creates contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eip1559Tx {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub gas_limit: u64,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
}

/// A signed transaction, as sent with `eth_sendRawTransaction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTx {
    /// `0x02 || rlp([fields..., y_parity, r, s])`.
    pub raw: Bytes,
    /// Keccak of `raw`: the transaction's hash on the chain.
    pub hash: B256,
}

/// Why a signature could not be made or read.
#[derive(Debug, Error)]
pub enum TxError {
    #[error("not a type-2 transaction: it must start with 0x02")]
    NotType2,
    #[error("the signed transaction is not valid RLP: {0}")]
    Rlp(#[from] RlpError),
    #[error("the signed transaction has {0} fields, a type-2 one has 12")]
    Fields(usize),
    #[error("the transaction has an access list; the signer writes only empty ones")]
    AccessList,
    #[error("the transaction has no recipient; the signer never creates contracts")]
    NoRecipient,
    #[error("the signature's s is in the upper half, which the chain refuses")]
    HighS,
    #[error("the signature does not recover a key: {0}")]
    Signature(String),
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    rlp::put_uint(out, &value.to_be_bytes());
}

fn put_u128(out: &mut Vec<u8>, value: u128) {
    rlp::put_uint(out, &value.to_be_bytes());
}

fn put_u256(out: &mut Vec<u8>, value: U256) {
    rlp::put_uint(out, &value.to_be_bytes::<32>());
}

impl Eip1559Tx {
    /// The nine fields that are signed, RLP-encoded one after the other
    /// (not yet wrapped as a list).
    fn fields(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + 96);
        put_u64(&mut out, self.chain_id);
        put_u64(&mut out, self.nonce);
        put_u128(&mut out, self.max_priority_fee_per_gas);
        put_u128(&mut out, self.max_fee_per_gas);
        put_u64(&mut out, self.gas_limit);
        rlp::put_bytes(&mut out, self.to.as_slice());
        put_u256(&mut out, self.value);
        rlp::put_bytes(&mut out, &self.data);
        // The access list: always empty.
        out.extend_from_slice(&rlp::list(&[]));
        out
    }

    /// `0x02 || rlp(fields)`: what the signature is over.
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut out = vec![TYPE_EIP1559];
        out.extend(rlp::list(&self.fields()));
        out
    }

    /// Keccak of the signing payload.
    pub fn signing_hash(&self) -> B256 {
        keccak256(self.signing_payload())
    }

    /// The four bytes the calldata starts with, if it has them.
    pub fn selector(&self) -> Option<[u8; 4]> {
        self.data.get(..4).map(|s| [s[0], s[1], s[2], s[3]])
    }

    /// The signed transaction with this signature.
    fn with_signature(&self, y_odd: bool, signature: &Signature) -> SignedTx {
        let mut payload = self.fields();
        put_u64(&mut payload, u64::from(y_odd));
        let (r, s) = signature.split_bytes();
        rlp::put_uint(&mut payload, &r);
        rlp::put_uint(&mut payload, &s);
        let mut raw = vec![TYPE_EIP1559];
        raw.extend(rlp::list(&payload));
        let hash = keccak256(&raw);
        SignedTx {
            raw: raw.into(),
            hash,
        }
    }
}

/// The keeper's key, in memory only. Its `Debug` shows the address and
/// nothing of the key; the key is wiped when it is dropped.
pub struct Key {
    signing: SigningKey,
    address: Address,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({})", self.address)
    }
}

/// The address of a public key: the last 20 bytes of the keccak of its
/// uncompressed point, without the leading 0x04.
pub fn address_of(key: &VerifyingKey) -> Address {
    let point = key.to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

impl Key {
    /// A key from its 32 secret bytes. The signer gets its key from a
    /// keystore (`keystore::decrypt`); this is for that and for tests with
    /// published development keys.
    pub fn from_secret(secret: &[u8]) -> Result<Self, TxError> {
        if secret.len() != 32 {
            return Err(TxError::Signature(format!(
                "a key is 32 bytes, not {}",
                secret.len()
            )));
        }
        let signing = SigningKey::from_slice(secret)
            .map_err(|_| TxError::Signature("the 32 bytes are not a valid secp256k1 key".into()))?;
        let address = address_of(signing.verifying_key());
        Ok(Self { signing, address })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// Signs `tx`. The signature is in the lower half of `s`, as the chain
    /// requires.
    pub fn sign(&self, tx: &Eip1559Tx) -> Result<SignedTx, TxError> {
        let hash = tx.signing_hash();
        let (signature, recovery) = self
            .signing
            .sign_prehash_recoverable(hash.as_slice())
            .map_err(|e| TxError::Signature(e.to_string()))?;
        let (signature, y_odd) = match signature.normalize_s() {
            Some(low) => (low, !recovery.is_y_odd()),
            None => (signature, recovery.is_y_odd()),
        };
        Ok(tx.with_signature(y_odd, &signature))
    }
}

fn u64_of(item: &Item<'_>) -> Result<u64, TxError> {
    let bytes = item.uint(8)?;
    Ok(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

fn u128_of(item: &Item<'_>) -> Result<u128, TxError> {
    let bytes = item.uint(16)?;
    Ok(bytes
        .iter()
        .fold(0u128, |acc, b| (acc << 8) | u128::from(*b)))
}

fn u256_of(item: &Item<'_>) -> Result<U256, TxError> {
    let bytes = item.uint(32)?;
    Ok(U256::from_be_slice(bytes))
}

fn scalar_of(item: &Item<'_>) -> Result<[u8; 32], TxError> {
    let bytes = item.uint(32)?;
    let mut out = [0u8; 32];
    out[32 - bytes.len()..].copy_from_slice(bytes);
    Ok(out)
}

/// Reads a signed type-2 transaction back: its fields and the address that
/// signed it. The client uses it to check the signer signed exactly what
/// was asked.
pub fn decode_signed(raw: &[u8]) -> Result<(Eip1559Tx, Address), TxError> {
    let (&kind, body) = raw.split_first().ok_or(TxError::NotType2)?;
    if kind != TYPE_EIP1559 {
        return Err(TxError::NotType2);
    }
    let item = rlp::decode(body)?;
    let fields = item.list()?;
    if fields.len() != 12 {
        return Err(TxError::Fields(fields.len()));
    }
    if !fields[8].list()?.is_empty() {
        return Err(TxError::AccessList);
    }
    let to = fields[5].bytes()?;
    if to.len() != 20 {
        return Err(TxError::NoRecipient);
    }
    let tx = Eip1559Tx {
        chain_id: u64_of(&fields[0])?,
        nonce: u64_of(&fields[1])?,
        max_priority_fee_per_gas: u128_of(&fields[2])?,
        max_fee_per_gas: u128_of(&fields[3])?,
        gas_limit: u64_of(&fields[4])?,
        to: Address::from_slice(to),
        value: u256_of(&fields[6])?,
        data: Bytes::copy_from_slice(fields[7].bytes()?),
    };
    let y_odd = match u64_of(&fields[9])? {
        0 => false,
        1 => true,
        _ => return Err(TxError::Signature("y parity must be 0 or 1".into())),
    };
    let signature = Signature::from_scalars(scalar_of(&fields[10])?, scalar_of(&fields[11])?)
        .map_err(|e| TxError::Signature(e.to_string()))?;
    if signature.normalize_s().is_some() {
        return Err(TxError::HighS);
    }
    let key = VerifyingKey::recover_from_prehash(
        tx.signing_hash().as_slice(),
        &signature,
        RecoveryId::new(y_odd, false),
    )
    .map_err(|e| TxError::Signature(e.to_string()))?;
    Ok((tx, address_of(&key)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_primitives::{address, hex};

    /// Anvil's and Hardhat's development key #0, published with both tools.
    pub(crate) const DEV_KEY: [u8; 32] =
        hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    pub(crate) const DEV_ADDRESS: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    pub(crate) fn transfer() -> Eip1559Tx {
        Eip1559Tx {
            chain_id: 1,
            nonce: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 30_000_000_000,
            gas_limit: 21_000,
            to: address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
            value: U256::from(1_000_000_000_000_000_000u128),
            data: Bytes::new(),
        }
    }

    #[test]
    fn the_development_key_has_its_published_address() {
        let key = Key::from_secret(&DEV_KEY).unwrap();
        assert_eq!(key.address(), DEV_ADDRESS);
        assert_eq!(format!("{key:?}"), format!("Key({DEV_ADDRESS})"));
        assert!(Key::from_secret(&[0u8; 32]).is_err());
        assert!(Key::from_secret(&[1u8; 31]).is_err());
    }

    #[test]
    fn a_transfer_encodes_and_signs_as_cast_writes_it() {
        // `cast mktx --private-key <DEV_KEY> --chain 1 --nonce 0
        //  --gas-limit 21000 --gas-price 30gwei --priority-gas-price 1gwei
        //  --value 1ether 0x70997970C51812dc3A010C7d01b50e0d17dc79C8`
        // (cast 1.6.0-nightly, offline), checked byte for byte. Both sign
        // deterministically (RFC 6979), so the signature matches too.
        let key = Key::from_secret(&DEV_KEY).unwrap();
        let signed = key.sign(&transfer()).unwrap();
        assert_eq!(hex::encode(&signed.raw), CAST_TRANSFER);
        assert_eq!(signed.hash, keccak256(&signed.raw));
        let (back, from) = decode_signed(&signed.raw).unwrap();
        assert_eq!(back, transfer());
        assert_eq!(from, DEV_ADDRESS);
    }

    /// The raw transaction cast wrote for `transfer()` with `DEV_KEY`.
    const CAST_TRANSFER: &str = "02f8730180843b9aca008506fc23ac008252089470997970c51812dc3a010c7d01b50e0d17dc79c8880de0b6b3a764000080c080a03113f8c10307e896ecc46d1a795e5e689d35936f6019c5f9c654469193fe3ceaa078f36ebfffa79403e073f7663a7c9f4ad14134dda35c9084727ca7f1a789e06f";

    /// `cast calldata "execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)"
    /// 0x1111111111111111111111111111111111111111 0 0x1234 0 <32 bytes, 1> true`.
    pub(crate) const CAST_ROLES_CALLDATA: &str = "c6fe87470000000000000000000000001111111111111111111111111111111111111111000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000021234000000000000000000000000000000000000000000000000000000000000";

    /// `cast mktx --private-key <DEV_KEY> --chain 1 --nonce 7 --gas-limit
    /// 500000 --gas-price 25gwei --priority-gas-price 2gwei
    /// 0x5FbDB2315678afecb367f032d93F642f64180aa3 <CAST_ROLES_CALLDATA>`.
    const CAST_ROLES_CALL: &str = "02f90172010784773594008505d21dba008307a120945fbdb2315678afecb367f032d93f642f64180aa380b90104c6fe87470000000000000000000000001111111111111111111111111111111111111111000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000021234000000000000000000000000000000000000000000000000000000000000c080a03a518c99b2fc14b94ca3bfabb752d5902a30d077955b45c371e7ece54688be13a03a93fcc3c643672688cc8c478f61ad2d9032346b260e7375848948734a92afb6";

    pub(crate) fn roles_call() -> Eip1559Tx {
        Eip1559Tx {
            chain_id: 1,
            nonce: 7,
            max_priority_fee_per_gas: 2_000_000_000,
            max_fee_per_gas: 25_000_000_000,
            gas_limit: 500_000,
            to: address!("5FbDB2315678afecb367f032d93F642f64180aa3"),
            value: U256::ZERO,
            data: hex::decode(CAST_ROLES_CALLDATA).unwrap().into(),
        }
    }

    #[test]
    fn a_contract_call_encodes_and_signs_as_cast_writes_it() {
        // Calldata over 55 bytes, so both the data and the list take the
        // long length prefix.
        let key = Key::from_secret(&DEV_KEY).unwrap();
        let signed = key.sign(&roles_call()).unwrap();
        assert_eq!(hex::encode(&signed.raw), CAST_ROLES_CALL);
        let (back, from) = decode_signed(&signed.raw).unwrap();
        assert_eq!(back, roles_call());
        assert_eq!(from, DEV_ADDRESS);
    }

    #[test]
    fn a_call_with_data_reads_back_with_its_signer_and_a_low_s() {
        let key = Key::from_secret(&DEV_KEY).unwrap();
        for nonce in 0..32 {
            let tx = Eip1559Tx {
                nonce,
                data: Bytes::from(vec![0xab; 100 + nonce as usize]),
                value: U256::ZERO,
                gas_limit: 500_000,
                ..transfer()
            };
            let signed = key.sign(&tx).unwrap();
            let (back, from) = decode_signed(&signed.raw).unwrap();
            assert_eq!(back, tx);
            assert_eq!(from, DEV_ADDRESS);
            assert_eq!(tx.selector(), Some([0xab; 4]));
        }
        assert_eq!(transfer().selector(), None);
    }

    #[test]
    fn a_signed_transaction_that_was_changed_no_longer_recovers_the_signer() {
        let key = Key::from_secret(&DEV_KEY).unwrap();
        let signed = key.sign(&transfer()).unwrap();
        let mut raw = signed.raw.to_vec();
        // The nonce byte (0x80, zero) becomes one.
        let at = raw.iter().position(|b| *b == 0x80).unwrap();
        raw[at] = 0x01;
        // It still reads, as another transaction from someone else.
        if let Ok((tx, from)) = decode_signed(&raw) {
            assert_eq!(tx.nonce, 1);
            assert_ne!(from, DEV_ADDRESS);
        }
        assert!(matches!(decode_signed(&raw[1..]), Err(TxError::NotType2)));
        assert!(matches!(decode_signed(&[]), Err(TxError::NotType2)));
        let mut legacy = signed.raw.to_vec();
        legacy[0] = 0x01;
        assert!(matches!(decode_signed(&legacy), Err(TxError::NotType2)));
    }

    #[test]
    fn the_signing_payload_starts_with_the_type_and_is_what_is_hashed() {
        let tx = transfer();
        let payload = tx.signing_payload();
        assert_eq!(payload[0], TYPE_EIP1559);
        assert_eq!(tx.signing_hash(), keccak256(&payload));
        let fields = rlp::decode(&payload[1..]).unwrap();
        assert_eq!(fields.list().unwrap().len(), 9);
    }
}
