//! Pool events: decoding them from logs and applying them to the replica.

use alloy_primitives::{keccak256, B256, I256, U256};
use thiserror::Error;

use super::pool::{Pool, PoolError, PoolResult, Replay, SwapEvent, SwapObserver};

/// An event that changes pool state. Events that do not (Collect,
/// CollectProtocol, observation-cardinality changes) are not represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolEvent {
    Initialize {
        sqrt_price_x96: U256,
        tick: i32,
    },
    Mint {
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
        amount0: U256,
        amount1: U256,
    },
    Burn {
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
        amount0: U256,
        amount1: U256,
    },
    Swap(SwapEvent),
    Flash {
        paid0: U256,
        paid1: U256,
    },
    SetFeeProtocol {
        fee_protocol0: u8,
        fee_protocol1: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("log has {got} topics, expected {expected}")]
    TopicCount { expected: usize, got: usize },
    #[error("log data is {got} bytes, expected {expected}")]
    DataLength { expected: usize, got: usize },
    #[error("a value does not fit its type")]
    ValueOutOfRange,
}

/// Event signature hashes (topic 0).
pub mod topics {
    use super::*;

    pub fn initialize() -> B256 {
        keccak256("Initialize(uint160,int24)")
    }
    pub fn mint() -> B256 {
        keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
    }
    pub fn burn() -> B256 {
        keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
    }
    pub fn swap() -> B256 {
        keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
    }
    pub fn flash() -> B256 {
        keccak256("Flash(address,address,uint256,uint256,uint256,uint256)")
    }
    pub fn set_fee_protocol() -> B256 {
        keccak256("SetFeeProtocol(uint8,uint8,uint8,uint8)")
    }

    /// Every topic the replica needs, for a log filter.
    pub fn all() -> Vec<B256> {
        vec![
            initialize(),
            mint(),
            burn(),
            swap(),
            flash(),
            set_fee_protocol(),
        ]
    }
}

fn word(data: &[u8], index: usize) -> U256 {
    U256::from_be_slice(&data[index * 32..(index + 1) * 32])
}

fn require(
    topics: &[B256],
    data: &[u8],
    n_topics: usize,
    n_words: usize,
) -> Result<(), DecodeError> {
    if topics.len() != n_topics {
        return Err(DecodeError::TopicCount {
            expected: n_topics,
            got: topics.len(),
        });
    }
    if data.len() != n_words * 32 {
        return Err(DecodeError::DataLength {
            expected: n_words * 32,
            got: data.len(),
        });
    }
    Ok(())
}

/// Reads a sign-extended `int24` from a 32-byte word.
fn int24(value: U256) -> Result<i32, DecodeError> {
    let signed = I256::from_raw(value);
    let tick = i32::try_from(signed).map_err(|_| DecodeError::ValueOutOfRange)?;
    if !(-(1 << 23)..(1 << 23)).contains(&tick) {
        return Err(DecodeError::ValueOutOfRange);
    }
    Ok(tick)
}

fn uint128(value: U256) -> Result<u128, DecodeError> {
    u128::try_from(value).map_err(|_| DecodeError::ValueOutOfRange)
}

fn uint8(value: U256) -> Result<u8, DecodeError> {
    u8::try_from(value).map_err(|_| DecodeError::ValueOutOfRange)
}

/// Encodes an event as the topics and data of a pool log, the inverse of
/// `decode_log`. Fields the replica does not track (sender, owner,
/// recipient and the like) are written as zero. Used to build histories
/// for tests and for simulated pools.
pub fn encode_log(event: &PoolEvent) -> (Vec<B256>, Vec<u8>) {
    fn signed(value: i32) -> U256 {
        I256::try_from(value).expect("an i32 fits").into_raw()
    }
    fn data(words: &[U256]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_be_bytes::<32>()).collect()
    }
    let topic = |value: U256| B256::from(value.to_be_bytes::<32>());
    let zero = B256::ZERO;

    match event {
        PoolEvent::Swap(swap) => (
            vec![topics::swap(), zero, zero],
            data(&[
                swap.amount0.into_raw(),
                swap.amount1.into_raw(),
                swap.sqrt_price_x96,
                U256::from(swap.liquidity),
                signed(swap.tick),
            ]),
        ),
        PoolEvent::Mint {
            tick_lower,
            tick_upper,
            amount,
            amount0,
            amount1,
        } => (
            vec![
                topics::mint(),
                zero,
                topic(signed(*tick_lower)),
                topic(signed(*tick_upper)),
            ],
            data(&[U256::ZERO, U256::from(*amount), *amount0, *amount1]),
        ),
        PoolEvent::Burn {
            tick_lower,
            tick_upper,
            amount,
            amount0,
            amount1,
        } => (
            vec![
                topics::burn(),
                zero,
                topic(signed(*tick_lower)),
                topic(signed(*tick_upper)),
            ],
            data(&[U256::from(*amount), *amount0, *amount1]),
        ),
        PoolEvent::Initialize {
            sqrt_price_x96,
            tick,
        } => (
            vec![topics::initialize()],
            data(&[*sqrt_price_x96, signed(*tick)]),
        ),
        PoolEvent::Flash { paid0, paid1 } => (
            vec![topics::flash(), zero, zero],
            data(&[U256::ZERO, U256::ZERO, *paid0, *paid1]),
        ),
        PoolEvent::SetFeeProtocol {
            fee_protocol0,
            fee_protocol1,
        } => (
            vec![topics::set_fee_protocol()],
            data(&[
                U256::ZERO,
                U256::ZERO,
                U256::from(*fee_protocol0),
                U256::from(*fee_protocol1),
            ]),
        ),
    }
}

/// Decodes a pool log. Returns `None` for events that do not change the
/// state the replica tracks.
pub fn decode_log(log_topics: &[B256], data: &[u8]) -> Result<Option<PoolEvent>, DecodeError> {
    let Some(topic0) = log_topics.first() else {
        return Ok(None);
    };

    if *topic0 == topics::swap() {
        require(log_topics, data, 3, 5)?;
        Ok(Some(PoolEvent::Swap(SwapEvent {
            amount0: I256::from_raw(word(data, 0)),
            amount1: I256::from_raw(word(data, 1)),
            sqrt_price_x96: word(data, 2),
            liquidity: uint128(word(data, 3))?,
            tick: int24(word(data, 4))?,
        })))
    } else if *topic0 == topics::mint() {
        // sender is not indexed, so it is the first data word.
        require(log_topics, data, 4, 4)?;
        Ok(Some(PoolEvent::Mint {
            tick_lower: int24(U256::from_be_bytes(log_topics[2].0))?,
            tick_upper: int24(U256::from_be_bytes(log_topics[3].0))?,
            amount: uint128(word(data, 1))?,
            amount0: word(data, 2),
            amount1: word(data, 3),
        }))
    } else if *topic0 == topics::burn() {
        require(log_topics, data, 4, 3)?;
        Ok(Some(PoolEvent::Burn {
            tick_lower: int24(U256::from_be_bytes(log_topics[2].0))?,
            tick_upper: int24(U256::from_be_bytes(log_topics[3].0))?,
            amount: uint128(word(data, 0))?,
            amount0: word(data, 1),
            amount1: word(data, 2),
        }))
    } else if *topic0 == topics::initialize() {
        require(log_topics, data, 1, 2)?;
        Ok(Some(PoolEvent::Initialize {
            sqrt_price_x96: word(data, 0),
            tick: int24(word(data, 1))?,
        }))
    } else if *topic0 == topics::flash() {
        require(log_topics, data, 3, 4)?;
        Ok(Some(PoolEvent::Flash {
            paid0: word(data, 2),
            paid1: word(data, 3),
        }))
    } else if *topic0 == topics::set_fee_protocol() {
        require(log_topics, data, 1, 4)?;
        Ok(Some(PoolEvent::SetFeeProtocol {
            fee_protocol0: uint8(word(data, 2))?,
            fee_protocol1: uint8(word(data, 3))?,
        }))
    } else {
        Ok(None)
    }
}

impl Pool {
    /// Applies one event to the replica.
    ///
    /// In strict mode the token amounts the replica computes for a mint or
    /// burn must equal the event's, and a swap must replay exactly.
    pub fn apply_event(
        &mut self,
        event: &PoolEvent,
        mode: Replay,
        observer: &mut impl SwapObserver,
    ) -> PoolResult<()> {
        match event {
            PoolEvent::Initialize {
                sqrt_price_x96,
                tick,
            } => {
                self.initialize(*sqrt_price_x96)?;
                if self.tick() != *tick {
                    return Err(PoolError::ReplayMismatch(format!(
                        "initialized at tick {}, event says {tick}",
                        self.tick()
                    )));
                }
                Ok(())
            }
            PoolEvent::Mint {
                tick_lower,
                tick_upper,
                amount,
                amount0,
                amount1,
            } => {
                let computed = self.mint(*tick_lower, *tick_upper, *amount)?;
                check_amounts("mint", mode, computed, (*amount0, *amount1))
            }
            PoolEvent::Burn {
                tick_lower,
                tick_upper,
                amount,
                amount0,
                amount1,
            } => {
                let computed = self.burn(*tick_lower, *tick_upper, *amount)?;
                check_amounts("burn", mode, computed, (*amount0, *amount1))
            }
            PoolEvent::Swap(swap) => self.apply_swap_event(swap, mode, observer),
            PoolEvent::Flash { paid0, paid1 } => self.flash(*paid0, *paid1),
            PoolEvent::SetFeeProtocol {
                fee_protocol0,
                fee_protocol1,
            } => {
                self.set_fee_protocol(*fee_protocol0, *fee_protocol1);
                Ok(())
            }
        }
    }
}

fn check_amounts(
    what: &str,
    mode: Replay,
    computed: (U256, U256),
    event: (U256, U256),
) -> PoolResult<()> {
    if mode == Replay::Strict && computed != event {
        return Err(PoolError::ReplayMismatch(format!(
            "{what} computed amounts {} and {}, event says {} and {}",
            computed.0, computed.1, event.0, event.1
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Q96;
    use super::*;

    #[test]
    fn every_event_survives_encoding_and_decoding() {
        let events = [
            PoolEvent::Initialize {
                sqrt_price_x96: Q96,
                tick: -887_272,
            },
            PoolEvent::Mint {
                tick_lower: -887_220,
                tick_upper: 887_220,
                amount: u128::MAX,
                amount0: U256::MAX,
                amount1: U256::from(1u8),
            },
            PoolEvent::Burn {
                tick_lower: -60,
                tick_upper: 120,
                amount: 0,
                amount0: U256::ZERO,
                amount1: U256::from(99u8),
            },
            PoolEvent::Swap(SwapEvent {
                amount0: I256::try_from(-123_456_789i64).unwrap(),
                amount1: I256::MAX,
                sqrt_price_x96: Q96 * U256::from(3u8),
                liquidity: 1u128 << 100,
                tick: -1,
            }),
            PoolEvent::Flash {
                paid0: U256::from(5u8),
                paid1: U256::MAX,
            },
            PoolEvent::SetFeeProtocol {
                fee_protocol0: 4,
                fee_protocol1: 10,
            },
        ];
        for event in events {
            let (log_topics, data) = encode_log(&event);
            assert_eq!(decode_log(&log_topics, &data), Ok(Some(event)));
        }
    }

    fn topic_from_i32(value: i32) -> B256 {
        B256::from(
            I256::try_from(value)
                .unwrap()
                .into_raw()
                .to_be_bytes::<32>(),
        )
    }

    fn words(values: &[U256]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes::<32>()).collect()
    }

    fn signed(value: i64) -> U256 {
        I256::try_from(value).unwrap().into_raw()
    }

    #[test]
    fn topic_hashes_match_the_deployed_contracts() {
        // Well-known topic 0 values of the Uniswap v3 pool events.
        let hex = |t: B256| format!("{t:#x}");
        assert_eq!(
            hex(topics::swap()),
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
        );
        assert_eq!(
            hex(topics::mint()),
            "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"
        );
        assert_eq!(
            hex(topics::burn()),
            "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"
        );
        assert_eq!(
            hex(topics::initialize()),
            "0x98636036cb66a9c19a37435efc1e90142190214e8abeb821bdba3f2990dd4c95"
        );
    }

    #[test]
    fn decodes_a_swap() {
        let data = words(&[
            signed(-1_500),
            signed(2_000),
            Q96,
            U256::from(123_456u32),
            signed(-12),
        ]);
        let log_topics = [topics::swap(), B256::ZERO, B256::ZERO];
        let event = decode_log(&log_topics, &data).unwrap().unwrap();
        assert_eq!(
            event,
            PoolEvent::Swap(SwapEvent {
                amount0: I256::try_from(-1_500).unwrap(),
                amount1: I256::try_from(2_000).unwrap(),
                sqrt_price_x96: Q96,
                liquidity: 123_456,
                tick: -12,
            })
        );
    }

    #[test]
    fn decodes_mint_and_burn_with_negative_ticks_in_topics() {
        let mint_topics = [
            topics::mint(),
            B256::ZERO,
            topic_from_i32(-887_220),
            topic_from_i32(60),
        ];
        let mint_data = words(&[
            U256::ZERO, // sender
            U256::from(999u16),
            U256::from(5u8),
            U256::from(7u8),
        ]);
        assert_eq!(
            decode_log(&mint_topics, &mint_data).unwrap().unwrap(),
            PoolEvent::Mint {
                tick_lower: -887_220,
                tick_upper: 60,
                amount: 999,
                amount0: U256::from(5u8),
                amount1: U256::from(7u8),
            }
        );

        let burn_topics = [
            topics::burn(),
            B256::ZERO,
            topic_from_i32(-60),
            topic_from_i32(120),
        ];
        let burn_data = words(&[U256::from(999u16), U256::from(4u8), U256::from(6u8)]);
        assert_eq!(
            decode_log(&burn_topics, &burn_data).unwrap().unwrap(),
            PoolEvent::Burn {
                tick_lower: -60,
                tick_upper: 120,
                amount: 999,
                amount0: U256::from(4u8),
                amount1: U256::from(6u8),
            }
        );
    }

    #[test]
    fn decodes_the_remaining_state_changing_events() {
        let init = decode_log(&[topics::initialize()], &words(&[Q96, signed(0)]))
            .unwrap()
            .unwrap();
        assert_eq!(
            init,
            PoolEvent::Initialize {
                sqrt_price_x96: Q96,
                tick: 0
            }
        );

        let flash = decode_log(
            &[topics::flash(), B256::ZERO, B256::ZERO],
            &words(&[
                U256::from(1u8),
                U256::from(2u8),
                U256::from(3u8),
                U256::from(4u8),
            ]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            flash,
            PoolEvent::Flash {
                paid0: U256::from(3u8),
                paid1: U256::from(4u8)
            }
        );

        let fee = decode_log(
            &[topics::set_fee_protocol()],
            &words(&[U256::ZERO, U256::ZERO, U256::from(4u8), U256::from(6u8)]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            fee,
            PoolEvent::SetFeeProtocol {
                fee_protocol0: 4,
                fee_protocol1: 6
            }
        );
    }

    #[test]
    fn ignores_events_that_do_not_change_tracked_state() {
        let collect = keccak256("Collect(address,address,int24,int24,uint128,uint128)");
        assert_eq!(decode_log(&[collect], &[]), Ok(None));
        assert_eq!(decode_log(&[], &[]), Ok(None));
    }

    #[test]
    fn rejects_malformed_logs() {
        assert!(matches!(
            decode_log(&[topics::swap()], &[0u8; 160]),
            Err(DecodeError::TopicCount { .. })
        ));
        assert!(matches!(
            decode_log(&[topics::swap(), B256::ZERO, B256::ZERO], &[0u8; 100]),
            Err(DecodeError::DataLength { .. })
        ));
        // A tick wider than int24.
        let bad_tick = words(&[Q96, U256::from(1u32 << 24)]);
        assert_eq!(
            decode_log(&[topics::initialize()], &bad_tick),
            Err(DecodeError::ValueOutOfRange)
        );
    }

    #[test]
    fn applying_events_builds_the_same_pool_as_calling_the_functions() {
        let mut by_events = Pool::new(500, 10);
        let mut by_calls = Pool::new(500, 10);

        by_calls.initialize(Q96).unwrap();
        let minted = by_calls.mint(-100, 100, 1_000_000_000_000).unwrap();
        let amounts = by_calls
            .swap(
                true,
                I256::try_from(1_000_000).unwrap(),
                super::super::tick_math::MIN_SQRT_RATIO + U256::from(1u8),
                &mut (),
            )
            .unwrap();
        let swap = SwapEvent {
            amount0: amounts.amount0,
            amount1: amounts.amount1,
            sqrt_price_x96: by_calls.sqrt_price_x96(),
            liquidity: by_calls.liquidity(),
            tick: by_calls.tick(),
        };
        by_calls.set_fee_protocol(4, 4);
        let burned = by_calls.burn(-100, 100, 400_000_000_000).unwrap();

        let events = [
            PoolEvent::Initialize {
                sqrt_price_x96: Q96,
                tick: 0,
            },
            PoolEvent::Mint {
                tick_lower: -100,
                tick_upper: 100,
                amount: 1_000_000_000_000,
                amount0: minted.0,
                amount1: minted.1,
            },
            PoolEvent::Swap(swap),
            PoolEvent::SetFeeProtocol {
                fee_protocol0: 4,
                fee_protocol1: 4,
            },
            PoolEvent::Burn {
                tick_lower: -100,
                tick_upper: 100,
                amount: 400_000_000_000,
                amount0: burned.0,
                amount1: burned.1,
            },
        ];
        for event in &events {
            by_events
                .apply_event(event, Replay::Strict, &mut ())
                .unwrap();
        }
        assert_eq!(by_events, by_calls);
    }

    #[test]
    fn strict_mode_refuses_a_mint_whose_amounts_do_not_match() {
        let mut pool = Pool::new(500, 10);
        pool.initialize(Q96).unwrap();
        let wrong = PoolEvent::Mint {
            tick_lower: -100,
            tick_upper: 100,
            amount: 1_000_000,
            amount0: U256::from(1u8),
            amount1: U256::from(1u8),
        };
        assert!(matches!(
            pool.clone().apply_event(&wrong, Replay::Strict, &mut ()),
            Err(PoolError::ReplayMismatch(_))
        ));
        assert!(pool.apply_event(&wrong, Replay::Tolerant, &mut ()).is_ok());
    }
}
