# Research log

What backtests on real history have shown, newest first. Each entry names the experiment files, so every figure can be rerun: `cargo run --release -p lpbot -- run <file>`, then `lpbot load-results` to see the runs in Grafana. All results here are in-sample unless an entry says otherwise.

## 19 November 2026: the first backtests on real history

**Data.** The 24 pools of the universe, 31 January to 18 November 2026, from the owner's archive node (13.9 million logs; every pool replays to its state on the chain). Most runs trade May to October 2026. ETH went from $2,258 down to $1,622 at the end of June (with a wick to about $1,520 on 6 June), up to $2,784 in mid-September and back to $2,254 at the end of October: flat over the window, with a 28% fall and a 72% rise inside it.

**Every run is counted in ETH and in dollars** (`result_pct`, `result_pct_usd`), with what simply holding the value token did in dollars (`hold_value_pct_usd`).

### 1. Why the first gates run lost

`width-and-patience-eth.yml` (16 runs, WETH/USDT 0.3%, ETH as value token): every run stopped at the 5% loss limit. Two settings made it quick: only a quarter of the capital goes into a campaign (`capital_share: 0.25`), so the deployed part could lose only about 20%; and the widths 10, 20 and 40 all round up to the pool's tick spacing of 60, so the sweep held two widths, three times over.

### 2. Scan: six pools, eight widths, two value tokens, three rules

`scan-usdc-weth.yml`, `scan-weth-usdt.yml` (288 runs; all capital in, 50% loss limit so every run sees the window).

| Rule | Value token | Median result, USD | Best | Runs at the loss limit | Median campaigns |
| --- | --- | --- | --- | --- | --- |
| close | ETH | −50.1% | −8.2% | 71% | 775 |
| close | dollars | −50.0% | −6.9% | 71% | 812 |
| flip | ETH | −50.1% | −2.5% | 71% | 404 |
| flip | dollars | −46.7% | −4.5% | 46% | 417 |
| wait | ETH | −4.8% | +6.5% | 0% | 37 |
| wait | dollars | −0.4% | +13.1% | 0% | 28 |

Exiting at every excursion (`close`) or chasing the price (`flip`) is churn: each cycle locks in a small loss, pays the pool fee on the closing swap and the gas of an open, a close and a swap (about $2 at 1 gwei; 800 cycles on $10,000 is 17% in gas alone).

### 3. Where results come from

Completed round trips earned in every pool: on WETH/USDT 0.3%, 16 of them made 24% of the capital in six months. What lost was one campaign per run: on 6 June ETH wicked to about $1,520 for an hour, every run that was flat then entered a range just above the wick (the wick itself counted as a crossing on the 7-day lookback), price went straight through it, and the position held dollars for 148 days. That one campaign cost more than all the round trips earned. `entry.min_passes_per_day` now lets a run require that a range has been crossed on the long lookback too.

The instant large losses of the `close` runs were not price moves: on WETH/USDT 0.01% with ranges of a few ticks, the cap on the share of the liquidity in the range let only $2–3 in, and each cycle's $2 of gas consumed it. `entry.min_gas_multiple` now refuses such entries.

### 4. What the conditions at entry say

Every one of the scan's 168,474 campaigns, with the pool's features in the hour it opened (from `lpbot pool-stats`): the day's realized volatility, trend, drift and volume over value held. The day's volatility against the range width separated results more than anything else:

| Day's volatility ÷ range width at entry | close: mean per campaign | wait: mean | wait: median |
| --- | --- | --- | --- |
| under 3.5 | −0.13% | +0.27% | +0.13% |
| 3.5 to 7 | −0.17% | −0.08% | +0.19% |
| 7 to 15 | −0.31% | −0.46% | +0.13% |
| 15 to 33 | −0.52% | −0.25% | +0.06% |
| over 33 | −2.82% | −2.57% | −0.79% |

Ranges of 20 ticks or less lost in every regime; ranges of 40 ticks and more with `wait` earned 0.6–0.8% a campaign when the day's volatility was under 1.9%. High volume relative to value held at entry went with larger losses; the trend measure (how much of the movement was net direction) showed no clear pattern.

### 5. Active management

`active-usdc-weth.yml`, `active-weth-usdt.yml` (192 runs): the plain `wait` rule against the new rules, all optional and off by default: an entry gate on the day's volatility in range widths (`entry.max_vol_to_width`), stepping aside to cash when it turns wild (`exit.max_vol_to_width`), a longest wait for a stranded position (`patience.max_wait`), and a minimum entry against its gas (`entry.min_gas_multiple: 100` in every run).

| Entry gate | Step aside | Longest wait | Average USD | Median USD | Best |
| --- | --- | --- | --- | --- | --- |
| 4 widths | 12 widths | none | −1.0% | −5.9% | +29.5% |
| off | off | none | −3.6% | −1.4% | +9.1% |
| any | any | 72h | −23% to −30% | −25% to −29% | +4.1% |

A longest wait of three days is the worst rule tried: it closes stranded positions near the bottom, then re-enters and does it again. Waiting matters. Stepping aside alone churns (back in at the next look); it only makes sense with an entry gate at or below it.

### 6. The value token, period by period: a directional bet that earns fees

The same rules on each half of the window (`split-*-h1.yml`, `split-*-h2.yml`, not out of sample) and on March and April, which none of the rules was chosen on (`unseen-*.yml`, zones looking back 7 and 30 days because the history starts on 31 January). Plain `wait`, averages over 8 runs each:

| Period | ETH held, in USD | ETH as value: in ETH | in USD | Dollars as value: in USD |
| --- | --- | --- | --- | --- |
| Mar–Apr (out of sample) | +16.5% | −6.5% | +9.0% | +3.9% |
| May–Jul | −17.6% | +2.9% | −15.2% | −18.4% |
| Aug–Oct | +21.2% | −15.7% | +2.1% | +6.5% |

A one-sided range is a limit order that earns fees while it fills. With dollars as the value token it sits below the price and buys ETH as ETH falls: long ETH after a fall, so it loses in a falling market and wins when the dips it bought come back. With ETH as the value token it is the mirror image: it sells ETH into a rise, so it gains ETH in a falling market and gives it up in a rally. Fees moved results by a few points; the trend against the value token moved them by 15 to 20. Over May to October the dollar side looked like the winner only because the window was a dip followed by a recovery.

The gates helped where they were chosen (dollars, August to October: +6.5% to +11.9%) and did not out of sample (March and April: 0.6 to 1 point worse). Under the design's rule they stay off by default; the plain `wait` rule ships.

### What follows

- A one-sided campaign carries the trend. Results should be read against holding the value token and against holding the other one, not against zero, and a strategy that wants to earn fees without the bet needs the entries that are not built yet: around the price, a ladder on both sides, or a hedge.
- The gates need more history to be fitted and scored walk-forward: the store starts on 31 January 2026, and one more year would give several regimes of each kind. With the batched backfill a year of the 24 pools takes about one to two hours on the node.
- Gas is charged at 1 gwei throughout; historical gas prices follow once block headers are stored.
- Much ETH-dollar volume has moved to Uniswap v4 pools, which the store does not cover.
