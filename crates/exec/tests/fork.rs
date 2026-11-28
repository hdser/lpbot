//! The planner's calls against the deployed Uniswap contracts, on a local
//! fork of mainnet (`docs/PLAN.md`, M4). Ignored by default: it needs an
//! Anvil fork of the local node, such as
//!
//!   anvil --fork-url http://127.0.0.1:18545 --port 18547
//!   LPBOT_FORK_RPC=http://127.0.0.1:18547 cargo test -p lpbot-exec --test fork -- --ignored
//!
//! From Anvil's first development account, standing in for the Safe, it
//! mints a one-sided WETH range in USDC/WETH 0.05% through the planner's
//! calls, takes it out again and swaps, each sent by the submitter and
//! signed by the signer's key code. It checks what the contracts did
//! against the engine's own maths: the liquidity minted, the WETH taken and
//! given back, to the unit, and the swap's output against its simulation.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use lpbot_core::{Step, StepRecord};
use lpbot_exec::calls::Call;
use lpbot_exec::journal::Journal;
use lpbot_exec::planner::{plan, Allowances, OpenPosition, PlanContext};
use lpbot_exec::reconcile;
use lpbot_exec::submit::{submit, FeePolicy, Node, Order, RpcNode};
use lpbot_signer::{Eip1559Tx, Key};
use lpbot_venues::uniswap_v3::liquidity_amounts::{
    get_amounts_for_liquidity, get_liquidity_for_amount1,
};
use lpbot_venues::uniswap_v3::sqrt_price_math::get_amount1_delta;
use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

const POSITION_MANAGER: Address = address!("C36442b4a4522E871399CD717aBDD847Ab11FE88");
const ROUTER: Address = address!("E592427A0AEce92De3Edee1F18E0157C05861564");
const MULTISEND_CALL_ONLY: Address = address!("9641d764fc13c8B624c04430C7356C1C7C8102e2");
const POOL: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Anvil's first development account: a public test key.
const DEV_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

fn word(data: &[u8], i: usize) -> U256 {
    U256::from_be_slice(&data[i * 32..(i + 1) * 32])
}

fn call_data(selector: &str, args: &[U256]) -> Vec<u8> {
    let mut data = lpbot_exec::abi::selector(selector).to_vec();
    for a in args {
        data.extend_from_slice(&a.to_be_bytes::<32>());
    }
    data
}

fn address_word(a: Address) -> U256 {
    U256::from_be_slice(a.as_slice())
}

struct Fork<'a> {
    node: RpcNode<'a>,
    key: Key,
    me: Address,
    journal: Journal,
    sent: usize,
}

impl Fork<'_> {
    fn read(&self, to: Address, data: &[u8]) -> Vec<u8> {
        self.node.call(self.me, to, data).unwrap()
    }

    fn balance(&self, token: Address) -> U256 {
        word(
            &self.read(
                token,
                &call_data("balanceOf(address)", &[address_word(self.me)]),
            ),
            0,
        )
    }

    /// Sends `call` from the development account through the submitter.
    fn send(&mut self, call: &Call) {
        assert!(call.value.is_zero());
        self.sent += 1;
        let order = Order {
            id: format!("fork-{}", self.sent),
            chain_id: 1,
            from: self.me,
            to: call.to,
            data: Bytes::from(call.data.clone()),
            exit: false,
        };
        let policy = FeePolicy {
            priority_wei: 1_000_000_000,
            cap_wei: 500_000_000_000,
            exit_cap_wei: 500_000_000_000,
            bump_ppm: 125_000,
            wait_blocks: 2,
            gas_margin_ppm: 300_000,
        };
        let key = &self.key;
        let mut sign = |tx: &Eip1559Tx| key.sign(tx).map_err(anyhow::Error::from);
        let mut wait = || std::thread::sleep(std::time::Duration::from_millis(200));
        let landed = submit(
            &self.node,
            &mut sign,
            &order,
            &policy,
            &self.journal,
            &mut wait,
            50,
        )
        .unwrap_or_else(|e| panic!("{}: {e:#}", order.id));
        assert!(landed.success, "{} reverted", order.id);
    }
}

#[test]
#[ignore = "needs an Anvil fork of mainnet: LPBOT_FORK_RPC"]
fn the_planners_calls_do_on_the_deployed_contracts_what_the_engine_counted() {
    let url = std::env::var("LPBOT_FORK_RPC").expect("LPBOT_FORK_RPC");
    let client = lpbot_chain::RpcClient::new(url);
    let key = Key::from_secret(&hex::decode(DEV_KEY).unwrap()).unwrap();
    let me = key.address();
    let journal = Journal::new(
        std::env::temp_dir().join(format!("lpbot-fork-journal-{}.jsonl", std::process::id())),
    );
    let mut fork = Fork {
        node: RpcNode { client: &client },
        key,
        me,
        journal,
        sent: 0,
    };

    // The contracts the planner names are deployed where it says.
    for (name, at) in [
        ("position manager", POSITION_MANAGER),
        ("router", ROUTER),
        ("MultiSendCallOnly 1.4.1", MULTISEND_CALL_ONLY),
        ("pool", POOL),
    ] {
        let code = client
            .call("eth_getCode", serde_json::json!([at, "latest"]))
            .unwrap();
        assert!(
            code.as_str().is_some_and(|c| c.len() > 2),
            "no code for the {name} at {at}"
        );
    }

    // The pool now.
    let slot0 = fork.read(POOL, &lpbot_exec::abi::selector("slot0()"));
    let sqrt_price = word(&slot0, 0);
    let tick = i32::from_be_bytes(slot0[60..64].try_into().unwrap());
    let spacing = 10;

    // 2 WETH to work with.
    let wrap = Eip1559Tx {
        chain_id: 1,
        nonce: fork.node.next_nonce(me).unwrap(),
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 500_000_000_000,
        gas_limit: 100_000,
        to: WETH,
        value: U256::from(2u128 * 10u128.pow(18)),
        data: Bytes::from(lpbot_exec::abi::selector("deposit()").to_vec()),
    };
    let signed = fork.key.sign(&wrap).unwrap();
    fork.node.send_raw(&signed.raw).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let weth_start = fork.balance(WETH);
    assert!(weth_start >= U256::from(2u128 * 10u128.pow(18)));

    // The engine's mint: one WETH in a range just below the price (WETH is
    // token1, so a range below the price holds only WETH).
    let upper = tick.div_euclid(spacing) * spacing - spacing;
    let lower = upper - 600;
    let (sqrt_a, sqrt_b) = (
        get_sqrt_ratio_at_tick(lower).unwrap(),
        get_sqrt_ratio_at_tick(upper).unwrap(),
    );
    let amount = U256::from(10u128.pow(18));
    let liquidity = get_liquidity_for_amount1(sqrt_a, sqrt_b, amount).unwrap();
    let used = get_amount1_delta(sqrt_a, sqrt_b, liquidity, true).unwrap();
    let record = |action: &str, step: Step| StepRecord {
        block: 1,
        time: 1,
        campaign: 1,
        action: action.into(),
        sqrt_price_x96: sqrt_price,
        step,
    };
    let deadline = u64::MAX / 2;
    let mut ctx = PlanContext {
        chain_id: 1,
        safe: me,
        position_manager: POSITION_MANAGER,
        router: ROUTER,
        multisend: MULTISEND_CALL_ONLY,
        roles: Address::repeat_byte(0x77),
        role_key: B256::repeat_byte(1),
        token0: USDC,
        token1: WETH,
        fee: 500,
        tick_spacing: spacing,
        position: None,
        allowances: Allowances::default(),
        zero_first: Vec::new(),
        slippage_ppm: 1_000,
        price_limit_ppm: 10_000,
        deadline,
    };
    let opened = plan(
        &[record(
            "open",
            Step::Mint {
                tick_lower: lower,
                tick_upper: upper,
                liquidity,
                amount0: U256::ZERO,
                amount1: used,
            },
        )],
        &ctx,
    )
    .unwrap();
    for call in &opened.inner {
        fork.send(call);
    }
    // The position the contract made: the engine's liquidity, for exactly
    // the WETH the engine counted.
    let count = word(
        &fork.read(
            POSITION_MANAGER,
            &call_data("balanceOf(address)", &[address_word(me)]),
        ),
        0,
    );
    let token_id = word(
        &fork.read(
            POSITION_MANAGER,
            &call_data(
                "tokenOfOwnerByIndex(address,uint256)",
                &[address_word(me), count - U256::from(1u8)],
            ),
        ),
        0,
    );
    let position = fork.read(
        POSITION_MANAGER,
        &call_data("positions(uint256)", &[token_id]),
    );
    assert_eq!(
        word(&position, 7),
        U256::from(liquidity),
        "liquidity minted"
    );
    // Read back as the executor does: the newest NFT, the plan's position.
    assert_eq!(
        reconcile::newest_token(&fork.node, POSITION_MANAGER, me).unwrap(),
        Some(token_id)
    );
    let found = reconcile::read_position(&fork.node, POSITION_MANAGER, me, token_id).unwrap();
    assert_eq!(
        reconcile::compare(
            Some(found),
            &reconcile::Expected::Position {
                tick_lower: lower,
                tick_upper: upper,
                min_liquidity: liquidity,
            }
        ),
        reconcile::Outcome::Adopt(Some(found))
    );
    assert_eq!((found.token0, found.token1, found.fee), (USDC, WETH, 500));
    assert_eq!(
        weth_start - fork.balance(WETH),
        used,
        "WETH taken by the mint"
    );

    // The engine's burn of the whole position, at the price now (still
    // above the range): every WETH back, rounded down as the pool does.
    let (out0, out1) = get_amounts_for_liquidity(sqrt_price, sqrt_a, sqrt_b, liquidity).unwrap();
    assert!(out0.is_zero());
    ctx.position = Some(OpenPosition {
        token_id,
        tick_lower: lower,
        tick_upper: upper,
        liquidity,
    });
    ctx.allowances = opened.allowances_after;
    let before = fork.balance(WETH);
    let closed = plan(
        &[record(
            "withdraw",
            Step::Burn {
                tick_lower: lower,
                tick_upper: upper,
                liquidity,
                amount0: out0,
                amount1: out1,
                fees0: U256::ZERO,
                fees1: U256::ZERO,
            },
        )],
        &ctx,
    )
    .unwrap();
    assert!(closed.burns_position);
    for call in &closed.inner {
        fork.send(call);
    }
    assert_eq!(fork.balance(WETH) - before, out1, "WETH given back");
    // The NFT is burned: reading it back finds no position to adopt.
    assert!(reconcile::read_position(&fork.node, POSITION_MANAGER, me, token_id).is_err());
    assert_eq!(
        reconcile::balance(&fork.node, WETH, me).unwrap(),
        fork.balance(WETH)
    );

    // A swap of 0.1 WETH for USDC: the output simulated first, then sent
    // with the planner's bounds, lands exactly as simulated.
    ctx.position = None;
    let amount_in = U256::from(10u128.pow(17));
    let probe = plan(
        &[record(
            "close",
            Step::Swap {
                zero_for_one: false,
                amount_in,
                amount_out: U256::from(1u8),
            },
        )],
        &ctx,
    )
    .unwrap();
    for call in probe.inner.iter().filter(|c| c.to != ROUTER) {
        fork.send(call);
    }
    let swap = probe.inner.iter().find(|c| c.to == ROUTER).unwrap();
    let simulated = word(&fork.read(ROUTER, &swap.data), 0);
    assert!(simulated > U256::ZERO);
    ctx.allowances = probe.allowances_after;
    ctx.allowances.router1 = U256::MAX;
    let swapped = plan(
        &[record(
            "close",
            Step::Swap {
                zero_for_one: false,
                amount_in,
                amount_out: simulated,
            },
        )],
        &ctx,
    )
    .unwrap();
    let usdc_before = fork.balance(USDC);
    for call in swapped.inner.iter().filter(|c| c.to == ROUTER) {
        fork.send(call);
    }
    assert_eq!(
        fork.balance(USDC) - usdc_before,
        simulated,
        "USDC out of the swap"
    );
    let _ = std::fs::remove_file(fork.journal.path());
}
