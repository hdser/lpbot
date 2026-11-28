//! Watching the Safe (`docs/DESIGN.md`, "What 2026 taught about Roles";
//! `docs/PLAN.md`, piece 9): every log that shows value leaving it, or its
//! permissions changing, is an alert. The November 2026 theft left a Safe
//! in one transaction through a role member's key; nothing the bot does
//! can stop a stolen key inside its allowances, but the owner must know at
//! once, to remove the role.
//!
//! Pure: [`classify`] takes the logs and what is allowed, and says which
//! are alerts. Reading the logs and delivering the alerts is the caller's.

use alloy_primitives::{keccak256, Address, B256, U256};

/// One log, as the node returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
    pub block: u64,
    pub transaction: B256,
}

/// What is allowed to receive from the Safe and to spend for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub safe: Address,
    pub roles: Address,
    pub position_manager: Address,
    /// Contracts an approval may go to: the position manager and the router.
    pub spenders: Vec<Address>,
    /// Addresses tokens may go to from the Safe: the pools it trades (a mint
    /// or a swap pays the pool directly).
    pub recipients: Vec<Address>,
    /// The tokens it holds.
    pub tokens: Vec<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Value may be leaving: remove the keeper's role and look now.
    Critical,
    /// The custody's configuration changed: check it was the owners.
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub severity: Severity,
    pub block: u64,
    pub transaction: B256,
    pub what: String,
}

fn topic(signature: &str) -> B256 {
    keccak256(signature.as_bytes())
}

fn address_of(topic: &B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..])
}

/// The events of a Safe that change who controls it or what may act on it.
const SAFE_CHANGES: &[&str] = &[
    "AddedOwner(address)",
    "RemovedOwner(address)",
    "ChangedThreshold(uint256)",
    "EnabledModule(address)",
    "DisabledModule(address)",
    "ChangedGuard(address)",
    "ChangedModuleGuard(address)",
    "ChangedFallbackHandler(address)",
];

/// Events of the roles modifier that only report its use.
const ROLES_ROUTINE: &[&str] = &[
    "ExecutionFromModuleSuccess(address)",
    "ExecutionFromModuleFailure(address)",
    "ConsumeAllowance(bytes32,uint128,uint128)",
];

/// The alerts among `logs`.
pub fn classify(logs: &[Log], allowed: &Allowed) -> Vec<Alert> {
    let transfer = topic("Transfer(address,address,uint256)");
    let approval = topic("Approval(address,address,uint256)");
    let safe_changes: Vec<(B256, &str)> = SAFE_CHANGES.iter().map(|s| (topic(s), *s)).collect();
    let roles_routine: Vec<B256> = ROLES_ROUTINE.iter().map(|s| topic(s)).collect();
    let mut alerts = Vec::new();
    for log in logs {
        let Some(first) = log.topics.first() else {
            continue;
        };
        let alert = |severity, what: String| Alert {
            severity,
            block: log.block,
            transaction: log.transaction,
            what,
        };
        if log.address == allowed.safe {
            if let Some((_, name)) = safe_changes.iter().find(|(t, _)| t == first) {
                alerts.push(alert(
                    Severity::Warning,
                    format!("the Safe's configuration changed: {name}"),
                ));
            }
            continue;
        }
        if log.address == allowed.roles {
            if !roles_routine.contains(first) {
                alerts.push(alert(
                    Severity::Warning,
                    format!(
                        "the roles modifier changed (event {}): a role, scope, allowance or \
                         member; check it was the owners",
                        &first.to_string()[..10]
                    ),
                ));
            }
            continue;
        }
        // The position NFT: an ERC-721 Transfer has the token id as a third
        // indexed topic.
        if log.address == allowed.position_manager && *first == transfer && log.topics.len() == 4 {
            let (from, to) = (address_of(&log.topics[1]), address_of(&log.topics[2]));
            if from == allowed.safe && to != Address::ZERO {
                alerts.push(alert(
                    Severity::Critical,
                    format!(
                        "position NFT {} left the Safe for {to:#x}",
                        U256::from_be_slice(log.topics[3].as_slice())
                    ),
                ));
            }
            continue;
        }
        if !allowed.tokens.contains(&log.address) || log.topics.len() < 3 {
            continue;
        }
        let (from, to) = (address_of(&log.topics[1]), address_of(&log.topics[2]));
        let amount = U256::from_be_slice(&log.data[..log.data.len().min(32)]);
        if *first == transfer && from == allowed.safe && !allowed.recipients.contains(&to) {
            alerts.push(alert(
                Severity::Critical,
                format!(
                    "{amount} of token {:#x} left the Safe for {to:#x}, which is not a pool it \
                     trades",
                    log.address
                ),
            ));
        }
        if *first == approval
            && from == allowed.safe
            && !allowed.spenders.contains(&to)
            && !amount.is_zero()
        {
            alerts.push(alert(
                Severity::Critical,
                format!(
                    "the Safe approved {to:#x} to spend {amount} of token {:#x}",
                    log.address
                ),
            ));
        }
    }
    alerts.sort_by_key(|a| (a.severity, a.block));
    alerts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(a: Address) -> B256 {
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(a.as_slice());
        B256::from(w)
    }

    fn amount(n: u64) -> Vec<u8> {
        U256::from(n).to_be_bytes::<32>().to_vec()
    }

    fn allowed() -> Allowed {
        Allowed {
            safe: Address::repeat_byte(0x5a),
            roles: Address::repeat_byte(0x70),
            position_manager: Address::repeat_byte(0xc3),
            spenders: vec![Address::repeat_byte(0xc3), Address::repeat_byte(0xe5)],
            recipients: vec![Address::repeat_byte(0x88)],
            tokens: vec![Address::repeat_byte(0xa0), Address::repeat_byte(0xc0)],
        }
    }

    fn log(address: Address, topics: Vec<B256>, data: Vec<u8>) -> Log {
        Log {
            address,
            topics,
            data,
            block: 7,
            transaction: B256::repeat_byte(1),
        }
    }

    #[test]
    fn value_leaving_the_safe_anywhere_but_its_pools_is_critical() {
        let a = allowed();
        let usdc = Address::repeat_byte(0xa0);
        let thief = Address::repeat_byte(0xee);
        let transfer = topic("Transfer(address,address,uint256)");
        let logs = vec![
            // A mint or swap pays the pool: fine.
            log(
                usdc,
                vec![transfer, word(a.safe), word(Address::repeat_byte(0x88))],
                amount(5),
            ),
            // Into the Safe: fine.
            log(usdc, vec![transfer, word(thief), word(a.safe)], amount(5)),
            // Out to anyone else: the November pattern.
            log(
                usdc,
                vec![transfer, word(a.safe), word(thief)],
                amount(4_390_000),
            ),
            // An unrelated token: not watched.
            log(
                Address::repeat_byte(0x99),
                vec![transfer, word(a.safe), word(thief)],
                amount(1),
            ),
        ];
        let alerts = classify(&logs, &a);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, Severity::Critical);
        assert!(alerts[0].what.contains("4390000"), "{}", alerts[0].what);
    }

    #[test]
    fn approvals_to_strangers_and_the_nft_leaving_are_critical() {
        let a = allowed();
        let approval = topic("Approval(address,address,uint256)");
        let transfer = topic("Transfer(address,address,uint256)");
        let weth = Address::repeat_byte(0xc0);
        let thief = Address::repeat_byte(0xee);
        let logs = vec![
            log(
                weth,
                vec![approval, word(a.safe), word(Address::repeat_byte(0xc3))],
                amount(9),
            ),
            log(weth, vec![approval, word(a.safe), word(thief)], amount(9)),
            // Setting an allowance to zero is always fine.
            log(weth, vec![approval, word(a.safe), word(thief)], amount(0)),
            // The NFT burned (to zero) is fine; sent elsewhere is not.
            log(
                a.position_manager,
                vec![
                    transfer,
                    word(a.safe),
                    word(Address::ZERO),
                    B256::repeat_byte(3),
                ],
                vec![],
            ),
            log(
                a.position_manager,
                vec![transfer, word(a.safe), word(thief), B256::repeat_byte(3)],
                vec![],
            ),
        ];
        let alerts = classify(&logs, &a);
        assert_eq!(alerts.len(), 2, "{alerts:?}");
        assert!(alerts.iter().all(|x| x.severity == Severity::Critical));
        assert!(alerts.iter().any(|x| x.what.contains("approved")));
        assert!(alerts.iter().any(|x| x.what.contains("position NFT")));
    }

    #[test]
    fn changes_to_the_safe_or_its_permissions_are_warnings_and_routine_use_is_not() {
        let a = allowed();
        let logs = vec![
            log(a.safe, vec![topic("EnabledModule(address)")], vec![]),
            log(
                a.safe,
                vec![topic("ExecutionSuccess(bytes32,uint256)")],
                vec![],
            ),
            log(
                a.roles,
                vec![topic("ExecutionFromModuleSuccess(address)")],
                vec![],
            ),
            log(
                a.roles,
                vec![topic("ConsumeAllowance(bytes32,uint128,uint128)")],
                vec![],
            ),
            log(
                a.roles,
                vec![topic("AssignRoles(address,bytes32[],bool[])")],
                vec![],
            ),
        ];
        let alerts = classify(&logs, &a);
        assert_eq!(alerts.len(), 2, "{alerts:?}");
        assert!(alerts.iter().all(|x| x.severity == Severity::Warning));
        assert!(
            alerts[0].what.contains("EnabledModule") || alerts[1].what.contains("EnabledModule")
        );
    }
}
