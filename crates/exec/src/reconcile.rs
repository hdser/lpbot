//! Reading back what landed (`docs/DESIGN.md`, "Execution", step 7;
//! `docs/PLAN.md`, piece 5). After a transaction, and at every start, the
//! position and the Safe's balances are read from the chain and compared
//! with what the plan expected; the engine books what the chain shows, not
//! what the plan hoped. A difference beyond the plan's own bounds is
//! reported: the action is then not adopted, and the run holds new entries
//! until someone looks.

use alloy_primitives::{Address, U256};

use crate::abi::selector;
use crate::submit::Node;

/// A position as the position manager holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub token_id: U256,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
    /// Owed and not yet collected.
    pub owed0: u128,
    pub owed1: u128,
}

fn word(data: &[u8], i: usize) -> anyhow::Result<&[u8]> {
    data.get(i * 32..(i + 1) * 32)
        .ok_or_else(|| anyhow::anyhow!("the answer is {} bytes, short of word {i}", data.len()))
}

fn uint(data: &[u8], i: usize) -> anyhow::Result<U256> {
    Ok(U256::from_be_slice(word(data, i)?))
}

fn small(data: &[u8], i: usize, bits: usize) -> anyhow::Result<U256> {
    let value = uint(data, i)?;
    if value.bit_len() > bits {
        anyhow::bail!("word {i} does not fit in {bits} bits: {value}");
    }
    Ok(value)
}

fn int24(data: &[u8], i: usize) -> anyhow::Result<i32> {
    let w = word(data, i)?;
    let value = i32::from_be_bytes(w[28..32].try_into().expect("four bytes"));
    let sign = if value < 0 { 0xff } else { 0 };
    if w[..28].iter().any(|b| *b != sign) || !(-8_388_608..8_388_608).contains(&value) {
        anyhow::bail!("word {i} is not an int24");
    }
    Ok(value)
}

fn address(data: &[u8], i: usize) -> anyhow::Result<Address> {
    let w = word(data, i)?;
    if w[..12].iter().any(|b| *b != 0) {
        anyhow::bail!("word {i} is not an address");
    }
    Ok(Address::from_slice(&w[12..]))
}

/// Reads `positions(token_id)` back from the answer's bytes: nonce,
/// operator, token0, token1, fee, tickLower, tickUpper, liquidity,
/// feeGrowthInside0LastX128, feeGrowthInside1LastX128, tokensOwed0,
/// tokensOwed1.
pub fn decode_position(token_id: U256, data: &[u8]) -> anyhow::Result<Position> {
    Ok(Position {
        token_id,
        token0: address(data, 2)?,
        token1: address(data, 3)?,
        fee: small(data, 4, 24)?.to::<u32>(),
        tick_lower: int24(data, 5)?,
        tick_upper: int24(data, 6)?,
        liquidity: small(data, 7, 128)?.to::<u128>(),
        owed0: small(data, 10, 128)?.to::<u128>(),
        owed1: small(data, 11, 128)?.to::<u128>(),
    })
}

fn call_with(sig: &str, args: &[U256]) -> Vec<u8> {
    let mut data = selector(sig).to_vec();
    for a in args {
        data.extend_from_slice(&a.to_be_bytes::<32>());
    }
    data
}

fn address_arg(a: Address) -> U256 {
    U256::from_be_slice(a.as_slice())
}

/// The position `token_id` as the chain holds it now.
pub fn read_position(
    node: &dyn Node,
    manager: Address,
    from: Address,
    token_id: U256,
) -> anyhow::Result<Position> {
    let data = node.call(from, manager, &call_with("positions(uint256)", &[token_id]))?;
    decode_position(token_id, &data)
}

/// The newest position NFT `owner` holds, if any.
pub fn newest_token(
    node: &dyn Node,
    manager: Address,
    owner: Address,
) -> anyhow::Result<Option<U256>> {
    let count = uint(
        &node.call(
            owner,
            manager,
            &call_with("balanceOf(address)", &[address_arg(owner)]),
        )?,
        0,
    )?;
    if count.is_zero() {
        return Ok(None);
    }
    let id = uint(
        &node.call(
            owner,
            manager,
            &call_with(
                "tokenOfOwnerByIndex(address,uint256)",
                &[address_arg(owner), count - U256::from(1u8)],
            ),
        )?,
        0,
    )?;
    Ok(Some(id))
}

/// `owner`'s balance of `token`.
pub fn balance(node: &dyn Node, token: Address, owner: Address) -> anyhow::Result<U256> {
    uint(
        &node.call(
            owner,
            token,
            &call_with("balanceOf(address)", &[address_arg(owner)]),
        )?,
        0,
    )
}

/// What a plan expected the position to be once it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// A position in this range with at least this much liquidity: the
    /// contract computes it from the amounts, which the plan bounded.
    Position {
        tick_lower: i32,
        tick_upper: i32,
        min_liquidity: u128,
    },
    /// No position: it was taken out and its NFT burned.
    None,
}

/// What reading back found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// As expected: the position the engine should now hold, if any.
    Adopt(Option<Position>),
    /// Not what the plan expected: don't adopt; hold entries and tell the
    /// owner.
    Differs(String),
}

/// Compares what the chain holds with what the plan expected.
pub fn compare(found: Option<Position>, expected: &Expected) -> Outcome {
    match (expected, found) {
        (Expected::None, None) => Outcome::Adopt(None),
        (Expected::None, Some(p)) if p.liquidity == 0 => Outcome::Adopt(None),
        (Expected::None, Some(p)) => Outcome::Differs(format!(
            "position {} still holds {} liquidity after it was to be taken out",
            p.token_id, p.liquidity
        )),
        (Expected::Position { .. }, None) => {
            Outcome::Differs("no position was found where the plan made one".into())
        }
        (
            Expected::Position {
                tick_lower,
                tick_upper,
                min_liquidity,
            },
            Some(p),
        ) => {
            if (p.tick_lower, p.tick_upper) != (*tick_lower, *tick_upper) {
                Outcome::Differs(format!(
                    "position {} is [{}, {}), the plan made [{tick_lower}, {tick_upper})",
                    p.token_id, p.tick_lower, p.tick_upper
                ))
            } else if p.liquidity < *min_liquidity {
                Outcome::Differs(format!(
                    "position {} holds {} liquidity, under the {min_liquidity} the plan bounded",
                    p.token_id, p.liquidity
                ))
            } else {
                Outcome::Adopt(Some(p))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(words: &[U256]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_be_bytes::<32>()).collect()
    }

    fn signed(v: i32) -> U256 {
        if v >= 0 {
            U256::from(v as u64)
        } else {
            U256::MAX - U256::from((-(v as i64) - 1) as u64)
        }
    }

    fn position_answer(lower: i32, upper: i32, liquidity: u128) -> Vec<u8> {
        let usdc = address_arg(Address::repeat_byte(0xa0));
        let weth = address_arg(Address::repeat_byte(0xc0));
        answer(&[
            U256::ZERO,
            U256::ZERO,
            usdc,
            weth,
            U256::from(500u32),
            signed(lower),
            signed(upper),
            U256::from(liquidity),
            U256::ZERO,
            U256::ZERO,
            U256::from(3u8),
            U256::from(4u8),
        ])
    }

    #[test]
    fn a_position_reads_back_from_the_position_managers_answer() {
        let p = decode_position(U256::from(9u8), &position_answer(-201_000, -200_400, 77)).unwrap();
        assert_eq!(
            (p.tick_lower, p.tick_upper, p.liquidity),
            (-201_000, -200_400, 77)
        );
        assert_eq!((p.fee, p.owed0, p.owed1), (500, 3, 4));
        assert_eq!(p.token1, Address::repeat_byte(0xc0));
        // Short or malformed answers are refused, not read as zeros.
        assert!(decode_position(U256::ZERO, &position_answer(0, 10, 1)[..300]).is_err());
        let mut bad = position_answer(0, 10, 1);
        bad[5 * 32] = 1;
        assert!(decode_position(U256::ZERO, &bad).is_err());
    }

    #[test]
    fn what_landed_is_adopted_only_when_it_is_what_the_plan_made() {
        let p = decode_position(U256::from(9u8), &position_answer(-600, 0, 100)).unwrap();
        let made = Expected::Position {
            tick_lower: -600,
            tick_upper: 0,
            min_liquidity: 99,
        };
        assert_eq!(compare(Some(p), &made), Outcome::Adopt(Some(p)));
        let more = Expected::Position {
            tick_lower: -600,
            tick_upper: 0,
            min_liquidity: 101,
        };
        assert!(
            matches!(compare(Some(p), &more), Outcome::Differs(r) if r.contains("under the 101"))
        );
        let elsewhere = Expected::Position {
            tick_lower: -1200,
            tick_upper: -600,
            min_liquidity: 1,
        };
        assert!(matches!(compare(Some(p), &elsewhere), Outcome::Differs(_)));
        assert!(matches!(compare(None, &made), Outcome::Differs(_)));
        assert_eq!(compare(None, &Expected::None), Outcome::Adopt(None));
        let empty = Position { liquidity: 0, ..p };
        assert_eq!(compare(Some(empty), &Expected::None), Outcome::Adopt(None));
        assert!(matches!(
            compare(Some(p), &Expected::None),
            Outcome::Differs(_)
        ));
    }
}
