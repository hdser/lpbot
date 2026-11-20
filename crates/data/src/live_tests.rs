//! The follower against a scripted chain that moves and reorganizes, the
//! replay of a stored history, and checkpoints.

use std::cell::{Cell, RefCell};

use alloy_primitives::{keccak256, Address, B256};
use lpbot_chain::eth::{BlockHeader, HashedLog, Log};
use lpbot_chain::uniswap_v3::ChainError;

use super::*;
use crate::manager::manager_root;
use crate::positions::tests::{key, owner, pool_address, Script};
use crate::positions::{PositionId, MANAGER};
use crate::store::testing::TempDir;
use crate::store::{EventStore, StoredLog};

fn hash(number: u64, branch: u8) -> B256 {
    let mut bytes = number.to_be_bytes().to_vec();
    bytes.push(branch);
    keccak256(bytes)
}

fn time_of(block: u64) -> u64 {
    1_700_000_000 + block * 12
}

/// A chain to follow: blocks from 101 on, each on a branch, with the
/// logs of a script. The block a branch starts at has the previous
/// branch's block as parent.
struct FakeHead {
    blocks: RefCell<Vec<(BlockHeader, Vec<ChainLog>)>>,
    head: Cell<u64>,
    finalized: Cell<Option<u64>>,
    asked: RefCell<Vec<Wanted>>,
}

impl FakeHead {
    fn new() -> Self {
        Self {
            blocks: RefCell::new(Vec::new()),
            head: Cell::new(100),
            finalized: Cell::new(None),
            asked: RefCell::new(Vec::new()),
        }
    }

    /// Puts the script's blocks `from` to `to` on `branch`, replacing what
    /// was there from `from` on.
    fn build(&self, script: &Script, from: u64, to: u64, branch: u8) {
        let mut blocks = self.blocks.borrow_mut();
        blocks.retain(|(h, _)| h.number < from);
        let mut parent = blocks
            .last()
            .map_or(hash(100, 0), |(header, _)| header.hash);
        for number in from..=to {
            let header = BlockHeader {
                number,
                hash: hash(number, branch),
                parent_hash: parent,
                time: time_of(number),
            };
            parent = header.hash;
            let logs = script
                .logs
                .iter()
                .filter(|l| l.block == number)
                .cloned()
                .collect();
            blocks.push((header, logs));
        }
        self.head.set(to);
    }

    fn header(&self, number: u64) -> Option<BlockHeader> {
        self.blocks
            .borrow()
            .iter()
            .find(|(h, _)| h.number == number)
            .map(|(h, _)| *h)
    }
}

impl HeadSource for FakeHead {
    fn latest(&self) -> Result<BlockHeader, ChainError> {
        Ok(self.header(self.head.get()).expect("the head is built"))
    }

    fn finalized(&self) -> Result<Option<u64>, ChainError> {
        Ok(self.finalized.get())
    }

    fn headers(&self, numbers: &[u64]) -> Result<Vec<BlockHeader>, ChainError> {
        numbers
            .iter()
            .map(|n| {
                if *n == 100 {
                    return Ok(BlockHeader {
                        number: 100,
                        hash: hash(100, 0),
                        parent_hash: B256::ZERO,
                        time: time_of(100),
                    });
                }
                self.header(*n)
                    .ok_or_else(|| ChainError::Other(format!("no block {n}")))
            })
            .collect()
    }

    fn logs(
        &self,
        addresses: &[Address],
        from: u64,
        to: u64,
    ) -> Result<Vec<HashedLog>, ChainError> {
        Ok(self
            .blocks
            .borrow()
            .iter()
            .filter(|(h, _)| (from..=to).contains(&h.number))
            .flat_map(|(h, logs)| {
                logs.iter()
                    .filter(|l| addresses.contains(&l.address))
                    .map(|l| HashedLog {
                        log: Log {
                            address: l.address,
                            topics: l.topics.clone(),
                            data: l.data.clone(),
                            block_number: l.block,
                            log_index: u64::from(l.log_index),
                            transaction_hash: l.tx_hash,
                        },
                        block_hash: h.hash,
                    })
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    fn seeds(&self, wanted: &[Wanted]) -> Result<Vec<Seed>, ChainError> {
        self.asked.borrow_mut().extend_from_slice(wanted);
        // Everything in these scripts starts after block 100: nothing held.
        Ok(wanted
            .iter()
            .map(|w| match w {
                Wanted::Pool { key, .. } => Seed::Pool {
                    key: *key,
                    liquidity: 0,
                    last_inside: Default::default(),
                    owed: (0, 0),
                },
                Wanted::Nft { token, .. } => Seed::Nft {
                    token: *token,
                    state: None,
                },
            })
            .collect())
    }
}

fn snapshot() -> StoredSnapshot {
    StoredSnapshot {
        pool: pool_address(),
        token0: Address::repeat_byte(0x0A),
        token1: Address::repeat_byte(0x0B),
        block: 100,
        day: Day::of_time(time_of(101)),
        state: Script::replica().snapshot(),
    }
}

fn start() -> Follower {
    let state = LiveState::from_snapshots(&[snapshot()], 86_400);
    Follower::new(state, Some(hash(100, 0)))
}

/// Blocks 101 to 104: a base position, an NFT, two swaps.
fn opening() -> Script {
    let mut s = Script::new();
    s.tx();
    s.mint(owner(0xB0), -600, 600, 10u128.pow(18));
    s.mint_nft(7, owner(0x77), -120, 120, 4 * 10u128.pow(17));
    s.tx();
    s.swap(true, 10u64.pow(15));
    s.tx();
    s.swap(false, 3 * 10u64.pow(15));
    s
}

#[test]
fn the_follower_applies_new_blocks_and_finalizes_behind_the_head() {
    let head = FakeHead::new();
    let mut s = opening();
    head.build(&s, 101, 104, 0);
    head.finalized.set(Some(102));
    let mut follower = start();
    assert_eq!(
        follower.step(&head).unwrap(),
        Step::Advanced { from: 101, to: 104 }
    );
    assert_eq!(follower.live.at.block, 104);
    assert_eq!(follower.base.at.block, 102);
    assert_eq!(follower.tail_len(), 2);
    assert_eq!(follower.live.pools[&pool_address()].pool, s.chain);
    assert!(head.asked.borrow().is_empty(), "every position starts here");
    assert_eq!(follower.step(&head).unwrap(), Step::Idle);

    // What the writer gets: the two swaps, their bars, the positions.
    let out = follower.live.take_outbox();
    assert_eq!(out.swaps.len(), 2);
    assert!(out.swaps[0].amount0 > 0.0 && out.swaps[1].amount1 > 0.0);
    // And every tick the swaps went through: down with the first, then up
    // past the start with the second.
    let swept = follower.live.take_swept();
    let (low, high) = swept[&pool_address()];
    assert_eq!(low, out.swaps[0].tick);
    assert_eq!(high, out.swaps[1].tick);
    assert!(low < 0 && high > 0, "{low} {high}");
    assert!(follower.live.take_swept().is_empty());
    assert!(follower.base.take_swept().is_empty());
    assert_eq!(out.pools.len(), 1);
    let flow = follower.live.flow(pool_address(), 0);
    assert_eq!(flow.swaps, 2);
    assert!(flow.fees0 > 0.0 && flow.fees1 > 0.0);
    let (changed, _) = follower.live.book.take_changes();
    assert_eq!(changed.len(), 2, "the base position and NFT 7");
    // The base state records nothing for the writer.
    assert!(follower.base.take_outbox().swaps.is_empty());

    // More blocks, a few at a time.
    s.skip_to(110);
    s.tx();
    s.swap(true, 10u64.pow(14));
    head.build(&s, 105, 110, 0);
    head.finalized.set(Some(108));
    follower.max_blocks = 4;
    assert_eq!(
        follower.step(&head).unwrap(),
        Step::Advanced { from: 105, to: 108 }
    );
    assert_eq!(
        follower.step(&head).unwrap(),
        Step::Advanced { from: 109, to: 110 }
    );
    assert_eq!(follower.base.at.block, 108);
    assert_eq!(follower.live.pools[&pool_address()].pool, s.chain);
}

#[test]
fn a_reorg_takes_back_the_dropped_blocks_and_follows_the_new_branch() {
    let head = FakeHead::new();
    let mut s = opening();
    let mut alt = s.clone();
    // The first branch: two more swaps.
    s.tx();
    s.swap(true, 10u64.pow(15));
    s.tx();
    s.swap(false, 10u64.pow(15));
    head.build(&s, 101, 106, 0);
    head.finalized.set(Some(103));
    let mut follower = start();
    follower.step(&head).unwrap();
    assert_eq!(follower.live.pools[&pool_address()].pool, s.chain);

    // The second branch from block 105: a new position, another swap.
    alt.tx();
    alt.mint(owner(0xA1), -60, 60, 10u128.pow(17));
    alt.tx();
    alt.swap(true, 2 * 10u64.pow(15));
    alt.skip_to(108);
    alt.tx();
    alt.swap(false, 10u64.pow(15));
    head.build(&alt, 105, 108, 1);
    assert_eq!(
        follower.step(&head).unwrap(),
        Step::Reorg {
            fork: 104,
            dropped: 2
        }
    );
    assert_eq!(follower.reorgs, 1);
    assert_eq!(follower.live.at.block, 104);
    assert_eq!(
        follower.step(&head).unwrap(),
        Step::Advanced { from: 105, to: 108 }
    );
    assert_eq!(follower.live.pools[&pool_address()].pool, alt.chain);
    assert!(follower
        .live
        .book
        .holds(&PositionId::Pool(key(owner(0xA1), -60, 60))));

    // Once final, the base has the new branch too.
    head.finalized.set(Some(108));
    assert_eq!(follower.step(&head).unwrap(), Step::Idle);
    assert_eq!(follower.base.at.block, 108);
    assert_eq!(follower.base.pools[&pool_address()].pool, alt.chain);
    assert_eq!(follower.base.book.state(), follower.live.book.state());
}

#[test]
fn a_reorg_below_the_finalized_block_stops_the_follower() {
    let head = FakeHead::new();
    let s = opening();
    let mut alt = Script::new();
    alt.tx();
    alt.swap(true, 1);
    head.build(&s, 101, 104, 0);
    head.finalized.set(Some(104));
    let mut follower = start();
    follower.step(&head).unwrap();
    // A tail block, then a chain that differs from the finalized block on.
    let mut more = s.clone();
    more.tx();
    more.swap(true, 10u64.pow(12));
    head.build(&more, 105, 105, 0);
    head.finalized.set(Some(104));
    follower.step(&head).unwrap();
    head.build(&alt, 104, 106, 3);
    assert!(matches!(
        follower.step(&head),
        Err(LiveError::DeepReorg { block: 104 })
    ));
}

#[test]
fn logs_of_a_block_the_chain_no_longer_has_are_not_applied() {
    let head = FakeHead::new();
    let s = opening();
    head.build(&s, 101, 104, 0);
    let mut follower = start();
    // A header of another branch than the logs: the chain moved meanwhile.
    head.blocks.borrow_mut()[2].0.hash = hash(103, 9);
    head.blocks.borrow_mut()[3].0.parent_hash = hash(103, 9);
    struct Moving<'a>(&'a FakeHead);
    impl HeadSource for Moving<'_> {
        fn latest(&self) -> Result<BlockHeader, ChainError> {
            self.0.latest()
        }
        fn finalized(&self) -> Result<Option<u64>, ChainError> {
            self.0.finalized()
        }
        fn headers(&self, numbers: &[u64]) -> Result<Vec<BlockHeader>, ChainError> {
            self.0.headers(numbers)
        }
        fn logs(&self, a: &[Address], from: u64, to: u64) -> Result<Vec<HashedLog>, ChainError> {
            let mut logs = self.0.logs(a, from, to)?;
            for log in logs.iter_mut().filter(|l| l.log.block_number == 103) {
                log.block_hash = hash(103, 0);
            }
            Ok(logs)
        }
        fn seeds(&self, wanted: &[Wanted]) -> Result<Vec<Seed>, ChainError> {
            self.0.seeds(wanted)
        }
    }
    assert!(matches!(
        follower.step(&Moving(&head)).unwrap(),
        Step::Retry(_)
    ));
    assert_eq!(follower.live.at.block, 100);
    assert_eq!(
        follower.step(&head).unwrap(),
        Step::Advanced { from: 101, to: 104 }
    );
}

fn stored(logs: &[ChainLog]) -> Vec<StoredLog> {
    logs.iter()
        .map(|l| StoredLog {
            block_number: l.block,
            block_time: l.time,
            log_index: l.log_index,
            tx_hash: l.tx_hash,
            topics: l.topics.clone(),
            data: l.data.clone(),
        })
        .collect()
}

#[test]
fn the_stored_history_replays_into_the_state_a_follower_starts_from() {
    let mut s = opening();
    // The next day: NFT 7 is taken out, and the base position collects.
    s.skip_to(600);
    s.tx();
    let principal = s.burn(MANAGER, -120, 120, 4 * 10u128.pow(17));
    s.decrease(7, 4 * 10u128.pow(17), principal);
    let (first, second) = (Day::of_time(time_of(101)), Day::of_time(time_of(600)));
    assert_eq!(second, first.next());

    let dir = TempDir::new("live-replay");
    let pools = EventStore::new(dir.path());
    let manager = EventStore::new(manager_root(dir.path()));
    for day in [first, second] {
        let of_day: Vec<ChainLog> = s
            .logs
            .iter()
            .filter(|l| Day::of_time(l.time) == day)
            .cloned()
            .collect();
        let (of_manager, of_pool): (Vec<ChainLog>, Vec<ChainLog>) =
            of_day.into_iter().partition(|l| l.address == MANAGER);
        pools
            .write_day("ethereum", pool_address(), day, &stored(&of_pool), true)
            .unwrap();
        manager
            .write_day("ethereum", MANAGER, day, &stored(&of_manager), true)
            .unwrap();
    }

    let mut state = LiveState::from_snapshots(&[snapshot()], 2 * 86_400);
    state.set_record(true);
    let mut days = Vec::new();
    let mut swaps = 0;
    let head = FakeHead::new();
    let state = replay_history(
        dir.path(),
        "ethereum",
        &[snapshot()],
        second,
        state,
        &head,
        |day, state| {
            days.push(day);
            swaps += state.take_outbox().swaps.len();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(days, vec![first, second]);
    assert_eq!(swaps, 2);
    assert_eq!(state.at.block, 600);
    assert_eq!(state.pools[&pool_address()].pool, s.chain);
    let nft = state.book.view(&PositionId::Nft(7)).unwrap();
    assert_eq!(nft.ledger.liquidity, 0);
    assert_eq!(nft.ledger.closed.unwrap().block, 600);

    // A follower carries on from it.
    let follower = Follower::new(state, None);
    assert_eq!(follower.live.at.block, 600);
    assert!(follower.live.records());
    assert!(!follower.base.records());

    // A day that is not stored stops the replay.
    let missing = replay_history(
        dir.path(),
        "ethereum",
        &[snapshot()],
        second.next(),
        LiveState::from_snapshots(&[snapshot()], 60),
        &head,
        |_, _| Ok(()),
    );
    assert!(matches!(missing, Err(LiveError::Store(_))));
}

#[test]
fn nfts_nobody_touched_are_found_on_the_node_and_adopted() {
    use alloy_primitives::{hex, U256};
    use lpbot_chain::testing::MockNode;
    use serde_json::json;

    let mut state = LiveState::from_snapshots(&[snapshot()], 60);
    let (token0, token1) = (snapshot().token0, snapshot().token1);
    // Token 5 the book has met; tokens 1 to 4 it has not.
    let range = key(MANAGER, -60, 60).range;
    let held = |liquidity: u128| NftSeed {
        range,
        holder: owner(0x55),
        liquidity,
        last_inside: Default::default(),
        owed: (0, 0),
    };
    assert!(state
        .book
        .adopt_nft(5, held(7), Default::default(), state.at));
    assert!(!state
        .book
        .adopt_nft(5, held(7), Default::default(), state.at));
    assert_eq!(state.book.max_token(), 5);

    let word = |v: U256| hex::encode(v.to_be_bytes::<32>());
    let addr = |a: Address| U256::from_be_slice(a.as_slice());
    let signed = |t: i32| alloy_primitives::I256::try_from(t).unwrap().into_raw();
    let node = MockNode::start(move |method, params| {
        assert_eq!(method, "eth_call");
        assert_eq!(params[1], "0x64", "read at the state's block");
        let data = hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
        let id = U256::from_be_slice(&data[4..36]).to::<u64>();
        let positions = hex::encode(&keccak256("positions(uint256)")[..4]);
        if hex::encode(&data[..4]) != positions {
            return Ok(json!(format!("0x{}", word(addr(owner(0x40 + id as u8))))));
        }
        let (t0, liquidity) = match id {
            1 => (Address::repeat_byte(0x01), 9u64),
            2 => (token0, 1_000),
            3 => return Err((3, "execution reverted: Invalid token ID".into())),
            4 => (token0, 0),
            other => panic!("token {other} is known and should not be read"),
        };
        let words = [
            U256::ZERO,
            U256::ZERO,
            addr(t0),
            addr(token1),
            U256::from(3_000u32),
            signed(-60),
            signed(60),
            U256::from(liquidity),
            U256::from(3u8),
            U256::from(4u8),
            U256::ZERO,
            U256::ZERO,
        ];
        Ok(json!(format!(
            "0x{}",
            words.iter().map(|w| word(*w)).collect::<String>()
        )))
    });
    let client = RpcClient::new(node.url());
    let found = state.untouched_nfts(&client, 2, |_| {}).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    let (token, seed) = &found[0];
    assert_eq!(*token, 2);
    assert_eq!(seed.holder, owner(0x42));
    assert_eq!(seed.range, range);
    assert_eq!(seed.liquidity, 1_000);
    assert_eq!(state.adopt_nfts(found), 1);
    let two = state.book.view(&PositionId::Nft(2)).unwrap();
    assert_eq!(two.owner, owner(0x42));
    assert_eq!(two.ledger.opened, None);
    assert_eq!(two.ledger.since, state.at);
    assert_eq!(state.book.active_liquidity(pool_address(), 0), 1_007);
}

#[test]
fn a_checkpoint_gives_back_the_base_state() {
    let head = FakeHead::new();
    let s = opening();
    head.build(&s, 101, 104, 0);
    head.finalized.set(Some(104));
    let mut follower = start();
    follower.step(&head).unwrap();
    let dir = TempDir::new("checkpoint");
    let path = dir.path().join("live").join("checkpoint.json");
    save_checkpoint(&path, "ethereum", &follower.base, follower.base_hash).unwrap();
    let (back, hash_back) = load_checkpoint(&path, "ethereum").unwrap().unwrap();
    assert_eq!(hash_back, Some(hash(104, 0)));
    assert_eq!(back.at, follower.base.at);
    assert_eq!(back.since, follower.base.since);
    assert_eq!(back.book.state(), follower.base.book.state());
    for (address, lp) in &follower.base.pools {
        assert_eq!(back.pools[address].pool, lp.pool);
        assert_eq!(back.pools[address].bars, lp.bars);
        assert_eq!(back.pools[address].token1, lp.token1);
    }
    // Following on from it gives the same as following on without it.
    let mut again = Follower::new(back, hash_back);
    let mut more = s.clone();
    more.tx();
    more.swap(true, 10u64.pow(13));
    head.build(&more, 105, 105, 0);
    again.step(&head).unwrap();
    follower.step(&head).unwrap();
    assert_eq!(
        again.live.pools[&pool_address()].pool,
        follower.live.pools[&pool_address()].pool
    );

    assert!(load_checkpoint(&dir.path().join("none.json"), "ethereum")
        .unwrap()
        .is_none());
    assert!(load_checkpoint(&path, "base").is_err());
    std::fs::write(&path, b"{").unwrap();
    assert!(matches!(
        load_checkpoint(&path, "ethereum"),
        Err(LiveError::Checkpoint { .. })
    ));
}
