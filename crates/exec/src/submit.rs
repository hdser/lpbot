//! Sending a transaction and seeing it in (`docs/DESIGN.md`, "Execution",
//! steps 5 and 6; `docs/PLAN.md`, piece 5).
//!
//! The submitter owns the keeper's nonce: it takes the next one from the
//! chain, sends, and if the transaction is not in after `wait_blocks`
//! blocks it replaces it with the same nonce and fees raised by at least
//! the share nodes require. An entry's fee never goes past the cap; an
//! exit's goes past it up to its own, higher ceiling, since a position left
//! in place costs more than gas. Every send is journaled before it is made.
//! Signing is not here: the caller passes the signer (the `lpbot signer`
//! process, through its socket).

use alloy_primitives::{Address, Bytes, B256, U256};
use lpbot_signer::{Eip1559Tx, SignedTx};
use serde_json::{json, Value};

use crate::journal::{Entry, Journal};

/// What the submitter needs from a node.
pub trait Node {
    /// The next nonce of `address`, counting transactions already pending.
    fn next_nonce(&self, address: Address) -> anyhow::Result<u64>;
    /// The base fee of the newest block, in wei.
    fn base_fee(&self) -> anyhow::Result<u128>;
    fn block_number(&self) -> anyhow::Result<u64>;
    /// Sends a signed transaction; returns its hash.
    fn send_raw(&self, raw: &[u8]) -> anyhow::Result<B256>;
    /// The receipt of `hash`, once it is in a block: (block, success).
    fn receipt(&self, hash: B256) -> anyhow::Result<Option<(u64, bool)>>;
    /// Gas the call would use from `from`.
    fn estimate_gas(&self, from: Address, to: Address, data: &[u8]) -> anyhow::Result<u64>;
    /// The call's return data against the latest state, or the revert.
    fn call(&self, from: Address, to: Address, data: &[u8]) -> anyhow::Result<Vec<u8>>;
}

/// How fees are set and raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeePolicy {
    /// The tip offered, in wei a gas.
    pub priority_wei: u128,
    /// The highest fee a gas an entry pays (risk.max_gas_gwei).
    pub cap_wei: u128,
    /// The highest an exit pays.
    pub exit_cap_wei: u128,
    /// How much a replacement raises both fees, in parts per million; nodes
    /// take a replacement only from 10% (100,000) up.
    pub bump_ppm: u32,
    /// Blocks to wait before replacing.
    pub wait_blocks: u64,
    /// Gas limit over the estimate, in parts per million.
    pub gas_margin_ppm: u32,
}

impl FeePolicy {
    fn cap(&self, exit: bool) -> u128 {
        if exit {
            self.exit_cap_wei.max(self.cap_wei)
        } else {
            self.cap_wei
        }
    }

    /// The first fees for a transaction: twice the base fee plus the tip,
    /// within the cap. `None` when the tip alone would be over it.
    pub fn first(&self, base_fee: u128, exit: bool) -> Option<(u128, u128)> {
        let cap = self.cap(exit);
        let priority = self.priority_wei;
        let max_fee = base_fee.saturating_mul(2).saturating_add(priority).min(cap);
        (max_fee >= priority && max_fee > base_fee).then_some((max_fee, priority))
    }

    /// The fees of a replacement: both raised by `bump_ppm`, rounded up.
    /// `None` when that is over the cap: the transaction is left to land at
    /// the fee it has, or is given up.
    pub fn bump(&self, (max_fee, priority): (u128, u128), exit: bool) -> Option<(u128, u128)> {
        let raise = |fee: u128| {
            let bump = u128::from(self.bump_ppm.max(100_000));
            fee.saturating_add((fee.saturating_mul(bump)).div_ceil(1_000_000).max(1))
        };
        let (max_fee, priority) = (raise(max_fee), raise(priority));
        (max_fee <= self.cap(exit)).then_some((max_fee, priority))
    }

    pub fn gas_limit(&self, estimate: u64) -> u64 {
        estimate.saturating_add(
            (u128::from(estimate) * u128::from(self.gas_margin_ppm) / 1_000_000) as u64,
        )
    }
}

/// What came of a submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub hash: B256,
    pub block: u64,
    pub success: bool,
    pub nonce: u64,
    /// How many transactions were sent for the nonce, the first included.
    pub sent: usize,
}

/// One transaction to send: the call, and whether it is an exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: String,
    pub chain_id: u64,
    pub from: Address,
    pub to: Address,
    pub data: Bytes,
    pub exit: bool,
}

/// Sends `order` and replaces it until it lands, journaling every send.
/// `sign` is the signer; `wait` lets a block go by (a sleep, or a mined
/// block in a test). Gives up, with the transaction possibly still
/// pending, after `max_rounds` waits.
pub fn submit(
    node: &dyn Node,
    sign: &mut dyn FnMut(&Eip1559Tx) -> anyhow::Result<SignedTx>,
    order: &Order,
    policy: &FeePolicy,
    journal: &Journal,
    wait: &mut dyn FnMut(),
    max_rounds: usize,
) -> anyhow::Result<Landed> {
    let estimate = node
        .estimate_gas(order.from, order.to, &order.data)
        .map_err(|e| anyhow::anyhow!("the transaction would fail ({e:#}); nothing was sent"))?;
    let nonce = node.next_nonce(order.from)?;
    let base = node.base_fee()?;
    let mut fees = policy.first(base, order.exit).ok_or_else(|| {
        anyhow::anyhow!(
            "gas is over the cap ({} gwei base fee); nothing was sent, the action waits",
            base / 1_000_000_000
        )
    })?;
    let mut hashes = Vec::new();
    let mut sent_at = node.block_number()?;
    let send = |fees: (u128, u128),
                sign: &mut dyn FnMut(&Eip1559Tx) -> anyhow::Result<SignedTx>,
                hashes: &mut Vec<B256>|
     -> anyhow::Result<()> {
        let tx = Eip1559Tx {
            chain_id: order.chain_id,
            nonce,
            max_priority_fee_per_gas: fees.1,
            max_fee_per_gas: fees.0,
            gas_limit: policy.gas_limit(estimate),
            to: order.to,
            value: U256::ZERO,
            data: order.data.clone(),
        };
        let signed = sign(&tx)?;
        journal.append(&Entry::Sent {
            id: order.id.clone(),
            nonce,
            hash: signed.hash,
            max_fee_per_gas: fees.0,
            max_priority_fee_per_gas: fees.1,
        })?;
        let hash = node.send_raw(&signed.raw)?;
        if hash != signed.hash {
            anyhow::bail!(
                "the node answered hash {hash} for transaction {}; stop and check the node",
                signed.hash
            );
        }
        hashes.push(hash);
        Ok(())
    };
    send(fees, sign, &mut hashes)?;
    for _ in 0..max_rounds {
        wait();
        for hash in hashes.iter().rev() {
            if let Some((block, success)) = node.receipt(*hash)? {
                journal.append(&Entry::Confirmed {
                    id: order.id.clone(),
                    hash: *hash,
                    block,
                    success,
                })?;
                return Ok(Landed {
                    hash: *hash,
                    block,
                    success,
                    nonce,
                    sent: hashes.len(),
                });
            }
        }
        let now = node.block_number()?;
        if now >= sent_at + policy.wait_blocks {
            if let Some(raised) = policy.bump(fees, order.exit) {
                fees = raised;
                send(fees, sign, &mut hashes)?;
                sent_at = now;
            }
        }
    }
    anyhow::bail!(
        "transaction {} (nonce {nonce}) is not in after {max_rounds} waits; it may still land. \
         The journal holds every hash sent; the executor waits for it on its next start",
        hashes.last().map(|h| h.to_string()).unwrap_or_default()
    )
}

/// A node over JSON-RPC.
pub struct RpcNode<'a> {
    pub client: &'a lpbot_chain::RpcClient,
}

fn quantity(value: &Value, what: &str) -> anyhow::Result<u128> {
    let text = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("{what}: expected a hex quantity, got {value}"))?;
    u128::from_str_radix(text.trim_start_matches("0x"), 16)
        .map_err(|e| anyhow::anyhow!("{what}: {text}: {e}"))
}

fn hex_bytes(data: &[u8]) -> String {
    format!("0x{}", hex::encode(data))
}

impl Node for RpcNode<'_> {
    fn next_nonce(&self, address: Address) -> anyhow::Result<u64> {
        let v = self
            .client
            .call("eth_getTransactionCount", json!([address, "pending"]))?;
        Ok(quantity(&v, "nonce")? as u64)
    }

    fn base_fee(&self) -> anyhow::Result<u128> {
        let block = self
            .client
            .call("eth_getBlockByNumber", json!(["latest", false]))?;
        quantity(&block["baseFeePerGas"], "baseFeePerGas")
    }

    fn block_number(&self) -> anyhow::Result<u64> {
        Ok(quantity(&self.client.call("eth_blockNumber", json!([]))?, "block")? as u64)
    }

    fn send_raw(&self, raw: &[u8]) -> anyhow::Result<B256> {
        let v = self
            .client
            .call("eth_sendRawTransaction", json!([hex_bytes(raw)]))?;
        let text = v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("eth_sendRawTransaction answered {v}"))?;
        text.parse()
            .map_err(|e| anyhow::anyhow!("transaction hash {text}: {e}"))
    }

    fn receipt(&self, hash: B256) -> anyhow::Result<Option<(u64, bool)>> {
        let v = self
            .client
            .call("eth_getTransactionReceipt", json!([hash]))?;
        if v.is_null() {
            return Ok(None);
        }
        let block = quantity(&v["blockNumber"], "blockNumber")? as u64;
        let status = quantity(&v["status"], "status")?;
        Ok(Some((block, status == 1)))
    }

    fn estimate_gas(&self, from: Address, to: Address, data: &[u8]) -> anyhow::Result<u64> {
        let v = self.client.call(
            "eth_estimateGas",
            json!([{"from": from, "to": to, "data": hex_bytes(data)}]),
        )?;
        Ok(quantity(&v, "gas")? as u64)
    }

    fn call(&self, from: Address, to: Address, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let v = self.client.call(
            "eth_call",
            json!([{"from": from, "to": to, "data": hex_bytes(data)}, "latest"]),
        )?;
        let text = v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("eth_call answered {v}"))?;
        Ok(hex::decode(text.trim_start_matches("0x"))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A node that includes a transaction once its tip reaches a level.
    struct Mock {
        block: RefCell<u64>,
        include_from_tip: u128,
        sent: RefCell<Vec<(B256, u128)>>,
        landed: RefCell<Option<(B256, u64)>>,
    }

    impl Mock {
        fn new(include_from_tip: u128) -> Self {
            Self {
                block: RefCell::new(100),
                include_from_tip,
                sent: RefCell::new(Vec::new()),
                landed: RefCell::new(None),
            }
        }

        fn mine(&self) {
            *self.block.borrow_mut() += 1;
            if self.landed.borrow().is_none() {
                let best = self
                    .sent
                    .borrow()
                    .iter()
                    .rev()
                    .find(|(_, tip)| *tip >= self.include_from_tip)
                    .copied();
                if let Some((hash, _)) = best {
                    *self.landed.borrow_mut() = Some((hash, *self.block.borrow()));
                }
            }
        }
    }

    impl Node for Mock {
        fn next_nonce(&self, _: Address) -> anyhow::Result<u64> {
            Ok(7)
        }
        fn base_fee(&self) -> anyhow::Result<u128> {
            Ok(10_000_000_000)
        }
        fn block_number(&self) -> anyhow::Result<u64> {
            Ok(*self.block.borrow())
        }
        fn send_raw(&self, raw: &[u8]) -> anyhow::Result<B256> {
            let (tx, _) = lpbot_signer::decode_signed(raw)?;
            let hash = alloy_primitives::keccak256(raw);
            self.sent
                .borrow_mut()
                .push((hash, tx.max_priority_fee_per_gas));
            Ok(hash)
        }
        fn receipt(&self, hash: B256) -> anyhow::Result<Option<(u64, bool)>> {
            Ok(self
                .landed
                .borrow()
                .filter(|(h, _)| *h == hash)
                .map(|(_, block)| (block, true)))
        }
        fn estimate_gas(&self, _: Address, _: Address, _: &[u8]) -> anyhow::Result<u64> {
            Ok(200_000)
        }
        fn call(&self, _: Address, _: Address, _: &[u8]) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    const DEV_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn policy() -> FeePolicy {
        FeePolicy {
            priority_wei: 1_000_000_000,
            cap_wei: 40_000_000_000,
            exit_cap_wei: 200_000_000_000,
            bump_ppm: 125_000,
            wait_blocks: 2,
            gas_margin_ppm: 250_000,
        }
    }

    fn temp(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lpbot-submit-{label}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn order(exit: bool) -> Order {
        Order {
            id: "a".into(),
            chain_id: 1,
            from: Address::repeat_byte(1),
            to: Address::repeat_byte(2),
            data: Bytes::from(vec![0xc6, 0xfe, 0x87, 0x47]),
            exit,
        }
    }

    #[test]
    fn fees_start_at_twice_the_base_and_rise_by_the_bump_within_the_cap() {
        let p = policy();
        assert_eq!(
            p.first(10_000_000_000, false),
            Some((21_000_000_000, 1_000_000_000))
        );
        // Capped for an entry; an exit gets its own ceiling.
        assert_eq!(
            p.first(30_000_000_000, false),
            Some((40_000_000_000, 1_000_000_000))
        );
        assert_eq!(
            p.first(50_000_000_000, false),
            None,
            "base fee over the cap"
        );
        assert_eq!(
            p.first(50_000_000_000, true),
            Some((101_000_000_000, 1_000_000_000))
        );
        let bumped = p.bump((21_000_000_000, 1_000_000_000), false).unwrap();
        assert_eq!(bumped, (23_625_000_000, 1_125_000_000));
        // Never less than the 10% a node takes, even if set lower.
        let low = FeePolicy { bump_ppm: 1, ..p };
        assert_eq!(low.bump((100, 10), false), Some((110, 11)));
        assert_eq!(p.bump((39_000_000_000, 1), false), None, "over the cap");
        assert!(
            p.bump((39_000_000_000, 1), true).is_some(),
            "an exit goes on"
        );
        assert_eq!(p.gas_limit(200_000), 250_000);
    }

    #[test]
    fn a_stuck_transaction_is_replaced_on_the_same_nonce_until_it_lands() {
        let key = lpbot_signer::Key::from_secret(&hex::decode(DEV_KEY).unwrap()).unwrap();
        let node = Mock::new(1_400_000_000);
        let journal = Journal::new(temp("stuck"));
        let mut sign = |tx: &Eip1559Tx| key.sign(tx).map_err(anyhow::Error::from);
        let mut wait = || node.mine();
        let landed = submit(
            &node,
            &mut sign,
            &order(false),
            &policy(),
            &journal,
            &mut wait,
            20,
        )
        .unwrap();
        // Tips 1.0, 1.125, 1.27, 1.42 gwei: the fourth is taken.
        assert!(landed.success);
        assert_eq!(landed.nonce, 7);
        assert_eq!(landed.sent, 4);
        let entries = journal.read().unwrap();
        let sent: Vec<u64> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Sent { nonce, .. } => Some(*nonce),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![7, 7, 7, 7], "every send is journaled, one nonce");
        assert!(
            matches!(entries.last(), Some(Entry::Confirmed { hash, .. }) if *hash == landed.hash)
        );
        std::fs::remove_file(journal.path()).unwrap();
    }

    #[test]
    fn an_entry_stops_raising_at_the_cap_and_gives_up_saying_it_may_still_land() {
        let key = lpbot_signer::Key::from_secret(&hex::decode(DEV_KEY).unwrap()).unwrap();
        // Nothing under 100 gwei of tip is ever taken.
        let node = Mock::new(100_000_000_000);
        let journal = Journal::new(temp("cap"));
        let mut sign = |tx: &Eip1559Tx| key.sign(tx).map_err(anyhow::Error::from);
        let mut wait = || node.mine();
        let error = submit(
            &node,
            &mut sign,
            &order(false),
            &policy(),
            &journal,
            &mut wait,
            30,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("may still land"), "{error}");
        let fees: Vec<u128> = journal
            .read()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Entry::Sent {
                    max_fee_per_gas, ..
                } => Some(*max_fee_per_gas),
                _ => None,
            })
            .collect();
        assert!(fees.iter().all(|f| *f <= policy().cap_wei), "{fees:?}");
        assert!(fees.windows(2).all(|w| w[1] > w[0]));
        std::fs::remove_file(journal.path()).unwrap();
    }
}
