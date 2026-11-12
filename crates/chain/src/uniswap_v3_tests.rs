//! End-to-end tests against a mock node that serves a simulated pool: its
//! state at every block through `eth_call`, and its events through
//! `eth_getLogs`. This exercises ABI encoding, log decoding, snapshots and
//! the verification flow without any network.

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy_primitives::{hex, keccak256, Address, B256, I256, U256};
use lpbot_venues::uniswap_v3::events::topics;
use lpbot_venues::uniswap_v3::pool::SwapEvent;
use lpbot_venues::uniswap_v3::tick_math::{
    get_sqrt_ratio_at_tick, MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK,
};
use lpbot_venues::uniswap_v3::Q96;
use serde_json::{json, Value};

use super::*;
use crate::eth::parse_quantity;
use crate::testing::MockNode;

const POOL: &str = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640";

fn pool_address() -> Address {
    POOL.parse().unwrap()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A pool's history: its state after each block and the logs of each block.
struct SimulatedChain {
    states: Vec<Pool>,
    logs: Vec<Vec<Value>>,
}

fn signed(value: i128) -> U256 {
    I256::try_from(value).unwrap().into_raw()
}

fn words(values: &[U256]) -> String {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes::<32>()).collect();
    format!("0x{}", hex::encode(bytes))
}

fn topic(value: U256) -> String {
    format!("0x{}", hex::encode(value.to_be_bytes::<32>()))
}

fn log(block: usize, index: usize, topics: Vec<String>, data: String) -> Value {
    json!({
        "address": POOL.to_lowercase(),
        "topics": topics,
        "data": data,
        "blockNumber": format!("0x{block:x}"),
        "logIndex": format!("0x{index:x}"),
        "transactionHash": format!("0x{:064x}", block * 1000 + index),
        "removed": false
    })
}

fn hex_topic(t: B256) -> String {
    format!("{t:#x}")
}

fn swap_log(block: usize, index: usize, e: &SwapEvent) -> Value {
    log(
        block,
        index,
        vec![
            hex_topic(topics::swap()),
            topic(U256::ZERO),
            topic(U256::ZERO),
        ],
        words(&[
            e.amount0.into_raw(),
            e.amount1.into_raw(),
            e.sqrt_price_x96,
            U256::from(e.liquidity),
            signed(e.tick as i128),
        ]),
    )
}

fn mint_log(
    block: usize,
    index: usize,
    lower: i32,
    upper: i32,
    amount: u128,
    a0: U256,
    a1: U256,
) -> Value {
    log(
        block,
        index,
        vec![
            hex_topic(topics::mint()),
            topic(U256::ZERO),
            topic(signed(lower as i128)),
            topic(signed(upper as i128)),
        ],
        words(&[U256::ZERO, U256::from(amount), a0, a1]),
    )
}

fn burn_log(
    block: usize,
    index: usize,
    lower: i32,
    upper: i32,
    amount: u128,
    a0: U256,
    a1: U256,
) -> Value {
    log(
        block,
        index,
        vec![
            hex_topic(topics::burn()),
            topic(U256::ZERO),
            topic(signed(lower as i128)),
            topic(signed(upper as i128)),
        ],
        words(&[U256::from(amount), a0, a1]),
    )
}

/// Builds a history of `blocks` blocks with random mints, burns and swaps,
/// plus one protocol-fee change, one flash and one unrelated Collect log.
fn simulate(seed: u64, blocks: usize) -> SimulatedChain {
    let mut rng = Rng(seed);
    let mut pool = Pool::new(500, 10);
    pool.initialize(Q96).unwrap();
    pool.mint(-30_000, 30_000, 5_000_000_000_000_000_000)
        .unwrap();

    let mut states = vec![pool.clone()];
    let mut logs: Vec<Vec<Value>> = vec![Vec::new()];
    let mut positions: Vec<(i32, i32, u128)> = Vec::new();

    for block in 1..=blocks {
        let mut block_logs = Vec::new();
        for _ in 0..rng.below(4) {
            let index = block_logs.len();
            match rng.below(10) {
                0..=1 => {
                    let centre = pool.tick() + rng.below(2_000) as i32 - 1_000;
                    let half = (1 + rng.below(80) as i32) * 10;
                    let lower = (centre - half).div_euclid(10) * 10;
                    let upper = lower + 2 * half;
                    let amount = (1 + rng.below(1_000_000) as u128) * 1_000_000_000_000;
                    if let Ok((a0, a1)) = pool.mint(lower, upper, amount) {
                        positions.push((lower, upper, amount));
                        block_logs.push(mint_log(block, index, lower, upper, amount, a0, a1));
                    }
                }
                2 if !positions.is_empty() => {
                    let i = rng.below(positions.len() as u64) as usize;
                    let (lower, upper, amount) = positions.swap_remove(i);
                    let (a0, a1) = pool.burn(lower, upper, amount).unwrap();
                    block_logs.push(burn_log(block, index, lower, upper, amount, a0, a1));
                }
                _ => {
                    let zero_for_one = rng.below(2) == 0;
                    let magnitude = (1 + rng.below(1_000_000) as i128)
                        * [1, 1_000_000, 1_000_000_000_000, 1_000_000_000_000_000]
                            [rng.below(4) as usize];
                    let amount = if rng.below(3) == 0 {
                        -magnitude
                    } else {
                        magnitude
                    };
                    let distance = 1 + rng.below(1_500) as i32;
                    let limit_tick = if zero_for_one {
                        pool.tick() - distance
                    } else {
                        pool.tick() + distance
                    }
                    .clamp(MIN_TICK + 1, MAX_TICK - 1);
                    let limit = get_sqrt_ratio_at_tick(limit_tick).unwrap().clamp(
                        MIN_SQRT_RATIO + U256::from(1u8),
                        MAX_SQRT_RATIO - U256::from(1u8),
                    );
                    if let Ok(amounts) = pool.swap(
                        zero_for_one,
                        I256::try_from(amount).unwrap(),
                        limit,
                        &mut (),
                    ) {
                        let event = SwapEvent {
                            amount0: amounts.amount0,
                            amount1: amounts.amount1,
                            sqrt_price_x96: pool.sqrt_price_x96(),
                            liquidity: pool.liquidity(),
                            tick: pool.tick(),
                        };
                        block_logs.push(swap_log(block, index, &event));
                    }
                }
            }
        }

        if block == blocks / 3 {
            pool.set_fee_protocol(4, 6);
            block_logs.push(log(
                block,
                block_logs.len(),
                vec![hex_topic(topics::set_fee_protocol())],
                words(&[U256::ZERO, U256::ZERO, U256::from(4u8), U256::from(6u8)]),
            ));
        }
        if block == blocks / 2 && pool.liquidity() > 0 {
            pool.flash(U256::from(12_345u32), U256::from(67_890u32))
                .unwrap();
            block_logs.push(log(
                block,
                block_logs.len(),
                vec![
                    hex_topic(topics::flash()),
                    topic(U256::ZERO),
                    topic(U256::ZERO),
                ],
                words(&[
                    U256::from(1u8),
                    U256::from(1u8),
                    U256::from(12_345u32),
                    U256::from(67_890u32),
                ]),
            ));
            // An event the replica must ignore.
            block_logs.push(log(
                block,
                block_logs.len(),
                vec![
                    hex_topic(keccak256(
                        "Collect(address,address,int24,int24,uint128,uint128)",
                    )),
                    topic(U256::ZERO),
                    topic(U256::ZERO),
                    topic(U256::ZERO),
                ],
                words(&[U256::ZERO, U256::from(1u8), U256::from(1u8)]),
            ));
        }

        states.push(pool.clone());
        logs.push(block_logs);
    }
    SimulatedChain { states, logs }
}

fn selector_of(signature: &str) -> String {
    hex::encode(&keccak256(signature)[..4])
}

/// Serves a simulated chain over JSON-RPC.
fn serve(chain: SimulatedChain) -> MockNode {
    let chain = Arc::new(chain);
    MockNode::start(move |method, params| match method {
        "eth_getLogs" => {
            let from = parse_quantity("t", &params[0]["fromBlock"]).unwrap() as usize;
            let to = parse_quantity("t", &params[0]["toBlock"]).unwrap() as usize;
            let wanted: Vec<String> = params[0]["topics"][0]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|t| t.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let mut out = Vec::new();
            for block in from..=to.min(chain.logs.len() - 1) {
                for entry in &chain.logs[block] {
                    let topic0 = entry["topics"][0].as_str().unwrap().to_string();
                    if wanted.is_empty() || wanted.contains(&topic0) {
                        out.push(entry.clone());
                    }
                }
            }
            Ok(Value::Array(out))
        }
        "eth_call" => {
            let block = parse_quantity("t", &params[1]).unwrap() as usize;
            let pool = chain
                .states
                .get(block)
                .ok_or((-32000, format!("no state for block {block}")))?;
            let data = hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
            let sel = hex::encode(&data[..4]);
            let arg = if data.len() >= 36 {
                i32::try_from(I256::from_raw(U256::from_be_slice(&data[4..36]))).unwrap()
            } else {
                0
            };
            let reply = if sel == selector_of("fee()") {
                words(&[U256::from(pool.fee_pips())])
            } else if sel == selector_of("tickSpacing()") {
                words(&[U256::from(pool.tick_spacing() as u32)])
            } else if sel == selector_of("slot0()") {
                let (p0, p1) = pool.fee_protocol();
                words(&[
                    pool.sqrt_price_x96(),
                    signed(pool.tick() as i128),
                    U256::ZERO,
                    U256::from(1u8),
                    U256::from(1u8),
                    U256::from(p0 + (p1 << 4)),
                    U256::from(1u8),
                ])
            } else if sel == selector_of("liquidity()") {
                words(&[U256::from(pool.liquidity())])
            } else if sel == selector_of("feeGrowthGlobal0X128()") {
                words(&[pool.fee_growth_global().0])
            } else if sel == selector_of("feeGrowthGlobal1X128()") {
                words(&[pool.fee_growth_global().1])
            } else if sel == selector_of("tickBitmap(int16)") {
                let mut bits = U256::ZERO;
                for (tick, info) in pool.ticks() {
                    let compressed = tick / pool.tick_spacing();
                    if info.liquidity_gross > 0 && compressed.div_euclid(256) == arg {
                        bits |= U256::from(1u8) << compressed.rem_euclid(256) as usize;
                    }
                }
                words(&[bits])
            } else if sel == selector_of("ticks(int24)") {
                let info = pool.tick_info(arg).copied().unwrap_or_default();
                words(&[
                    U256::from(info.liquidity_gross),
                    signed(info.liquidity_net),
                    info.fee_growth_outside0,
                    info.fee_growth_outside1,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::from((info.liquidity_gross > 0) as u8),
                ])
            } else {
                return Err((-32601, format!("unknown selector {sel}")));
            };
            Ok(json!(reply))
        }
        other => Err((-32601, format!("unknown method {other}"))),
    })
}

#[test]
fn a_snapshot_reads_back_the_pool_state_at_a_block() {
    let chain = simulate(0xC0FF_EE00_1234_5678, 60);
    let expected_mid = chain.states[30].clone();
    let expected_end = chain.states[60].clone();
    assert!(expected_end.ticks().count() >= 2);
    let node = serve(chain);
    let client = RpcClient::new(node.url());

    let mid = Pool::from_snapshot(fetch_snapshot(&client, pool_address(), 30).unwrap());
    assert_eq!(mid, expected_mid);
    let end = Pool::from_snapshot(fetch_snapshot(&client, pool_address(), 60).unwrap());
    assert_eq!(end, expected_end);
    assert_eq!(end.fee_protocol(), (4, 6));
}

#[test]
fn events_are_fetched_in_order_and_unrelated_logs_are_dropped() {
    let chain = simulate(0x1111_2222_3333_4444, 80);
    let total_state_changing: usize = chain
        .logs
        .iter()
        .flatten()
        .filter(|l| !l["topics"][0].as_str().unwrap().starts_with("0x70935338"))
        .count();
    let node = serve(chain);
    let client = RpcClient::new(node.url());

    let events = fetch_events(&client, pool_address(), 1, 80, 7, |_, _| {}).unwrap();
    assert!(events.len() > 40);
    assert!(events.len() <= total_state_changing);
    assert!(events
        .windows(2)
        .all(|w| (w[0].block_number, w[0].log_index) < (w[1].block_number, w[1].log_index)));
    assert!(events
        .iter()
        .any(|e| matches!(e.event, PoolEvent::Flash { .. })));
    assert!(events.iter().any(|e| matches!(
        e.event,
        PoolEvent::SetFeeProtocol {
            fee_protocol0: 4,
            fee_protocol1: 6
        }
    )));
}

#[test]
fn the_replica_verifies_against_the_simulated_chain() {
    for seed in [0xAAAA_0001u64, 0xBBBB_0002, 0xCCCC_0003] {
        let node = serve(simulate(seed, 150));
        let client = RpcClient::new(node.url());
        let mut progress = Vec::new();
        let report = verify_pool(&client, pool_address(), 10, 150, 16, |m| {
            progress.push(m.to_string())
        })
        .unwrap();
        assert!(report.passed(), "seed {seed:#x}: {:?}", report.differences);
        assert_eq!((report.from_block, report.to_block), (10, 150));
        assert!(report.swaps > 50, "seed {seed:#x}: {} swaps", report.swaps);
        assert!(report.mints > 5 && report.burns > 0);
        assert_eq!(progress.len(), 4);
    }
}

#[test]
fn a_tampered_event_is_caught_and_located() {
    let mut chain = simulate(0xDDDD_0004, 120);
    // Change the liquidity one swap reports, in the second half of the range.
    let (block, position) = chain
        .logs
        .iter()
        .enumerate()
        .skip(60)
        .find_map(|(b, logs)| {
            logs.iter()
                .position(|l| l["topics"][0] == json!(hex_topic(topics::swap())))
                .map(|p| (b, p))
        })
        .expect("a swap in the second half");
    let original = chain.logs[block][position]["data"]
        .as_str()
        .unwrap()
        .to_string();
    let mut bytes = hex::decode(&original).unwrap();
    // Flip the lowest bit of the liquidity word (the fourth word).
    bytes[127] ^= 1;
    chain.logs[block][position]["data"] = json!(format!("0x{}", hex::encode(bytes)));

    let node = serve(chain);
    let client = RpcClient::new(node.url());
    let err = verify_pool(&client, pool_address(), 10, 120, 50, |_| {}).unwrap_err();
    match err {
        ChainError::Replay { block: b, .. } => assert_eq!(b as usize, block),
        other => panic!("expected a replay error, got {other}"),
    }
}

#[test]
fn a_missing_event_shows_up_as_a_difference_or_a_replay_error() {
    let mut chain = simulate(0xEEEE_0005, 120);
    let (block, position) = chain
        .logs
        .iter()
        .enumerate()
        .skip(20)
        .find_map(|(b, logs)| {
            logs.iter()
                .position(|l| l["topics"][0] == json!(hex_topic(topics::mint())))
                .map(|p| (b, p))
        })
        .expect("a mint after block 20");
    chain.logs[block].remove(position);

    let node = serve(chain);
    let client = RpcClient::new(node.url());
    match verify_pool(&client, pool_address(), 10, 120, 50, |_| {}) {
        Ok(report) => assert!(!report.passed()),
        Err(ChainError::Replay { .. }) => {}
        Err(other) => panic!("unexpected error {other}"),
    }
}

#[test]
fn verification_needs_a_forward_range() {
    let node = serve(simulate(1, 5));
    let client = RpcClient::new(node.url());
    assert!(matches!(
        verify_pool(&client, pool_address(), 5, 5, 10, |_| {}),
        Err(ChainError::Other(_))
    ));
}

#[test]
fn pools_are_discovered_through_the_factory_in_token_order() {
    let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .unwrap();
    let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse()
        .unwrap();
    let wbtc: Address = "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"
        .parse()
        .unwrap();
    let factory: Address = "0x1F98431c8aD98523631AE4a59f267346ea31F984"
        .parse()
        .unwrap();

    // The mock factory knows two pools.
    let mut known: BTreeMap<(Address, Address, u32), Address> = BTreeMap::new();
    known.insert((usdc, weth, 500), pool_address());
    known.insert(
        (wbtc, weth, 3000),
        "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD"
            .parse()
            .unwrap(),
    );
    let known = Arc::new(known);

    let node = MockNode::start(move |method, params| {
        assert_eq!(method, "eth_call");
        assert_eq!(
            params[0]["to"].as_str().unwrap(),
            "0x1f98431c8ad98523631ae4a59f267346ea31f984"
        );
        let data = hex::decode(params[0]["data"].as_str().unwrap()).unwrap();
        assert_eq!(
            hex::encode(&data[..4]),
            selector_of("getPool(address,address,uint24)")
        );
        let token0 = Address::from_slice(&data[16..36]);
        let token1 = Address::from_slice(&data[48..68]);
        let fee = U256::from_be_slice(&data[68..100]).to::<u32>();
        assert!(token0 < token1, "the factory is asked in token order");
        let pool = known
            .get(&(token0, token1, fee))
            .copied()
            .unwrap_or(Address::ZERO);
        Ok(json!(words(&[U256::from_be_slice(pool.as_slice())])))
    });
    let client = RpcClient::new(node.url());

    let pools = discover_pools(&client, factory, &[weth, usdc, wbtc], 100).unwrap();
    assert_eq!(pools.len(), 2);
    assert!(pools.contains(&DiscoveredPool {
        address: pool_address(),
        token0: usdc,
        token1: weth,
        fee_pips: 500
    }));
    assert!(pools
        .iter()
        .any(|p| p.token0 == wbtc && p.token1 == weth && p.fee_pips == 3000));
}
