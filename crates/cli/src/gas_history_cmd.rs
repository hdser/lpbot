//! `lpbot gas-history`: each block's base fee and median priority fee,
//! read from the node's fee history and stored in files of 100,000 blocks
//! (`lpbot_data::gas`). A backtest charges every action the price of the
//! block it lands in.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use lpbot_chain::eth::FeeHistory;
use lpbot_chain::RpcClient;
use lpbot_data::gas::{chunk_of, BlockFees, GasStore, CHUNK};

/// Most blocks a fee history request may cover.
pub const PER_REQUEST: u64 = 1_024;

/// Where the fee history comes from.
pub trait FeeSource: Sync {
    /// The fees of the `count` blocks ending at `newest`.
    fn fees(&self, newest: u64, count: u64) -> Result<FeeHistory>;
}

impl FeeSource for RpcClient {
    fn fees(&self, newest: u64, count: u64) -> Result<FeeHistory> {
        Ok(self.fee_history(newest, count)?)
    }
}

/// The fees of the blocks `first` to `last`, both included, a request of
/// at most `PER_REQUEST` blocks at a time.
pub fn fetch(source: &impl FeeSource, first: u64, last: u64) -> Result<BlockFees> {
    let mut fees = BlockFees::default();
    let mut from = first;
    while from <= last {
        let newest = (from + PER_REQUEST - 1).min(last);
        let count = newest + 1 - from;
        let reply = source
            .fees(newest, count)
            .with_context(|| format!("reading the fees of blocks {from} to {newest}"))?;
        if reply.oldest != from {
            return Err(anyhow!(
                "asked for the fees from block {from}, the node answered from block {}",
                reply.oldest
            ));
        }
        let next = BlockFees::new(from, reply.base_fees, reply.median_tips)
            .ok_or_else(|| anyhow!("the node's base fees and tips differ in length"))?;
        fees.extend(next).map_err(|e| anyhow!(e))?;
        from = newest + 1;
    }
    Ok(fees)
}

/// What a fetch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fetched {
    /// Chunks read from the node.
    pub chunks: usize,
    /// Chunks already stored whole and left alone.
    pub kept: usize,
}

/// Stores the fees of every block from `from` to `to`, both included: the
/// chunks not yet stored whole, `threads` at a time. The chunk that
/// holds `to` is stored as far as `to`, and whole once the chain has
/// passed its end.
pub fn run(
    source: &impl FeeSource,
    root: &Path,
    chain: &str,
    from: u64,
    to: u64,
    threads: usize,
) -> Result<Fetched> {
    let store = GasStore::new(root);
    let complete = store.complete_chunks(chain)?;
    let chunks: Vec<u64> = (chunk_of(from)..=to)
        .step_by(CHUNK as usize)
        .filter(|first| !complete.contains(first))
        .collect();
    let all = (chunk_of(from)..=to).step_by(CHUNK as usize).count();
    let next = AtomicUsize::new(0);
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                if failure.lock().map(|f| f.is_some()).unwrap_or(true) {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&first) = chunks.get(i) else {
                    return;
                };
                let last = (first + CHUNK - 1).min(to);
                let stored = fetch(source, first, last)
                    .and_then(|fees| Ok(store.write_chunk(chain, &fees)?));
                match stored {
                    Ok(()) => println!(
                        "  blocks {first} to {last} stored ({} of {} chunk(s))",
                        i + 1,
                        chunks.len()
                    ),
                    Err(error) => {
                        if let Ok(mut slot) = failure.lock() {
                            slot.get_or_insert(error);
                        }
                        return;
                    }
                }
            });
        }
    });
    if let Some(error) = failure.into_inner().ok().flatten() {
        return Err(error);
    }
    Ok(Fetched {
        chunks: chunks.len(),
        kept: all - chunks.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// A node whose base fee is the block number and whose tip is a tenth
    /// of it.
    struct Fake {
        requests: AtomicU64,
    }

    impl FeeSource for Fake {
        fn fees(&self, newest: u64, count: u64) -> Result<FeeHistory> {
            assert!(
                count <= PER_REQUEST,
                "at most {PER_REQUEST} blocks a request"
            );
            self.requests.fetch_add(1, Ordering::Relaxed);
            let oldest = newest + 1 - count;
            Ok(FeeHistory {
                oldest,
                base_fees: (oldest..=newest).collect(),
                median_tips: (oldest..=newest).map(|b| b / 10).collect(),
            })
        }
    }

    fn temp(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lpbot-gas-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_range_is_read_in_requests_of_at_most_1024_blocks() {
        let fake = Fake {
            requests: AtomicU64::new(0),
        };
        let fees = fetch(&fake, 1_000, 3_500).unwrap();
        assert_eq!((fees.first(), fees.end()), (1_000, 3_501));
        assert_eq!(fees.at(3_500), Some((3_500, 350)));
        assert_eq!(fake.requests.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_node_that_answers_other_blocks_is_refused() {
        struct Off;
        impl FeeSource for Off {
            fn fees(&self, newest: u64, count: u64) -> Result<FeeHistory> {
                Ok(FeeHistory {
                    oldest: newest + 2 - count,
                    base_fees: vec![1; count as usize],
                    median_tips: vec![0; count as usize],
                })
            }
        }
        assert!(fetch(&Off, 10, 20).is_err());
    }

    #[test]
    fn whole_chunks_are_fetched_once_and_the_head_chunk_again() {
        let dir = temp("run");
        let fake = Fake {
            requests: AtomicU64::new(0),
        };
        let to = 2 * CHUNK + 500;
        let first = run(&fake, &dir, "ethereum", CHUNK + 7, to, 2).unwrap();
        assert_eq!(first, Fetched { chunks: 2, kept: 0 });
        let store = GasStore::new(&dir);
        let fees = store.read("ethereum", CHUNK + 7, to).unwrap().unwrap();
        assert_eq!(fees.at(to), Some((to, to / 10)));

        let again = run(&fake, &dir, "ethereum", CHUNK, to + 1_000, 2).unwrap();
        assert_eq!(
            again,
            Fetched { chunks: 1, kept: 1 },
            "the head chunk is read again"
        );
        assert!(store.read("ethereum", CHUNK, to + 1_000).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
