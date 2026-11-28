//! `lpbot live`: a forward experiment followed at the head, as paper mode
//! does, with each of the engine's actions carried out on the chain by the
//! trader (`lpbot_exec::trader`; `docs/PLAN.md`, M5 and M6).
//!
//! It acts only on actions decided after it started, once their block has
//! `confirmations` blocks on top, and each once: an action's id is its run,
//! block, campaign and name, and an id already in the journal is never sent
//! again. On start it reads the journal and refuses to go on while an
//! action is left open. The pause flag holds entries; `exit-all` takes the
//! open position out and swaps back, once. The simulated run is not
//! changed by what happens on the chain: paper mode's tables are its twin.
//!
//! Custody is a test account on a local fork (chain id 31337) until the
//! owner's Safe exists; with a Safe, the roles modifier, the Safe's modules
//! and the members are checked (`lpbot_exec::custody`) before anything runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alloy_primitives::{address, Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::StepRecord;
use lpbot_exec::custody;
use lpbot_exec::journal::{open_action, recovery, ChainView, Entry, Journal, Recovery};
use lpbot_exec::submit::{FeePolicy, Node, RpcNode};
use lpbot_exec::trader::{Contracts, Custody, Limits, PoolInfo, Trader, LOCAL_CHAIN_ID};
use lpbot_risk::checks::ActionLog;
use lpbot_risk::pause::{PauseFlag, State};
use lpbot_signer::{Eip1559Tx, Key, SignedTx};
use lpbot_sim::engine::Stream;

use crate::run_cmd::RunSetup;

/// The Uniswap v3 and Safe contracts on Ethereum mainnet (and on a fork of
/// it): the position manager, the v1 router, and MultiSendCallOnly 1.4.1,
/// each checked on a fork (`crates/exec/tests/fork.rs`).
pub const MAINNET: Contracts = Contracts {
    position_manager: address!("C36442b4a4522E871399CD717aBDD847Ab11FE88"),
    router: address!("E592427A0AEce92De3Edee1F18E0157C05861564"),
    multisend: address!("9641d764fc13c8B624c04430C7356C1C7C8102e2"),
};

/// Anvil's first development key: public, and accepted only on chain id
/// 31337, where it stands in for the Safe in tests on a fork.
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// One action of the engine: its id and its steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub id: String,
    pub name: String,
    pub block: u64,
    pub steps: Vec<StepRecord>,
}

/// The engine's actions to carry out now: steps grouped by block, campaign
/// and action, after `after_block`, at or below `confirmed_to`, and not
/// already in `done`.
pub fn pending_actions(
    run: &str,
    steps: &[StepRecord],
    after_block: u64,
    confirmed_to: u64,
    done: &BTreeSet<String>,
) -> Vec<Action> {
    let mut actions: Vec<Action> = Vec::new();
    for record in steps {
        if record.block <= after_block || record.block > confirmed_to {
            continue;
        }
        let id = format!(
            "{run}/{}/{}/{}",
            record.block, record.campaign, record.action
        );
        match actions.last_mut() {
            Some(last) if last.id == id => last.steps.push(record.clone()),
            _ => actions.push(Action {
                id,
                name: record.action.clone(),
                block: record.block,
                steps: vec![record.clone()],
            }),
        }
    }
    actions.retain(|a| !done.contains(&a.id));
    actions
}

/// The ids of every action the journal holds an intent for.
pub fn done_ids(entries: &[Entry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|e| match e {
            Entry::Intent { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// The custody `lpbot live` sends through.
pub enum CustodyArg {
    /// Anvil's development account, on a local fork only.
    TestAccount,
    /// The Safe and its roles modifier, the keeper signing through the
    /// signer's socket.
    Roles {
        safe: Address,
        roles: Address,
        role_key: B256,
        keeper: Address,
        socket: PathBuf,
    },
}

pub struct LiveArgs<'a> {
    pub runs: &'a [lpbot_core::ResolvedRun],
    pub root: &'a Path,
    pub results: &'a Path,
    pub chain: &'a str,
    /// Where transactions go and the chain is read back: the node, or a
    /// local fork in tests.
    pub rpc: &'a str,
    /// Where the run's history is caught up from: the local node.
    pub data_rpc: Option<&'a str>,
    pub custody: CustodyArg,
    pub confirmations: u64,
    pub every: std::time::Duration,
    pub steps: Option<u32>,
    pub pause: &'a PauseFlag,
    pub state_dir: PathBuf,
    pub limits: Limits,
    pub gas_cap_wei: u128,
}

fn code_len(node: &RpcNode<'_>, at: Address) -> Result<usize> {
    let code = node
        .client
        .call("eth_getCode", serde_json::json!([at, "latest"]))?;
    let text = code.as_str().unwrap_or("0x");
    Ok(text.trim_start_matches("0x").len() / 2)
}

/// The Safe's enabled modules: `getModulesPaginated(sentinel, 10)`.
fn safe_modules(node: &RpcNode<'_>, safe: Address) -> Result<Vec<Address>> {
    let mut data = lpbot_exec::abi::selector("getModulesPaginated(address,uint256)").to_vec();
    data.extend_from_slice(&U256::from(1u8).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(10u8).to_be_bytes::<32>());
    let out = node.call(Address::ZERO, safe, &data)?;
    let word = |i: usize| -> Result<U256> {
        out.get(i * 32..(i + 1) * 32)
            .map(U256::from_be_slice)
            .ok_or_else(|| anyhow!("getModulesPaginated answered too few bytes"))
    };
    let offset = word(0)?.to::<usize>() / 32;
    let count = word(offset)?.to::<usize>();
    (0..count)
        .map(|i| {
            let w = word(offset + 1 + i)?;
            Ok(Address::from_slice(&w.to_be_bytes::<32>()[12..]))
        })
        .collect()
}

/// Checks the custody before anything is sent.
fn preflight(node: &RpcNode<'_>, chain_id: u64, custody: &CustodyArg) -> Result<()> {
    match custody {
        CustodyArg::TestAccount => {
            if chain_id != LOCAL_CHAIN_ID {
                bail!(
                    "--test-account is for a local fork only (chain id {LOCAL_CHAIN_ID}); this \
                     node answers {chain_id}. Real custody is the Safe: --safe, --roles, \
                     --role-key, --keeper and --signer"
                );
            }
        }
        CustodyArg::Roles {
            safe,
            roles,
            keeper,
            ..
        } => {
            let code = node
                .client
                .call("eth_getCode", serde_json::json!([roles, "latest"]))?;
            let code = hex::decode(code.as_str().unwrap_or("0x").trim_start_matches("0x"))?;
            custody::check_roles_modifier(*roles, &code)?;
            custody::check_safe_modules(&safe_modules(node, *safe)?, *roles)?;
            custody::check_members(&[(*keeper, code_len(node, *keeper)?)])?;
        }
    }
    Ok(())
}

/// Follows the runs and carries out their actions until stopped.
pub fn live(args: &LiveArgs<'_>) -> Result<()> {
    let client = lpbot_chain::RpcClient::new(args.rpc);
    let node = RpcNode { client: &client };
    let chain_id = {
        let v = client.call("eth_chainId", serde_json::json!([]))?;
        u64::from_str_radix(v.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)?
    };
    preflight(&node, chain_id, &args.custody)?;
    let (custody, socket) = match &args.custody {
        CustodyArg::TestAccount => {
            let key = Key::from_secret(&hex::decode(ANVIL_KEY)?)?;
            (
                Custody::TestAccount {
                    account: key.address(),
                },
                None,
            )
        }
        CustodyArg::Roles {
            safe,
            roles,
            role_key,
            keeper,
            socket,
        } => (
            Custody::Roles {
                safe: *safe,
                roles: *roles,
                role_key: *role_key,
                keeper: *keeper,
            },
            Some(socket.clone()),
        ),
    };
    let key = Key::from_secret(&hex::decode(ANVIL_KEY)?)?;
    let mut sign = |tx: &Eip1559Tx| -> Result<SignedTx> {
        match &socket {
            None => Ok(key.sign(tx)?),
            Some(path) => Ok(lpbot_signer::sign(path, tx)?.tx),
        }
    };
    std::fs::create_dir_all(&args.state_dir)?;
    let journal = Journal::new(args.state_dir.join("journal.jsonl"));
    // An action a crash left open is settled before anything else.
    let entries = journal.read()?;
    let stage = open_action(&entries);
    if stage.is_some() {
        let next_nonce = node.next_nonce(custody.sender())?;
        match recovery(
            stage,
            &ChainView {
                receipt: None,
                next_nonce,
            },
        ) {
            Recovery::Nothing => {}
            other => bail!(
                "the journal {} has an action left open ({other:?}); read the chain back and \
                 settle it before `lpbot live` starts again",
                journal.path().display()
            ),
        }
    }
    let mut done = done_ids(&entries);
    // Actions are carried out only from the block live mode first started.
    let state_path = args.state_dir.join("state.json");
    let start_block = match std::fs::read_to_string(&state_path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)?["start_block"]
            .as_u64()
            .ok_or_else(|| anyhow!("{} has no start_block", state_path.display()))?,
        Err(_) => {
            let head = node.block_number()?;
            std::fs::write(
                &state_path,
                serde_json::json!({"start_block": head}).to_string(),
            )?;
            head
        }
    };
    println!(
        "live from block {start_block} on chain {chain_id}, holder {:#x}, journal {}",
        custody.holder(),
        journal.path().display()
    );
    let fees = FeePolicy {
        priority_wei: 1_000_000_000,
        cap_wei: args.gas_cap_wei,
        exit_cap_wei: args.gas_cap_wei.saturating_mul(5),
        bump_ppm: 125_000,
        wait_blocks: 3,
        gas_margin_ppm: 300_000,
    };
    let mut actions = ActionLog::new();
    let mut exited = false;
    let mut hook = |setup: &RunSetup, head: &Stream<'_>| -> Result<()> {
        let snapshot = &setup.history.snapshot;
        let (_, tokens) = crate::chain_cmd::chain_addresses(&setup.config, args.chain)?;
        let usdt = tokens.get("USDT").map(|(a, _)| *a);
        let pool = PoolInfo {
            address: setup.pool,
            token0: snapshot.token0,
            token1: snapshot.token1,
            fee: snapshot.state.fee_pips,
            tick_spacing: snapshot.state.tick_spacing,
            zero_first: usdt
                .into_iter()
                .filter(|t| *t == snapshot.token0 || *t == snapshot.token1)
                .collect(),
        };
        let value = tokens
            .get(&setup.strategy.value_token)
            .map(|(a, _)| *a)
            .ok_or_else(|| {
                anyhow!(
                    "value token {} is not in the config",
                    setup.strategy.value_token
                )
            })?;
        let mut trader = Trader {
            node: &node,
            chain_id,
            custody: custody.clone(),
            contracts: MAINNET,
            pool,
            limits: args.limits,
            fees,
            gas_cap: args.gas_cap_wei,
            journal: journal.clone(),
            actions: std::mem::take(&mut actions),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut wait = || std::thread::sleep(std::time::Duration::from_secs(4));
        let state = args.pause.state();
        if let State::Exiting(why) = &state {
            if !exited {
                let records = trader.exit_records(value == snapshot.token0, 0)?;
                let id = format!("{}/exit-all/{now}", setup.info.run_id);
                match trader.act(
                    &id,
                    &setup.info.run_id,
                    "close",
                    &records,
                    &state,
                    now,
                    &mut sign,
                    &mut wait,
                )? {
                    Ok(acted) => println!("exit-all ({why}): {:?}", acted.outcome),
                    Err(refusal) => println!("exit-all ({why}): {refusal}"),
                }
                exited = true;
            }
        } else {
            exited = false;
        }
        let head_block = node.block_number()?;
        for action in pending_actions(
            &setup.info.run_id,
            head.steps(),
            start_block,
            head_block.saturating_sub(args.confirmations),
            &done,
        ) {
            let result = trader.act(
                &action.id,
                &setup.info.run_id,
                &action.name,
                &action.steps,
                &state,
                now,
                &mut sign,
                &mut wait,
            );
            done.insert(action.id.clone());
            match result? {
                Ok(acted) => println!(
                    "{} {} at block {}: {} transaction(s), {:?}",
                    setup.info.run_id,
                    action.name,
                    action.block,
                    acted.landed.len(),
                    acted.outcome
                ),
                Err(refusal) => {
                    // Kept in the journal, closed, so it is not tried again.
                    journal.append(&Entry::Intent {
                        id: action.id.clone(),
                        run: setup.info.run_id.clone(),
                        campaign: action.steps.first().map_or(0, |r| r.campaign),
                        action: action.name.clone(),
                        steps: action.steps.clone(),
                    })?;
                    journal.append(&Entry::Abandoned {
                        id: action.id.clone(),
                        reason: refusal.to_string(),
                    })?;
                    println!(
                        "{} {} at block {}: not carried out: {refusal}",
                        setup.info.run_id, action.name, action.block
                    );
                }
            }
        }
        actions = std::mem::take(&mut trader.actions);
        Ok(())
    };
    let journal_dir = args.state_dir.join("paper");
    crate::paper_cmd::follow(&mut crate::paper_cmd::PaperArgs {
        runs: args.runs,
        root: args.root,
        results: args.results,
        journal: &journal_dir,
        chain: args.chain,
        rpc: args.data_rpc,
        every: args.every,
        check_every: u32::MAX,
        steps: args.steps,
        pause: args.pause,
        on_head: Some(&mut hook),
    })
    .context("following the run")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_core::Step;

    fn record(block: u64, campaign: u32, action: &str) -> StepRecord {
        StepRecord {
            block,
            time: block * 12,
            campaign,
            action: action.into(),
            sqrt_price_x96: U256::from(1u8) << 96,
            step: Step::Swap {
                zero_for_one: true,
                amount_in: U256::from(1u8),
                amount_out: U256::from(1u8),
            },
        }
    }

    #[test]
    fn only_new_confirmed_actions_are_carried_out_and_each_once() {
        let steps = [
            record(100, 1, "open"),
            record(150, 1, "close"),
            record(150, 1, "close"),
            record(160, 2, "open"),
            record(170, 2, "withdraw"),
        ];
        let done = BTreeSet::new();
        // Started at block 120; the head is at 172 with 3 confirmations.
        let actions = pending_actions("r", &steps, 120, 169, &done);
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["r/150/1/close", "r/160/2/open"]);
        assert_eq!(actions[0].steps.len(), 2, "one action, both its steps");
        // Once journaled, never again.
        let done: BTreeSet<String> = ["r/150/1/close".to_string()].into();
        let again = pending_actions("r", &steps, 120, 175, &done);
        let ids: Vec<&str> = again.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["r/160/2/open", "r/170/2/withdraw"]);
    }

    #[test]
    fn the_journals_intents_are_what_was_done() {
        let entries = [
            Entry::Intent {
                id: "r/150/1/close".into(),
                run: "r".into(),
                campaign: 1,
                action: "close".into(),
                steps: Vec::new(),
            },
            Entry::Abandoned {
                id: "r/150/1/close".into(),
                reason: "x".into(),
            },
        ];
        assert_eq!(done_ids(&entries), ["r/150/1/close".to_string()].into());
    }
}
