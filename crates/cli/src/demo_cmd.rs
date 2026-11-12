//! `lpbot demo-data`: a simulated pool history, to try the pipeline
//! before the node is synced.

use std::path::Path;

use alloy_primitives::{address, Address};
use anyhow::{anyhow, bail, Result};
use lpbot_data::backfill::PoolRef;
use lpbot_data::lake::stored_pools;
use lpbot_data::synth::{generate, write_history, SynthConfig};

use crate::chain_cmd::Tokens;

/// Address the simulated pool is stored under. No contract lives there.
pub const DEMO_POOL: Address = address!("5157000000000000000000000000000000000001");

/// The simulated pool: USDC and WETH from the config, in address order.
pub fn demo_pool(tokens: &Tokens) -> Result<PoolRef> {
    let token = |symbol: &str| {
        tokens
            .get(symbol)
            .map(|(address, _)| *address)
            .ok_or_else(|| anyhow!("the demo pool needs {symbol} in the universe"))
    };
    let (usdc, weth) = (token("USDC")?, token("WETH")?);
    Ok(PoolRef {
        address: DEMO_POOL,
        token0: usdc.min(weth),
        token1: usdc.max(weth),
    })
}

pub fn run(root: &Path, chain_name: &str, tokens: &Tokens, days: u32, seed: u64) -> Result<()> {
    if days == 0 {
        bail!("--days must be at least 1");
    }
    // Made-up history must never sit beside history from the chain.
    if stored_pools(root, chain_name)?
        .iter()
        .any(|pool| *pool != DEMO_POOL)
    {
        bail!(
            "{} already holds pool history from a chain; pick another --data-dir for demo data",
            root.display()
        );
    }
    let pool = demo_pool(tokens)?;
    let config = SynthConfig {
        seed,
        days,
        ..Default::default()
    };
    let history = generate(&config, pool).map_err(|e| anyhow!("simulated pool failed: {e}"))?;
    write_history(root, chain_name, &history)?;

    let folder = root.display();
    println!(
        "wrote {} simulated events over {days} days ({} to {}) under {folder}",
        history.events.len(),
        config.start,
        history.last_day,
    );
    println!("pool address: {DEMO_POOL}");
    println!();
    println!("The price is made up: use it to try the commands, not to judge a strategy.");
    println!("  lpbot rank --data-dir {folder}");
    println!("  lpbot zones {DEMO_POOL} --data-dir {folder} --days 7,30");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_data::day::Day;
    use lpbot_data::store::{DayState, EventStore};

    fn tokens() -> Tokens {
        let mut tokens = Tokens::new();
        tokens.insert(
            "WETH".into(),
            (address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), 18),
        );
        tokens.insert(
            "USDC".into(),
            (address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), 6),
        );
        tokens
    }

    fn temp(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("lpbot-demo-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn the_demo_pool_orders_its_tokens_as_the_chain_would() {
        let pool = demo_pool(&tokens()).unwrap();
        assert_eq!(pool.token0, tokens()["USDC"].0);
        assert_eq!(pool.token1, tokens()["WETH"].0);
        assert!(demo_pool(&Tokens::new()).is_err());
    }

    #[test]
    fn demo_data_is_written_whole_and_never_beside_real_history() {
        let root = temp("write");
        run(&root, "ethereum", &tokens(), 2, 7).unwrap();
        let days = EventStore::new(&root).days("ethereum", DEMO_POOL).unwrap();
        assert_eq!(
            days,
            vec![
                (Day(20_454), DayState::Complete),
                (Day(20_455), DayState::Complete)
            ]
        );
        // Writing again over demo data is fine.
        run(&root, "ethereum", &tokens(), 2, 7).unwrap();

        // A folder with another pool's history is refused.
        EventStore::new(&root)
            .write_day("ethereum", Address::repeat_byte(9), Day(20_454), &[], true)
            .unwrap();
        let error = run(&root, "ethereum", &tokens(), 2, 7).unwrap_err();
        assert!(error.to_string().contains("another --data-dir"), "{error}");
        assert!(run(&root, "ethereum", &tokens(), 0, 7).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
