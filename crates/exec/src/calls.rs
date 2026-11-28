//! The calls the executor makes, each with a decoder that reads it back.
//!
//! Uniswap v3's NonfungiblePositionManager (mint, increase, decrease,
//! collect, burn), its SwapRouter (one pool, exact input), ERC-20 approvals,
//! the Safe's MultiSendCallOnly batch and the Zodiac Roles v2 entry point.
//! No address is written here: every builder takes the contract it calls,
//! and the planner takes them from the run's configuration.

use alloy_primitives::{Address, B256, U256};

use crate::abi::{self, AbiError, Decoder, Token};

/// `NonfungiblePositionManager.mint(MintParams)`.
pub const MINT: &str =
    "mint((address,address,uint24,int24,int24,uint256,uint256,uint256,uint256,address,uint256))";
/// `NonfungiblePositionManager.increaseLiquidity(IncreaseLiquidityParams)`.
pub const INCREASE_LIQUIDITY: &str =
    "increaseLiquidity((uint256,uint256,uint256,uint256,uint256,uint256))";
/// `NonfungiblePositionManager.decreaseLiquidity(DecreaseLiquidityParams)`.
pub const DECREASE_LIQUIDITY: &str = "decreaseLiquidity((uint256,uint128,uint256,uint256,uint256))";
/// `NonfungiblePositionManager.collect(CollectParams)`.
pub const COLLECT: &str = "collect((uint256,address,uint128,uint128))";
/// `NonfungiblePositionManager.burn(uint256)`.
pub const BURN: &str = "burn(uint256)";
/// `SwapRouter.exactInputSingle(ExactInputSingleParams)`, the first router,
/// whose parameters carry a deadline.
pub const EXACT_INPUT_SINGLE: &str =
    "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))";
/// ERC-20 `approve(address,uint256)`.
pub const APPROVE: &str = "approve(address,uint256)";
/// Safe `MultiSendCallOnly.multiSend(bytes)`.
pub const MULTI_SEND: &str = "multiSend(bytes)";
/// Zodiac Roles v2 `execTransactionWithRole(to, value, data, operation,
/// roleKey, shouldRevert)`.
///
/// NOT VERIFIED: this signature is written from the Roles v2 documentation
/// as remembered, not read from the deployed contract. Before any call is
/// sent with it (milestone M4), confirm it on a fork against the deployed
/// Roles v2 modifier: a call built here must reach the module's checks, and
/// one with a role the sender lacks must revert for that reason.
pub const EXEC_TRANSACTION_WITH_ROLE: &str =
    "execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)";

/// How a Safe carries out a call: a plain call, or a delegate call (which
/// runs the target's code as the Safe; only the MultiSend batch is made so).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Call = 0,
    DelegateCall = 1,
}

impl Operation {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Operation::Call),
            1 => Some(Operation::DelegateCall),
            _ => None,
        }
    }
}

/// One call: its target, the ether it sends, its data and how it is made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Call {
    pub to: Address,
    pub value: U256,
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
    pub operation: Operation,
}

mod hex_bytes {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{}", hex::encode(value)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        hex::decode(text.trim_start_matches("0x")).map_err(D::Error::custom)
    }
}

impl Call {
    fn plain(to: Address, data: Vec<u8>) -> Self {
        Self {
            to,
            value: U256::ZERO,
            data,
            operation: Operation::Call,
        }
    }
}

/// The position manager's `MintParams`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintParams {
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub amount0_desired: U256,
    pub amount1_desired: U256,
    pub amount0_min: U256,
    pub amount1_min: U256,
    pub recipient: Address,
    pub deadline: U256,
}

/// Mints a new position NFT in `[tick_lower, tick_upper)`.
pub fn mint(manager: Address, p: &MintParams) -> Result<Call, AbiError> {
    let tokens = [
        Token::Word(abi::address(p.token0)),
        Token::Word(abi::address(p.token1)),
        Token::Word(abi::uint24(p.fee)?),
        Token::Word(abi::int24(p.tick_lower)?),
        Token::Word(abi::int24(p.tick_upper)?),
        Token::Word(abi::uint(p.amount0_desired)),
        Token::Word(abi::uint(p.amount1_desired)),
        Token::Word(abi::uint(p.amount0_min)),
        Token::Word(abi::uint(p.amount1_min)),
        Token::Word(abi::address(p.recipient)),
        Token::Word(abi::uint(p.deadline)),
    ];
    Ok(Call::plain(
        manager,
        abi::encode_call(abi::selector(MINT), &tokens),
    ))
}

pub fn decode_mint(data: &[u8]) -> Result<MintParams, AbiError> {
    let d = Decoder::new(data, MINT)?;
    Ok(MintParams {
        token0: d.address(0)?,
        token1: d.address(1)?,
        fee: d.uint24(2)?,
        tick_lower: d.int24(3)?,
        tick_upper: d.int24(4)?,
        amount0_desired: d.uint(5)?,
        amount1_desired: d.uint(6)?,
        amount0_min: d.uint(7)?,
        amount1_min: d.uint(8)?,
        recipient: d.address(9)?,
        deadline: d.uint(10)?,
    })
}

/// The position manager's `IncreaseLiquidityParams`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncreaseParams {
    pub token_id: U256,
    pub amount0_desired: U256,
    pub amount1_desired: U256,
    pub amount0_min: U256,
    pub amount1_min: U256,
    pub deadline: U256,
}

/// Adds liquidity to an existing position NFT.
pub fn increase_liquidity(manager: Address, p: &IncreaseParams) -> Call {
    let tokens = [
        Token::Word(abi::uint(p.token_id)),
        Token::Word(abi::uint(p.amount0_desired)),
        Token::Word(abi::uint(p.amount1_desired)),
        Token::Word(abi::uint(p.amount0_min)),
        Token::Word(abi::uint(p.amount1_min)),
        Token::Word(abi::uint(p.deadline)),
    ];
    Call::plain(
        manager,
        abi::encode_call(abi::selector(INCREASE_LIQUIDITY), &tokens),
    )
}

pub fn decode_increase_liquidity(data: &[u8]) -> Result<IncreaseParams, AbiError> {
    let d = Decoder::new(data, INCREASE_LIQUIDITY)?;
    Ok(IncreaseParams {
        token_id: d.uint(0)?,
        amount0_desired: d.uint(1)?,
        amount1_desired: d.uint(2)?,
        amount0_min: d.uint(3)?,
        amount1_min: d.uint(4)?,
        deadline: d.uint(5)?,
    })
}

/// The position manager's `DecreaseLiquidityParams`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecreaseParams {
    pub token_id: U256,
    pub liquidity: u128,
    pub amount0_min: U256,
    pub amount1_min: U256,
    pub deadline: U256,
}

/// Takes liquidity out of a position NFT; the tokens become owed to it and
/// are taken with `collect`.
pub fn decrease_liquidity(manager: Address, p: &DecreaseParams) -> Call {
    let tokens = [
        Token::Word(abi::uint(p.token_id)),
        Token::Word(abi::uint128(p.liquidity)),
        Token::Word(abi::uint(p.amount0_min)),
        Token::Word(abi::uint(p.amount1_min)),
        Token::Word(abi::uint(p.deadline)),
    ];
    Call::plain(
        manager,
        abi::encode_call(abi::selector(DECREASE_LIQUIDITY), &tokens),
    )
}

pub fn decode_decrease_liquidity(data: &[u8]) -> Result<DecreaseParams, AbiError> {
    let d = Decoder::new(data, DECREASE_LIQUIDITY)?;
    Ok(DecreaseParams {
        token_id: d.uint(0)?,
        liquidity: d.uint128(1)?,
        amount0_min: d.uint(2)?,
        amount1_min: d.uint(3)?,
        deadline: d.uint(4)?,
    })
}

/// The position manager's `CollectParams`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectParams {
    pub token_id: U256,
    pub recipient: Address,
    pub amount0_max: u128,
    pub amount1_max: u128,
}

/// Sends what a position NFT is owed (principal taken out and fees) to
/// `recipient`, up to the maxima.
pub fn collect(manager: Address, p: &CollectParams) -> Call {
    let tokens = [
        Token::Word(abi::uint(p.token_id)),
        Token::Word(abi::address(p.recipient)),
        Token::Word(abi::uint128(p.amount0_max)),
        Token::Word(abi::uint128(p.amount1_max)),
    ];
    Call::plain(manager, abi::encode_call(abi::selector(COLLECT), &tokens))
}

pub fn decode_collect(data: &[u8]) -> Result<CollectParams, AbiError> {
    let d = Decoder::new(data, COLLECT)?;
    Ok(CollectParams {
        token_id: d.uint(0)?,
        recipient: d.address(1)?,
        amount0_max: d.uint128(2)?,
        amount1_max: d.uint128(3)?,
    })
}

/// Burns an emptied position NFT (no liquidity, nothing owed).
pub fn burn(manager: Address, token_id: U256) -> Call {
    Call::plain(
        manager,
        abi::encode_call(abi::selector(BURN), &[Token::Word(abi::uint(token_id))]),
    )
}

pub fn decode_burn(data: &[u8]) -> Result<U256, AbiError> {
    Decoder::new(data, BURN)?.uint(0)
}

/// The router's `ExactInputSingleParams`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactInputSingleParams {
    pub token_in: Address,
    pub token_out: Address,
    pub fee: u32,
    pub recipient: Address,
    pub deadline: U256,
    pub amount_in: U256,
    pub amount_out_minimum: U256,
    pub sqrt_price_limit_x96: U256,
}

/// Swaps exactly `amount_in` through one pool, reverting below
/// `amount_out_minimum` or past `sqrt_price_limit_x96`.
pub fn exact_input_single(router: Address, p: &ExactInputSingleParams) -> Result<Call, AbiError> {
    let tokens = [
        Token::Word(abi::address(p.token_in)),
        Token::Word(abi::address(p.token_out)),
        Token::Word(abi::uint24(p.fee)?),
        Token::Word(abi::address(p.recipient)),
        Token::Word(abi::uint(p.deadline)),
        Token::Word(abi::uint(p.amount_in)),
        Token::Word(abi::uint(p.amount_out_minimum)),
        Token::Word(abi::uint160(p.sqrt_price_limit_x96)?),
    ];
    Ok(Call::plain(
        router,
        abi::encode_call(abi::selector(EXACT_INPUT_SINGLE), &tokens),
    ))
}

pub fn decode_exact_input_single(data: &[u8]) -> Result<ExactInputSingleParams, AbiError> {
    let d = Decoder::new(data, EXACT_INPUT_SINGLE)?;
    Ok(ExactInputSingleParams {
        token_in: d.address(0)?,
        token_out: d.address(1)?,
        fee: d.uint24(2)?,
        recipient: d.address(3)?,
        deadline: d.uint(4)?,
        amount_in: d.uint(5)?,
        amount_out_minimum: d.uint(6)?,
        sqrt_price_limit_x96: d.uint_bits(7, 160)?,
    })
}

/// Lets `spender` take up to `amount` of `token` from the caller.
pub fn approve(token: Address, spender: Address, amount: U256) -> Call {
    Call::plain(
        token,
        abi::encode_call(
            abi::selector(APPROVE),
            &[
                Token::Word(abi::address(spender)),
                Token::Word(abi::uint(amount)),
            ],
        ),
    )
}

/// `(spender, amount)` of an approval.
pub fn decode_approve(data: &[u8]) -> Result<(Address, U256), AbiError> {
    let d = Decoder::new(data, APPROVE)?;
    Ok((d.address(0)?, d.uint(1)?))
}

/// Why a batch cannot be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BatchError {
    #[error(
        "call {index} of the batch is a delegate call; MultiSendCallOnly takes plain calls only"
    )]
    DelegateInside { index: usize },
    #[error("the batch is empty; there is nothing to send")]
    Empty,
    #[error("the batch is cut short at byte {at}; it was not made by `multi_send`")]
    Truncated { at: usize },
    #[error("call {index} of the batch has operation {found}, not 0 or 1")]
    Operation { index: usize, found: u8 },
    #[error(transparent)]
    Abi(#[from] AbiError),
}

/// The packed transactions `multiSend` takes: for each call its operation
/// (one byte), target (20), value (32), data length (32) and data, with no
/// padding between them.
pub fn pack_transactions(calls: &[Call]) -> Result<Vec<u8>, BatchError> {
    if calls.is_empty() {
        return Err(BatchError::Empty);
    }
    let mut packed = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        if call.operation != Operation::Call {
            return Err(BatchError::DelegateInside { index });
        }
        packed.push(call.operation as u8);
        packed.extend_from_slice(call.to.as_slice());
        packed.extend_from_slice(&abi::uint(call.value));
        packed.extend_from_slice(&abi::uint_u64(call.data.len() as u64));
        packed.extend_from_slice(&call.data);
    }
    Ok(packed)
}

/// Reads packed transactions back.
pub fn unpack_transactions(packed: &[u8]) -> Result<Vec<Call>, BatchError> {
    let mut calls = Vec::new();
    let mut at = 0;
    while at < packed.len() {
        let header = packed
            .get(at..at + 85)
            .ok_or(BatchError::Truncated { at })?;
        let operation = Operation::from_u8(header[0]).ok_or(BatchError::Operation {
            index: calls.len(),
            found: header[0],
        })?;
        let to = Address::from_slice(&header[1..21]);
        let value = U256::from_be_slice(&header[21..53]);
        let len = usize::try_from(U256::from_be_slice(&header[53..85]))
            .map_err(|_| BatchError::Truncated { at })?;
        let start = at + 85;
        let data = packed
            .get(start..start + len)
            .ok_or(BatchError::Truncated { at: start })?;
        calls.push(Call {
            to,
            value,
            data: data.to_vec(),
            operation,
        });
        at = start + len;
    }
    Ok(calls)
}

/// The calls as one `multiSend`, which the Safe makes as a delegate call to
/// `multisend` (MultiSendCallOnly), so each inner call is made by the Safe.
pub fn multi_send(multisend: Address, calls: &[Call]) -> Result<Call, BatchError> {
    let packed = pack_transactions(calls)?;
    Ok(Call {
        to: multisend,
        value: U256::ZERO,
        data: abi::encode_call(abi::selector(MULTI_SEND), &[Token::Bytes(packed)]),
        operation: Operation::DelegateCall,
    })
}

/// The inner calls of a `multiSend`.
pub fn decode_multi_send(data: &[u8]) -> Result<Vec<Call>, BatchError> {
    let d = Decoder::new(data, MULTI_SEND)?;
    unpack_transactions(&d.bytes(0)?)
}

/// What a roles call asks the Safe to do, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolesCall {
    pub inner: Call,
    pub role_key: B256,
    pub should_revert: bool,
}

/// The call the keeper sends: to the roles module, which checks `inner`
/// against the keeper's role and has the Safe make it. With
/// `should_revert`, a call the Safe makes and that fails reverts the whole
/// transaction. See [`EXEC_TRANSACTION_WITH_ROLE`]: not yet verified on a
/// fork.
pub fn exec_transaction_with_role(
    roles: Address,
    inner: &Call,
    role_key: B256,
    should_revert: bool,
) -> Call {
    let tokens = [
        Token::Word(abi::address(inner.to)),
        Token::Word(abi::uint(inner.value)),
        Token::Bytes(inner.data.clone()),
        Token::Word(abi::uint8(inner.operation as u8)),
        Token::Word(abi::bytes32(role_key)),
        Token::Word(abi::boolean(should_revert)),
    ];
    Call::plain(
        roles,
        abi::encode_call(abi::selector(EXEC_TRANSACTION_WITH_ROLE), &tokens),
    )
}

pub fn decode_exec_transaction_with_role(data: &[u8]) -> Result<RolesCall, AbiError> {
    let d = Decoder::new(data, EXEC_TRANSACTION_WITH_ROLE)?;
    let operation_value = d.uint8(3)?;
    let operation = Operation::from_u8(operation_value).ok_or(AbiError::Invalid {
        index: 3,
        kind: "operation (0 or 1)",
        found: operation_value.to_string(),
    })?;
    Ok(RolesCall {
        inner: Call {
            to: d.address(0)?,
            value: d.uint(1)?,
            data: d.bytes(2)?,
            operation,
        },
        role_key: d.bytes32(4)?,
        should_revert: d.boolean(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const MANAGER: Address = address!("00000000000000000000000000000000000000a1");
    const ROUTER: Address = address!("00000000000000000000000000000000000000a2");
    const SAFE: Address = address!("00000000000000000000000000000000000000a3");
    const TOKEN0: Address = address!("0000000000000000000000000000000000000010");
    const TOKEN1: Address = address!("0000000000000000000000000000000000000020");

    fn word(data: &[u8], index: usize) -> &[u8] {
        &data[4 + 32 * index..4 + 32 * (index + 1)]
    }

    fn u(n: u128) -> U256 {
        U256::from(n)
    }

    #[test]
    fn the_selectors_are_the_ones_the_contracts_answer_to() {
        // Computed from the signatures, and checked against the selectors
        // known from the deployed contracts' verified sources.
        assert_eq!(hex::encode(abi::selector(MINT)), "88316456");
        assert_eq!(hex::encode(abi::selector(INCREASE_LIQUIDITY)), "219f5d17");
        assert_eq!(hex::encode(abi::selector(DECREASE_LIQUIDITY)), "0c49ccbe");
        assert_eq!(hex::encode(abi::selector(COLLECT)), "fc6f7865");
        assert_eq!(hex::encode(abi::selector(BURN)), "42966c68");
        assert_eq!(hex::encode(abi::selector(EXACT_INPUT_SINGLE)), "414bf389");
        assert_eq!(hex::encode(abi::selector(APPROVE)), "095ea7b3");
        assert_eq!(hex::encode(abi::selector(MULTI_SEND)), "8d80ff0a");
    }

    #[test]
    fn a_mint_is_written_word_by_word_and_reads_back() {
        let p = MintParams {
            token0: TOKEN0,
            token1: TOKEN1,
            fee: 500,
            tick_lower: -200_100,
            tick_upper: -199_500,
            amount0_desired: U256::ZERO,
            amount1_desired: u(5_000_000_000_000_000_000),
            amount0_min: U256::ZERO,
            amount1_min: u(4_990_000_000_000_000_000),
            recipient: SAFE,
            deadline: u(1_800_000_000),
        };
        let call = mint(MANAGER, &p).unwrap();
        assert_eq!(call.to, MANAGER);
        assert_eq!(call.operation, Operation::Call);
        assert_eq!(call.value, U256::ZERO);
        assert_eq!(call.data.len(), 4 + 11 * 32);
        assert_eq!(&call.data[..4], &abi::selector(MINT));
        assert_eq!(word(&call.data, 0), &abi::address(TOKEN0));
        assert_eq!(word(&call.data, 2), &abi::uint24(500).unwrap());
        assert_eq!(word(&call.data, 3), &abi::int24(-200_100).unwrap());
        assert_eq!(word(&call.data, 9), &abi::address(SAFE));
        assert_eq!(decode_mint(&call.data).unwrap(), p);

        let too_wide = MintParams {
            tick_lower: -9_000_000,
            ..p.clone()
        };
        assert!(mint(MANAGER, &too_wide).is_err());
        let bad_fee = MintParams { fee: 1 << 24, ..p };
        assert!(mint(MANAGER, &bad_fee).is_err());
    }

    #[test]
    fn increase_decrease_collect_and_burn_read_back() {
        let inc = IncreaseParams {
            token_id: u(1234),
            amount0_desired: u(1),
            amount1_desired: u(2),
            amount0_min: u(3),
            amount1_min: u(4),
            deadline: u(5),
        };
        let call = increase_liquidity(MANAGER, &inc);
        assert_eq!(call.data.len(), 4 + 6 * 32);
        assert_eq!(decode_increase_liquidity(&call.data).unwrap(), inc);

        let dec = DecreaseParams {
            token_id: u(1234),
            liquidity: u128::MAX,
            amount0_min: u(7),
            amount1_min: u(8),
            deadline: u(9),
        };
        let call = decrease_liquidity(MANAGER, &dec);
        assert_eq!(word(&call.data, 1), &abi::uint128(u128::MAX));
        assert_eq!(decode_decrease_liquidity(&call.data).unwrap(), dec);

        let col = CollectParams {
            token_id: u(1234),
            recipient: SAFE,
            amount0_max: u128::MAX,
            amount1_max: u128::MAX,
        };
        let call = collect(MANAGER, &col);
        assert_eq!(word(&call.data, 1), &abi::address(SAFE));
        assert_eq!(decode_collect(&call.data).unwrap(), col);

        let call = burn(MANAGER, u(1234));
        assert_eq!(call.data.len(), 4 + 32);
        assert_eq!(decode_burn(&call.data).unwrap(), u(1234));
        assert!(decode_mint(&call.data).is_err(), "a burn is not a mint");
    }

    #[test]
    fn a_swap_and_an_approval_read_back_and_a_price_limit_must_fit_160_bits() {
        let p = ExactInputSingleParams {
            token_in: TOKEN1,
            token_out: TOKEN0,
            fee: 3000,
            recipient: SAFE,
            deadline: u(10),
            amount_in: u(1_000_000),
            amount_out_minimum: u(990_000),
            sqrt_price_limit_x96: U256::from(1u8) << 100,
        };
        let call = exact_input_single(ROUTER, &p).unwrap();
        assert_eq!(call.to, ROUTER);
        assert_eq!(call.data.len(), 4 + 8 * 32);
        assert_eq!(word(&call.data, 3), &abi::address(SAFE));
        assert_eq!(decode_exact_input_single(&call.data).unwrap(), p);
        let wide = ExactInputSingleParams {
            sqrt_price_limit_x96: U256::from(1u8) << 160,
            ..p
        };
        assert!(exact_input_single(ROUTER, &wide).is_err());

        let call = approve(TOKEN0, MANAGER, u(77));
        assert_eq!(call.to, TOKEN0);
        assert_eq!(decode_approve(&call.data).unwrap(), (MANAGER, u(77)));
    }

    #[test]
    fn a_batch_packs_each_call_without_padding_and_reads_back() {
        let first = approve(TOKEN0, MANAGER, u(1));
        let second = burn(MANAGER, u(2));
        let packed = pack_transactions(&[first.clone(), second.clone()]).unwrap();
        // 85 bytes of header a call, then its data.
        assert_eq!(packed.len(), 85 + first.data.len() + 85 + second.data.len());
        assert_eq!(packed[0], 0, "a plain call");
        assert_eq!(&packed[1..21], TOKEN0.as_slice());
        assert_eq!(U256::from_be_slice(&packed[21..53]), U256::ZERO);
        assert_eq!(
            U256::from_be_slice(&packed[53..85]),
            U256::from(first.data.len())
        );
        assert_eq!(&packed[85..85 + first.data.len()], &first.data[..]);
        assert_eq!(
            unpack_transactions(&packed).unwrap(),
            vec![first.clone(), second.clone()]
        );

        let multisend = address!("00000000000000000000000000000000000000a4");
        let batch = multi_send(multisend, &[first.clone(), second.clone()]).unwrap();
        assert_eq!(batch.to, multisend);
        assert_eq!(batch.operation, Operation::DelegateCall);
        assert_eq!(&batch.data[..4], &abi::selector(MULTI_SEND));
        assert_eq!(decode_multi_send(&batch.data).unwrap(), vec![first, second]);

        assert_eq!(multi_send(multisend, &[]), Err(BatchError::Empty));
        let inner_delegate = Call {
            operation: Operation::DelegateCall,
            ..burn(MANAGER, u(3))
        };
        assert_eq!(
            multi_send(multisend, &[inner_delegate]),
            Err(BatchError::DelegateInside { index: 0 })
        );
        assert!(unpack_transactions(&packed[..90]).is_err());
    }

    #[test]
    fn a_roles_call_wraps_the_batch_and_reads_back() {
        let multisend = address!("00000000000000000000000000000000000000a4");
        let roles = address!("00000000000000000000000000000000000000a5");
        let batch = multi_send(multisend, &[burn(MANAGER, u(2))]).unwrap();
        let key = B256::repeat_byte(0x42);
        let call = exec_transaction_with_role(roles, &batch, key, true);
        assert_eq!(call.to, roles);
        assert_eq!(call.operation, Operation::Call);
        assert_eq!(&call.data[..4], &abi::selector(EXEC_TRANSACTION_WITH_ROLE));
        // The bytes come after the six head words.
        assert_eq!(U256::from_be_slice(word(&call.data, 2)), U256::from(6 * 32));
        assert_eq!(word(&call.data, 3), &abi::uint8(1));
        let back = decode_exec_transaction_with_role(&call.data).unwrap();
        assert_eq!(
            back,
            RolesCall {
                inner: batch,
                role_key: key,
                should_revert: true
            }
        );
    }

    #[test]
    fn a_call_reads_back_from_json() {
        let call = approve(TOKEN0, MANAGER, u(5));
        let text = serde_json::to_string(&call).unwrap();
        assert!(text.contains("\"operation\":\"call\""), "{text}");
        assert!(text.contains("\"data\":\"0x095ea7b3"), "{text}");
        assert_eq!(serde_json::from_str::<Call>(&text).unwrap(), call);
        assert_eq!(Operation::from_u8(1), Some(Operation::DelegateCall));
        assert_eq!(Operation::from_u8(2), None);
    }
}
