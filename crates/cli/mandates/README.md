# Mandates

A mandate is the way of trading an agent is told to follow: a section of its brief (`lpbot agent brief`), chosen in the experiment file with `agent.mandate: <name>`. Each file here is one mandate, exactly as the agent reads it, except for the words in braces, which are written for the side the value token trades:

| In the file | With dollars as the value token | With the volatile token as the value token |
| --- | --- | --- |
| `{value}` | USDC or USDT | WETH or WBTC |
| `{one_sided}` | below the price, buying the volatile token with USDC as it dips | above the price, selling WETH as it rises |
| `{favoured}` | rising or holding | falling or holding |
| `{against}` | falling | rising |
| `{beyond_zero}` | below zero | above zero |
| `{entry_move}` | a fall | a rise |
| `{side}` | below | above |

| File | What it is |
| --- | --- |
| `trend.md` | Long timeframe decides whether, short decides when; a falling month is a test in figures; a filled range is held through a pullback |
| `trend-v1.md` | The first text of it, kept for comparison: "give a position up when the long timeframe turns" |
| `trend-stop.md` | `trend`, plus a way out of a filled range that price has left by more than its width |
| `trend-steps.md` | `trend` as a numbered procedure |
| `trend-why.md` | The reasons a range earns and loses; the agent names its own two exit conditions before each entry |
| `range.md` | Only in a sideways market, inside the band the month held |
| `fade.md` | Out by default; one entry after a fast move, betting it stalls |
| `patient.md` | Few, wide and slow |

`open`, or no mandate, leaves the agent to itself and has no file.

To add or change one: write the file, add its line to `MANDATE_TEXTS` in `crates/cli/src/agent_brief.rs`, and build. The binary carries the texts it was built with, so a run's wording cannot change under it; a wording that has run forward is not edited, a new one gets a new name. No dates in a mandate, and nothing the look does not show. `docs/RESEARCH.md` has how the wordings compared.
