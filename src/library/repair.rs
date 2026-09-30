//! `lime-rate-repair`: the background worker that rewrites FLAC files whose sample rate no output
//! device offers (`audio::rate_repair`), so the CPU-heavy decode/resample/encode never runs on the
//! UI thread, the audio controller, or the library scanner thread.
//!
//! The scanner hands a candidate to this worker during phase 1 and holds that one track back until
//! the worker reports (`RepairDone`), then probes the file again and lets it continue through the
//! ordinary pipeline — so a track only ever appears in the library or the queue with the
//! `AudioInfo` of the file that is actually on disk. Everything else in the same batch keeps
//! flowing meanwhile.

use std::collections::HashSet;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::audio::rate_repair::{
    RepairError, RepairOptions, RepairReport, repair_flac_sample_rate,
};

/// Turns the feature on for a `LibraryScanner` (`LibraryScanner::with_rate_repair`). Absent, the
/// scanner never touches a file.
#[derive(Clone, Debug)]
pub struct RateRepairConfig {
    /// Where the untouched original of every repaired file is kept.
    pub backup_dir: PathBuf,
}

struct RepairRequest {
    batch: u64,
    path: PathBuf,
}

pub(super) enum RepairOutcome {
    Repaired(RepairReport),
    /// Nothing was changed, and the user should hear about why (a diagnostic, not a crash).
    Failed(String),
    /// Nothing to do, silently: the file turned out not to need it, or an earlier attempt this
    /// session already failed and reported (no retry loop).
    Skipped,
}

pub(super) struct RepairDone {
    pub batch: u64,
    pub path: PathBuf,
    pub outcome: RepairOutcome,
}

pub(super) struct RepairWorker {
    requests: Sender<RepairRequest>,
    results: Receiver<RepairDone>,
}

impl RepairWorker {
    pub(super) fn spawn(config: RateRepairConfig) -> Self {
        let (request_tx, request_rx) = unbounded::<RepairRequest>();
        let (result_tx, result_rx) = unbounded::<RepairDone>();
        thread::Builder::new()
            .name("lime-rate-repair".into())
            .spawn(move || run(config, request_rx, result_tx))
            .expect("could not start Lime Player rate repair worker");
        Self {
            requests: request_tx,
            results: result_rx,
        }
    }

    /// Queues `path` for repair on behalf of `batch`. `false` means the worker is gone, and the
    /// caller must carry on with the file as it is.
    pub(super) fn submit(&self, batch: u64, path: PathBuf) -> bool {
        self.requests.send(RepairRequest { batch, path }).is_ok()
    }

    pub(super) fn results(&self) -> Receiver<RepairDone> {
        self.results.clone()
    }
}

/// Repairs one file at a time, in arrival order. Exits when the scanner (the only holder of the
/// request sender) is dropped. A path that failed once is not tried again this session, so a
/// read-only volume costs one cheap failed attempt, not one per rescan.
fn run(config: RateRepairConfig, requests: Receiver<RepairRequest>, results: Sender<RepairDone>) {
    let options = RepairOptions {
        backup_dir: config.backup_dir,
    };
    let mut failed: HashSet<PathBuf> = HashSet::new();
    while let Ok(RepairRequest { batch, path }) = requests.recv() {
        let outcome = if failed.contains(&path) {
            RepairOutcome::Skipped
        } else {
            match panic::catch_unwind(AssertUnwindSafe(|| {
                repair_flac_sample_rate(&path, &options)
            })) {
                Ok(Ok(report)) => RepairOutcome::Repaired(report),
                Ok(Err(RepairError::NotNeeded)) => RepairOutcome::Skipped,
                Ok(Err(error)) => {
                    failed.insert(path.clone());
                    RepairOutcome::Failed(error.to_string())
                }
                Err(_) => {
                    failed.insert(path.clone());
                    RepairOutcome::Failed("internal error while repairing this file".to_owned())
                }
            }
        };
        if results
            .send(RepairDone {
                batch,
                path,
                outcome,
            })
            .is_err()
        {
            return;
        }
    }
}

/// What the repair worker did for one scan batch, collected by `main.rs` per batch id and turned
/// into one status line when the batch finishes (a status set at the moment a repair completes
/// would be overwritten by the batch's own route-status restore a few milliseconds later).
#[derive(Default)]
pub struct RepairNotes {
    /// File name, rate before, rate after.
    repaired: Vec<(String, u32, u32)>,
    /// File name, why nothing was changed.
    failed: Vec<(String, String)>,
}

impl RepairNotes {
    pub fn record_repaired(&mut self, name: String, from_rate: u32, to_rate: u32) {
        self.repaired.push((name, from_rate, to_rate));
    }

    pub fn record_failed(&mut self, name: String, error: String) {
        self.failed.push((name, error));
    }

    /// The status line for this batch, or `None` when there is nothing to say. Failures are left
    /// out when `include_failures` is `false` (the quiet startup restore reports them on stderr only).
    pub fn summary(&self, include_failures: bool) -> Option<String> {
        let mut parts = Vec::new();
        match self.repaired.as_slice() {
            [] => {}
            [(name, from, to)] => parts.push(format!(
                "Converted \"{name}\" from {from} Hz to {to} Hz; the original was backed up."
            )),
            many => parts.push(format!(
                "Converted {} files to a playable sample rate; the originals were backed up.",
                many.len()
            )),
        }
        if include_failures {
            match self.failed.as_slice() {
                [] => {}
                [(name, error)] => parts.push(format!(
                    "Could not fix the sample rate of \"{name}\": {error}"
                )),
                many => parts.push(format!(
                    "Could not fix the sample rate of {} files (see the log for details).",
                    many.len()
                )),
            }
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_summarize_one_or_many_repairs_and_failures() {
        let mut notes = RepairNotes::default();
        assert_eq!(notes.summary(true), None);

        notes.record_repaired("Heat.flac".into(), 37_800, 44_100);
        assert_eq!(
            notes.summary(true).as_deref(),
            Some("Converted \"Heat.flac\" from 37800 Hz to 44100 Hz; the original was backed up.")
        );

        notes.record_repaired("Other.flac".into(), 37_800, 44_100);
        assert_eq!(
            notes.summary(true).as_deref(),
            Some("Converted 2 files to a playable sample rate; the originals were backed up.")
        );

        notes.record_failed("Locked.flac".into(), "the file is read-only".into());
        assert_eq!(
            notes.summary(true).as_deref(),
            Some(
                "Converted 2 files to a playable sample rate; the originals were backed up. \
                 Could not fix the sample rate of \"Locked.flac\": the file is read-only"
            )
        );
        assert_eq!(
            notes.summary(false).as_deref(),
            Some("Converted 2 files to a playable sample rate; the originals were backed up."),
            "the quiet startup restore leaves failures out of the status line"
        );
    }

    #[test]
    fn failures_alone_are_silent_when_quiet() {
        let mut notes = RepairNotes::default();
        notes.record_failed("Locked.flac".into(), "the file is read-only".into());
        assert_eq!(notes.summary(false), None);
        assert!(notes.summary(true).unwrap().contains("Locked.flac"));
    }
}
