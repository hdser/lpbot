//! The position book against a replica, on scripted histories whose
//! amounts come from the pool math itself.

use alloy_primitives::{Address, B256, I256, U256};
use lpbot_venues::uniswap_v3::events::{encode_log, topics, PoolEvent};
use lpbot_venues::uniswap_v3::pool::{fees_earned, Pool, Replay, SwapEvent};
use lpbot_venues::uniswap_v3::tick_math::{MAX_SQRT_RATIO, MIN_SQRT_RATIO};
use lpbot_venues::uniswap_v3::Q96;

use super::*;
use crate::fee_check::collect_topic;

pub(crate) fn pool_address() -> Address {
    "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
        .parse()
        .unwrap()
}

pub(crate) fn owner(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

fn signed(value: i32) -> B256 {
    B256::from(
        I256::try_from(value)
            .unwrap()
            .into_raw()
            .to_be_bytes::<32>(),
    )
}

fn address_topic(address: Address) -> B256 {
    address.into_word()
}

fn id_topic(token: u64) -> B256 {
    B256::from(U256::from(token).to_be_bytes::<32>())
}

fn words(values: &[U256]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes::<32>()).collect()
}

/// Writes a history: logs in chain order, with amounts from a pool that
/// runs alongside, the "chain".
#[derive(Clone)]
pub(crate) struct Script {
    pub(crate) chain: Pool,
    pub(crate) logs: Vec<ChainLog>,
    pub(crate) block: u64,
    index: u32,
    tx: u64,
}

impl Script {
    pub(crate) fn new() -> Self {
        let mut chain = Pool::new(3_000, 60);
        chain.initialize(Q96).unwrap();
        Self {
            chain,
            logs: Vec::new(),
            block: 100,
            index: 0,
            tx: 0,
        }
    }

    /// The pool the book replays: the chain as it was at the start.
    pub(crate) fn replica() -> Pool {
        let mut pool = Pool::new(3_000, 60);
        pool.initialize(Q96).unwrap();
        pool
    }

    /// Moves on to a later block, leaving the ones between empty.
    pub(crate) fn skip_to(&mut self, block: u64) {
        assert!(block > self.block);
        self.block = block - 1;
    }

    /// Starts a new transaction in a new block.
    pub(crate) fn tx(&mut self) {
        self.block += 1;
        self.tx += 1;
        self.index = 0;
    }

    pub(crate) fn push(&mut self, address: Address, topics: Vec<B256>, data: Vec<u8>) -> ChainLog {
        let log = ChainLog {
            address,
            block: self.block,
            time: 1_700_000_000 + self.block * 12,
            log_index: self.index,
            tx_hash: B256::from(U256::from(self.tx).to_be_bytes::<32>()),
            topics,
            data,
        };
        self.index += 1;
        self.logs.push(log.clone());
        log
    }

    pub(crate) fn mint(
        &mut self,
        who: Address,
        lower: i32,
        upper: i32,
        amount: u128,
    ) -> (U256, U256) {
        let (a0, a1) = self.chain.mint(lower, upper, amount).unwrap();
        self.push(
            pool_address(),
            vec![
                topics::mint(),
                address_topic(who),
                signed(lower),
                signed(upper),
            ],
            words(&[U256::ZERO, U256::from(amount), a0, a1]),
        );
        (a0, a1)
    }

    pub(crate) fn burn(
        &mut self,
        who: Address,
        lower: i32,
        upper: i32,
        amount: u128,
    ) -> (U256, U256) {
        let (a0, a1) = self.chain.burn(lower, upper, amount).unwrap();
        self.push(
            pool_address(),
            vec![
                topics::burn(),
                address_topic(who),
                signed(lower),
                signed(upper),
            ],
            words(&[U256::from(amount), a0, a1]),
        );
        (a0, a1)
    }

    pub(crate) fn collect(&mut self, who: Address, lower: i32, upper: i32, amounts: (U256, U256)) {
        self.push(
            pool_address(),
            vec![
                collect_topic(),
                address_topic(who),
                signed(lower),
                signed(upper),
            ],
            words(&[U256::ZERO, amounts.0, amounts.1]),
        );
    }

    pub(crate) fn swap(&mut self, zero_for_one: bool, amount_in: u64) {
        let limit = if zero_for_one {
            MIN_SQRT_RATIO + U256::from(1u8)
        } else {
            MAX_SQRT_RATIO - U256::from(1u8)
        };
        let amounts = self
            .chain
            .swap(
                zero_for_one,
                I256::try_from(amount_in).unwrap(),
                limit,
                &mut (),
            )
            .unwrap();
        let event = PoolEvent::Swap(SwapEvent {
            amount0: amounts.amount0,
            amount1: amounts.amount1,
            sqrt_price_x96: self.chain.sqrt_price_x96(),
            liquidity: self.chain.liquidity(),
            tick: self.chain.tick(),
        });
        let (topics, data) = encode_log(&event);
        self.push(pool_address(), topics, data);
    }

    pub(crate) fn transfer(&mut self, from: Address, to: Address, token: u64) {
        self.push(
            MANAGER,
            vec![
                manager_topics::transfer(),
                address_topic(from),
                address_topic(to),
                id_topic(token),
            ],
            Vec::new(),
        );
    }

    pub(crate) fn increase(&mut self, token: u64, liquidity: u128, amounts: (U256, U256)) {
        self.push(
            MANAGER,
            vec![manager_topics::increase_liquidity(), id_topic(token)],
            words(&[U256::from(liquidity), amounts.0, amounts.1]),
        );
    }

    pub(crate) fn decrease(&mut self, token: u64, liquidity: u128, amounts: (U256, U256)) {
        self.push(
            MANAGER,
            vec![manager_topics::decrease_liquidity(), id_topic(token)],
            words(&[U256::from(liquidity), amounts.0, amounts.1]),
        );
    }

    pub(crate) fn manager_collect(&mut self, token: u64, amounts: (U256, U256)) {
        self.push(
            MANAGER,
            vec![manager_topics::collect(), id_topic(token)],
            words(&[U256::ZERO, amounts.0, amounts.1]),
        );
    }

    /// Mints an NFT through the manager, as `NonfungiblePositionManager.mint` does.
    pub(crate) fn mint_nft(
        &mut self,
        token: u64,
        holder: Address,
        lower: i32,
        upper: i32,
        liquidity: u128,
    ) {
        self.tx();
        let amounts = self.mint(MANAGER, lower, upper, liquidity);
        self.transfer(Address::ZERO, holder, token);
        self.increase(token, liquidity, amounts);
    }
}

/// Feeds logs to a book and its replica, as the live state does.
pub(crate) fn feed(book: &mut PositionBook, replica: &mut Pool, logs: &[ChainLog]) {
    for log in logs {
        if log.address == MANAGER {
            if let Some(event) = decode_manager(&log.topics, &log.data).unwrap() {
                book.manager_event(&event, log.stamp(), log.tx_hash);
            }
        } else {
            apply_pool_log(book, replica, log).unwrap();
        }
    }
}

pub(crate) fn key(who: Address, lower: i32, upper: i32) -> PoolKey {
    PoolKey {
        owner: who,
        range: Range {
            pool: pool_address(),
            tick_lower: lower,
            tick_upper: upper,
        },
    }
}

fn pair(a: (U256, U256), b: (U256, U256)) -> (U256, U256) {
    (a.0 + b.0, a.1 + b.1)
}

#[test]
fn a_pool_position_is_followed_from_mint_to_collect_and_then_let_go() {
    let mut s = Script::new();
    let (base, lp) = (owner(0xB0), owner(0xA1));
    s.tx();
    s.mint(base, -600, 600, 10u128.pow(18));
    s.tx();
    let deposited = s.mint(lp, -120, 120, 5 * 10u128.pow(17));
    let at_mint = s.chain.fee_growth_inside(-120, 120);
    s.tx();
    s.swap(true, 10u64.pow(15));
    s.tx();
    s.swap(false, 2 * 10u64.pow(15));
    s.tx();
    let at_burn = s.chain.fee_growth_inside(-120, 120);
    let principal = s.burn(lp, -120, 120, 5 * 10u128.pow(17));
    let fees = (
        fees_earned(at_burn.0, at_mint.0, 5 * 10u128.pow(17)),
        fees_earned(at_burn.1, at_mint.1, 5 * 10u128.pow(17)),
    );
    assert!(
        !fees.0.is_zero() && !fees.1.is_zero(),
        "the swaps paid fees"
    );

    let mut book = PositionBook::new([pool_address()]);
    book.record = true;
    let mut replica = Script::replica();
    feed(&mut book, &mut replica, &s.logs);

    let view = book.view(&PositionId::Pool(key(lp, -120, 120))).unwrap();
    let ledger = &view.ledger;
    assert_eq!(ledger.liquidity, 0);
    assert_eq!(ledger.owed, pair(principal, fees));
    assert_eq!(ledger.fees, fees);
    assert_eq!(ledger.deposited, deposited);
    assert_eq!(ledger.withdrawn, principal);
    assert_eq!(ledger.opened.unwrap().block, 102);
    assert_eq!(ledger.closed.unwrap().block, 105);
    let (changed, finished) = book.take_changes();
    assert_eq!(changed.len(), 2, "the base position and this one");
    assert!(finished.is_empty());

    // Collecting everything owed finishes it: it is recorded and let go.
    let owed = ledger.owed;
    s.logs.clear();
    s.tx();
    s.collect(lp, -120, 120, owed);
    feed(&mut book, &mut replica, &s.logs);
    let (changed, finished) = book.take_changes();
    assert!(changed.is_empty());
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].ledger.collected, owed);
    assert!(finished[0].ledger.is_empty());
    assert!(!book.holds(&PositionId::Pool(key(lp, -120, 120))));

    // Used again, it is known to start from nothing: no chain read.
    s.logs.clear();
    s.tx();
    s.mint(lp, -120, 120, 1_000);
    assert!(book.wanted(&s.logs, &|_| true).is_empty());
    feed(&mut book, &mut replica, &s.logs);
    let again = book.view(&PositionId::Pool(key(lp, -120, 120))).unwrap();
    assert_eq!(again.ledger.liquidity, 1_000);
    assert_eq!(again.ledger.opened.unwrap().block, s.block);
    assert_eq!(book.take_anomalies().1, 0);
}

#[test]
fn nfts_get_their_share_of_the_managers_pool_position() {
    let mut s = Script::new();
    let (holder, other, buyer) = (owner(0x11), owner(0x22), owner(0x33));
    s.tx();
    s.mint(owner(0xB0), -600, 600, 10u128.pow(18));
    s.mint_nft(7, holder, -120, 120, 4 * 10u128.pow(17));
    let mint_inside = s.chain.fee_growth_inside(-120, 120);
    s.mint_nft(8, other, -120, 120, 10u128.pow(17));
    s.tx();
    s.swap(true, 10u64.pow(15));
    s.tx();
    s.swap(false, 2 * 10u64.pow(15));
    // Token 7 is taken out in full, then collected: no `burn(0)` first, as
    // it holds no liquidity by then.
    s.tx();
    let at_decrease = s.chain.fee_growth_inside(-120, 120);
    let principal = s.burn(MANAGER, -120, 120, 4 * 10u128.pow(17));
    s.decrease(7, 4 * 10u128.pow(17), principal);
    // Token 8 changes hands; an NFT of a pool not followed pairs with nothing.
    s.tx();
    s.transfer(other, buyer, 8);
    s.increase(99, 5, (U256::from(1u8), U256::from(1u8)));

    let mut book = PositionBook::new([pool_address()]);
    book.record = true;
    let mut replica = Script::replica();
    feed(&mut book, &mut replica, &s.logs);

    let seven = book.view(&PositionId::Nft(7)).unwrap();
    assert_eq!(seven.owner, holder);
    assert_eq!(seven.range, key(MANAGER, -120, 120).range);
    let fees7 = (
        fees_earned(at_decrease.0, mint_inside.0, 4 * 10u128.pow(17)),
        fees_earned(at_decrease.1, mint_inside.1, 4 * 10u128.pow(17)),
    );
    assert_eq!(seven.ledger.liquidity, 0);
    assert_eq!(seven.ledger.owed, pair(principal, fees7));
    assert_eq!(seven.ledger.fees, fees7);
    assert_eq!(book.view(&PositionId::Nft(8)).unwrap().owner, buyer);
    assert!(book.view(&PositionId::Nft(99)).is_none());

    // The two NFTs' fees add up to the manager's pool position's, but for
    // rounding: each is rounded down on its own.
    let manager = book
        .view(&PositionId::Pool(key(MANAGER, -120, 120)))
        .unwrap();
    let eight = book.view(&PositionId::Nft(8)).unwrap();
    let inside_now = replica.fee_growth_inside(-120, 120);
    let (_, pending8) = eight.ledger.pending(inside_now);
    let nft_fees = pair(seven.ledger.fees, pair(eight.ledger.fees, pending8));
    let (_, manager_pending) = manager.ledger.pending(inside_now);
    let manager_fees = pair(manager.ledger.fees, manager_pending);
    assert!(manager_fees.0 - nft_fees.0 <= U256::from(2u8));
    assert!(manager_fees.1 - nft_fees.1 <= U256::from(2u8));

    // The manager's own pool positions are not shown, the NFTs are.
    let shown: Vec<PositionId> = book.shown().map(|v| v.id).collect();
    assert!(shown.contains(&PositionId::Nft(7)) && shown.contains(&PositionId::Nft(8)));
    assert!(!shown.contains(&PositionId::Pool(key(MANAGER, -120, 120))));
    let (changed, _) = book.take_changes();
    assert!(changed
        .iter()
        .all(|v| !matches!(v.id, PositionId::Pool(k) if k.owner == MANAGER)));

    // Collecting finishes token 7; its liquidity is gone from the range.
    s.logs.clear();
    s.tx();
    let owed7 = seven.ledger.owed;
    s.collect(MANAGER, -120, 120, owed7);
    s.manager_collect(7, owed7);
    feed(&mut book, &mut replica, &s.logs);
    let (_, finished) = book.take_changes();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].id, PositionId::Nft(7));
    assert_eq!(finished[0].ledger.collected, owed7);
    assert_eq!(
        book.active_liquidity(pool_address(), 0),
        10u128.pow(18) + 10u128.pow(17)
    );
    assert_eq!(book.manager_liquidity(pool_address(), 0), 10u128.pow(17));
    assert_eq!(book.take_anomalies().1, 0);

    // A burned NFT leaves the book.
    s.logs.clear();
    s.tx();
    s.transfer(holder, Address::ZERO, 7);
    feed(&mut book, &mut replica, &s.logs);
    assert!(!book.holds(&PositionId::Nft(7)));
}

#[test]
fn an_nft_emptied_and_collected_in_one_transaction_is_paid_and_let_go() {
    // The only NFT of its range: taking it out and collecting in one
    // transaction empties the manager's pool position before the NFT's
    // own collect, which must still be paid.
    let mut s = Script::new();
    s.tx();
    s.mint(owner(0xB0), -600, 600, 10u128.pow(18));
    s.mint_nft(9, owner(0x99), -60, 60, 3 * 10u128.pow(17));
    s.tx();
    s.swap(true, 10u64.pow(15));
    s.tx();
    s.swap(false, 2 * 10u64.pow(15));
    s.tx();
    let principal = s.burn(MANAGER, -60, 60, 3 * 10u128.pow(17));
    s.decrease(9, 3 * 10u128.pow(17), principal);

    let mut book = PositionBook::new([pool_address()]);
    book.record = true;
    let mut replica = Script::replica();
    feed(&mut book, &mut replica, &s.logs);
    let owed = book.view(&PositionId::Nft(9)).unwrap().ledger.owed;
    let manager_owed = book
        .view(&PositionId::Pool(key(MANAGER, -60, 60)))
        .unwrap()
        .ledger
        .owed;
    assert_eq!(owed, manager_owed, "one NFT: its share is the whole");

    // The same transaction goes on: the pool pays the manager, the manager
    // pays the NFT.
    s.logs.clear();
    s.collect(MANAGER, -60, 60, manager_owed);
    s.manager_collect(9, owed);
    feed(&mut book, &mut replica, &s.logs);
    let (anomalies, count) = book.take_anomalies();
    assert_eq!(count, 0, "{anomalies:?}");
    assert!(!book.holds(&PositionId::Nft(9)));
    assert!(!book.holds(&PositionId::Pool(key(MANAGER, -60, 60))));
    let (_, finished) = book.take_changes();
    let nine = finished
        .iter()
        .find(|v| v.id == PositionId::Nft(9))
        .unwrap();
    assert_eq!(nine.ledger.collected, owed);
    assert!(nine.ledger.is_empty());

    // Collecting nothing from it later changes nothing, and does not
    // write it again as a new, empty position.
    s.logs.clear();
    s.tx();
    s.collect(MANAGER, -60, 60, (U256::ZERO, U256::ZERO));
    s.manager_collect(9, (U256::ZERO, U256::ZERO));
    s.collect(owner(0xC1), -60, 60, (U256::ZERO, U256::ZERO));
    assert_eq!(
        book.wanted(&s.logs, &|_| true).len(),
        1,
        "only the owner never seen"
    );
    book.stage([Seed::Pool {
        key: key(owner(0xC1), -60, 60),
        liquidity: 0,
        last_inside: Default::default(),
        owed: (0, 0),
    }]);
    feed(&mut book, &mut replica, &s.logs);
    book.clear_staged();
    assert_eq!(book.take_anomalies().1, 0);
    assert_eq!(book.take_changes(), (vec![], vec![]));
    assert!(!book.holds(&PositionId::Nft(9)));
}

#[test]
fn positions_from_before_the_history_are_read_from_the_chain_once() {
    let mut s = Script::new();
    let lp = owner(0xA1);
    // The history before the book starts: positions it has never seen.
    s.tx();
    s.mint(owner(0xB0), -600, 600, 10u128.pow(18));
    s.tx();
    s.mint(lp, -120, 120, 3 * 10u128.pow(17));
    s.mint_nft(5, owner(0x55), -60, 60, 2 * 10u128.pow(17));
    s.tx();
    s.swap(true, 10u64.pow(15));
    let mut replica = s.chain.clone();
    let lp_last = s.chain.fee_growth_inside(-120, 120);
    let nft_last = s.chain.fee_growth_inside(-60, 60);
    s.logs.clear();

    // What the book sees: more swaps, then a burn by each.
    s.tx();
    s.swap(false, 10u64.pow(15));
    s.tx();
    let lp_inside = s.chain.fee_growth_inside(-120, 120);
    let lp_principal = s.burn(lp, -120, 120, 10u128.pow(17));
    s.tx();
    let nft_principal = s.burn(MANAGER, -60, 60, 2 * 10u128.pow(17));
    s.decrease(5, 2 * 10u128.pow(17), nft_principal);

    let mut book = PositionBook::new([pool_address()]);
    let wanted = book.wanted(&s.logs, &|_| true);
    let burn_block = s.block - 1;
    assert_eq!(
        wanted,
        vec![
            Wanted::Pool {
                key: key(lp, -120, 120),
                block: burn_block - 1
            },
            Wanted::Pool {
                key: key(MANAGER, -60, 60),
                block: burn_block
            },
            Wanted::Nft {
                token: 5,
                range: key(MANAGER, -60, 60).range,
                block: burn_block
            },
        ]
    );
    // The chain's answers: as they were after the history before.
    book.stage([
        Seed::Pool {
            key: key(lp, -120, 120),
            liquidity: 3 * 10u128.pow(17),
            last_inside: (U256::ZERO, U256::ZERO),
            owed: (10, 20),
        },
        Seed::Pool {
            key: key(MANAGER, -60, 60),
            liquidity: 2 * 10u128.pow(17),
            last_inside: (U256::ZERO, U256::ZERO),
            owed: (0, 0),
        },
        Seed::Nft {
            token: 5,
            state: Some(NftSeed {
                range: key(MANAGER, -60, 60).range,
                holder: owner(0x55),
                liquidity: 2 * 10u128.pow(17),
                last_inside: (U256::ZERO, U256::ZERO),
                owed: (0, 0),
            }),
        },
    ]);
    feed(&mut book, &mut replica, &s.logs);
    book.clear_staged();

    let lp_view = book.view(&PositionId::Pool(key(lp, -120, 120))).unwrap();
    let ledger = &lp_view.ledger;
    assert_eq!(ledger.liquidity, 2 * 10u128.pow(17));
    // What it was owed before, plus fees since its mint, plus the principal.
    let earned = (
        fees_earned(lp_inside.0, U256::ZERO, 3 * 10u128.pow(17)),
        fees_earned(lp_inside.1, U256::ZERO, 3 * 10u128.pow(17)),
    );
    assert_eq!(
        ledger.owed,
        pair(
            pair((U256::from(10u8), U256::from(20u8)), earned),
            lp_principal
        )
    );
    // Fees counted as earned while followed start at its first event.
    assert_eq!(ledger.fees, (U256::ZERO, U256::ZERO));
    assert_eq!(ledger.opened, None);
    assert_eq!(ledger.since.block, burn_block);
    assert!(lp_last.0 <= lp_inside.0);

    let nft = book.view(&PositionId::Nft(5)).unwrap();
    assert_eq!(nft.owner, owner(0x55));
    assert_eq!(nft.ledger.liquidity, 0);
    assert_eq!(nft.ledger.withdrawn, nft_principal);
    assert!(nft.ledger.owed.0 >= nft_principal.0);
    assert!(nft_last.0 > U256::ZERO);
    assert_eq!(book.take_anomalies().1, 0);

    // Seen once, never wanted again.
    assert!(book.wanted(&s.logs, &|_| true).is_empty());
}

#[test]
fn a_fresh_nft_needs_no_chain_read_and_a_book_without_its_past_says_so() {
    let mut s = Script::new();
    s.tx();
    s.mint(owner(0xB0), -600, 600, 10u128.pow(18));
    s.mint_nft(3, owner(0x33), -120, 120, 1_000);
    let mut book = PositionBook::new([pool_address()]);
    // The base LP and the manager's pool position are read; the NFT minted
    // in the same transaction is not.
    let wanted = book.wanted(&s.logs, &|_| true);
    assert_eq!(wanted.len(), 2);
    assert!(wanted.iter().all(|w| matches!(w, Wanted::Pool { .. })));
    // Minting into ranges no one holds, they start from nothing: no read.
    assert!(book.wanted(&s.logs, &|_| false).is_empty());

    // A burn larger than what the book knows is kept, and reported.
    let mut replica = Script::replica();
    feed(&mut book, &mut replica, &s.logs);
    let pretend = PoolPositionEvent::Burn {
        key: key(owner(0xC0), -60, 60),
        amount: 5,
        amounts: (U256::ZERO, U256::ZERO),
    };
    book.pool_event(
        &pretend,
        (U256::ZERO, U256::ZERO),
        Stamp::default(),
        B256::ZERO,
    );
    let (anomalies, count) = book.take_anomalies();
    assert_eq!(count, 1);
    assert!(anomalies[0].what.contains("more than the book knew"));
}

#[test]
fn events_decode_from_their_logs() {
    let mut s = Script::new();
    s.tx();
    s.mint(owner(0xB0), -600, 600, 77);
    s.collect(owner(0xB0), -600, 600, (U256::from(3u8), U256::from(4u8)));
    s.transfer(owner(1), owner(2), 42);
    s.manager_collect(42, (U256::from(5u8), U256::from(6u8)));
    let logs = &s.logs;
    assert!(matches!(
        decode_pool_position(pool_address(), &logs[0].topics, &logs[0].data).unwrap(),
        Some(PoolPositionEvent::Mint { amount: 77, key: k, .. }) if k == key(owner(0xB0), -600, 600)
    ));
    assert_eq!(
        decode_pool_position(pool_address(), &logs[1].topics, &logs[1].data).unwrap(),
        Some(PoolPositionEvent::Collect {
            key: key(owner(0xB0), -600, 600),
            amounts: (U256::from(3u8), U256::from(4u8))
        })
    );
    assert_eq!(
        decode_manager(&logs[2].topics, &logs[2].data).unwrap(),
        Some(ManagerEvent::Transfer {
            token: 42,
            from: owner(1),
            to: owner(2)
        })
    );
    assert_eq!(
        decode_manager(&logs[3].topics, &logs[3].data).unwrap(),
        Some(ManagerEvent::Collect {
            token: 42,
            amounts: (U256::from(5u8), U256::from(6u8))
        })
    );
    // Logs the book does not follow are passed over, broken ones refused.
    let (swap_topics, swap_data) = encode_log(&PoolEvent::Flash {
        paid0: U256::ZERO,
        paid1: U256::ZERO,
    });
    assert_eq!(
        decode_pool_position(pool_address(), &swap_topics, &swap_data).unwrap(),
        None
    );
    assert!(decode_pool_position(pool_address(), &logs[0].topics[..2], &logs[0].data).is_err());
    assert!(decode_manager(&logs[3].topics, &logs[3].data[..40]).is_err());
    let approval = keccak256("Approval(address,address,uint256)");
    assert_eq!(decode_manager(&[approval], &[]).unwrap(), None);
}

#[test]
fn a_book_comes_back_whole_from_its_state() {
    let mut s = Script::new();
    s.tx();
    s.mint(owner(0xB0), -600, 600, 10u128.pow(18));
    s.mint_nft(3, owner(0x33), -120, 120, 1_000);
    s.tx();
    s.swap(true, 10u64.pow(12));
    let mut book = PositionBook::new([pool_address()]);
    let mut replica = Script::replica();
    feed(&mut book, &mut replica, &s.logs);
    let state = book.state();
    let json = serde_json::to_string(&state).unwrap();
    let back = PositionBook::from_state(serde_json::from_str(&json).unwrap());
    assert_eq!(back.state(), state);
    assert_eq!(back.len(), (2, 1));
    assert_eq!(
        back.view(&PositionId::Nft(3)),
        book.view(&PositionId::Nft(3))
    );
    // The replica stays as strict as the chain's history: replaying it
    // again from the start gives the same pool.
    let mut again = Script::replica();
    for log in &s.logs {
        if log.address != MANAGER {
            if let Some(event) =
                lpbot_venues::uniswap_v3::events::decode_log(&log.topics, &log.data).unwrap()
            {
                again.apply_event(&event, Replay::Strict, &mut ()).unwrap();
            }
        }
    }
    assert_eq!(again, replica);
}
