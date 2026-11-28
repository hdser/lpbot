# Research log

What backtests on real history have shown, newest first. Each entry names the experiment files, so every figure can be rerun: `cargo run --release -p lpbot -- run <file>`, then `lpbot load-results` to see the runs in Grafana. All results here are in-sample unless an entry says otherwise.

## 27 November 2026: the long timeframe decides whether, the short one when

**The question.** Round 4's one family that came near holding up waits for ETH to move 3% to 5% within four hours and provides on the side that bets on the move stalling. It knows nothing of the weeks before the move. Does asking the long timeframe first, and entering on the short one only when it agrees, help? The engine got three long-timeframe gates (`regime.*`: the trend over a window, how straight price went, where it sits in its band) and a trend exit, and `lpbot select walk-forward` now does the out-of-sample scoring that round 4 did by hand.

**The regime grid** (`regime-usd-*`, `regime-eth-*`, 624 runs, `runs/regime`): the anchored family as it was (600 ticks next to the price, left in place, a 3% or 5% move within four hours on a five-minute look, out after one, three or seven days, at a 5% loss or on the first round trip, a day's wait after each campaign), on USDC/WETH and WETH/USDT at 0.05% and 0.3%, May 2025 to October 2026, gas at each block's price, under thirteen regimes: no gate; ETH up (or, for the ETH side, down) on the week, the month, the quarter; the month's move at least 10% the right way, or no worse than 10% the wrong way; the opposite trend as a check; a week or a month going sideways; price in the lower or upper half of the month's band; and two combinations.

**Dollars after ETH fell, by regime, in sample.** The average of six runs a pool (two move sizes, three time limits), in dollars against holding them; in brackets the campaigns a run made.

| Regime | USDC/WETH 0.05% | USDC/WETH 0.3% | WETH/USDT 0.05% | WETH/USDT 0.3% | All | Runs above zero |
| --- | --- | --- | --- | --- | --- | --- |
| No gate | −7.5% (21) | +2.6% (17) | +6.7% (22) | +6.4% (18) | +2.1% | 16 of 24 |
| Up at least 10% on the month | +3.3% (7) | +4.4% (5) | +2.7% (8) | +5.4% (7) | +4.0% | 19 of 24 |
| Upper half of the month's band | +2.3% (14) | +5.5% (11) | +1.5% (15) | +6.1% (12) | +3.8% | 13 of 24 |
| No worse than −10% on the month | +1.2% (12) | +3.0% (9) | +0.4% (13) | +3.9% (11) | +2.1% | 15 of 24 |
| Up on the quarter | +1.4% (8) | +2.7% (6) | −0.2% (9) | +4.0% (7) | +2.0% | 13 of 24 |
| A month going sideways | +0.3% (9) | +1.9% (7) | +0.2% (9) | +3.0% (8) | +1.4% | 13 of 24 |
| Up on the month | +0.2% (10) | +1.3% (7) | −0.2% (10) | +2.2% (9) | +0.9% | 14 of 24 |
| Down on the month (the check) | −7.7% (12) | +1.4% (10) | +4.6% (12) | +4.0% (9) | +0.6% | 16 of 24 |
| Up on the week | −2.3% (13) | +0.3% (10) | −0.3% (13) | +1.1% (11) | −0.3% | 11 of 24 |
| Lower half of the month's band | −8.8% (9) | −1.1% (8) | +6.3% (8) | +1.5% (8) | −0.5% | 14 of 24 |

Two gates were positive on all four pools, where the ungated family lost 7.5% on the busiest one: the month up at least 10%, and price in the upper half of the month's band. Both say the same thing: buy the dip of a market that is high and rising, not the dip of one already falling. They also trade a third to two thirds as often, five to fifteen campaigns in eighteen months, which is little to stand on. The lower half of the band, buying where price has already fallen, is the worst gate.

**Out of sample** (`lpbot select walk-forward --results-dir runs/regime --experiment regime-usd`, months of 30 days, scored on the last twelve, the first six only fitted on):

- *Picking a single setting* (78 a pool) by what made most over the last three months, the last six, or everything so far: 3 of 12 picks held up, all three on USDC/WETH 0.3%; the others lost 2% to 13% where the average setting made −2.5% to +1.5%. As in round 4.
- *Picking a regime* (`--by regime`, each judged on the average of its six runs), pool by pool: 1 of 12 held up.
- *Picking a regime for all four pools at once* (`--across-pools`, 24 runs a regime): the best of the last six months made +0.9% and the best so far +0.4% over the twelve months, against −0.1% for the average regime and +1.1% for the best with hindsight; the best of the last three made nothing. Over those twelve months the regimes ranked, out of sample: up 10% on the month +1.1%, no worse than −10% +1.0%, a rising month with a sideways week +0.9%, up on the month +0.7%, a month sideways +0.6%, upper half of the band +0.5%; no gate −0.8%; lower half of the band −1.1%, up on the week −1.2%, a week sideways −1.5%, and down on the month, the check against the thesis, last at −1.5%.

So the order is what the thesis says: with the long trend at the top, against it at the bottom, no gate in between, in sample and in the twelve months no gate was fitted on. The size is small, about a point a year over holding dollars and two over the ungated family, in a year when ETH fell 24%. It is the first choice of settings in this log that held up at all out of sample, and only when the choice is among regimes, pooled over the pools; choosing among single runs is still noise.

**ETH after it rose** (`regime-eth-*`): no. Over the last seventeen months the average regime lost 2.5% in ETH and the ungated family 6.1%. Two regimes ended above zero, both asking for a sideways week (+1.7%, and +2.4% with a falling month); a falling month alone lost 5.1%. Out of sample, picking a regime for all pools held up for one way of picking in three, at +0.2%. As in round 4, the ETH side does not work, with or without a long timeframe.

**The frozen rules, forward** (`forward-regime-usdc`, `forward-regime-usdt`, 28 runs: a 3% fall, three days, seven regimes, four pools, settings fixed on history to 31 October). Over 1 to 19 November two runs made one campaign each, +0.9% on WETH/USDT 0.05% (no gate, and down on the month); the other 26 saw no entry. `lpbot select ladder` lists all 28 as needing more days and more campaigns. Nineteen days say nothing yet.

**Agents on a mandate, practice** (`agent-mandates-practice`, USDC/WETH 0.05%, $10,000 in USDC, five days of October 2026, after the models' training data; each agent alone, about 40 looks at most). The three agents on the `trend` mandate:

| Agent | Looks | Campaigns | Result | Fees | Edge over cash a decision, over a day | Decisions that were the best, over its next look |
| --- | --- | --- | --- | --- | --- | --- |
| Haiku 5.5 | 31 | 3 | −1.4% | $49 | −0.29% | 25 of 31 |
| Sonnet 5.5 | 15 | 6 | −5.2% | $169 | −0.54% | 7 of 15 |
| Opus 5.5 | 19 | 5 | −6.1% | $172 | −0.48% | 7 of 19 |

All three lost to holding dollars. The quarter was up 30% to 40% and the month about flat, so the mandate allowed ranges below the price; ETH then fell by several percent twice within the five days, through the ranges, and each agent sold what the range had bought once the 30-day line turned, near the low, before the bounce. Sonnet and Opus kept ranges next to the price and earned 1.7% in fees against 7% to 8% lost that way; Haiku stayed out more and lost less. Their own reports name the same two faults: the range was too close to the price for a day's volatility of 3% to 4%, and one new 30-day low is not a turn of the trend. Three agents, five days and one market: this shows the panel, the scoring and the mandate text working end to end, not which model is better. The three Sonnet agents on the other mandates (none, range, fade) did not run: the model's safeguards stopped each session at its first message, so the comparison of mandates is still to do.

**The `trend` mandate rewritten** (28 November 2026). The agents had read "the long timeframe turned" off the 30-day change, which sat near zero: a 4% day took it from +1% to −3%, and they sold. The mandate now says what clearly against the trend means, in figures a day's move cannot reach: the 90-day change against the agent's side, or the 30-day change against it by more than three times that line's daily volatility. A range is at least three times the day's volatility wide; one that price has moved into is held through the pullback and given up only on that test; the exit is the round trip; a resting range is not moved after the price. The same five days again:

| Agent | Looks | Campaigns | Result | Largest drawdown | Fees | Edge over cash a decision, over a day | Decisions that were the best, over its next look |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Haiku 5.5 | 30 | 3 | +1.1% | 1.9% | $114 | +0.21% | 19 of 30 |
| Opus 5.5 | 25 | 3 | +0.9% | 2.3% | $138 | +0.15% | 15 of 25 |

Both now did what the mandate says: ranges 4% to 9% wide entered after a fall had slowed, no range moved after the price, every exit a round trip, and the position held through the 6% day that had made all three sell. Read this as the wording being followed, not as a result. These are the five days the wording was written on, and they ended with the bounce: holding through is exactly what pays when price comes back and what loses most when it does not, with only the test and the run's loss limit behind it (20% in the practice file). The 30-day change reached −6.1% against a line of −7.1% at the low. The Sonnet agent did not run this time either (the safeguards again), so there is no third row. The agents' own notes: closing after a round trip of a few hundredths of a percent is churn, and five days are too short for a 90-day test to mean anything.

**Five wordings of the trend mandate, on four weeks they were not written on** (28 November 2026; `agent-wording-a` to `-d`, USDC/WETH 0.05%, $10,000 in USDC, Haiku 5.5, each agent alone, about 40 looks a week). The wordings are files in `crates/cli/mandates/`: `trend-v1` (the first text, "give a position up when the long timeframe turns"), `trend` (the rewrite above), `trend-stop` (the rewrite, plus giving a filled range up once price is beyond its far edge by more than the range's width), `trend-steps` (the rewrite as a numbered procedure) and `trend-why` (how a range earns and loses, with the agent naming its own two exit conditions before each entry), beside a model with no mandate. The weeks were fixed by rule before any run, from the Monday-to-Sunday weeks of September and October 2026 other than the one the rewrite was made on: the largest rise (14 September, +6.6%), the smallest distance between high and low (28 September), the largest fall (5 October, −7.1%, low −11.4%) and the most negative 30-day change at its start (26 October, −13.4%). Result against holding dollars, with the largest drawdown where there was one:

| Setup | Rise | Quiet | Fall | Month down | Sum |
| --- | --- | --- | --- | --- | --- |
| No mandate | 0.00% | 0.00% | 0.00% | 0.00% | 0.0% |
| `trend-v1` | +1.56% (3.1%) | +0.37% | −5.72% (8.0%) | 0.00% | −3.8% |
| `trend` | +1.19% (1.5%) | +0.10% | −2.92% (8.1%) | 0.00% | −1.6% |
| `trend-stop` | +0.88% (0.7%) | −0.01% | +0.97% (2.9%) | 0.00% | +1.8% |
| `trend-steps` | +0.91% (1.1%) | +0.35% | −4.09% (9.2%) | 0.00% | −2.8% |
| `trend-why` | +2.29% (2.2%) | −0.02% | −2.69% (6.1%) | 0.00% | −0.4% |

One run a cell, one model: this screens wordings, it does not rank them. What it shows:

- **Without a mandate the model never traded:** 137 looks over four weeks, every one `stay_out`. A wording is what makes it provide liquidity at all.
- **The test for a falling month works as written.** In the week that began with the month down 13% every wording stayed out at every look, the first text included, and missed nothing: the week ended lower after a spike.
- **The falling week is the one that tells them apart, and it is one week.** The first text sold three filled ranges once the 30-day change dipped under zero and lost most, as on the days it was rewritten for. The rewrite did not sell, and carried an 8% drawdown instead: its test for a falling month never fires in a week's slide inside a rising quarter, so it has no exit for the case that costs most. The procedure followed the same rule most literally and lost more. The agent that named its own conditions got out after the range was run through, a look late.
- **`trend-stop` did not win by its stop.** It entered two days later than the others, after the fall, and its stop never fired. Its figure says something about waiting for a fall to slow and nothing about the stop.
- **In the rising and the quiet week every wording that entered made a little**, more the longer it left a range alone: one range held all week made +2.3%, three round trips +0.9%.

What the agents found unclear, in most reports: "a fall has slowed" has no measure; a range's edges are printed in percent from the price now, so a range left alone seems to move from look to look; nothing says what to do with a resting range that price has left far above; "round trip" covers a touch of a few hundredths of a percent. The second is the look's wording, not the mandate's. A typo in the procedure ("show no a fall") ran in all four weeks and is fixed since.

### What follows

- The regime gate that held up is slow: five to fifteen campaigns in eighteen months. The forward runs need months, not weeks, before the ladder's gates mean anything; earlier history would say more, sooner.
- The next wording of the trend mandate: keep the test for a falling month, give a filled range that price has left an exit of its own (a distance or a time, in figures), say what "slowed" means, and say when a resting range is stale. Test it on weeks none of the five ran on, and on Opus as well as Haiku; do not tune it on these four. The other mandates (range, fade, patient) have not run at all.
- Print a position's range in a frame that does not move with the price (how far price is from each edge, and which side it is on), since every wording was harder to follow for it.
- The trend exit (`exit.trend_min_pct`) is built and tested and has not been swept on real history.
- Regimes on other families: the wide one-sided ETH ranges of round 2, and the agents' own entries scored by the regime they were made in.

## 20 November 2026, last: selective entries the way a person trades them, scored out of sample

**The question.** Trading by hand, in for hours or days, out with the value token back plus fees, makes money; can a rule do the same? Round 3 showed the entry decides it. This round builds the rule that way: one-sided in the value token, entered only after ETH moved, on the side that bets on the move stalling (dollars after a fall, ETH after a rise), looked for every five minutes, out after a time limit, at a stop, or (`exit.after_round_trips: 1`, new) as soon as a round trip brings it back wholly into the value token with its fees. Gas at each block's price. Every result is in the value token, against holding it; over May 2025 to October 2026 holding ETH made +25.6% in dollars and half and half +12.8%, and from February 2026 on ETH fell 24%.

**The selective grid** (`selective-*`, 2,304 runs on USDC/WETH and WETH/USDT, 0.05% and 0.3%): moves of 2%, 3% or 5% within one, four or twelve hours; ranges 120 or 600 ticks wide; one or three days; the round-trip exit or not; a 2% or 5% stop; a day's wait after each campaign or none. 339 of the 2,304 runs ended with more of their value token, 13.7% less on average; the best made +26.7% in dollars (WETH/USDT 0.05%). With dollars as the value token 288 of 1,152 ended ahead, with ETH 51. A 600-tick range averaged −6.2%, a 120-tick one −21.1%.

What it showed besides:

- **Re-entering the same move.** Looking every five minutes, the move signal stays true for hours after it fires, and without a wait the run went back into the same falling market after each exit and took the same stop again. `entry.cooldown` (new) holds it out for a while after a campaign: a day's wait raised the average by six points (−10.5% against −16.8%).
- **Following the price.** 57% to 65% of all campaigns ended by being moved: a resting range that price had left was withdrawn and entered again nearer the price, each time paying gas and entering closer to a move still under way. `entry.reposition: false` (new) leaves a range where it was placed until its time limit, stop or round trip, as an order that does not fill would be left.

**The anchored grid** (`anchored-*`, 576 runs): the settings the 0.3% pools favoured (moves of 3% or 5%, a day's wait, a 5% stop) with ranges left in place, 300 or 600 ticks wide, one, three or seven days.

| Dollars after ETH fell, 600 ticks | First nine months | Last nine months | Whole window | Runs ending with more dollars |
| --- | --- | --- | --- | --- |
| WETH/USDT 0.3% | +2.6% | +3.4% | +6.1% | 27 of 36 in the last nine months |
| WETH/USDT 0.05% | +0.5% | +4.4% | +4.9% | 34 of 36 |
| USDC/WETH 0.3% | +3.9% | −0.4% | +3.4% | 17 of 36 |
| USDC/WETH 0.05% | −6.0% | −0.3% | −6.3% | 11 of 36 |

(The averages of 36 runs each: three windows, two move sizes, three time limits, with and without the round-trip exit.) The worst drawdown was about 5%. ETH after a rise lost on every pool, −1% to −16% on average at 600 ticks, and on the WETH/USDT pools every one of its runs lost in the last nine months.

**Out of sample.** Three ways of choosing in advance, none of which made money:

- Month by month, the setting that did best over the three months before, or six, or everything since May 2025: on the selective grid's 0.3% pools it lost 11% to 41% in dollars (in ETH, from +3% to −35%); on round 2's fast-move family −1% to −29%, where the setting chosen with hindsight had made +8.0%.
- The first nine months to choose, the last nine to score: the best settings of the first half made 0% to −16% in the second.
- The dollar family's best value of each setting on the first half, pooled over the pools (a day, a 5% move within four hours, the round-trip exit, 600 ticks): −0.6%, −0.6%, +1.8% and −0.5% in the last nine months, below the family's own average there.

So: a family that waits for ETH to fall 3% to 5%, provides dollars in a wide range below the price for a day or so, and does it once per move, earned a few percent over holding dollars on three of four pools, at small drawdowns; no way of picking its settings in advance turned that into a reliable result, and the version that does the same with ETH does not work at all. That is weaker than what trading by hand is said to make. What a person does that these rules do not: chooses the level, not the nearest range; chooses when not to trade at all, beyond a move signal; and decides when to give up on a position that has not come back. The way to find out which of those matters is to replay a person's own positions (`lpbot lp-positions --account`), priced at the block, against holding.

### What follows

- Replay the owner's own positions, when available, and turn what they do into rules.
- Levels instead of the nearest range: enter at a level price has turned at before (the zone map has the passes), after the move.
- A larger window: eighteen months with one rise and one fall is little to choose settings on.

## 20 November 2026, later: gas block by block, intraday positions, and copying real providers

**How results are counted.** A run starts in its value token and is turned back into it at the end. Its result is the value token it ends with against what it started with: against simply holding that token outside the pool, after fees, gas, swaps and price moving through its ranges. A run that reaches the 50% loss limit stops and holds the value token to the end of the window. The same capital is also valued in dollars, which can be set against holding ETH or half ETH and half dollars over the same window: May 2025 to October 2026 holding ETH made +25.6% and half and half +12.8%; December 2025 to October 2026, −24.6% and −12.3%. A dollar run that lost 6.7% in those last months lost to holding dollars but did better than holding ETH or the mix, only because it held dollars while ETH fell.

**What changed in the engine.** The replay already applied every event in block order, accrued fees swap by swap and checked stops after every block. What was not at that grain: gas was a fixed 1 gwei, converted to dollars at the run's first price; the signals counted on ten-minute steps; zone lookbacks were whole days, scored once a day; and time in range was counted from hourly series rows. Now each action pays the base fee of its block plus the median tip of the last 20 blocks, from `lpbot gas-history` (every block since 31 January 2025, 4.7 million, read from the node's fee history), turned into the value token at the pool's price at that moment; signals, lookbacks and the look for an entry can each run down to one block; and each campaign counts its seconds in range block by block. Every figure below is at each block's gas price.

**What gas cost.** The fixed 1 gwei was neither high nor low throughout. The median block paid, base fee and tip together:

| Months | Median block, gwei | 95th percentile of base fee |
| --- | --- | --- |
| May to July 2025 | 1.3 to 1.9 | 6 to 7 |
| August to November 2025 | 0.2 to 0.5 | 1.8 to 2.8 |
| December 2025 to November 2026 | 0.05 to 0.3 | 0.2 to 2.1 |

The strategies paid more than the median block: they act most in the first months of the window, and at volatile hours, when everyone else does. The round 2 families paid 1.7 to 6.2 gwei on average per action; entries after a fast move within 15 minutes or an hour paid 9 to 62 gwei on average in 2025.

### 1. Round 2 again at each block's gas price (520 runs, `runs/chain`)

The ranking of the families did not change, and every family did a little worse, the dollar runs most: their gas had been understated twice, by the price per gas and by converting it at May 2025's ETH price.

| Family | Value token | Average, USD, at 1 gwei | At each block's price | Best, at each block's price | Gas, share of capital |
| --- | --- | --- | --- | --- | --- |
| After a fast move | ETH | +14.7% | +11.6% | +33.0% | 4.2% |
| One-sided, plain | ETH | +8.1% | +6.4% | +72.9% | 0.4% |
| Stops | ETH | −29.9% | −31.5% | +72.9% | 11.3% |
| Around the price | ETH | −34.4% | −35.2% | −7.9% | 3.8% |
| After a fast move | dollars | −13.9% | −17.3% | +8.0% | 6.0% |
| Leave on the return | dollars | −17.5% | −28.8% | −0.9% | 8.4% |
| One-sided, plain | dollars | −25.3% | −33.8% | −3.9% | 8.1% |
| Holding horizons | dollars | −38.0% | −40.1% | −10.2% | 21.0% |
| Stops | dollars | −39.3% | −41.9% | −6.7% | 21.9% |

The four fast-move settings that held up in round 2 (one-sided, leave after a day or at −2%, on the side that bets on the move stalling): dollars after a 3% fall in four hours made +8.0% on USDC/WETH 0.05% (+9.6% at 1 gwei) and −0.3% on WETH/USDT 0.3% (+1.3%); ETH after a 3% rise made +5.9% in ETH on USDC/WETH 0.05% (unchanged) and +2.5% on WETH/USDT 0.3% (+2.6%). They trade a few dozen times and paid 1 to 1.9 gwei on average. It is still the one family worth taking further.

Of the 520 runs, 37 ended with more of their value token than they started with: 32 with ETH, 5 with dollars. The ones that did either entered selectively (the fast-move family) or held wide one-sided ETH ranges through to the return of the price: 600 ticks on WETH/USDT 0.3% ended with 37.4% more ETH over 8 campaigns, at a worst drawdown of 54%, and the same leaving after seven days at most with 25.5% more ETH over 89 campaigns at 32%. Those sold ETH through the range near the 2025 top and bought it back lower: a bet on where ETH would go that earned fees on the way, not a yield.

### 2. Intraday positions (`intraday-hold-*`, `intraday-spikes-*`)

On the three busiest small-fee pools (USDC/WETH 0.05% and 0.01%, WETH/USDT 0.05%): one-sided ranges next to the price, 10, 30 or 100 ticks wide, ranked on the last hour and day, looked for every minute, closed back to the value token after 15 minutes, an hour or four hours, with or without a 0.5% stop (`hold`, 36 runs a pool); and the same entered only after ETH moved 1% or 2% within 15 minutes or an hour (`spikes`, 64 runs a pool).

- **May 2025 to October 2026, $10,000:** all 300 runs reached the 50% loss limit, half of them by the middle of June 2025. Gas did much of it at first: a campaign cost $10 in gas on average ($3 to $27) in the summer of 2025, and one entered after a fast move $47 ($12 to $277).
- **December 2025 to October 2026, the cheap months (`*-cheap-gas`), $10,000:** still every hold run at the loss limit. At well under a dollar a round trip, entering again at once after each close made 430 to 10,000 campaigns, and gas came to 7% to 47% of the capital.
- **The same at $100,000** (`--capital 100000`): gas fell to 1% to 49% of the capital and fees came to 1% to 72%, and no run made money: the best lost 6.7% (WETH/USDT 0.05%, 100 ticks, an hour), and 62 of 108 reached the loss limit. On USDC/WETH 0.05% fees came to 37% of the capital on average and the runs lost 49% on average: price moving through a range next to it, which the position buys going down and sells going up, cost more than everything the range earned.
- **Entering only after a fast move, the cheap months:** 188 of 192 runs at $10,000 reached the loss limit and none made money; at $100,000 none reached it and none made money either, the best losing 4.6% (dollars on USDC/WETH 0.01%, after a 2% rise in 15 minutes). These entries paid 7.6 gwei on average at $10,000 and 14 at $100,000, up to 66, in months when the median block paid 0.05 to 0.3: a fast move is when everyone else sends transactions too.

So the intraday play as written, in and out next to the price on a clock, loses before gas, not because of it. What decides it is the entry, not the cadence: the one entry that held up waits for a fast move and holds for a day.

### 3. Copying the providers revert.finance shows (`follow-revert-*`)

The owners of the top open positions on revert.finance's pool pages on 20 November 2026: 18 of the 21 had positions on the stored pools since May 2025, 771 stretches of liquidity in all (`lpbot lp-positions`). Two of them are intraday providers: one put about $173,000 into each 20-tick USDC/WETH 0.05% range (and 17 ticks on 0.01%) for a median of 51 minutes, 105 times; another about $110,000 into one or two tick spacings of WETH/USDT 0.05% for half an hour, 92 times. The rest hold ranges of 300 to 7,000 ticks for one to twenty days.

What they made, position by position, their own gas left out, priced at the pool's tick in the block each position opened and closed (from the stored swap events):

| Provider | Positions | Fees per position | Against holding what they put in | In dollars |
| --- | --- | --- | --- | --- |
| Intraday, USDC/WETH 0.05% | 105 | 0.13% | +0.01% (better on 50% of them) | −0.14% |
| Intraday, WETH/USDT 0.05% | 92 | 0.25% | −0.04% (48%) | −0.27% |
| All 17 with positions closed by 17 November, four pools | 659 | 1.09% | −0.27% (50%) | −0.56% |

Priced with the hourly ETH price instead, the intraday USDC/WETH provider looked better than holding by +0.15% a position on 67% of them: for a position of 50 minutes the hour's price moves as much as its edge, which is why the comparison is made at the block. At the block, none of these providers beat holding their own deposits by more than the noise, and most did worse.

Copied with $10,000, one value token, entering each of their positions in the block after theirs and closing in the block after they close (521 of their 659 positions; the others opened while one was being copied): the copies earned the same fees as the providers, position by position (correlation 0.98 to 0.996; 0.124% against 0.125% for the intraday USDC/WETH provider), which checks the engine's fee accounting from outside. But a copy enters from one token and leaves to it, so it swaps into each range's mix and back: about 0.1% a round trip in a 0.05% pool, plus 0.02% to 0.05% gas, more than the providers' edge over holding, which was none: the copies' dollar runs lost 0.71% a position where the providers lost 0.59%. The copy of the intraday USDC/WETH provider lost 20% in dollars over 77 positions; across all 27 copies the dollar runs lost 13% on average (6 above zero) and the ETH runs 0.4% (15 above zero, the best +31% from timing ETH's fall, not from fees).

### What follows

- Copying an intraday provider needs a run that keeps both tokens between positions and swaps only the difference, measured against holding the mix: the provider's way. It is not built.
- The fast-move entry, at real gas, walk-forward and on the other pools.
- Entries on a clock lose to price moving through the range; the next intraday tests should change the entry (after a move, at a level price keeps returning to), not the holding time.

## 20 November 2026: eighteen months, 520 runs, every way in and out

**Data.** The store now starts on 31 January 2025: a year more of the 24 pools, and every pool replays to its state on the chain. The runs trade May 2025 to October 2026 (zones look back up to 90 days before that). ETH went from about $1,800 at the start to a monthly average of $4,342 in September 2025, down to $1,695 in June 2026 and back to $2,252: holding ETH made +25.9% in dollars over the window, +66.1% to the end of December 2025 and −24.2% after. Holding half ETH and half dollars made +12.9%.

**Settings.** As in the first round: all capital in each campaign, a 50% loss limit so every run sees the window (a run that hits it shows −50% in its value token; with ETH as the value token that is −37.2% in dollars), gas at 1 gwei, the plain `wait` rule. Around the price and one-sided ran on four pools (USDC/WETH and WETH/USDT, 0.05% and 0.3%); the other families on USDC/WETH 0.05% and WETH/USDT 0.3%. Each family is one experiment file per pair: `around-*`, `one-sided-*`, `horizons-*`, `stops-*`, `return-exit-*`, `spikes-*`, `vol-width-*`.

| Family | Value token | Runs | Average, USD | Median, USD | Best | At the loss limit |
| --- | --- | --- | --- | --- | --- | --- |
| After a fast move (`spikes`) | ETH | 48 | +14.7% | +20.5% | +33.0% | 0 |
| One-sided, plain | ETH | 16 | +8.1% | +26.0% | +73.7% | 7 |
| Leave on the return (`return-exit`) | ETH | 32 | −24.1% | −37.2% | +73.7% | 27 |
| Holding horizons | ETH | 40 | −25.5% | −37.1% | +73.7% | 31 |
| Width from volatility | ETH | 16 | −27.4% | −37.2% | +60.2% | 14 |
| Stops | ETH | 72 | −29.9% | −37.2% | +73.7% | 52 |
| Around the price | ETH | 36 | −34.4% | −37.2% | −7.4% | 31 |
| After a fast move | dollars | 48 | −13.9% | −7.9% | +9.6% | 5 |
| Leave on the return | dollars | 32 | −17.5% | −17.1% | +2.7% | 3 |
| Width from volatility | dollars | 16 | −22.0% | −19.5% | −2.3% | 4 |
| One-sided, plain | dollars | 16 | −25.3% | −21.6% | +14.0% | 5 |
| Around the price | dollars | 36 | −32.1% | −30.5% | −4.8% | 13 |
| Holding horizons | dollars | 40 | −38.0% | −50.0% | −4.8% | 25 |
| Stops | dollars | 72 | −39.3% | −50.0% | +1.3% | 48 |

Every family's best ETH run is the same one: plain one-sided, 600 ticks wide, on WETH/USDT 0.3% (the families include the plain rule as their baseline).

### 1. Around the price: the loss of price leaving the range, realized

A range around the price earns from the first block and lost in every configuration: −34% and −32% on average in dollars. Moving it back around the price as soon as price leaves (`exit.recenter_after: 0`) makes the loss of the move permanent each time: a ±3% range on USDC/WETH 0.05% moved 132 times in six months of 2026, earned fees of 67% of its capital, and lost 31% in ETH. A day's grace changed nothing; never moving was the least bad for dollar runs (−16% against −40%). The fees a range earns per unit of capital and what each move locks in both grow as the range narrows, so width alone does not fix it: what decides it is the pool's fees against its volatility, and on these pools in this window volatility won.

### 2. Cutting losses only works with a selective entry

Every stop did worse than no stop: −30% to −48% in dollars against −12% to −13% without, with 170 to 490 campaigns instead of 4 to 43. Closing a campaign at a 1% or 3% loss and entering again at the next look puts the bot straight back into the same conditions, and it takes the same loss again. Shorter horizons were worse in steps (an hour: every run at the loss limit; then six hours, a day, a week; never closing was best), and leaving on the return after a 2% excursion was worse than after 10%, which was no different from waiting.

### 3. After a fast move: the one family that held up

Entering only after ETH moved 3% or 6% within four hours, in a range a day's volatility wide, leaving after six hours or a day or at a 2% loss, was the only family near or above its benchmark, with no run at the loss limit with ETH as the value token. The direction matters, and it is the one the owner suggested: provide liquidity on the side that bets on the move stalling.

| Setting (one-sided, leave after a day or at −2%) | Pool | In the value token | Worst drawdown | Campaigns | First half | Second half |
| --- | --- | --- | --- | --- | --- | --- |
| Dollars, after ETH fell 3% in four hours (buys the dip) | USDC/WETH 0.05% | +9.6% | 5% | 54 | +7.8% | +1.6% |
| Dollars, same | WETH/USDT 0.3% | +1.3% | 5% | 48 | +4.8% | −3.3% |
| ETH, after ETH rose 3% in four hours (sells the rip) | USDC/WETH 0.05% | +5.9% | 3% | 46 | +11.4% vs ETH | −0.7% vs ETH |
| ETH, same | WETH/USDT 0.3% | +2.6% | 5% | 40 | +7.9% vs ETH | −1.5% vs ETH |

(For ETH runs the halves are the run's dollar result less what holding ETH did.) Around the price after the same moves lost, as it did everywhere. Directions that bet on the move continuing (dollars after a rise, ETH after a fall) lost. These were chosen in sample among 96 settings; each trades a few dozen times in eighteen months, so a few campaigns more or less move the figures.

### 4. The rest

- One-sided wide ranges with ETH as the value token earned in ETH (600 ticks: +23% on average, +38% on WETH/USDT 0.3%), with drawdowns above 50%: they sold ETH on the way up in the summer of 2025 and held dollars through the fall. That is the trend bet of the first round, made with good timing.
- Ranges sized from the day's volatility did no better than fixed widths.
- How settings ranked in the first half said little about the second: within a family and value token the correlation between the halves ran from −0.7 to 0.96, positive in nine of the fourteen groups and negative for around the price, stops and dollar horizons. Almost every run lost in the second half, when ETH fell 24%.

### What follows

- Score the fast-move entries walk-forward (choose on one stretch, score the next) and on the other pools, with the bot's own latency, before trusting them.
- Gas is still at 1 gwei; the stop-heavy families would look worse at real gas prices.

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
