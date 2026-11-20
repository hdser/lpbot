//! A liquidity provider's positions, read from the chain: the NFTs the
//! Uniswap v3 position manager minted or passed to an owner, the pool and
//! range of each, and every change of its liquidity and every collection.
//! What `lpbot lp-positions` writes, and what a backtest that follows a
//! provider replays.

use std::collections::BTreeMap;

use alloy_primitives::{keccak256, Address, B256, U256};
use lpbot_chain::eth::Log;
use lpbot_chain::uniswap_v3::ChainError;

/// The Uniswap v3 NonfungiblePositionManager on Ethereum mainnet.
pub const POSITION_MANAGER: Address = Address::new([
    0xc3, 0x64, 0x42, 0xb4, 0xa4, 0x52, 0x2e, 0x87, 0x13, 0x99, 0xcd, 0x71, 0x7a, 0xbd, 0xd8, 0x47,
    0xab, 0x11, 0xfe, 0x88,
]);

/// Token ids asked about in one log request.
const IDS_PER_REQUEST: usize = 50;

pub fn event_topic(signature: &str) -> B256 {
    keccak256(signature.as_bytes())
}

pub fn transfer_topic() -> B256 {
    event_topic("Transfer(address,address,uint256)")
}

pub fn increase_topic() -> B256 {
    event_topic("IncreaseLiquidity(uint256,uint128,uint256,uint256)")
}

pub fn decrease_topic() -> B256 {
    event_topic("DecreaseLiquidity(uint256,uint128,uint256,uint256)")
}

pub fn collect_topic() -> B256 {
    event_topic("Collect(uint256,address,uint256,uint256)")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LpEventKind {
    Increase,
    Decrease,
    Collect,
}

/// One change to a position: liquidity added or taken out with the
/// amounts that moved, or fees and withdrawn amounts collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LpEvent {
    pub block: u64,
    pub log_index: u64,
    pub kind: LpEventKind,
    /// Liquidity added or taken out; 0 for a collection.
    pub liquidity: u128,
    pub amount0: U256,
    pub amount1: U256,
}

/// What `positions(tokenId)` says of an NFT: its pool's tokens and fee,
/// and its range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionKey {
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LpPosition {
    pub token_id: U256,
    pub owner: Address,
    pub key: PositionKey,
    /// In chain order.
    pub events: Vec<LpEvent>,
}

/// One stretch of an NFT with liquidity in place, and what moved in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub open_block: u64,
    /// The block whose decrease left no liquidity; `None` while open.
    pub close_block: Option<u64>,
    /// Put in, taken out of the range, and collected (what was taken out
    /// plus the fees), in each token.
    pub deposited: (U256, U256),
    pub withdrawn: (U256, U256),
    pub collected: (U256, U256),
}

impl LpPosition {
    /// The stretches with liquidity in place: each from an increase on a
    /// position without liquidity to the decrease that leaves none.
    /// Collections count with the stretch they follow.
    pub fn spans(&self) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        let mut liquidity: u128 = 0;
        for event in &self.events {
            match event.kind {
                LpEventKind::Increase => {
                    if liquidity == 0 && event.liquidity > 0 {
                        spans.push(Span {
                            open_block: event.block,
                            close_block: None,
                            deposited: (U256::ZERO, U256::ZERO),
                            withdrawn: (U256::ZERO, U256::ZERO),
                            collected: (U256::ZERO, U256::ZERO),
                        });
                    }
                    liquidity = liquidity.saturating_add(event.liquidity);
                    if let Some(span) = spans.last_mut() {
                        span.deposited.0 += event.amount0;
                        span.deposited.1 += event.amount1;
                    }
                }
                LpEventKind::Decrease => {
                    liquidity = liquidity.saturating_sub(event.liquidity);
                    if let Some(span) = spans.last_mut() {
                        span.withdrawn.0 += event.amount0;
                        span.withdrawn.1 += event.amount1;
                        if liquidity == 0 && span.close_block.is_none() {
                            span.close_block = Some(event.block);
                        }
                    }
                }
                LpEventKind::Collect => {
                    if let Some(span) = spans.last_mut() {
                        span.collected.0 += event.amount0;
                        span.collected.1 += event.amount1;
                    }
                }
            }
        }
        spans
    }
}

fn word(data: &[u8], index: usize) -> Option<&[u8]> {
    data.get(index * 32..(index + 1) * 32)
}

fn word_u256(data: &[u8], index: usize) -> Option<U256> {
    word(data, index).map(U256::from_be_slice)
}

fn word_address(data: &[u8], index: usize) -> Option<Address> {
    word(data, index).map(|w| Address::from_slice(&w[12..]))
}

/// The low four bytes of a word as a signed number: an `int24` or `uint24`
/// the ABI has sign- or zero-extended to 32 bytes.
fn word_i32(data: &[u8], index: usize) -> Option<i32> {
    word(data, index).map(|w| i32::from_be_bytes([w[28], w[29], w[30], w[31]]))
}

/// A token id or address in an indexed topic.
fn topic_u256(log: &Log, index: usize) -> Option<U256> {
    log.topics.get(index).map(|t| U256::from_be_bytes(t.0))
}

fn topic_address(log: &Log, index: usize) -> Option<Address> {
    log.topics
        .get(index)
        .map(|t| Address::from_slice(&t.0[12..]))
}

/// An address as an indexed topic.
pub fn address_topic(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::new(bytes)
}

/// A position manager event as (token id, event). `None` for any other log.
pub fn decode_event(log: &Log) -> Option<(U256, LpEvent)> {
    let topic = *log.topics.first()?;
    let id = topic_u256(log, 1)?;
    let (kind, liquidity, amount0, amount1) = if topic == increase_topic() {
        let liquidity = u128::try_from(word_u256(&log.data, 0)?).ok()?;
        (
            LpEventKind::Increase,
            liquidity,
            word_u256(&log.data, 1)?,
            word_u256(&log.data, 2)?,
        )
    } else if topic == decrease_topic() {
        let liquidity = u128::try_from(word_u256(&log.data, 0)?).ok()?;
        (
            LpEventKind::Decrease,
            liquidity,
            word_u256(&log.data, 1)?,
            word_u256(&log.data, 2)?,
        )
    } else if topic == collect_topic() {
        // Data: recipient, amount0, amount1.
        (
            LpEventKind::Collect,
            0,
            word_u256(&log.data, 1)?,
            word_u256(&log.data, 2)?,
        )
    } else {
        return None;
    };
    Some((
        id,
        LpEvent {
            block: log.block_number,
            log_index: log.log_index,
            kind,
            liquidity,
            amount0,
            amount1,
        },
    ))
}

/// An NFT transfer as (from, to, token id); a mint comes from address zero.
pub fn decode_transfer(log: &Log) -> Option<(Address, Address, U256)> {
    if log.topics.first() != Some(&transfer_topic()) {
        return None;
    }
    Some((
        topic_address(log, 1)?,
        topic_address(log, 2)?,
        topic_u256(log, 3)?,
    ))
}

fn call_with_id(selector: [u8; 4], token_id: U256) -> Vec<u8> {
    let mut data = selector.to_vec();
    data.extend_from_slice(&token_id.to_be_bytes::<32>());
    data
}

/// `positions(uint256)`.
pub fn positions_call(token_id: U256) -> Vec<u8> {
    call_with_id([0x99, 0xfb, 0xab, 0x88], token_id)
}

/// `ownerOf(uint256)`.
pub fn owner_of_call(token_id: U256) -> Vec<u8> {
    call_with_id([0x63, 0x52, 0x21, 0x1e], token_id)
}

/// The pool and range from what `positions(tokenId)` returns: nonce,
/// operator, token0, token1, fee, tickLower, tickUpper and five more.
pub fn decode_positions(data: &[u8]) -> Option<PositionKey> {
    if data.len() < 7 * 32 {
        return None;
    }
    Some(PositionKey {
        token0: word_address(data, 2)?,
        token1: word_address(data, 3)?,
        fee: u32::try_from(word_i32(data, 4)?).ok()?,
        tick_lower: word_i32(data, 5)?,
        tick_upper: word_i32(data, 6)?,
    })
}

/// What reading a provider's history needs from the chain.
pub trait LpSource {
    /// The position manager's logs in a block range whose topics match,
    /// position by position (an empty list allows any), in chain order.
    fn logs(&self, topics: &[Vec<B256>], from: u64, to: u64) -> Result<Vec<Log>, ChainError>;
    /// A view call to the position manager at a block.
    fn call(&self, data: &[u8], block: u64) -> Result<Vec<u8>, ChainError>;
}

/// Who holds an NFT at a block; `None` once it is burned.
pub fn owner_of(
    source: &impl LpSource,
    token_id: U256,
    block: u64,
) -> Result<Option<Address>, ChainError> {
    match source.call(&owner_of_call(token_id), block) {
        Ok(data) => Ok(word_address(&data, 0)),
        // A burned NFT makes the call revert.
        Err(ChainError::Rpc(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

/// When an owner held an NFT: from the block it arrived in (minted or sent
/// to it) to the block it left in (burned or sent on), or the end of the
/// range asked about while it is still held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holding {
    pub token_id: U256,
    pub from_block: u64,
    pub to_block: u64,
}

/// Every NFT that arrived at `owner` in the block range, with when it
/// arrived and left, by token id. An NFT held from before the range is not
/// in it: what is read is what the owner opened.
pub fn holdings(
    source: &impl LpSource,
    owner: Address,
    from: u64,
    to: u64,
) -> Result<Vec<Holding>, ChainError> {
    Ok(holdings_of_many(source, &[owner], from, to)?
        .remove(&owner)
        .unwrap_or_default())
}

/// `holdings` for several owners at once: the node is asked twice in all
/// (transfers to any of them, transfers from any of them), not twice for
/// each owner, which is what makes a dozen providers quick to read.
pub fn holdings_of_many(
    source: &impl LpSource,
    owners: &[Address],
    from: u64,
    to: u64,
) -> Result<BTreeMap<Address, Vec<Holding>>, ChainError> {
    let mut by_owner: BTreeMap<Address, BTreeMap<U256, Holding>> =
        owners.iter().map(|o| (*o, BTreeMap::new())).collect();
    if owners.is_empty() {
        return Ok(BTreeMap::new());
    }
    let topics: Vec<B256> = owners.iter().map(|o| address_topic(*o)).collect();
    let received = source.logs(&[vec![transfer_topic()], vec![], topics.clone()], from, to)?;
    let sent = source.logs(&[vec![transfer_topic()], topics], from, to)?;
    for log in &received {
        if let Some((_, receiver, id)) = decode_transfer(log) {
            if let Some(held) = by_owner.get_mut(&receiver) {
                held.entry(id).or_insert(Holding {
                    token_id: id,
                    from_block: log.block_number,
                    to_block: to,
                });
            }
        }
    }
    for log in &sent {
        if let Some((sender, _, id)) = decode_transfer(log) {
            if let Some(holding) = by_owner.get_mut(&sender).and_then(|h| h.get_mut(&id)) {
                if log.block_number >= holding.from_block {
                    holding.to_block = holding.to_block.min(log.block_number);
                }
            }
        }
    }
    Ok(by_owner
        .into_iter()
        .map(|(owner, held)| (owner, held.into_values().collect()))
        .collect())
}

/// Every NFT passed to `owner` in the block range, by token id.
pub fn token_ids_received(
    source: &impl LpSource,
    owner: Address,
    from: u64,
    to: u64,
) -> Result<Vec<U256>, ChainError> {
    Ok(holdings(source, owner, from, to)?
        .into_iter()
        .map(|h| h.token_id)
        .collect())
}

/// Requests for the events of `held`: up to `IDS_PER_REQUEST` NFTs each,
/// taken in the order they arrived, over the blocks from the first one's
/// arrival to the last one's departure. NFTs are rarely burned when they
/// are emptied, so most count as held to the end of the range; grouping by
/// count keeps the requests few however long they were held.
fn event_requests(held: &[Holding]) -> Vec<(Vec<U256>, u64, u64)> {
    let mut sorted = held.to_vec();
    sorted.sort_by_key(|h| (h.from_block, h.token_id));
    sorted
        .chunks(IDS_PER_REQUEST)
        .map(|group| {
            let start = group.iter().map(|h| h.from_block).min().unwrap_or(0);
            let end = group.iter().map(|h| h.to_block).max().unwrap_or(0);
            (group.iter().map(|h| h.token_id).collect(), start, end)
        })
        .collect()
}

/// The positions of the NFTs `owner` held, with their events while held,
/// each with its pool and range read at its first event. NFTs without an
/// event, or whose `positions` cannot be read, are left out.
pub fn read_positions(
    source: &impl LpSource,
    owner: Address,
    held: &[Holding],
) -> Result<Vec<LpPosition>, ChainError> {
    let mut events: BTreeMap<U256, Vec<LpEvent>> = BTreeMap::new();
    for (ids, start, end) in event_requests(held) {
        let topics = [
            vec![increase_topic(), decrease_topic(), collect_topic()],
            ids.iter()
                .map(|id| B256::new(id.to_be_bytes::<32>()))
                .collect(),
        ];
        for log in source.logs(&topics, start, end)? {
            if let Some((id, event)) = decode_event(&log) {
                events.entry(id).or_default().push(event);
            }
        }
    }
    let mut positions = Vec::new();
    for (token_id, mut list) in events {
        list.sort_by_key(|e| (e.block, e.log_index));
        let Some(first) = list.first() else {
            continue;
        };
        let Ok(data) = source.call(&positions_call(token_id), first.block) else {
            continue;
        };
        let Some(key) = decode_positions(&data) else {
            continue;
        };
        positions.push(LpPosition {
            token_id,
            owner,
            key,
            events: list,
        });
    }
    Ok(positions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn id_topic(id: u64) -> B256 {
        B256::new(U256::from(id).to_be_bytes::<32>())
    }

    fn words(values: &[U256]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes::<32>()).collect()
    }

    fn change(kind: LpEventKind, id: u64, block: u64, liquidity: u128, a0: u64, a1: u64) -> Log {
        let (topic, data) = match kind {
            LpEventKind::Increase => (
                increase_topic(),
                words(&[U256::from(liquidity), U256::from(a0), U256::from(a1)]),
            ),
            LpEventKind::Decrease => (
                decrease_topic(),
                words(&[U256::from(liquidity), U256::from(a0), U256::from(a1)]),
            ),
            LpEventKind::Collect => (
                collect_topic(),
                words(&[U256::from(7u8), U256::from(a0), U256::from(a1)]),
            ),
        };
        Log {
            address: POSITION_MANAGER,
            topics: vec![topic, id_topic(id)],
            data,
            block_number: block,
            log_index: 0,
            transaction_hash: B256::ZERO,
        }
    }

    fn signed_word(value: i32) -> U256 {
        // Sign-extended as the ABI does.
        let mut bytes = if value < 0 { [0xff; 32] } else { [0; 32] };
        bytes[28..].copy_from_slice(&value.to_be_bytes());
        U256::from_be_bytes(bytes)
    }

    fn positions_return(lower: i32, upper: i32) -> Vec<u8> {
        let token = |b: u8| U256::from_be_slice(Address::repeat_byte(b).as_slice());
        words(&[
            U256::ZERO,
            U256::ZERO,
            token(0x11),
            token(0x22),
            U256::from(500u32),
            signed_word(lower),
            signed_word(upper),
            U256::from(123u32),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
        ])
    }

    #[test]
    fn event_signatures_are_the_position_managers() {
        assert_eq!(
            format!("{:#x}", transfer_topic()),
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
        assert_eq!(
            format!("{:#x}", increase_topic()),
            "0x3067048beee31b25b2f1681f88dac838c8bba36af25bfb2b7cf7473a5847e35f"
        );
        assert_eq!(
            format!("{:#x}", decrease_topic()),
            "0x26f6a048ee9138f2c0ce266f322cb99228e8d619ae2bff30c67f8dcf9d2377b4"
        );
        assert_eq!(
            format!("{:#x}", collect_topic()),
            "0x40d0efd1a53d60ecbf40971b9daf7dc90178c3aadc7aab1765632738fa8b8f01"
        );
        assert_eq!(
            format!("{POSITION_MANAGER:#x}"),
            "0xc36442b4a4522e871399cd717abdd847ab11fe88"
        );
    }

    #[test]
    fn changes_collections_and_ranges_are_decoded() {
        let (id, event) =
            decode_event(&change(LpEventKind::Increase, 9, 100, 5_000, 30, 40)).unwrap();
        assert_eq!(id, U256::from(9u8));
        assert_eq!(
            (event.kind, event.liquidity, event.amount0, event.amount1),
            (
                LpEventKind::Increase,
                5_000,
                U256::from(30u8),
                U256::from(40u8)
            )
        );
        let (_, collect) = decode_event(&change(LpEventKind::Collect, 9, 101, 0, 3, 4)).unwrap();
        assert_eq!(
            (collect.kind, collect.amount0),
            (LpEventKind::Collect, U256::from(3u8))
        );
        let key = decode_positions(&positions_return(-198_990, -198_950)).unwrap();
        assert_eq!(
            (key.tick_lower, key.tick_upper, key.fee),
            (-198_990, -198_950, 500)
        );
        assert_eq!(key.token0, Address::repeat_byte(0x11));
        assert!(decode_positions(&[0u8; 64]).is_none());
        // A log of another event is not one of these.
        let mut other = change(LpEventKind::Increase, 9, 100, 1, 1, 1);
        other.topics[0] = B256::repeat_byte(1);
        assert!(decode_event(&other).is_none());
    }

    #[test]
    fn a_position_is_split_into_stretches_with_liquidity() {
        let position = LpPosition {
            token_id: U256::from(1u8),
            owner: Address::ZERO,
            key: decode_positions(&positions_return(0, 10)).unwrap(),
            events: [
                change(LpEventKind::Increase, 1, 10, 100, 5, 5),
                change(LpEventKind::Increase, 1, 12, 50, 2, 2),
                change(LpEventKind::Decrease, 1, 20, 150, 6, 3),
                change(LpEventKind::Collect, 1, 20, 0, 7, 4),
                change(LpEventKind::Increase, 1, 30, 80, 4, 0),
            ]
            .iter()
            .map(|log| decode_event(log).unwrap().1)
            .collect(),
        };
        let spans = position.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].open_block, spans[0].close_block), (10, Some(20)));
        assert_eq!(spans[0].deposited, (U256::from(7u8), U256::from(7u8)));
        assert_eq!(spans[0].withdrawn, (U256::from(6u8), U256::from(3u8)));
        // Fees are what was collected beyond what was taken out.
        assert_eq!(spans[0].collected, (U256::from(7u8), U256::from(4u8)));
        assert_eq!((spans[1].open_block, spans[1].close_block), (30, None));
    }

    /// A chain with one owner's transfers and position changes.
    struct FakeChain {
        logs: Vec<Log>,
        asked: RefCell<Vec<Vec<Vec<B256>>>>,
    }

    impl LpSource for FakeChain {
        fn logs(&self, topics: &[Vec<B256>], from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
            self.asked.borrow_mut().push(topics.to_vec());
            Ok(self
                .logs
                .iter()
                .filter(|l| (from..=to).contains(&l.block_number))
                .filter(|l| {
                    topics.iter().enumerate().all(|(i, allowed)| {
                        allowed.is_empty() || l.topics.get(i).is_some_and(|t| allowed.contains(t))
                    })
                })
                .cloned()
                .collect())
        }

        fn call(&self, data: &[u8], _block: u64) -> Result<Vec<u8>, ChainError> {
            assert_eq!(&data[..4], &[0x99, 0xfb, 0xab, 0x88]);
            let id = U256::from_be_slice(&data[4..]);
            Ok(positions_return(
                id.to::<i32>() * 10,
                id.to::<i32>() * 10 + 20,
            ))
        }
    }

    fn transfer(from: Address, to: Address, id: u64, block: u64) -> Log {
        Log {
            address: POSITION_MANAGER,
            topics: vec![
                transfer_topic(),
                address_topic(from),
                address_topic(to),
                id_topic(id),
            ],
            data: Vec::new(),
            block_number: block,
            log_index: 1,
            transaction_hash: B256::ZERO,
        }
    }

    #[test]
    fn an_owners_positions_are_read_with_their_ranges() {
        let owner = Address::repeat_byte(0xaa);
        let someone = Address::repeat_byte(0xbb);
        let chain = FakeChain {
            logs: vec![
                transfer(Address::ZERO, owner, 3, 5),
                transfer(Address::ZERO, someone, 4, 6),
                transfer(someone, owner, 5, 7),
                change(LpEventKind::Increase, 3, 8, 10, 1, 1),
                change(LpEventKind::Increase, 4, 8, 10, 1, 1),
                change(LpEventKind::Increase, 5, 9, 10, 1, 1),
                change(LpEventKind::Decrease, 5, 11, 10, 1, 1),
            ],
            asked: RefCell::new(Vec::new()),
        };
        let held = holdings(&chain, owner, 0, 100).unwrap();
        assert_eq!(
            held.iter().map(|h| h.token_id).collect::<Vec<_>>(),
            vec![U256::from(3u8), U256::from(5u8)]
        );
        // NFT 5 arrived at block 7 and is still held: read to the end.
        assert_eq!((held[1].from_block, held[1].to_block), (7, 100));
        let positions = read_positions(&chain, owner, &held).unwrap();
        assert_eq!(positions.len(), 2);
        assert_eq!(
            (positions[0].key.tick_lower, positions[0].key.tick_upper),
            (30, 50)
        );
        assert_eq!(positions[1].spans()[0].close_block, Some(11));
        // The owner and the token ids were filters on the node's side.
        let asked = chain.asked.borrow();
        assert_eq!(asked[0][2], vec![address_topic(owner)]);
        assert_eq!(asked[1][1], vec![address_topic(owner)]);
        assert_eq!(asked[2][1], vec![id_topic(3), id_topic(5)]);
    }

    #[test]
    fn several_owners_are_read_with_two_requests() {
        let a = Address::repeat_byte(0xaa);
        let b = Address::repeat_byte(0xbb);
        let chain = FakeChain {
            logs: vec![
                transfer(Address::ZERO, a, 3, 5),
                transfer(Address::ZERO, b, 4, 6),
                transfer(b, Address::ZERO, 4, 9),
                transfer(Address::ZERO, Address::repeat_byte(0xcc), 5, 7),
            ],
            asked: RefCell::new(Vec::new()),
        };
        let held = holdings_of_many(&chain, &[a, b], 0, 100).unwrap();
        assert_eq!(chain.asked.borrow().len(), 2);
        assert_eq!(held[&a].len(), 1);
        assert_eq!((held[&b][0].from_block, held[&b][0].to_block), (6, 9));
        assert_eq!(held.len(), 2, "only the owners asked about");
    }

    #[test]
    fn an_nft_sent_away_is_held_until_then_and_requests_stay_short() {
        let owner = Address::repeat_byte(0xaa);
        let chain = FakeChain {
            logs: vec![
                transfer(Address::ZERO, owner, 3, 5),
                transfer(owner, Address::ZERO, 3, 50),
            ],
            asked: RefCell::new(Vec::new()),
        };
        let held = holdings(&chain, owner, 0, 1_000).unwrap();
        assert_eq!((held[0].from_block, held[0].to_block), (5, 50));
        // Requests take NFTs in the order they arrived, up to the most a
        // request asks about, over the blocks they span.
        let h = |id: u64, from: u64, to: u64| Holding {
            token_id: U256::from(id),
            from_block: from,
            to_block: to,
        };
        let many: Vec<Holding> = (0..IDS_PER_REQUEST as u64 + 1)
            .map(|i| h(1_000 - i, i * 10, 1_000_000))
            .collect();
        let requests = event_requests(&many);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0.len(), IDS_PER_REQUEST);
        assert_eq!((requests[0].1, requests[0].2), (0, 1_000_000));
        assert_eq!(
            requests[1].0,
            vec![U256::from(1_000 - IDS_PER_REQUEST as u64)]
        );
    }
}
