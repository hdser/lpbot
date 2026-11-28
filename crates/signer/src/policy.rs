//! What the signer agrees to sign: one kind of transaction and nothing
//! else. A call to the Safe's roles module, on the configured chain, moving
//! no ETH, through `execTransactionWithRole`, within a gas ceiling. The
//! roles module then checks the inner call against the keeper's role; the
//! signer's check keeps a leaked socket from being used for anything else,
//! such as sending the keeper's gas money away.

use alloy_primitives::{keccak256, Address, U256};
use thiserror::Error;

use crate::tx::Eip1559Tx;

/// The Zodiac Roles v2 entry point the keeper calls:
/// `execTransactionWithRole(address to, uint256 value, bytes data,
/// uint8 operation, bytes32 roleKey, bool shouldRevert)`.
///
/// To be confirmed against the deployed Roles v2 contract on a fork (M4 of
/// `docs/PLAN.md`) before any real use.
pub const ROLES_SIGNATURE: &str =
    "execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)";

/// The four bytes of [`ROLES_SIGNATURE`].
pub fn roles_selector() -> [u8; 4] {
    let hash = keccak256(ROLES_SIGNATURE);
    [hash[0], hash[1], hash[2], hash[3]]
}

/// The ceilings the owner sets when starting the signer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub chain_id: u64,
    pub roles_module: Address,
    pub max_gas_limit: u64,
    /// Wei per gas.
    pub max_fee_per_gas: u128,
}

/// Why a transaction was refused: the rule that failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Refusal {
    #[error("chain {got}: the signer signs for chain {want} only")]
    WrongChain { got: u64, want: u64 },
    #[error("the call goes to {to}: the signer signs calls to the roles module {roles} only")]
    NotRolesModule { to: Address, roles: Address },
    #[error("the call moves {0} wei of ETH: the signer signs calls that move none")]
    MovesEth(U256),
    #[error("the call starts with {got}: the signer signs execTransactionWithRole ({want}) only")]
    WrongSelector { got: String, want: String },
    #[error("gas limit {got} is over the signer's ceiling of {max}")]
    GasLimit { got: u64, max: u64 },
    #[error("max fee {got} wei per gas is over the signer's ceiling of {max}")]
    MaxFee { got: u128, max: u128 },
    #[error("priority fee {priority} is over the max fee {max}: the transaction cannot be valid")]
    PriorityOverMax { priority: u128, max: u128 },
}

impl Policy {
    /// `Ok` when `tx` is the one kind of transaction the signer signs. A
    /// contract creation cannot be asked for: `Eip1559Tx::to` is always an
    /// address.
    pub fn check(&self, tx: &Eip1559Tx) -> Result<(), Refusal> {
        if tx.chain_id != self.chain_id {
            return Err(Refusal::WrongChain {
                got: tx.chain_id,
                want: self.chain_id,
            });
        }
        if tx.to != self.roles_module {
            return Err(Refusal::NotRolesModule {
                to: tx.to,
                roles: self.roles_module,
            });
        }
        if !tx.value.is_zero() {
            return Err(Refusal::MovesEth(tx.value));
        }
        let want = roles_selector();
        if tx.selector() != Some(want) {
            return Err(Refusal::WrongSelector {
                got: tx
                    .selector()
                    .map_or_else(|| "no selector".into(), |s| format!("0x{}", hex::encode(s))),
                want: format!("0x{}", hex::encode(want)),
            });
        }
        if tx.gas_limit > self.max_gas_limit {
            return Err(Refusal::GasLimit {
                got: tx.gas_limit,
                max: self.max_gas_limit,
            });
        }
        if tx.max_fee_per_gas > self.max_fee_per_gas {
            return Err(Refusal::MaxFee {
                got: tx.max_fee_per_gas,
                max: self.max_fee_per_gas,
            });
        }
        if tx.max_priority_fee_per_gas > tx.max_fee_per_gas {
            return Err(Refusal::PriorityOverMax {
                priority: tx.max_priority_fee_per_gas,
                max: tx.max_fee_per_gas,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tx::tests::roles_call;
    use alloy_primitives::{address, Bytes};

    pub(crate) fn policy() -> Policy {
        Policy {
            chain_id: 1,
            roles_module: address!("5FbDB2315678afecb367f032d93F642f64180aa3"),
            max_gas_limit: 1_000_000,
            max_fee_per_gas: 100_000_000_000,
        }
    }

    #[test]
    fn the_selector_is_the_one_cast_computes() {
        // `cast sig "execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)"`.
        assert_eq!(roles_selector(), [0xc6, 0xfe, 0x87, 0x47]);
    }

    #[test]
    fn a_roles_call_within_the_ceilings_is_signed() {
        assert_eq!(policy().check(&roles_call()), Ok(()));
    }

    #[test]
    fn every_other_transaction_is_refused_with_the_rule_it_broke() {
        let p = policy();
        let refused = |tx: Eip1559Tx| p.check(&tx).unwrap_err();
        assert_eq!(
            refused(Eip1559Tx {
                chain_id: 10,
                ..roles_call()
            }),
            Refusal::WrongChain { got: 10, want: 1 }
        );
        let elsewhere = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
        assert!(matches!(
            refused(Eip1559Tx { to: elsewhere, ..roles_call() }),
            Refusal::NotRolesModule { to, .. } if to == elsewhere
        ));
        assert_eq!(
            refused(Eip1559Tx {
                value: U256::from(1u8),
                ..roles_call()
            }),
            Refusal::MovesEth(U256::from(1u8))
        );
        // A plain transfer of ETH to the module: no calldata.
        assert!(matches!(
            refused(Eip1559Tx { data: Bytes::new(), ..roles_call() }),
            Refusal::WrongSelector { got, .. } if got == "no selector"
        ));
        // A token transfer through the module's address.
        let mut data = roles_call().data.to_vec();
        data[..4].copy_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
        assert!(matches!(
            refused(Eip1559Tx { data: data.into(), ..roles_call() }),
            Refusal::WrongSelector { got, .. } if got == "0xa9059cbb"
        ));
        assert_eq!(
            refused(Eip1559Tx {
                gas_limit: 1_000_001,
                ..roles_call()
            }),
            Refusal::GasLimit {
                got: 1_000_001,
                max: 1_000_000
            }
        );
        assert!(matches!(
            refused(Eip1559Tx {
                max_fee_per_gas: 100_000_000_001,
                ..roles_call()
            }),
            Refusal::MaxFee { .. }
        ));
        assert!(matches!(
            refused(Eip1559Tx {
                max_priority_fee_per_gas: 30_000_000_000,
                ..roles_call()
            }),
            Refusal::PriorityOverMax { .. }
        ));
        // Messages say which rule failed.
        assert!(Refusal::GasLimit { got: 2, max: 1 }
            .to_string()
            .contains("ceiling"));
    }
}
