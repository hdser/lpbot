//! `lpbot paper`: a forward experiment followed at the chain's head with
//! virtual positions (`docs/PLAN.md`, piece 1 and M2).
//!
//! Each run is the engine as a [`Stream`]: a base copy fed only blocks at
//! or behind the finalized block, and at every step a copy of it fed the
//! blocks past it, which may still be replaced. The tables written are the
//! head copy's, with whatever is open closed on the newest state, as a
//! forward replay shows them. At every step the stored history is brought
//! to the head first (the forward catch-up, under the store's lock), so a
//! replaced block is simply read again.
//!
//! The same code decides as in a backtest, so a paper run must equal the
//! forward replay of the same file over the same blocks; every
//! `check_every` steps it replays the whole history and says whether they
//! are equal, which is M2's gate. The changes the run makes to its
//! position on final blocks are appended to `<journal>/<run>.steps.jsonl`:
//! what a live run would have sent.
//!
//! An agent's run is set up again whenever its decisions file changes. The
//! pause flag is reported but does not change a paper run: holding entries
//! is the executor's (`docs/PLAN.md`, piece 4), and paper stays equal to
//! the replay.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use lpbot_core::experiment::ResolvedRun;
use lpbot_data::day::Day;
use lpbot_data::lake::{read_events, TimedEvent};
use lpbot_risk::pause::{PauseFlag, State};
use lpbot_sim::engine::{self, Output, Stream};
use lpbot_sim::tables::write_run;

use crate::agent_cmd::sync;
use crate::agent_lock;
use crate::run_cmd::{self, setup, Caches, RunSetup};

/// What `lpbot paper` is asked to do.
pub struct PaperArgs<'a> {
    pub runs: &'a [ResolvedRun],
    pub root: &'a Path,
    pub results: &'a Path,
    pub journal: &'a Path,
    pub chain: &'a str,
    pub rpc: Option<&'a str>,
    pub every: Duration,
    /// Steps between replays of the whole history against the paper run.
    pub check_every: u32,
    /// Stop after this many steps; `None` runs until stopped.
    pub steps: Option<u32>,
    pub pause: &'a PauseFlag,
}

/// A run followed at the head.
struct Paper<'a> {
    setup: &'a RunSetup,
    /// Fed only events of final blocks.
    base: Stream<'a>,
    /// The newest event the base was fed, as (block, log index).
    base_at: (u64, u32),
    /// Where the base stood when it was set up: the stored history up to
    /// here is what it started from.
    start_at: (u64, u32),
    /// Events of final blocks fed after the stored history was read, for
    /// the replay check.
    after: Vec<TimedEvent>,
    /// Events past the finalized block, read again at every step.
    tail: Vec<TimedEvent>,
    /// Steps of the base already in the journal.
    journaled: usize,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn position(event: &TimedEvent) -> (u64, u32) {
    (event.block, event.log_index)
}

/// Feeds `events` (in chain order) to `stream`, ending each block once the
/// next event is of another block, and the last one too: every block given
/// is complete.
fn feed(stream: &mut Stream<'_>, events: &[TimedEvent]) -> Result<()> {
    for (i, event) in events.iter().enumerate() {
        if stream.stopped() {
            break;
        }
        stream.push(event)?;
        if events
            .get(i + 1)
            .is_none_or(|next| next.block != event.block)
        {
            stream.end_block()?;
        }
    }
    Ok(())
}

impl<'a> Paper<'a> {
    /// The base fed the stored history up to `final_block`; the rest is the
    /// tail.
    fn start(setup: &'a RunSetup, final_block: u64) -> Result<Self> {
        let events = &setup.history.events;
        let split = events.partition_point(|e| e.block <= final_block);
        let mut base = Stream::start(setup.history.snapshot.state.clone(), &setup.settings)
            .with_context(|| format!("run {}", setup.info.run_id))?;
        feed(&mut base, &events[..split])?;
        let base_at = events[..split].last().map_or((0, 0), position);
        Ok(Self {
            setup,
            base,
            base_at,
            start_at: base_at,
            after: Vec::new(),
            tail: events[split..].to_vec(),
            journaled: 0,
        })
    }

    /// Takes in what the store holds now: events after the base up to
    /// `final_block` go to the base, the rest replace the tail.
    fn advance(&mut self, stored: Vec<TimedEvent>, final_block: u64) -> Result<()> {
        let fresh: Vec<TimedEvent> = stored
            .into_iter()
            .filter(|e| position(e) > self.base_at)
            .collect();
        let split = fresh.partition_point(|e| e.block <= final_block);
        let (now_final, tail) = fresh.split_at(split);
        feed(&mut self.base, now_final)?;
        if let Some(last) = now_final.last() {
            self.base_at = position(last);
        }
        self.after.extend_from_slice(now_final);
        self.tail = tail.to_vec();
        Ok(())
    }

    /// The run as it stands at the head: a copy of the base fed the tail.
    fn head(&self) -> Result<Stream<'a>> {
        let mut head = self.base.clone();
        feed(&mut head, &self.tail)?;
        Ok(head)
    }

    /// Every event the run has seen, stored history first.
    fn all_events(&self) -> Vec<TimedEvent> {
        let mut all = self.setup.history.events.clone();
        let stored_final = all.partition_point(|e| position(e) <= self.start_at);
        all.truncate(stored_final);
        all.extend(self.after.iter().cloned());
        all.extend(self.tail.iter().cloned());
        all
    }

    /// Whether the run equals a replay of everything it has seen, with the
    /// gas prices of every block as stored now.
    fn check(&self, head: &Output, root: &Path, chain: &str) -> Result<bool> {
        let all = self.all_events();
        let mut settings = self.setup.settings.clone();
        settings.block_fees = run_cmd::block_fees(
            &self.setup.config,
            root,
            chain,
            &all,
            &settings,
            Day::of_time(settings.trade_from),
            &mut run_cmd::FeeCache::new(),
        )?;
        let replay = engine::run(self.setup.history.snapshot.state.clone(), &all, &settings)?;
        Ok(&replay == head)
    }

    /// Appends the base's new steps to the journal.
    fn journal(&mut self, folder: &Path) -> Result<usize> {
        let steps = self.base.steps();
        let new = &steps[self.journaled.min(steps.len())..];
        if new.is_empty() {
            return Ok(0);
        }
        std::fs::create_dir_all(folder)
            .with_context(|| format!("creating {}", folder.display()))?;
        let path = folder.join(format!("{}.steps.jsonl", self.setup.info.run_id));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        for record in new {
            writeln!(file, "{}", serde_json::to_string(record)?)?;
        }
        file.sync_all()?;
        self.journaled = steps.len();
        Ok(new.len())
    }
}

/// When each agent's decisions file last changed, to set its run up again.
fn decisions_stamp(setups: &[RunSetup]) -> Vec<Option<(SystemTime, u64)>> {
    setups
        .iter()
        .map(|s| {
            let who = crate::agent_log::identity(&s.strategy).ok().flatten()?;
            let meta = std::fs::metadata(&who.decisions).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        })
        .collect()
}

/// Brings every run's pool to the head under the store's lock, and returns
/// the finalized block.
fn catch_up(args: &PaperArgs<'_>) -> Result<u64> {
    let _operations = agent_lock::lake(args.root)?;
    let mut final_block = u64::MAX;
    for run in args.runs {
        let synced = sync(run, args.root, args.chain, args.rpc)
            .with_context(|| format!("catching up {}", run.run_id))?;
        final_block = final_block.min(synced.final_block);
    }
    Ok(final_block)
}

/// Follows the runs at the head until stopped, or for `args.steps` steps.
pub fn follow(args: &PaperArgs<'_>) -> Result<()> {
    let mut steps_done = 0u32;
    loop {
        // Set up: the stored history as it is, every run from its start.
        let final_block = catch_up(args)?;
        let mut setups = Vec::new();
        {
            let _operations = agent_lock::lake(args.root)?;
            let mut caches = Caches::default();
            for run in args.runs {
                let mut ready = setup(run, args.root, args.chain, &mut caches, "paper")?;
                if !ready.open {
                    return Err(anyhow!(
                        "run {} has a window with a last day; paper follows forward runs, \
                         whose window is `to: open`",
                        run.run_id
                    ));
                }
                // A paper run trades on past its first day's end.
                ready.settings.trade_to = u64::MAX;
                setups.push(ready);
            }
        }
        let stamp = decisions_stamp(&setups);
        let mut papers = setups
            .iter()
            .map(|s| Paper::start(s, final_block))
            .collect::<Result<Vec<_>>>()?;
        println!(
            "{} following {} run(s) from block {final_block} (finalized)",
            stamp_text(),
            papers.len()
        );
        let mut since_check = 0u32;
        loop {
            since_check += 1;
            let checking = since_check >= args.check_every.max(1);
            if checking {
                since_check = 0;
            }
            for paper in &mut papers {
                let head = paper.head()?;
                let (time, block) = head.last();
                let out = head.finish()?;
                let mut info = paper.setup.info.clone();
                info.window_to = Day::of_time(time).to_string();
                write_run(args.results, &info, &out, &paper.setup.rank_rows)?;
                let journaled = paper.journal(args.journal)?;
                let check = if checking {
                    if paper.check(&out, args.root, args.chain)? {
                        "; equals the replay".to_string()
                    } else {
                        "; DIFFERS from the replay of the same blocks".to_string()
                    }
                } else {
                    String::new()
                };
                println!(
                    "{} {} block {block}: {:+.3}% in {}, {} campaign(s){}{}{}",
                    stamp_text(),
                    info.run_id,
                    out.summary.result_pct,
                    info.value_token,
                    out.summary.campaigns,
                    if journaled > 0 {
                        format!(", {journaled} new step(s) in the journal")
                    } else {
                        String::new()
                    },
                    if paper.base.stopped() {
                        ", waiting for the agent's decision"
                    } else {
                        ""
                    },
                    check
                );
            }
            match args.pause.state() {
                State::Running => {}
                State::Paused(why) | State::Exiting(why) => println!(
                    "{} the pause flag is set ({why}); a paper run is not changed by it",
                    stamp_text()
                ),
            }
            steps_done += 1;
            if args.steps.is_some_and(|steps| steps_done >= steps) {
                return Ok(());
            }
            std::thread::sleep(args.every);
            if decisions_stamp(&setups) != stamp {
                println!(
                    "{} an agent decided: setting the runs up again",
                    stamp_text()
                );
                break;
            }
            let final_block = catch_up(args)?;
            let _operations = agent_lock::lake(args.root)?;
            for paper in &mut papers {
                let from = Day::of_time(paper.base.last().0.max(paper.setup.settings.trade_from));
                let today = Day::of_time(now());
                let stored = read_events(args.root, args.chain, paper.setup.pool, from, today)?;
                // The gas of the blocks just stored, before they are fed.
                if let Some(fees) = run_cmd::block_fees(
                    &paper.setup.config,
                    args.root,
                    args.chain,
                    &stored,
                    &paper.setup.settings,
                    from,
                    &mut run_cmd::FeeCache::new(),
                )? {
                    paper.base.set_block_fees(fees);
                }
                paper.advance(stored, final_block)?;
            }
        }
    }
}

fn stamp_text() -> String {
    let t = now();
    let day = Day::of_time(t);
    let s = t % 86_400;
    format!("{day}T{:02}:{:02}:{:02}Z", s / 3_600, s / 60 % 60, s % 60)
}

/// Where a paper experiment's journal of steps is kept.
pub fn journal_dir(root_data: &Path, experiment: &str) -> PathBuf {
    root_data.join("paper").join(experiment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_cmd::tests::demo;

    #[test]
    fn a_paper_run_fed_its_blocks_as_they_become_final_equals_the_replay() {
        let (root, mut runs, _) = demo("paper", 12);
        let lake = root.join("lake");
        let run = &mut runs[0];
        run.config.window.as_mut().unwrap().to = "open".into();
        let mut ready = setup(run, &lake, "ethereum", &mut Caches::default(), "paper").unwrap();
        assert!(ready.open);
        ready.settings.trade_to = u64::MAX;
        let events = ready.history.events.clone();
        let replay = engine::run(
            ready.history.snapshot.state.clone(),
            &events,
            &ready.settings,
        )
        .unwrap();
        assert!(!replay.steps.is_empty(), "the demo run trades");

        // Half the blocks are final at the start; the rest is the tail.
        let middle = events[events.len() / 2].block;
        let mut paper = Paper::start(&ready, middle).unwrap();
        assert!(paper.tail.iter().all(|e| e.block > middle));
        assert_eq!(paper.head().unwrap().finish().unwrap(), replay);
        assert!(paper.check(&replay, &lake, "ethereum").unwrap());

        // A step later a few more blocks are final, the rest still tail;
        // then all of them. The store is read whole each time.
        let later = events[events.len() * 3 / 4].block;
        paper.advance(events.clone(), later).unwrap();
        assert!(paper
            .after
            .iter()
            .all(|e| e.block > middle && e.block <= later));
        assert_eq!(paper.head().unwrap().finish().unwrap(), replay);
        paper.advance(events.clone(), u64::MAX).unwrap();
        assert!(paper.tail.is_empty());
        assert_eq!(paper.all_events(), events);
        assert_eq!(paper.base.clone().finish().unwrap(), replay);
        assert!(paper.check(&replay, &lake, "ethereum").unwrap());

        // The journal holds the final steps once, however often it is
        // written.
        let journal = root.join("journal");
        let written = paper.journal(&journal).unwrap();
        assert_eq!(written, paper.base.steps().len());
        assert_eq!(paper.journal(&journal).unwrap(), 0);
        let path = journal.join(format!("{}.steps.jsonl", ready.info.run_id));
        let lines = std::fs::read_to_string(path).unwrap();
        assert_eq!(lines.lines().count(), written);
        let first: lpbot_core::StepRecord =
            serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!(first, replay.steps[0]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_journal_folder_is_under_data_and_named_by_experiment() {
        assert_eq!(
            journal_dir(Path::new("data"), "forward-regime-usdc"),
            PathBuf::from("data/paper/forward-regime-usdc")
        );
    }
}
