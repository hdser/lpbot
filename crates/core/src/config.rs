//! Typed view of a resolved run configuration.
//!
//! Only the parts the code already relies on are typed. Everything else is
//! carried through untouched in `extra`, so the YAML can grow ahead of the
//! code without breaking parsing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use thiserror::Error;

/// How a run is executed. `paper` is simulation mode: live data, virtual positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Backtest,
    Paper,
    Live,
}

/// How much the bot does without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    /// Everything is automatic.
    Auto,
    /// Any flip or close chosen by the patience decision needs approval.
    #[default]
    Approve,
}

/// Backtest window, as ISO dates (`YYYY-MM-DD`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub from: String,
    pub to: String,
}

/// Inputs given per run. Capital and the loss limit have no default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RunInputs {
    #[serde(default)]
    pub capital_usd: Option<f64>,
    /// Most the run may lose, in percent, counted in the value token.
    #[serde(default)]
    pub loss_limit_pct: Option<f64>,
    #[serde(default)]
    pub autonomy: Autonomy,
}

/// Minimums a pool must clear before it is ranked, in dollars.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FloorsConfig {
    /// Average daily volume.
    pub volume_usd_per_day: f64,
    /// Value within 1% of the price.
    pub in_range_liquidity_usd: f64,
}

/// Tokens the bot may hold. A pool qualifies only if both its tokens are listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Universe {
    pub tokens: Vec<String>,
    /// Tokens counted as one dollar when valuing volume and liquidity.
    #[serde(default)]
    pub usd_stables: Vec<String>,
    #[serde(default)]
    pub floors: Option<FloorsConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Universe {
    pub fn allows_pair(&self, token0: &str, token1: &str) -> bool {
        token0 != token1 && self.contains(token0) && self.contains(token1)
    }

    pub fn contains(&self, token: &str) -> bool {
        self.tokens.iter().any(|t| t == token)
    }
}

/// What to do when price is beyond the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatienceMode {
    /// Compare waiting, flipping and closing at each decision.
    #[default]
    Adaptive,
    /// Fixed rule: always wait.
    Wait,
    /// Fixed rule: always flip.
    Flip,
    /// Fixed rule: always close.
    Close,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PatienceConfig {
    #[serde(default)]
    pub mode: PatienceMode,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub id: String,
    pub kind: String,
    /// Token the strategy enters and exits in, and counts results in.
    pub value_token: String,
    #[serde(default)]
    pub patience: PatienceConfig,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// How to reach one chain's nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Your own node. Every read that feeds a decision goes here.
    pub local: String,
    /// Public endpoints, for cross-checks, exit-only fallback and backfill.
    #[serde(default)]
    pub public: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A token the bot may hold on a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenConfig {
    pub address: String,
    pub decimals: u8,
}

/// One chain: how to reach it and where its contracts are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainConfig {
    /// Expected chain id of the endpoints; a mismatch stops the command.
    #[serde(default)]
    pub chain_id: Option<u64>,
    pub rpc: RpcConfig,
    #[serde(default)]
    pub uniswap_v3_factory: Option<String>,
    /// Token symbol to address and decimals.
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A fully resolved run configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    #[serde(default)]
    pub window: Option<Window>,
    #[serde(default)]
    pub run: RunInputs,
    #[serde(default)]
    pub chains: BTreeMap<String, ChainConfig>,
    pub universe: Universe,
    pub strategies: Vec<StrategyConfig>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    #[error("run.capital_usd is required and must be greater than zero; the run refuses to start without it")]
    MissingCapital,
    #[error("run.loss_limit_pct is required and must be between 0 and 100; the run refuses to start without it")]
    MissingLossLimit,
    #[error("a backtest needs a window with `from` and `to` dates")]
    BacktestNeedsWindow,
    #[error("window dates must be YYYY-MM-DD with `from` before `to` (got from={from}, to={to})")]
    BadWindow { from: String, to: String },
    #[error("universe.tokens is empty")]
    EmptyUniverse,
    #[error("no strategy is configured")]
    NoStrategy,
    #[error("strategy `{strategy}` uses value token `{token}`, which is not in universe.tokens")]
    ValueTokenNotInUniverse { strategy: String, token: String },
    #[error("chain `{chain}` has no address for universe token `{token}`")]
    TokenWithoutAddress { chain: String, token: String },
}

impl Config {
    /// Checks everything that must hold before a run may start.
    /// Returns every problem found, not just the first.
    pub fn validate(&self) -> Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();

        match self.run.capital_usd {
            Some(c) if c.is_finite() && c > 0.0 => {}
            _ => errors.push(ConfigError::MissingCapital),
        }
        match self.run.loss_limit_pct {
            Some(l) if l.is_finite() && l > 0.0 && l <= 100.0 => {}
            _ => errors.push(ConfigError::MissingLossLimit),
        }

        match &self.window {
            None if self.mode == Mode::Backtest => errors.push(ConfigError::BacktestNeedsWindow),
            Some(w) if !(is_iso_date(&w.from) && is_iso_date(&w.to) && w.from < w.to) => {
                errors.push(ConfigError::BadWindow {
                    from: w.from.clone(),
                    to: w.to.clone(),
                });
            }
            _ => {}
        }

        if self.universe.tokens.is_empty() {
            errors.push(ConfigError::EmptyUniverse);
        }
        if self.strategies.is_empty() {
            errors.push(ConfigError::NoStrategy);
        }
        for s in &self.strategies {
            if !self.universe.contains(&s.value_token) {
                errors.push(ConfigError::ValueTokenNotInUniverse {
                    strategy: s.id.clone(),
                    token: s.value_token.clone(),
                });
            }
        }

        for (name, chain) in &self.chains {
            for token in &self.universe.tokens {
                if !chain.tokens.is_empty() && !chain.tokens.contains_key(token) {
                    errors.push(ConfigError::TokenWithoutAddress {
                        chain: name.clone(),
                        token: token.clone(),
                    });
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = s[5..7].parse().unwrap_or(0);
    let day: u32 = s[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        serde_yaml::from_str(
            r#"
mode: backtest
window: { from: 2026-05-01, to: 2026-10-31 }
run: { capital_usd: 10000, loss_limit_pct: 5, autonomy: auto }
universe: { tokens: [WETH, WBTC, USDC, USDT] }
strategies:
  - { id: s1, kind: zone_round_trip, value_token: WETH }
risk: { max_actions_per_day: 12 }
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_complete_config_is_valid_and_keeps_unknown_sections() {
        let c = base();
        assert_eq!(c.validate(), Ok(()));
        assert!(c.extra.contains_key("risk"));
        assert_eq!(c.strategies[0].patience.mode, PatienceMode::Adaptive);
    }

    #[test]
    fn capital_and_loss_limit_have_no_default() {
        let mut c = base();
        c.run.capital_usd = None;
        c.run.loss_limit_pct = None;
        let errors = c.validate().unwrap_err();
        assert!(errors.contains(&ConfigError::MissingCapital));
        assert!(errors.contains(&ConfigError::MissingLossLimit));
    }

    #[test]
    fn value_token_must_be_in_the_universe() {
        let mut c = base();
        c.strategies[0].value_token = "cbBTC".into();
        let errors = c.validate().unwrap_err();
        assert_eq!(
            errors,
            vec![ConfigError::ValueTokenNotInUniverse {
                strategy: "s1".into(),
                token: "cbBTC".into()
            }]
        );
    }

    #[test]
    fn backtest_needs_a_sane_window() {
        let mut c = base();
        c.window = None;
        assert!(c
            .validate()
            .unwrap_err()
            .contains(&ConfigError::BacktestNeedsWindow));

        c.window = Some(Window {
            from: "2026-10-31".into(),
            to: "2026-05-01".into(),
        });
        assert!(matches!(
            c.validate().unwrap_err()[0],
            ConfigError::BadWindow { .. }
        ));
    }

    #[test]
    fn every_universe_token_needs_an_address_on_a_chain_that_lists_tokens() {
        let mut c = base();
        c.chains = serde_yaml::from_str(
            r#"
ethereum:
  chain_id: 1
  rpc: { local: "http://127.0.0.1:8545", public: ["https://example.org"] }
  uniswap_v3_factory: "0x1F98431c8aD98523631AE4a59f267346ea31F984"
  tokens:
    WETH: { address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", decimals: 18 }
    USDC: { address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", decimals: 6 }
    USDT: { address: "0xdAC17F958D2ee523a2206206994597C13D831ec7", decimals: 6 }
"#,
        )
        .unwrap();
        assert_eq!(c.chains["ethereum"].chain_id, Some(1));
        assert_eq!(c.chains["ethereum"].tokens["USDC"].decimals, 6);
        assert_eq!(
            c.validate().unwrap_err(),
            vec![ConfigError::TokenWithoutAddress {
                chain: "ethereum".into(),
                token: "WBTC".into()
            }]
        );
    }

    #[test]
    fn universe_allows_only_listed_pairs() {
        let u = base().universe;
        assert!(u.allows_pair("WETH", "USDC"));
        assert!(u.allows_pair("WBTC", "WETH"));
        assert!(!u.allows_pair("WETH", "cbBTC"));
        assert!(!u.allows_pair("WETH", "WETH"));
    }
}
