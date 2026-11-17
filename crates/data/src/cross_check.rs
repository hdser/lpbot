//! Cross-check: one stored day against the same day read from a second
//! source.
//!
//! The replay check shows that the stored events lead to the pool's state
//! on the chain. It cannot see a missing event that changes no state, such
//! as a Collect. This check compares a stored day log by log with what an
//! independent endpoint returns for the same blocks.

use std::collections::BTreeMap;

use lpbot_chain::eth::Log;
use lpbot_chain::uniswap_v3::ChainError;

use crate::backfill::{first_block_at_or_after, ChainSource};
use crate::day::Day;
use crate::store::{DayState, StoredLog};

/// Differences listed one by one before the rest is only counted.
const LISTED: usize = 10;

/// The newest day that is stored complete.
pub fn last_complete_day(days: &[(Day, DayState)]) -> Option<Day> {
    days.iter()
        .rev()
        .find(|(_, state)| *state == DayState::Complete)
        .map(|(day, _)| *day)
}

/// The first and the last block of a day, or `None` when the day has no
/// block or is not over yet at the source's head.
pub fn day_blocks(source: &impl ChainSource, day: Day) -> Result<Option<(u64, u64)>, ChainError> {
    let (head, _) = source.head()?;
    let first = first_block_at_or_after(source, day.start(), 0, head)?;
    let next = first_block_at_or_after(source, day.end(), first, head)?;
    // Past the head the next day has not begun, so this one may still grow.
    if next > head || first >= next {
        return Ok(None);
    }
    Ok(Some((first, next - 1)))
}

type Key = (u64, u64);

fn describe(key: Key) -> String {
    format!("block {} log {}", key.0, key.1)
}

/// How the stored logs differ from another source's logs for the same
/// blocks. Empty when the two agree log for log: the same logs, in the
/// same transactions, with the same topics and data.
pub fn differences(stored: &[StoredLog], other: &[Log]) -> Vec<String> {
    let ours: BTreeMap<Key, &StoredLog> = stored
        .iter()
        .map(|log| ((log.block_number, u64::from(log.log_index)), log))
        .collect();
    let theirs: BTreeMap<Key, &Log> = other
        .iter()
        .map(|log| ((log.block_number, log.log_index), log))
        .collect();

    let mut found = Vec::new();
    for (key, log) in &ours {
        match theirs.get(key) {
            None => found.push(format!(
                "{} is stored here but the other source does not have it",
                describe(*key)
            )),
            Some(their) => {
                if their.transaction_hash != log.tx_hash {
                    found.push(format!(
                        "{} is in another transaction there",
                        describe(*key)
                    ));
                } else if their.topics != log.topics || their.data != log.data {
                    found.push(format!("{} has other content there", describe(*key)));
                }
            }
        }
    }
    for key in theirs.keys().filter(|key| !ours.contains_key(key)) {
        found.push(format!(
            "{} is at the other source but not stored here",
            describe(*key)
        ));
    }
    // Two logs with the same position would hide behind one key.
    if ours.len() != stored.len() {
        found.push(format!(
            "{} stored logs share a position with another stored log",
            stored.len() - ours.len()
        ));
    }
    if theirs.len() != other.len() {
        found.push(format!(
            "the other source returned {} logs twice",
            other.len() - theirs.len()
        ));
    }

    if found.len() > LISTED {
        let more = found.len() - LISTED;
        found.truncate(LISTED);
        found.push(format!("and {more} more"));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backfill::testing::FakeChain;
    use crate::snapshot::tests::sample;
    use alloy_primitives::{Address, B256};

    fn day(text: &str) -> Day {
        text.parse().unwrap()
    }

    /// Blocks from noon on 30 October 2026 for about three and a half days.
    fn chain() -> FakeChain {
        let genesis = day("2026-10-30").start() + 43_200;
        FakeChain::new(genesis, 25_000, sample("2026-11-01", 0).state)
    }

    #[test]
    fn a_days_blocks_are_the_ones_dated_that_day() {
        let chain = chain();
        let (first, last) = day_blocks(&chain, day("2026-10-31")).unwrap().unwrap();
        assert_eq!(Day::of_time(chain.times[first as usize]), day("2026-10-31"));
        assert_eq!(Day::of_time(chain.times[last as usize]), day("2026-10-31"));
        assert_eq!(
            Day::of_time(chain.times[first as usize - 1]),
            day("2026-10-30")
        );
        assert_eq!(
            Day::of_time(chain.times[last as usize + 1]),
            day("2026-11-01")
        );
    }

    #[test]
    fn a_day_that_is_not_over_or_has_no_block_has_no_range() {
        let chain = chain();
        let last_day = Day::of_time(*chain.times.last().unwrap());
        // Still in progress at the head.
        assert_eq!(day_blocks(&chain, last_day).unwrap(), None);
        // Not begun yet.
        assert_eq!(day_blocks(&chain, last_day.next()).unwrap(), None);
        // Over before the chain's first block.
        assert_eq!(day_blocks(&chain, day("2026-10-29")).unwrap(), None);
        // The day before the last one is whole.
        assert!(day_blocks(&chain, Day(last_day.0 - 1)).unwrap().is_some());
    }

    #[test]
    fn the_newest_complete_day_is_picked() {
        let days = [
            (day("2026-11-01"), DayState::Complete),
            (day("2026-11-02"), DayState::Complete),
            (day("2026-11-03"), DayState::Partial),
        ];
        assert_eq!(last_complete_day(&days), Some(day("2026-11-02")));
        assert_eq!(last_complete_day(&days[2..]), None);
        assert_eq!(last_complete_day(&[]), None);
    }

    fn stored(block: u64, index: u32, data: u8) -> StoredLog {
        StoredLog {
            block_number: block,
            block_time: block * 12,
            log_index: index,
            tx_hash: B256::repeat_byte(block as u8),
            topics: vec![B256::repeat_byte(0x11)],
            data: vec![data],
        }
    }

    fn remote(log: &StoredLog) -> Log {
        Log {
            address: Address::repeat_byte(1),
            topics: log.topics.clone(),
            data: log.data.clone(),
            block_number: log.block_number,
            log_index: u64::from(log.log_index),
            transaction_hash: log.tx_hash,
        }
    }

    #[test]
    fn identical_logs_have_no_differences_whatever_their_order() {
        let ours = vec![stored(10, 0, 1), stored(10, 4, 2), stored(12, 1, 3)];
        let mut theirs: Vec<Log> = ours.iter().map(remote).collect();
        assert!(differences(&ours, &theirs).is_empty());
        theirs.reverse();
        assert!(differences(&ours, &theirs).is_empty());
        assert!(differences(&[], &[]).is_empty());
    }

    #[test]
    fn a_missing_an_extra_and_a_changed_log_are_each_named() {
        let ours = vec![stored(10, 0, 1), stored(10, 4, 2), stored(12, 1, 3)];
        let complete: Vec<Log> = ours.iter().map(remote).collect();

        let found = differences(&ours[..2], &complete);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].contains("block 12 log 1") && found[0].contains("not stored here"),
            "{found:?}"
        );

        let found = differences(&ours, &complete[1..]);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].contains("block 10 log 0") && found[0].contains("does not have it"),
            "{found:?}"
        );

        let mut changed = complete.clone();
        changed[1].data = vec![9];
        let found = differences(&ours, &changed);
        assert_eq!(found, ["block 10 log 4 has other content there"]);

        let mut moved = complete.clone();
        moved[2].transaction_hash = B256::repeat_byte(0xee);
        let found = differences(&ours, &moved);
        assert_eq!(found, ["block 12 log 1 is in another transaction there"]);
    }

    #[test]
    fn duplicates_and_long_lists_are_reported_briefly() {
        let ours = vec![stored(10, 0, 1)];
        let twice = vec![remote(&ours[0]), remote(&ours[0])];
        let found = differences(&ours, &twice);
        assert_eq!(found, ["the other source returned 1 logs twice"]);

        let many: Vec<StoredLog> = (0..40).map(|i| stored(100 + i, 0, 0)).collect();
        let found = differences(&many, &[]);
        assert_eq!(found.len(), LISTED + 1);
        assert_eq!(found.last().unwrap(), "and 30 more");
    }
}
