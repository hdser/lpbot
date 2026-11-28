//! The executor's journal (`docs/DESIGN.md`, "Execution", step 6;
//! `docs/PLAN.md`, piece 5): one JSON line a step, appended and synced
//! before the step is taken, so a crash between any two steps is recovered
//! from the journal's last action and the chain. Local files under `data/`,
//! never committed.
//!
//! The chain is the source of truth: the journal says what was asked and
//! sent, the chain what happened. [`recovery`] works out what to do with an
//! action left open: plan it again (nothing was sent), wait for or replace
//! what was sent, or read back what landed.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use alloy_primitives::{B256, U256};
use lpbot_core::StepRecord;
use serde::{Deserialize, Serialize};

/// One line of the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum Entry {
    /// An action the engine decided, as the steps it takes.
    Intent {
        id: String,
        run: String,
        campaign: u32,
        action: String,
        steps: Vec<StepRecord>,
    },
    /// The transaction built for it: where it goes and the SHA-256 of its
    /// calldata.
    Planned {
        id: String,
        to: String,
        calldata_sha256: String,
    },
    /// The simulation against the node's latest state.
    Simulated { id: String, ok: bool, note: String },
    /// A signed transaction sent; a replacement is another `Sent` with the
    /// same nonce.
    Sent {
        id: String,
        nonce: u64,
        hash: B256,
        #[serde(with = "lpbot_core::steps::u128_text")]
        max_fee_per_gas: u128,
        #[serde(with = "lpbot_core::steps::u128_text")]
        max_priority_fee_per_gas: u128,
    },
    /// The receipt of one of the transactions sent.
    Confirmed {
        id: String,
        hash: B256,
        block: u64,
        success: bool,
    },
    /// The position and balances read back after it landed.
    Reconciled {
        id: String,
        token_id: Option<U256>,
        #[serde(with = "lpbot_core::steps::u128_text")]
        liquidity: u128,
        note: String,
    },
    /// Given up, with why: refused by a check, reverted, or superseded.
    Abandoned { id: String, reason: String },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Intent { id, .. }
            | Entry::Planned { id, .. }
            | Entry::Simulated { id, .. }
            | Entry::Sent { id, .. }
            | Entry::Confirmed { id, .. }
            | Entry::Reconciled { id, .. }
            | Entry::Abandoned { id, .. } => id,
        }
    }
}

/// An append-only journal file.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends an entry and syncs it to disk before returning.
    pub fn append(&self, entry: &Entry) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(entry).expect("an entry always serializes");
        writeln!(file, "{line}")?;
        file.sync_all()
    }

    /// Every entry, in order. A last line cut short by a crash is left out;
    /// any other line that does not read is an error naming it.
    pub fn read(&self) -> std::io::Result<Vec<Entry>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut entries = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            match serde_json::from_str(line) {
                Ok(entry) => entries.push(entry),
                Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => break,
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} line {}: {e}; the journal is the record of what was sent, so \
                             look at it before the executor starts again",
                            self.path.display(),
                            i + 1
                        ),
                    ))
                }
            }
        }
        Ok(entries)
    }
}

/// Where an action left open stands, from the journal alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Decided, nothing sent: plan it again on the state now, or drop it.
    NotSent { id: String },
    /// Sent, with these hashes for one nonce, none known to have landed.
    Sent {
        id: String,
        nonce: u64,
        hashes: Vec<B256>,
    },
    /// Landed (or reverted) and not read back yet.
    Landed {
        id: String,
        hash: B256,
        success: bool,
    },
}

/// The last action that was neither read back nor given up, if any.
pub fn open_action(entries: &[Entry]) -> Option<Stage> {
    let last_intent = entries
        .iter()
        .rposition(|e| matches!(e, Entry::Intent { .. }))?;
    let id = entries[last_intent].id().to_string();
    let mine: Vec<&Entry> = entries[last_intent..]
        .iter()
        .filter(|e| e.id() == id)
        .collect();
    if mine
        .iter()
        .any(|e| matches!(e, Entry::Reconciled { .. } | Entry::Abandoned { .. }))
    {
        return None;
    }
    if let Some(Entry::Confirmed { hash, success, .. }) = mine
        .iter()
        .rev()
        .find(|e| matches!(e, Entry::Confirmed { .. }))
    {
        return Some(Stage::Landed {
            id,
            hash: *hash,
            success: *success,
        });
    }
    let sent: Vec<(u64, B256)> = mine
        .iter()
        .filter_map(|e| match e {
            Entry::Sent { nonce, hash, .. } => Some((*nonce, *hash)),
            _ => None,
        })
        .collect();
    match sent.last() {
        None => Some(Stage::NotSent { id }),
        Some((nonce, _)) => Some(Stage::Sent {
            id,
            nonce: *nonce,
            hashes: sent
                .iter()
                .filter(|(n, _)| n == nonce)
                .map(|(_, h)| *h)
                .collect(),
        }),
    }
}

/// What the chain says about an action that was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainView {
    /// The receipt of one of the hashes, if one landed: (hash, success).
    pub receipt: Option<(B256, bool)>,
    /// The keeper's nonce on the chain now: the next one it may use.
    pub next_nonce: u64,
}

/// What to do on start with an action left open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    Nothing,
    /// Nothing was sent: plan it again from the chain's state, or let the
    /// engine decide afresh.
    PlanAgain {
        id: String,
    },
    /// Keep waiting for, or replace, the transactions sent with this nonce.
    Wait {
        id: String,
        nonce: u64,
    },
    /// It landed: read the position and balances back, and book what the
    /// chain shows.
    ReadBack {
        id: String,
        hash: B256,
        success: bool,
    },
    /// The nonce was used, but by none of our hashes: something else sent
    /// with the keeper's key. Stop and tell the owner.
    Alarm {
        id: String,
        reason: String,
    },
}

/// Works out what to do from the journal's open action and the chain.
pub fn recovery(stage: Option<Stage>, chain: &ChainView) -> Recovery {
    match stage {
        None => Recovery::Nothing,
        Some(Stage::NotSent { id }) => Recovery::PlanAgain { id },
        Some(Stage::Landed { id, hash, success }) => Recovery::ReadBack { id, hash, success },
        Some(Stage::Sent { id, nonce, hashes }) => match chain.receipt {
            Some((hash, success)) if hashes.contains(&hash) => {
                Recovery::ReadBack { id, hash, success }
            }
            _ if chain.next_nonce > nonce => Recovery::Alarm {
                id,
                reason: format!(
                    "nonce {nonce} is used on the chain but by none of the {} transaction(s) \
                     the journal sent with it: the keeper's key signed something the \
                     executor did not. Pause (`lpbot pause`) and check the key",
                    hashes.len()
                ),
            },
            _ => Recovery::Wait { id, nonce },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_core::Step;

    fn temp(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lpbot-journal-{label}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn intent(id: &str) -> Entry {
        Entry::Intent {
            id: id.into(),
            run: "r".into(),
            campaign: 1,
            action: "open".into(),
            steps: vec![StepRecord {
                block: 1,
                time: 2,
                campaign: 1,
                action: "open".into(),
                sqrt_price_x96: U256::from(1u8) << 96,
                step: Step::Mint {
                    tick_lower: -60,
                    tick_upper: 0,
                    liquidity: 7,
                    amount0: U256::ZERO,
                    amount1: U256::from(9u8),
                },
            }],
        }
    }

    fn sent(id: &str, nonce: u64, hash: u8) -> Entry {
        Entry::Sent {
            id: id.into(),
            nonce,
            hash: B256::repeat_byte(hash),
            max_fee_per_gas: 2,
            max_priority_fee_per_gas: 1,
        }
    }

    #[test]
    fn the_journal_reads_back_what_was_appended_and_survives_a_cut_last_line() {
        let path = temp("rw");
        let journal = Journal::new(&path);
        assert!(journal.read().unwrap().is_empty());
        journal.append(&intent("a")).unwrap();
        journal.append(&sent("a", 4, 1)).unwrap();
        assert_eq!(journal.read().unwrap(), vec![intent("a"), sent("a", 4, 1)]);
        // A crash in the middle of a line: that line is left out.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"entry\":\"sent\",\"id\":\"a\",\"no");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(journal.read().unwrap().len(), 2);
        // A bad line in the middle is an error that names it.
        std::fs::write(
            &path,
            format!(
                "not json\n{}\n",
                serde_json::to_string(&intent("a")).unwrap()
            ),
        )
        .unwrap();
        let error = journal.read().unwrap_err().to_string();
        assert!(error.contains("line 1"), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_open_action_is_the_last_one_neither_read_back_nor_given_up() {
        assert_eq!(open_action(&[]), None);
        let done = [
            intent("a"),
            sent("a", 4, 1),
            Entry::Confirmed {
                id: "a".into(),
                hash: B256::repeat_byte(1),
                block: 9,
                success: true,
            },
            Entry::Reconciled {
                id: "a".into(),
                token_id: Some(U256::from(5u8)),
                liquidity: 7,
                note: String::new(),
            },
        ];
        assert_eq!(open_action(&done), None);
        let mut entries = done.to_vec();
        entries.push(intent("b"));
        assert_eq!(
            open_action(&entries),
            Some(Stage::NotSent { id: "b".into() })
        );
        entries.push(sent("b", 5, 2));
        entries.push(sent("b", 5, 3));
        assert_eq!(
            open_action(&entries),
            Some(Stage::Sent {
                id: "b".into(),
                nonce: 5,
                hashes: vec![B256::repeat_byte(2), B256::repeat_byte(3)]
            })
        );
        entries.push(Entry::Confirmed {
            id: "b".into(),
            hash: B256::repeat_byte(3),
            block: 10,
            success: false,
        });
        assert_eq!(
            open_action(&entries),
            Some(Stage::Landed {
                id: "b".into(),
                hash: B256::repeat_byte(3),
                success: false
            })
        );
        entries.push(Entry::Abandoned {
            id: "b".into(),
            reason: "reverted".into(),
        });
        assert_eq!(open_action(&entries), None);
    }

    #[test]
    fn recovery_never_sends_twice_and_raises_a_nonce_used_by_someone_else() {
        let quiet = ChainView {
            receipt: None,
            next_nonce: 5,
        };
        assert_eq!(recovery(None, &quiet), Recovery::Nothing);
        assert_eq!(
            recovery(Some(Stage::NotSent { id: "b".into() }), &quiet),
            Recovery::PlanAgain { id: "b".into() }
        );
        let sent = Stage::Sent {
            id: "b".into(),
            nonce: 5,
            hashes: vec![B256::repeat_byte(2), B256::repeat_byte(3)],
        };
        // Not landed and the nonce still free: wait or replace, never a new
        // nonce.
        assert_eq!(
            recovery(Some(sent.clone()), &quiet),
            Recovery::Wait {
                id: "b".into(),
                nonce: 5
            }
        );
        // The replacement landed while the executor was down.
        let landed = ChainView {
            receipt: Some((B256::repeat_byte(3), true)),
            next_nonce: 6,
        };
        assert_eq!(
            recovery(Some(sent.clone()), &landed),
            Recovery::ReadBack {
                id: "b".into(),
                hash: B256::repeat_byte(3),
                success: true
            }
        );
        // The nonce is gone, and not to any of our hashes.
        let stolen = ChainView {
            receipt: None,
            next_nonce: 6,
        };
        assert!(
            matches!(recovery(Some(sent), &stolen), Recovery::Alarm { reason, .. } if reason.contains("nonce 5"))
        );
    }
}
