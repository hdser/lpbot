//! Commands that talk to a node: `pools` and `verify-pool`.

use std::collections::BTreeMap;
use std::path::Path;

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Context, Result};
use lpbot_chain::uniswap_v3::{discover_pools, fetch_snapshot, verify_pool};
use lpbot_chain::RpcClient;
use lpbot_core::{ChainConfig, Config};
use lpbot_venues::uniswap_v3::pool::Pool;

/// Blocks to stay behind the head, so a reorg cannot change what was read.
const HEAD_MARGIN: u64 = 5;

/// Universe tokens of a chain: symbol to address and decimals.
pub type Tokens = BTreeMap<String, (Address, u8)>;

/// A chain's settings with its addresses parsed and checked.
pub struct Chain {
    pub name: String,
    pub client: RpcClient,
    pub factory: Address,
    pub tokens: Tokens,
}

/// Parses an address and insists on a correct checksum, so a mistyped
/// address in a config file cannot slip through.
pub fn checked_address(what: &str, text: &str) -> Result<Address> {
    Address::parse_checksummed(text, None)
        .map_err(|e| anyhow!("{what} `{text}` is not a valid checksummed address: {e}"))
}

pub fn load_base_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
}

/// Resolves a chain's addresses from the config, without touching the network.
pub fn chain_addresses(config: &Config, chain_name: &str) -> Result<(Address, Tokens)> {
    let chain = chain_config(config, chain_name)?;
    let factory_text = chain
        .uniswap_v3_factory
        .as_deref()
        .ok_or_else(|| anyhow!("chain `{chain_name}` has no uniswap_v3_factory"))?;
    let factory = checked_address("uniswap_v3_factory", factory_text)?;

    let mut tokens = BTreeMap::new();
    for symbol in &config.universe.tokens {
        let token = chain
            .tokens
            .get(symbol)
            .ok_or_else(|| anyhow!("chain `{chain_name}` has no address for token `{symbol}`"))?;
        tokens.insert(
            symbol.clone(),
            (
                checked_address(&format!("token {symbol}"), &token.address)?,
                token.decimals,
            ),
        );
    }
    Ok((factory, tokens))
}

fn chain_config<'a>(config: &'a Config, chain_name: &str) -> Result<&'a ChainConfig> {
    config.chains.get(chain_name).ok_or_else(|| {
        let known: Vec<&str> = config.chains.keys().map(String::as_str).collect();
        anyhow!(
            "chain `{chain_name}` is not in the config (known: {})",
            known.join(", ")
        )
    })
}

/// Connects to a chain's node and checks it is the chain the config expects.
pub fn connect(config_path: &Path, chain_name: &str, rpc_override: Option<&str>) -> Result<Chain> {
    let config = load_base_config(config_path)?;
    let chain = chain_config(&config, chain_name)?;
    let (factory, tokens) = chain_addresses(&config, chain_name)?;

    let url = rpc_override
        .map(str::to_string)
        .or_else(|| std::env::var("LPBOT_RPC").ok())
        .unwrap_or_else(|| chain.rpc.local.clone());
    let client = RpcClient::new(&url);

    let chain_id = client
        .chain_id()
        .with_context(|| format!("is the node running? Try `make status`. Endpoint: {url}"))?;
    if let Some(expected) = chain.chain_id {
        if chain_id != expected {
            bail!("{url} serves chain id {chain_id}, but `{chain_name}` expects {expected}");
        }
    }
    Ok(Chain {
        name: chain_name.to_string(),
        client,
        factory,
        tokens,
    })
}

fn symbol_of(chain: &Chain, address: Address) -> String {
    chain
        .tokens
        .iter()
        .find(|(_, (a, _))| *a == address)
        .map(|(s, _)| s.clone())
        .unwrap_or_else(|| format!("{address:#x}"))
}

/// Lists every Uniswap v3 pool whose two tokens are both in the universe.
pub fn pools(chain: &Chain) -> Result<()> {
    let block = chain.client.block_number()?.saturating_sub(HEAD_MARGIN);
    let addresses: Vec<Address> = chain.tokens.values().map(|(a, _)| *a).collect();
    let mut found = discover_pools(&chain.client, chain.factory, &addresses, block)?;
    found.sort_by_key(|p| {
        (
            symbol_of(chain, p.token0),
            symbol_of(chain, p.token1),
            p.fee_pips,
        )
    });

    println!("{} pools on {} at block {block}", found.len(), chain.name);
    println!();
    println!(
        "{:<12} {:>7}  {:<42}  {:>9}  {:>6}  in-range liquidity",
        "pair", "fee", "address", "tick", "ticks"
    );
    for pool in &found {
        let state = Pool::from_snapshot(fetch_snapshot(&chain.client, pool.address, block)?);
        println!(
            "{:<12} {:>6.2}%  {:<42}  {:>9}  {:>6}  {}",
            format!(
                "{}/{}",
                symbol_of(chain, pool.token0),
                symbol_of(chain, pool.token1)
            ),
            pool.fee_pips as f64 / 10_000.0,
            format!("{:#x}", pool.address),
            state.tick(),
            state.ticks().count(),
            state.liquidity(),
        );
    }
    Ok(())
}

/// Replays a pool's recent events and compares the replica with the chain.
pub fn verify(
    chain: &Chain,
    pool_text: &str,
    from_block: Option<u64>,
    to_block: Option<u64>,
    blocks: u64,
    log_chunk: u64,
) -> Result<bool> {
    let pool: Address = pool_text
        .parse()
        .map_err(|_| anyhow!("`{pool_text}` is not an address"))?;
    let head = chain.client.block_number()?;
    let to = to_block.unwrap_or_else(|| head.saturating_sub(HEAD_MARGIN));
    let from = from_block.unwrap_or_else(|| to.saturating_sub(blocks));
    if to > head {
        bail!("block {to} is ahead of the node's head {head}");
    }

    println!(
        "verifying pool {pool:#x} on {} from block {from} to {to}",
        chain.name
    );
    let report = verify_pool(&chain.client, pool, from, to, log_chunk, |step| {
        println!("  {step}");
    })
    .with_context(|| {
        "verification could not finish. If the error mentions missing state, the start \
         block is older than this node keeps; use a smaller --blocks or an archive node"
    })?;

    println!();
    println!(
        "replayed {} events ({} swaps, {} mints, {} burns); the pool has {} initialized ticks",
        report.events, report.swaps, report.mints, report.burns, report.ticks_at_end
    );
    if report.passed() {
        println!("PASS: the replica matches the chain exactly at block {to}");
    } else {
        println!(
            "FAIL: {} differences at block {to}",
            report.differences.len()
        );
        for difference in report.differences.iter().take(20) {
            println!("  {difference}");
        }
        if report.differences.len() > 20 {
            println!("  ... and {} more", report.differences.len() - 20);
        }
    }
    Ok(report.passed())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_file(relative: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative)
    }

    #[test]
    fn the_shipped_config_has_valid_checksummed_addresses_for_the_whole_universe() {
        let config = load_base_config(&repo_file("config/mainnet.yml")).unwrap();
        let (factory, tokens) = chain_addresses(&config, "ethereum").unwrap();
        assert_ne!(factory, Address::ZERO);
        let symbols: Vec<&str> = tokens.keys().map(String::as_str).collect();
        assert_eq!(symbols, ["USDC", "USDT", "WBTC", "WETH"]);
        assert_eq!(tokens["WETH"].1, 18);
        assert_eq!(tokens["WBTC"].1, 8);
        assert_eq!(tokens["USDC"].1, 6);
        assert_eq!(tokens["USDT"].1, 6);
        assert_eq!(config.chains["ethereum"].chain_id, Some(1));
    }

    #[test]
    fn a_mistyped_address_is_refused() {
        // One character changed: the checksum no longer matches.
        assert!(checked_address("token", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc3").is_err());
        assert!(checked_address("token", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").is_ok());
        assert!(checked_address("token", "not an address").is_err());
    }

    #[test]
    fn an_unknown_chain_names_the_known_ones() {
        let config = load_base_config(&repo_file("config/mainnet.yml")).unwrap();
        let err = chain_addresses(&config, "base").unwrap_err().to_string();
        assert!(err.contains("base") && err.contains("ethereum"), "{err}");
    }
}
