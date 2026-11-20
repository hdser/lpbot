//! Liquidity positions, followed event by event.
//!
//! A pool keeps a position per owner and range. Most of them belong to
//! Uniswap's position manager (the NonfungiblePositionManager), which holds
//! one pool position per range for all its NFTs together and keeps each
//! NFT's share itself. The book follows both, the way the contracts do:
//!
//! - every pool position (owner, range): liquidity, the fee growth inside
//!   it when it was last touched, and what the pool owes it, from the
//!   pool's `Mint`, `Burn` and `Collect` events;
//! - every position NFT of the pools followed: its range, holder,
//!   liquidity, last fee growth and what it is owed, from the position
//!   manager's `IncreaseLiquidity`, `DecreaseLiquidity`, `Collect` and
//!   `Transfer` events. An NFT event names only its token; the pool event
//!   the manager made just before it, in the same transaction, says which
//!   pool and range it belongs to.
//!
//! The book holds only positions that hold something: liquidity, or tokens
//! owed. A position that is emptied and collected is finished: it leaves
//! the book (and the caller records it), and when it is used again it
//! starts from nothing, as it does on the chain.
//!
//! A position seen for the first time may have been opened before the
//! history began. [`PositionBook::wanted`] lists them; the caller reads
//! their state from the chain at the block before, and
//! [`PositionBook::stage`] hands it over. A position nobody touches during
//! the history is never seen: what it holds stays unattributed.
//!
//! Exact integer arithmetic throughout, as in the contracts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use alloy_primitives::{keccak256, Address, B256, I256, U256};
use lpbot_chain::uniswap_v3::POSITION_MANAGER;
use lpbot_venues::uniswap_v3::events::topics;
use lpbot_venues::uniswap_v3::pool::fees_earned;
use serde::{Deserialize, Serialize};

use crate::fee_check::collect_topic;

/// The position manager the book follows NFTs of.
pub const MANAGER: Address = POSITION_MANAGER;

/// Signature hashes of the position manager's events.
pub mod manager_topics {
    use super::*;

    pub fn increase_liquidity() -> B256 {
        keccak256("IncreaseLiquidity(uint256,uint128,uint256,uint256)")
    }
    pub fn decrease_liquidity() -> B256 {
        keccak256("DecreaseLiquidity(uint256,uint128,uint256,uint256)")
    }
    pub fn collect() -> B256 {
        keccak256("Collect(uint256,address,uint256,uint256)")
    }
    pub fn transfer() -> B256 {
        keccak256("Transfer(address,address,uint256)")
    }

    /// The four the book needs, for a log filter.
    pub fn all() -> Vec<B256> {
        vec![
            increase_liquidity(),
            decrease_liquidity(),
            collect(),
            transfer(),
        ]
    }
}

/// A block and its time: when something happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Stamp {
    pub block: u64,
    /// Unix seconds.
    pub time: u64,
}

/// A pool and a range of ticks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Range {
    pub pool: Address,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

/// A position as the pool files it: owner and range.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct PoolKey {
    pub owner: Address,
    pub range: Range,
}

/// What names a position: a pool position, or a position NFT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PositionId {
    Pool(PoolKey),
    Nft(u64),
}

/// A position's accounts: what the contract keeps, and what was seen while
/// the book followed it. Amounts are in each token's smallest unit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub liquidity: u128,
    /// The contract's fee growth inside the range when last touched.
    pub last_inside: (U256, U256),
    /// What the contract owes and has not paid out: fees credited, and
    /// principal of burns not yet collected.
    pub owed: (U256, U256),
    /// The fee growth inside the range when the book last counted fees.
    /// Differs from `last_inside` only between a seed and the next event.
    pub mark: (U256, U256),
    /// Fees earned while followed, up to the last event.
    pub fees: (U256, U256),
    /// Put in by mints, taken out by burns, and collected, while followed.
    pub deposited: (U256, U256),
    pub withdrawn: (U256, U256),
    pub collected: (U256, U256),
    /// When liquidity last went from none to some; `None` when it already
    /// held some when the book started to follow it.
    pub opened: Option<Stamp>,
    /// When liquidity last went to none, while it has none.
    pub closed: Option<Stamp>,
    /// When the book started to follow it.
    pub since: Stamp,
    /// Its last event.
    pub changed: Stamp,
}

fn add(a: (U256, U256), b: (U256, U256)) -> (U256, U256) {
    (a.0.saturating_add(b.0), a.1.saturating_add(b.1))
}

impl Ledger {
    fn starting(at: Stamp) -> Self {
        Self {
            since: at,
            changed: at,
            ..Self::default()
        }
    }

    /// Credits the fees earned since the last touch at fee growth `inside`,
    /// as the contracts do before every change.
    fn accrue(&mut self, inside: (U256, U256)) {
        let l = self.liquidity;
        self.owed = add(
            self.owed,
            (
                fees_earned(inside.0, self.last_inside.0, l),
                fees_earned(inside.1, self.last_inside.1, l),
            ),
        );
        self.fees = add(
            self.fees,
            (
                fees_earned(inside.0, self.mark.0, l),
                fees_earned(inside.1, self.mark.1, l),
            ),
        );
        self.last_inside = inside;
        self.mark = inside;
    }

    fn add_liquidity(&mut self, amount: u128, amounts: (U256, U256), at: Stamp) -> bool {
        if self.liquidity == 0 && amount > 0 {
            self.opened = Some(at);
            self.closed = None;
        }
        let ok = match self.liquidity.checked_add(amount) {
            Some(l) => {
                self.liquidity = l;
                true
            }
            None => false,
        };
        self.deposited = add(self.deposited, amounts);
        self.changed = at;
        ok
    }

    /// Takes out liquidity and credits its principal. False when it takes
    /// more than the book knew of, which leaves none.
    fn remove_liquidity(&mut self, amount: u128, principal: (U256, U256), at: Stamp) -> bool {
        let ok = amount <= self.liquidity;
        self.liquidity = self.liquidity.saturating_sub(amount);
        if self.liquidity == 0 && amount > 0 {
            self.closed = Some(at);
        }
        self.owed = add(self.owed, principal);
        self.withdrawn = add(self.withdrawn, principal);
        self.changed = at;
        ok
    }

    /// Pays out. False when it pays more than the book says is owed.
    fn pay(&mut self, amounts: (U256, U256), at: Stamp) -> bool {
        let ok = amounts.0 <= self.owed.0 && amounts.1 <= self.owed.1;
        self.owed = (
            self.owed.0.saturating_sub(amounts.0),
            self.owed.1.saturating_sub(amounts.1),
        );
        self.collected = add(self.collected, amounts);
        self.changed = at;
        ok
    }

    /// Holds nothing: no liquidity and nothing owed.
    pub fn is_empty(&self) -> bool {
        self.liquidity == 0 && self.owed.0.is_zero() && self.owed.1.is_zero()
    }

    /// Fees earned since the last touch, at fee growth `inside`: what the
    /// contract would credit if touched now, and the part earned while
    /// followed.
    pub fn pending(&self, inside: (U256, U256)) -> ((U256, U256), (U256, U256)) {
        let l = self.liquidity;
        (
            (
                fees_earned(inside.0, self.last_inside.0, l),
                fees_earned(inside.1, self.last_inside.1, l),
            ),
            (
                fees_earned(inside.0, self.mark.0, l),
                fees_earned(inside.1, self.mark.1, l),
            ),
        )
    }
}

/// A position NFT of a followed pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nft {
    pub range: Range,
    pub holder: Address,
    pub ledger: Ledger,
}

/// A position as the caller shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionView {
    pub id: PositionId,
    pub range: Range,
    /// The pool position's owner, or the NFT's holder.
    pub owner: Address,
    pub ledger: Ledger,
}

/// A pool event that changes a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolPositionEvent {
    Mint {
        key: PoolKey,
        amount: u128,
        amounts: (U256, U256),
    },
    Burn {
        key: PoolKey,
        amount: u128,
        amounts: (U256, U256),
    },
    Collect {
        key: PoolKey,
        amounts: (U256, U256),
    },
}

impl PoolPositionEvent {
    pub fn key(&self) -> PoolKey {
        match self {
            Self::Mint { key, .. } | Self::Burn { key, .. } | Self::Collect { key, .. } => *key,
        }
    }
}

/// A position manager event the book follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerEvent {
    Increase {
        token: u64,
        liquidity: u128,
        amounts: (U256, U256),
    },
    Decrease {
        token: u64,
        liquidity: u128,
        amounts: (U256, U256),
    },
    Collect {
        token: u64,
        amounts: (U256, U256),
    },
    Transfer {
        token: u64,
        from: Address,
        to: Address,
    },
}

fn word(data: &[u8], index: usize) -> Result<U256, String> {
    data.get(index * 32..(index + 1) * 32)
        .map(U256::from_be_slice)
        .ok_or_else(|| format!("the log data has no word {index}"))
}

fn tick(topic: &B256) -> Result<i32, String> {
    i32::try_from(I256::from_raw(U256::from_be_bytes(topic.0)))
        .map_err(|_| "a tick does not fit".to_string())
}

fn token_id(topic: &B256) -> Result<u64, String> {
    u64::try_from(U256::from_be_bytes(topic.0)).map_err(|_| "a token id does not fit".to_string())
}

fn uint128(value: U256) -> Result<u128, String> {
    u128::try_from(value).map_err(|_| "an amount does not fit 128 bits".to_string())
}

/// Decodes a pool log that changes a position: `Mint`, `Burn` or
/// `Collect`. `None` for any other log.
pub fn decode_pool_position(
    pool: Address,
    log_topics: &[B256],
    data: &[u8],
) -> Result<Option<PoolPositionEvent>, String> {
    let Some(topic0) = log_topics.first() else {
        return Ok(None);
    };
    let (mint, burn, collect) = (
        *topic0 == topics::mint(),
        *topic0 == topics::burn(),
        *topic0 == collect_topic(),
    );
    if !(mint || burn || collect) {
        return Ok(None);
    }
    if log_topics.len() != 4 {
        return Err("a position event needs four topics".into());
    }
    let key = PoolKey {
        owner: Address::from_word(log_topics[1]),
        range: Range {
            pool,
            tick_lower: tick(&log_topics[2])?,
            tick_upper: tick(&log_topics[3])?,
        },
    };
    Ok(Some(if mint {
        // The sender is the first data word.
        PoolPositionEvent::Mint {
            key,
            amount: uint128(word(data, 1)?)?,
            amounts: (word(data, 2)?, word(data, 3)?),
        }
    } else if burn {
        PoolPositionEvent::Burn {
            key,
            amount: uint128(word(data, 0)?)?,
            amounts: (word(data, 1)?, word(data, 2)?),
        }
    } else {
        // The recipient is the first data word.
        PoolPositionEvent::Collect {
            key,
            amounts: (word(data, 1)?, word(data, 2)?),
        }
    }))
}

/// Decodes a position manager log. `None` for the ones the book does not
/// need (approvals).
pub fn decode_manager(log_topics: &[B256], data: &[u8]) -> Result<Option<ManagerEvent>, String> {
    let Some(topic0) = log_topics.first() else {
        return Ok(None);
    };
    let need = |n: usize| {
        if log_topics.len() == n {
            Ok(())
        } else {
            Err(format!("expected {n} topics, got {}", log_topics.len()))
        }
    };
    if *topic0 == manager_topics::increase_liquidity() {
        need(2)?;
        Ok(Some(ManagerEvent::Increase {
            token: token_id(&log_topics[1])?,
            liquidity: uint128(word(data, 0)?)?,
            amounts: (word(data, 1)?, word(data, 2)?),
        }))
    } else if *topic0 == manager_topics::decrease_liquidity() {
        need(2)?;
        Ok(Some(ManagerEvent::Decrease {
            token: token_id(&log_topics[1])?,
            liquidity: uint128(word(data, 0)?)?,
            amounts: (word(data, 1)?, word(data, 2)?),
        }))
    } else if *topic0 == manager_topics::collect() {
        need(2)?;
        Ok(Some(ManagerEvent::Collect {
            token: token_id(&log_topics[1])?,
            amounts: (word(data, 1)?, word(data, 2)?),
        }))
    } else if *topic0 == manager_topics::transfer() {
        need(4)?;
        Ok(Some(ManagerEvent::Transfer {
            from: Address::from_word(log_topics[1]),
            to: Address::from_word(log_topics[2]),
            token: token_id(&log_topics[3])?,
        }))
    } else {
        Ok(None)
    }
}

/// What the position manager just did in a followed pool, in the current
/// transaction: the event its own NFT event pairs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Call {
    Mint { range: Range, liquidity: u128 },
    Burn { range: Range, liquidity: u128 },
    Collect { range: Range },
}

/// The pairing of position manager events with the pool events before
/// them, within one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TxContext {
    tx: B256,
    last: Option<Call>,
    /// Tokens minted in this transaction, with their first holder.
    minted: Vec<(u64, Address)>,
    /// The fee growth inside each range at the manager's last mint or burn
    /// there in this transaction: what it reads for its NFTs. Its pool
    /// position may be gone by the NFT's own event, emptied by a collect.
    insides: Vec<(Range, (U256, U256))>,
}

impl TxContext {
    fn enter(&mut self, tx: B256) {
        if self.tx != tx {
            *self = TxContext {
                tx,
                ..TxContext::default()
            };
        }
    }

    fn pool_event(&mut self, tx: B256, event: &PoolPositionEvent, inside: Option<(U256, U256)>) {
        self.enter(tx);
        if event.key().owner != MANAGER {
            return;
        }
        let range = event.key().range;
        if let (Some(inside), false) = (inside, matches!(event, PoolPositionEvent::Collect { .. }))
        {
            self.insides.retain(|(r, _)| *r != range);
            self.insides.push((range, inside));
        }
        self.last = Some(match *event {
            PoolPositionEvent::Mint { amount, .. } => Call::Mint {
                range,
                liquidity: amount,
            },
            PoolPositionEvent::Burn { amount, .. } => Call::Burn {
                range,
                liquidity: amount,
            },
            PoolPositionEvent::Collect { .. } => Call::Collect { range },
        });
    }

    /// The range a position manager event acts on, when it pairs with the
    /// pool event before it; `None` for an NFT of a pool not followed.
    fn pair(&mut self, tx: B256, event: &ManagerEvent) -> Option<Range> {
        self.enter(tx);
        let range = match (*event, self.last) {
            (
                ManagerEvent::Increase { liquidity, .. },
                Some(Call::Mint {
                    range,
                    liquidity: minted,
                }),
            ) if liquidity == minted => range,
            (
                ManagerEvent::Decrease { liquidity, .. },
                Some(Call::Burn {
                    range,
                    liquidity: burned,
                }),
            ) if liquidity == burned => range,
            (ManagerEvent::Collect { .. }, Some(Call::Collect { range })) => range,
            (ManagerEvent::Transfer { token, from, to }, _) => {
                if from == Address::ZERO {
                    self.minted.push((token, to));
                }
                return None;
            }
            _ => return None,
        };
        self.last = None;
        Some(range)
    }

    fn inside(&self, range: &Range) -> Option<(U256, U256)> {
        self.insides
            .iter()
            .find(|(r, _)| r == range)
            .map(|(_, inside)| *inside)
    }

    fn minted_holder(&self, token: u64) -> Option<Address> {
        self.minted
            .iter()
            .find(|(t, _)| *t == token)
            .map(|(_, holder)| *holder)
    }
}

/// A log of a followed contract: a pool, or the position manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLog {
    pub address: Address,
    pub block: u64,
    pub time: u64,
    pub log_index: u32,
    pub tx_hash: B256,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
}

impl ChainLog {
    pub fn stamp(&self) -> Stamp {
        Stamp {
            block: self.block,
            time: self.time,
        }
    }
}

/// A position the book meets for the first time, whose state must be read
/// from the chain after `block`: the block before its first event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wanted {
    Pool {
        key: PoolKey,
        block: u64,
    },
    Nft {
        token: u64,
        range: Range,
        block: u64,
    },
}

/// What the chain held for a position the book had not met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seed {
    Pool {
        key: PoolKey,
        liquidity: u128,
        last_inside: (U256, U256),
        owed: (u128, u128),
    },
    Nft {
        token: u64,
        /// `None` when the token does not exist there: it is minted later.
        state: Option<NftSeed>,
    },
}

/// A position NFT's state as the position manager keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NftSeed {
    pub range: Range,
    pub holder: Address,
    pub liquidity: u128,
    pub last_inside: (U256, U256),
    pub owed: (u128, u128),
}

/// Something that did not add up. The book carries on: a position whose
/// past it could not see is shown with what it knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anomaly {
    pub at: Stamp,
    pub what: String,
}

/// Every position of the followed pools that holds something.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PositionBook {
    pools: BTreeSet<Address>,
    positions: HashMap<PoolKey, Ledger>,
    nfts: HashMap<u64, Nft>,
    /// Pool positions seen emptied: they hold nothing until their next
    /// event, which the book sees, so they are not read from the chain
    /// when used again.
    emptied: HashSet<PoolKey>,
    /// NFTs seen emptied, with their holder.
    emptied_nfts: HashMap<u64, Address>,
    tx: TxContext,
    staged: HashMap<PositionId, Seed>,
    /// The highest token id of any position manager event seen: how far
    /// a scan for NFTs nobody touched has to read.
    max_token: u64,
    /// Whether to collect what changed for the caller to write.
    pub record: bool,
    touched: BTreeSet<PositionId>,
    finished: Vec<PositionView>,
    anomalies: Vec<Anomaly>,
    anomaly_count: u64,
}

/// What a book holds, for a checkpoint. The book's working state between
/// blocks (the transaction pairing, staged seeds) is empty there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookState {
    pub pools: Vec<Address>,
    pub positions: Vec<(PoolKey, Ledger)>,
    pub nfts: Vec<(u64, Nft)>,
    pub emptied: Vec<PoolKey>,
    pub emptied_nfts: Vec<(u64, Address)>,
    #[serde(default)]
    pub max_token: u64,
    pub anomaly_count: u64,
}

/// Emptied positions the book remembers at most; past it, it forgets them
/// all and reads them from the chain again when they are used.
const EMPTIED_LIMIT: usize = 2_000_000;

impl PositionBook {
    pub fn new(pools: impl IntoIterator<Item = Address>) -> Self {
        Self {
            pools: pools.into_iter().collect(),
            ..Self::default()
        }
    }

    pub fn pools(&self) -> &BTreeSet<Address> {
        &self.pools
    }

    /// Whether the book holds the position.
    pub fn holds(&self, id: &PositionId) -> bool {
        match id {
            PositionId::Pool(key) => self.positions.contains_key(key),
            PositionId::Nft(token) => self.nfts.contains_key(token),
        }
    }

    /// The positions to read from the chain before applying `logs`: those
    /// met for the first time, not known to be empty, and not minted in
    /// the same transaction. Each is read after the block before its first
    /// event, when it was as it is just before that event.
    ///
    /// `initialized` says whether both ticks of a range are initialized in
    /// the replica before `logs`. A position holding liquidity keeps both
    /// its ticks initialized, so a pool position first met minting into a
    /// range that is not held none: it starts from nothing and is not read.
    /// (It could still be owed tokens from long ago, not collected; a
    /// collect of those is reported as an anomaly.)
    pub fn wanted(&self, logs: &[ChainLog], initialized: &dyn Fn(&Range) -> bool) -> Vec<Wanted> {
        let mut tx = self.tx.clone();
        let mut met: HashSet<PositionId> = HashSet::new();
        let mut out = Vec::new();
        for log in logs {
            let known = |id: &PositionId, met: &HashSet<PositionId>| {
                let emptied = match id {
                    PositionId::Pool(key) => self.emptied.contains(key),
                    PositionId::Nft(token) => self.emptied_nfts.contains_key(token),
                };
                self.holds(id) || met.contains(id) || self.staged.contains_key(id) || emptied
            };
            let before = log.block.saturating_sub(1);
            if log.address == MANAGER {
                let Ok(Some(event)) = decode_manager(&log.topics, &log.data) else {
                    continue;
                };
                let Some(range) = tx.pair(log.tx_hash, &event) else {
                    continue;
                };
                let token = manager_token(&event);
                let id = PositionId::Nft(token);
                let fresh = matches!(event, ManagerEvent::Increase { .. })
                    && tx.minted_holder(token).is_some();
                if !fresh && !known(&id, &met) {
                    out.push(Wanted::Nft {
                        token,
                        range,
                        block: before,
                    });
                }
                met.insert(id);
            } else if self.pools.contains(&log.address) {
                let Ok(Some(event)) = decode_pool_position(log.address, &log.topics, &log.data)
                else {
                    continue;
                };
                tx.pool_event(log.tx_hash, &event, None);
                let id = PositionId::Pool(event.key());
                let fresh = matches!(event, PoolPositionEvent::Mint { .. })
                    && !initialized(&event.key().range);
                if !fresh && !known(&id, &met) {
                    out.push(Wanted::Pool {
                        key: event.key(),
                        block: before,
                    });
                }
                met.insert(id);
            }
        }
        out
    }

    /// Hands over what the chain held for positions [`wanted`] listed.
    /// Each takes effect at the position's next event.
    ///
    /// [`wanted`]: PositionBook::wanted
    pub fn stage(&mut self, seeds: impl IntoIterator<Item = Seed>) {
        for seed in seeds {
            let id = match &seed {
                Seed::Pool { key, .. } => PositionId::Pool(*key),
                Seed::Nft { token, .. } => PositionId::Nft(*token),
            };
            self.staged.insert(id, seed);
        }
    }

    /// Whether a position the book does not hold is known to hold nothing:
    /// seen emptied, or read from the chain as empty.
    fn known_empty(&self, id: &PositionId) -> bool {
        if self.holds(id) {
            return false;
        }
        let emptied = match id {
            PositionId::Pool(key) => self.emptied.contains(key),
            PositionId::Nft(token) => self.emptied_nfts.contains_key(token),
        };
        emptied
            || match self.staged.get(id) {
                Some(Seed::Pool {
                    liquidity, owed, ..
                }) => *liquidity == 0 && *owed == (0, 0),
                Some(Seed::Nft { state: None, .. }) => true,
                Some(Seed::Nft {
                    state: Some(seed), ..
                }) => seed.liquidity == 0 && seed.owed == (0, 0),
                None => false,
            }
    }

    /// Forgets what was staged and not used. Call after each batch.
    pub fn clear_staged(&mut self) {
        self.staged.clear();
    }

    fn anomaly(&mut self, at: Stamp, what: String) {
        self.anomaly_count += 1;
        if self.anomalies.len() < 1_000 {
            self.anomalies.push(Anomaly { at, what });
        }
    }

    /// The ledger of a pool position, met or seeded now. `inside` is the
    /// fee growth inside its range now, the mark for a seeded position.
    fn pool_ledger(&mut self, key: PoolKey, inside: (U256, U256), at: Stamp) -> &mut Ledger {
        if !self.positions.contains_key(&key) {
            let mut ledger = Ledger::starting(at);
            match self.staged.remove(&PositionId::Pool(key)) {
                Some(Seed::Pool {
                    liquidity,
                    last_inside,
                    owed,
                    ..
                }) => {
                    ledger.liquidity = liquidity;
                    ledger.last_inside = last_inside;
                    ledger.owed = (U256::from(owed.0), U256::from(owed.1));
                    ledger.mark = if liquidity > 0 { inside } else { last_inside };
                }
                _ => ledger.mark = inside,
            }
            self.positions.insert(key, ledger);
        }
        self.positions.get_mut(&key).expect("just inserted")
    }

    /// Applies a pool's `Mint`, `Burn` or `Collect`. `inside` is the fee
    /// growth inside the range as the contract reads it: after a mint has
    /// set up its ticks, before a burn clears them; for a collect, now.
    pub fn pool_event(
        &mut self,
        event: &PoolPositionEvent,
        inside: (U256, U256),
        at: Stamp,
        tx: B256,
    ) {
        self.tx.pool_event(tx, event, Some(inside));
        let key = event.key();
        if let PoolPositionEvent::Collect { amounts, .. } = event {
            if amounts.0.is_zero()
                && amounts.1.is_zero()
                && self.known_empty(&PositionId::Pool(key))
            {
                // Nothing moves and nothing was held: nothing to follow.
                self.staged.remove(&PositionId::Pool(key));
                return;
            }
        }
        let ledger = self.pool_ledger(key, inside, at);
        let ok = match *event {
            PoolPositionEvent::Mint {
                amount, amounts, ..
            } => {
                ledger.accrue(inside);
                ledger.add_liquidity(amount, amounts, at)
            }
            PoolPositionEvent::Burn {
                amount, amounts, ..
            } => {
                if ledger.liquidity > 0 {
                    ledger.accrue(inside);
                }
                ledger.remove_liquidity(amount, amounts, at)
            }
            PoolPositionEvent::Collect { amounts, .. } => ledger.pay(amounts, at),
        };
        if !ok {
            self.anomaly(
                at,
                format!(
                    "{:#x} {}..{} in {:#x}: the event takes more than the book knew of",
                    key.owner, key.range.tick_lower, key.range.tick_upper, key.range.pool
                ),
            );
        }
        self.settle(PositionId::Pool(key));
    }

    /// Applies a position manager event. NFT events of pools not followed
    /// pair with no pool event and are passed over.
    pub fn manager_event(&mut self, event: &ManagerEvent, at: Stamp, tx: B256) {
        self.max_token = self.max_token.max(manager_token(event));
        if let ManagerEvent::Transfer { token, to, .. } = *event {
            self.tx.pair(tx, event);
            if to == Address::ZERO {
                if let Some(nft) = self.nfts.get(&token) {
                    if !nft.ledger.is_empty() {
                        self.anomaly(at, format!("NFT {token} was burned holding something"));
                    }
                }
                self.nfts.remove(&token);
                self.emptied_nfts.remove(&token);
                self.staged.remove(&PositionId::Nft(token));
            } else if let Some(nft) = self.nfts.get_mut(&token) {
                nft.holder = to;
                nft.ledger.changed = at;
                if self.record {
                    self.touched.insert(PositionId::Nft(token));
                }
            } else if let Some(holder) = self.emptied_nfts.get_mut(&token) {
                *holder = to;
            }
            return;
        }
        let Some(range) = self.tx.pair(tx, event) else {
            return;
        };
        let token = manager_token(event);
        if let ManagerEvent::Collect { amounts, .. } = event {
            if amounts.0.is_zero()
                && amounts.1.is_zero()
                && self.known_empty(&PositionId::Nft(token))
            {
                self.staged.remove(&PositionId::Nft(token));
                return;
            }
        }
        // The fee growth the manager reads: its pool position's, which its
        // mint or burn just before set. An NFT that holds no liquidity
        // earns nothing, and its own last reading does.
        let own = || {
            let nft = self
                .nfts
                .get(&token)
                .map(|n| (n.ledger.liquidity, n.ledger.last_inside));
            let staged = match self.staged.get(&PositionId::Nft(token)) {
                Some(Seed::Nft {
                    state: Some(seed), ..
                }) => Some((seed.liquidity, seed.last_inside)),
                _ => None,
            };
            nft.or(staged)
                .filter(|(liquidity, _)| *liquidity == 0)
                .map(|(_, last)| last)
        };
        let inside = self
            .tx
            .inside(&range)
            .or_else(|| {
                self.positions
                    .get(&PoolKey {
                        owner: MANAGER,
                        range,
                    })
                    .map(|ledger| ledger.last_inside)
            })
            .or_else(own);
        let Some(inside) = inside else {
            self.anomaly(
                at,
                format!("NFT {token}: no fee growth to read for its range"),
            );
            return;
        };
        if !self.nfts.contains_key(&token) {
            let minted = self.tx.minted_holder(token);
            let nft = self.nft_met(token, range, minted, inside, at);
            self.nfts.insert(token, nft);
        }
        let nft = self.nfts.get_mut(&token).expect("just inserted");
        if nft.range != range {
            let what = format!(
                "NFT {token}: the manager acted on {}..{}, the book has {}..{}",
                range.tick_lower, range.tick_upper, nft.range.tick_lower, nft.range.tick_upper
            );
            self.anomaly(at, what);
            return;
        }
        let ledger = &mut nft.ledger;
        let ok = match *event {
            ManagerEvent::Increase {
                liquidity, amounts, ..
            } => {
                ledger.accrue(inside);
                ledger.add_liquidity(liquidity, amounts, at)
            }
            ManagerEvent::Decrease {
                liquidity, amounts, ..
            } => {
                ledger.accrue(inside);
                ledger.remove_liquidity(liquidity, amounts, at)
            }
            ManagerEvent::Collect { amounts, .. } => {
                if ledger.liquidity > 0 {
                    ledger.accrue(inside);
                }
                ledger.pay(amounts, at)
            }
            ManagerEvent::Transfer { .. } => unreachable!("handled above"),
        };
        if !ok {
            self.anomaly(
                at,
                format!("NFT {token}: the event takes more than the book knew of"),
            );
        }
        self.settle(PositionId::Nft(token));
    }

    /// An NFT the book meets: minted in this transaction, or seeded.
    fn nft_met(
        &mut self,
        token: u64,
        range: Range,
        minted_to: Option<Address>,
        inside: (U256, U256),
        at: Stamp,
    ) -> Nft {
        let mut ledger = Ledger::starting(at);
        ledger.mark = inside;
        ledger.last_inside = inside;
        let staged = self.staged.remove(&PositionId::Nft(token));
        let holder = match (minted_to, staged) {
            (Some(holder), _) => holder,
            (
                None,
                Some(Seed::Nft {
                    state: Some(seed), ..
                }),
            ) => {
                if seed.range != range {
                    self.anomaly(
                        at,
                        format!(
                            "NFT {token}: the chain says {}..{}, the events {}..{}",
                            seed.range.tick_lower,
                            seed.range.tick_upper,
                            range.tick_lower,
                            range.tick_upper
                        ),
                    );
                }
                ledger.liquidity = seed.liquidity;
                ledger.last_inside = seed.last_inside;
                ledger.owed = (U256::from(seed.owed.0), U256::from(seed.owed.1));
                if seed.liquidity == 0 {
                    ledger.mark = seed.last_inside;
                }
                seed.holder
            }
            (None, _) => match self.emptied_nfts.remove(&token) {
                Some(holder) => holder,
                None => {
                    self.anomaly(
                        at,
                        format!("NFT {token}: met without its state or its mint"),
                    );
                    Address::ZERO
                }
            },
        };
        Nft {
            range,
            holder,
            ledger,
        }
    }

    /// Records a change, and lets a finished position go.
    fn settle(&mut self, id: PositionId) {
        let view = self.view(&id);
        let Some(view) = view else {
            return;
        };
        let empty = view.ledger.is_empty();
        // The manager's own pool positions are its NFTs added up.
        let shown = !(matches!(id, PositionId::Pool(_)) && view.owner == MANAGER);
        if self.record && shown {
            if empty {
                self.touched.remove(&id);
                self.finished.push(view.clone());
            } else {
                self.touched.insert(id);
            }
        }
        if empty {
            if self.emptied.len() + self.emptied_nfts.len() >= EMPTIED_LIMIT {
                self.emptied.clear();
                self.emptied_nfts.clear();
            }
            match id {
                PositionId::Pool(key) => {
                    self.positions.remove(&key);
                    self.emptied.insert(key);
                }
                PositionId::Nft(token) => {
                    self.nfts.remove(&token);
                    self.emptied_nfts.insert(token, view.owner);
                }
            }
        }
    }

    /// The highest token id of any position manager event seen.
    pub fn max_token(&self) -> u64 {
        self.max_token
    }

    /// Whether the book has met an NFT: it holds it, or saw it emptied.
    pub fn knows_nft(&self, token: u64) -> bool {
        self.nfts.contains_key(&token) || self.emptied_nfts.contains_key(&token)
    }

    /// Takes in an NFT the book has not met, as the chain holds it now:
    /// one that nobody touched since the book started. `mark` is the fee
    /// growth inside its range now, from which fees count as earned while
    /// followed. False, and nothing changes, when the book knows it or it
    /// holds nothing.
    pub fn adopt_nft(&mut self, token: u64, seed: NftSeed, mark: (U256, U256), at: Stamp) -> bool {
        if self.knows_nft(token) || (seed.liquidity == 0 && seed.owed == (0, 0)) {
            return false;
        }
        let ledger = Ledger {
            liquidity: seed.liquidity,
            last_inside: seed.last_inside,
            owed: (U256::from(seed.owed.0), U256::from(seed.owed.1)),
            mark: if seed.liquidity > 0 {
                mark
            } else {
                seed.last_inside
            },
            since: at,
            changed: at,
            ..Ledger::default()
        };
        self.max_token = self.max_token.max(token);
        self.nfts.insert(
            token,
            Nft {
                range: seed.range,
                holder: seed.holder,
                ledger,
            },
        );
        if self.record {
            self.touched.insert(PositionId::Nft(token));
        }
        true
    }

    /// One position as the caller shows it.
    pub fn view(&self, id: &PositionId) -> Option<PositionView> {
        match id {
            PositionId::Pool(key) => self.positions.get(key).map(|ledger| PositionView {
                id: *id,
                range: key.range,
                owner: key.owner,
                ledger: ledger.clone(),
            }),
            PositionId::Nft(token) => self.nfts.get(token).map(|nft| PositionView {
                id: *id,
                range: nft.range,
                owner: nft.holder,
                ledger: nft.ledger.clone(),
            }),
        }
    }

    /// Every position worth showing: the pool positions of owners other
    /// than the position manager, and the NFTs. The manager's own pool
    /// positions are the NFTs added up.
    pub fn shown(&self) -> impl Iterator<Item = PositionView> + '_ {
        let pool = self
            .positions
            .iter()
            .filter(|(key, _)| key.owner != MANAGER)
            .map(|(key, ledger)| PositionView {
                id: PositionId::Pool(*key),
                range: key.range,
                owner: key.owner,
                ledger: ledger.clone(),
            });
        let nfts = self.nfts.iter().map(|(token, nft)| PositionView {
            id: PositionId::Nft(*token),
            range: nft.range,
            owner: nft.holder,
            ledger: nft.ledger.clone(),
        });
        pool.chain(nfts)
    }

    /// Liquidity in a pool's positions whose range holds `tick`: what the
    /// book can attribute, against the pool's active liquidity.
    pub fn active_liquidity(&self, pool: Address, tick: i32) -> u128 {
        let inside = |range: &Range| {
            range.pool == pool && range.tick_lower <= tick && tick < range.tick_upper
        };
        let direct: u128 = self
            .positions
            .iter()
            .filter(|(key, _)| key.owner != MANAGER && inside(&key.range))
            .map(|(_, ledger)| ledger.liquidity)
            .sum();
        let nfts: u128 = self
            .nfts
            .values()
            .filter(|nft| inside(&nft.range))
            .map(|nft| nft.ledger.liquidity)
            .sum();
        direct.saturating_add(nfts)
    }

    /// The position manager's liquidity in its pool positions holding
    /// `tick`, whether or not the book knows each NFT.
    pub fn manager_liquidity(&self, pool: Address, tick: i32) -> u128 {
        self.positions
            .iter()
            .filter(|(key, _)| {
                key.owner == MANAGER
                    && key.range.pool == pool
                    && key.range.tick_lower <= tick
                    && tick < key.range.tick_upper
            })
            .map(|(_, ledger)| ledger.liquidity)
            .sum()
    }

    /// The positions changed since the last call, and the ones that
    /// finished, for the caller to write. Empty unless `record` is set.
    pub fn take_changes(&mut self) -> (Vec<PositionView>, Vec<PositionView>) {
        let touched: Vec<PositionId> = std::mem::take(&mut self.touched).into_iter().collect();
        let changed = touched.iter().filter_map(|id| self.view(id)).collect();
        (changed, std::mem::take(&mut self.finished))
    }

    /// What did not add up since the last call, and how often in all.
    pub fn take_anomalies(&mut self) -> (Vec<Anomaly>, u64) {
        (std::mem::take(&mut self.anomalies), self.anomaly_count)
    }

    /// How many positions the book holds: pool positions (the manager's
    /// included) and NFTs.
    pub fn len(&self) -> (usize, usize) {
        (self.positions.len(), self.nfts.len())
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty() && self.nfts.is_empty()
    }

    /// What the book holds, for a checkpoint.
    pub fn state(&self) -> BookState {
        let mut positions: Vec<(PoolKey, Ledger)> = self
            .positions
            .iter()
            .map(|(k, l)| (*k, l.clone()))
            .collect();
        positions.sort_by_key(|(k, _)| *k);
        let nfts: BTreeMap<u64, Nft> = self.nfts.iter().map(|(t, n)| (*t, n.clone())).collect();
        let mut emptied: Vec<PoolKey> = self.emptied.iter().copied().collect();
        emptied.sort();
        let mut emptied_nfts: Vec<(u64, Address)> =
            self.emptied_nfts.iter().map(|(t, h)| (*t, *h)).collect();
        emptied_nfts.sort();
        BookState {
            pools: self.pools.iter().copied().collect(),
            positions,
            nfts: nfts.into_iter().collect(),
            emptied,
            emptied_nfts,
            max_token: self.max_token,
            anomaly_count: self.anomaly_count,
        }
    }

    /// A book from a checkpoint.
    pub fn from_state(state: BookState) -> Self {
        Self {
            pools: state.pools.into_iter().collect(),
            positions: state.positions.into_iter().collect(),
            nfts: state.nfts.into_iter().collect(),
            emptied: state.emptied.into_iter().collect(),
            emptied_nfts: state.emptied_nfts.into_iter().collect(),
            max_token: state.max_token,
            anomaly_count: state.anomaly_count,
            ..Self::default()
        }
    }
}

/// Applies one log of a followed pool to its replica and to the book, in
/// the order the contract works: the fee growth inside a range is read
/// after a mint has set up its ticks and before a burn clears them.
/// Returns the pool event the replica applied, if the log was one.
///
/// An event the replica refuses is an error, and leaves the replica and
/// the book as they were.
pub fn apply_pool_log(
    book: &mut PositionBook,
    pool: &mut lpbot_venues::uniswap_v3::pool::Pool,
    log: &ChainLog,
) -> Result<Option<lpbot_venues::uniswap_v3::events::PoolEvent>, String> {
    use lpbot_venues::uniswap_v3::events::decode_log;
    use lpbot_venues::uniswap_v3::pool::Replay;

    let event = decode_log(&log.topics, &log.data).map_err(|e| e.to_string())?;
    let position = decode_pool_position(log.address, &log.topics, &log.data)?;
    let at = log.stamp();
    let apply = |pool: &mut lpbot_venues::uniswap_v3::pool::Pool| -> Result<(), String> {
        if let Some(event) = &event {
            pool.apply_event(event, Replay::Strict, &mut ())
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    match position {
        Some(change @ PoolPositionEvent::Mint { key, .. }) => {
            apply(pool)?;
            let inside = pool.fee_growth_inside(key.range.tick_lower, key.range.tick_upper);
            book.pool_event(&change, inside, at, log.tx_hash);
        }
        Some(change @ PoolPositionEvent::Burn { key, .. }) => {
            let inside = pool.fee_growth_inside(key.range.tick_lower, key.range.tick_upper);
            apply(pool)?;
            book.pool_event(&change, inside, at, log.tx_hash);
        }
        Some(change @ PoolPositionEvent::Collect { key, .. }) => {
            let inside = pool.fee_growth_inside(key.range.tick_lower, key.range.tick_upper);
            book.pool_event(&change, inside, at, log.tx_hash);
        }
        None => apply(pool)?,
    }
    Ok(event)
}

fn manager_token(event: &ManagerEvent) -> u64 {
    match *event {
        ManagerEvent::Increase { token, .. }
        | ManagerEvent::Decrease { token, .. }
        | ManagerEvent::Collect { token, .. }
        | ManagerEvent::Transfer { token, .. } => token,
    }
}

#[cfg(test)]
#[path = "positions_tests.rs"]
pub(crate) mod tests;
