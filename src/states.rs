//! The mapping from slskd transfer states to what `poll-download` reports.
//!
//! slskd reports a transfer's state as comma-joined flags, so `Completed,
//! Succeeded` is a transfer that finished and wrote its file, while `Completed`
//! with `Cancelled`, `TimedOut`, `Errored`, `Rejected` or `Aborted` is a
//! transfer that ended without one. A transfer slskd no longer has a record for
//! cannot be judged at all, which is why the assessment separates "every file
//! succeeded" from "some file is unaccounted for".
//!
//! Everything here is pure: it never touches the network, so it is unit-tested
//! on the host target against captured response fixtures.

use crate::slskd;
use crate::tokens;

/// What a transfer's state flags mean for the plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The transfer finished and its file was written.
    Succeeded,
    /// The transfer ended without its file, or slskd ended it without saying
    /// that the file arrived.
    Failed,
    /// The transfer is waiting, in the plugin's queue or in the peer's.
    Queued,
    /// The transfer is moving.
    Running,
}

/// Classifies one transfer state.
///
/// # Parameters
///
/// - `state`: the state flags slskd reported, comma-joined.
///
/// # Returns
///
/// [`Class::Succeeded`] when the flags carry `Succeeded`, [`Class::Failed`]
/// when they carry `Completed` without it or any of the failure flags,
/// [`Class::Queued`] for `Queued`, `Requested` and `Initializing`, and
/// [`Class::Running`] otherwise.
///
/// # Example
///
/// ```text
/// classify("Completed, Succeeded") -> Class::Succeeded
/// classify("Completed, TimedOut")  -> Class::Failed
/// classify("Queued, Remotely")     -> Class::Queued
/// classify("InProgress")           -> Class::Running
/// ```
pub fn classify(state: &str) -> Class {
    let flags: Vec<String> = state
        .split(',')
        .map(|flag| flag.trim().to_lowercase())
        .collect();
    let has = |name: &str| flags.iter().any(|flag| flag == name);

    if has("succeeded") {
        return Class::Succeeded;
    }

    // A completed transfer that did not report success is a failure: slskd has
    // ended it and did not say the file arrived.
    if has("completed") || FAILURE_FLAGS.iter().any(|flag| has(flag)) {
        return Class::Failed;
    }

    if has("queued") || has("requested") || has("initializing") {
        return Class::Queued;
    }

    Class::Running
}

/// State flags that end a transfer without its file.
const FAILURE_FLAGS: [&str; 5] = ["cancelled", "timedout", "errored", "rejected", "aborted"];

/// A transfer as slskd reported it, reduced to what monitoring needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    /// Remote path of the file, as it was enqueued.
    pub filename: String,
    /// State flags, comma-joined.
    pub state: String,
    /// Bytes transferred so far.
    pub bytes_transferred: u64,
    /// Average transfer speed so far, in bytes per second.
    pub average_speed_bps: f64,
    /// Position in the peer's upload queue, when slskd knows it.
    pub place_in_queue: Option<u32>,
    /// The exception slskd recorded for a failed transfer.
    pub exception: String,
}

impl From<&slskd::Transfer> for Observed {
    fn from(transfer: &slskd::Transfer) -> Self {
        Self {
            filename: transfer.filename.clone(),
            state: transfer.state.clone(),
            bytes_transferred: transfer.bytes_transferred,
            average_speed_bps: transfer.average_speed,
            place_in_queue: transfer.place_in_queue,
            exception: transfer.exception.clone(),
        }
    }
}

/// How much of a download has moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes transferred so far.
    pub bytes: u64,
    /// Bytes the download is expected to produce.
    pub total_bytes: u64,
    /// Transfer speed so far, in bytes per second.
    pub speed_bps: u64,
}

/// What the observed transfers of a download mean.
///
/// A download with at least one file moving is running; the queue timeout only
/// applies when nothing moves, which is why [`Assessment::Running`] wins over
/// [`Assessment::Queued`] as soon as any file of the candidate is transferring.
#[derive(Debug, Clone, PartialEq)]
pub enum Assessment {
    /// A transfer failed; the message is slskd's exception text.
    Failed(String),
    /// Nothing moves and at least one file waits in a peer's queue, with the
    /// shortest queue position slskd knows.
    Queued(Option<u32>),
    /// The download is moving.
    Running(Progress),
    /// Every expected file is reported successful; the caller still has to find
    /// the files on disk before it reports completion, because slskd signals
    /// completion before the file leaves its incomplete directory.
    AwaitingFiles(Progress),
    /// At least one expected file has no transfer record, because slskd no
    /// longer knows its batch; the caller completes the download only when
    /// every file is present at its pinned path.
    Unverifiable,
}

/// Assesses a download from the transfers slskd reported.
///
/// A download with at least one file moving is running; the queue timeout only
/// applies when nothing moves.
///
/// # Parameters
///
/// - `expected`: the files the download is expected to produce.
/// - `observed`: the transfers slskd reported for the download's batches. A
///   batch slskd no longer knows contributes nothing, which leaves its files
///   unaccounted for.
///
/// # Returns
///
/// The [`Assessment`]: a failure as soon as one transfer failed, then whether
/// every file succeeded, then whether the download is moving, then whether it
/// is waiting in a queue, and otherwise how far it has moved.
pub fn assess(expected: &[tokens::ExpectedFile], observed: &[Observed]) -> Assessment {
    let mut succeeded = 0;
    let mut running = false;
    let mut queued = false;
    let mut place_in_queue: Option<u32> = None;
    let mut unaccounted = false;

    for file in expected {
        let Some(transfer) = observed
            .iter()
            .find(|transfer| transfer.filename == file.filename)
        else {
            unaccounted = true;

            continue;
        };

        match classify(&transfer.state) {
            Class::Failed => return Assessment::Failed(exception(transfer)),
            Class::Succeeded => succeeded += 1,
            Class::Queued => {
                queued = true;

                if let Some(place) = transfer.place_in_queue {
                    place_in_queue =
                        Some(place_in_queue.map_or(place, |current| current.min(place)));
                }
            }
            Class::Running => running = true,
        }
    }

    if unaccounted {
        return Assessment::Unverifiable;
    }

    let progress = progress(expected, observed);

    if succeeded == expected.len() {
        return Assessment::AwaitingFiles(progress);
    }

    if running {
        return Assessment::Running(progress);
    }

    if queued {
        return Assessment::Queued(place_in_queue);
    }

    Assessment::Running(progress)
}

/// Sums how far a download has moved.
///
/// # Parameters
///
/// - `expected`: the files the download is expected to produce.
/// - `observed`: the transfers slskd reported.
///
/// # Returns
///
/// The bytes transferred, the expected total and the summed transfer speed.
fn progress(expected: &[tokens::ExpectedFile], observed: &[Observed]) -> Progress {
    let speed: f64 = observed
        .iter()
        .map(|transfer| transfer.average_speed_bps)
        .sum();

    Progress {
        bytes: observed
            .iter()
            .map(|transfer| transfer.bytes_transferred)
            .sum(),
        total_bytes: expected.iter().map(|file| file.size).sum(),
        speed_bps: if speed.is_finite() && speed > 0.0 {
            speed as u64
        } else {
            0
        },
    }
}

/// Renders why a transfer failed.
///
/// # Parameters
///
/// - `transfer`: the failed transfer.
///
/// # Returns
///
/// slskd's exception text, and the state flags when slskd recorded no
/// exception.
fn exception(transfer: &Observed) -> String {
    let text = transfer.exception.trim();

    if text.is_empty() {
        format!(
            "slskd reports `{}` for `{}`",
            transfer.state, transfer.filename
        )
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an observed transfer.
    fn observed(filename: &str, state: &str) -> Observed {
        Observed {
            filename: filename.to_string(),
            state: state.to_string(),
            bytes_transferred: 0,
            average_speed_bps: 0.0,
            place_in_queue: None,
            exception: String::new(),
        }
    }

    /// Builds an expected file.
    fn expected(filename: &str, size: u64) -> tokens::ExpectedFile {
        tokens::ExpectedFile {
            filename: filename.to_string(),
            name: slskd::basename(filename).to_string(),
            relative: slskd::basename(filename).to_string(),
            size,
        }
    }

    #[test]
    fn a_moving_file_wins_over_a_queued_one() {
        let mut queued = observed("b.flac", "Queued, Remotely");
        queued.place_in_queue = Some(2);
        let mut running = observed("a.flac", "InProgress");
        running.bytes_transferred = 40;
        running.average_speed_bps = 900.0;

        let assessment = assess(
            &[expected("a.flac", 100), expected("b.flac", 60)],
            &[queued, running],
        );

        assert_eq!(
            assessment,
            Assessment::Running(Progress {
                bytes: 40,
                total_bytes: 160,
                speed_bps: 900,
            })
        );
    }

    #[test]
    fn a_queued_pair_reports_the_queue() {
        let observed = vec![
            observed("a.flac", "Queued, Remotely"),
            observed("b.flac", "Queued, Locally"),
        ];

        let assessment = assess(&[expected("a.flac", 1), expected("b.flac", 2)], &observed);

        assert_eq!(assessment, Assessment::Queued(None));
    }

    #[test]
    fn classifies_every_state_slskd_reports() {
        assert_eq!(classify("Completed, Succeeded"), Class::Succeeded);
        assert_eq!(classify("Completed, Cancelled"), Class::Failed);
        assert_eq!(classify("Completed, TimedOut"), Class::Failed);
        assert_eq!(classify("Completed, Errored"), Class::Failed);
        assert_eq!(classify("Completed, Rejected"), Class::Failed);
        assert_eq!(classify("Completed, Aborted"), Class::Failed);
        assert_eq!(classify("Completed, Cancelled, Remotely"), Class::Failed);
        assert_eq!(classify("Queued, Remotely"), Class::Queued);
        assert_eq!(classify("Queued, Locally"), Class::Queued);
        assert_eq!(classify("Requested"), Class::Queued);
        assert_eq!(classify("Initializing"), Class::Queued);
        assert_eq!(classify("InProgress"), Class::Running);
        assert_eq!(classify(""), Class::Running);
    }

    #[test]
    fn treats_a_completion_without_success_as_a_failure() {
        assert_eq!(classify("Completed"), Class::Failed);
        assert_eq!(classify("completed, locally"), Class::Failed);
    }

    #[test]
    fn reads_a_failed_transfer_from_a_fixture() {
        let batch: slskd::Batch =
            serde_json::from_str(include_str!("fixtures/batch_failed.json")).unwrap();
        let observed: Vec<Observed> = batch.transfers.iter().map(Observed::from).collect();

        let assessment = assess(
            &[expected("@@tqkz\\Music\\01 Windowlicker.flac", 48_410_233)],
            &observed,
        );

        assert_eq!(
            assessment,
            Assessment::Failed(
                "A connection attempt failed because the connected party did not properly respond"
                    .to_string()
            )
        );
    }

    #[test]
    fn reports_a_download_that_is_still_queued() {
        let batch: slskd::Batch =
            serde_json::from_str(include_str!("fixtures/batch_queued.json")).unwrap();
        let observed: Vec<Observed> = batch.transfers.iter().map(Observed::from).collect();

        let assessment = assess(
            &[expected("@@tqkz\\Music\\01 Windowlicker.flac", 48_410_233)],
            &observed,
        );

        assert_eq!(assessment, Assessment::Queued(Some(4)));
    }

    #[test]
    fn reports_a_queue_without_a_known_position() {
        let observed = vec![observed("a.flac", "Queued, Locally")];

        let assessment = assess(&[expected("a.flac", 1)], &observed);

        assert_eq!(assessment, Assessment::Queued(None));
    }

    #[test]
    fn reports_the_shortest_known_queue_position() {
        let mut first = observed("a.flac", "Queued, Remotely");
        first.place_in_queue = Some(12);
        let mut second = observed("b.flac", "Queued, Locally");
        second.place_in_queue = Some(3);

        let assessment = assess(
            &[expected("a.flac", 1), expected("b.flac", 2)],
            &[first, second],
        );

        assert_eq!(assessment, Assessment::Queued(Some(3)));
    }

    #[test]
    fn reports_the_progress_of_a_running_download() {
        let batch: slskd::Batch =
            serde_json::from_str(include_str!("fixtures/batch_downloading.json")).unwrap();
        let observed: Vec<Observed> = batch.transfers.iter().map(Observed::from).collect();

        let assessment = assess(
            &[expected("@@tqkz\\Music\\01 Windowlicker.flac", 48_410_233)],
            &observed,
        );

        assert_eq!(
            assessment,
            Assessment::Running(Progress {
                bytes: 12_000_000,
                total_bytes: 48_410_233,
                speed_bps: 640_000,
            })
        );
    }

    #[test]
    fn awaits_the_files_when_every_transfer_succeeded() {
        let observed = vec![observed("a.flac", "Completed, Succeeded")];

        let assessment = assess(&[expected("a.flac", 1)], &observed);

        assert_eq!(
            assessment,
            Assessment::AwaitingFiles(Progress {
                bytes: 0,
                total_bytes: 1,
                speed_bps: 0,
            })
        );
    }

    #[test]
    fn is_unverifiable_when_a_file_has_no_transfer() {
        let observed = vec![observed("a.flac", "Completed, Succeeded")];

        let assessment = assess(&[expected("a.flac", 1), expected("b.flac", 2)], &observed);

        assert_eq!(assessment, Assessment::Unverifiable);
    }

    #[test]
    fn is_unverifiable_when_no_batch_is_known() {
        let assessment = assess(&[expected("a.flac", 1)], &[]);

        assert_eq!(assessment, Assessment::Unverifiable);
    }

    #[test]
    fn a_failure_wins_over_an_unaccounted_file() {
        let observed = vec![observed("a.flac", "Completed, Errored")];

        let assessment = assess(&[expected("a.flac", 1), expected("b.flac", 2)], &observed);

        assert!(matches!(assessment, Assessment::Failed(_)));
    }

    #[test]
    fn sums_the_speed_of_every_transfer() {
        let mut first = observed("a.flac", "InProgress");
        first.average_speed_bps = 100.5;
        first.bytes_transferred = 10;
        let mut second = observed("b.flac", "InProgress");
        second.average_speed_bps = 250.25;
        second.bytes_transferred = 20;

        let assessment = assess(
            &[expected("a.flac", 40), expected("b.flac", 60)],
            &[first, second],
        );

        assert_eq!(
            assessment,
            Assessment::Running(Progress {
                bytes: 30,
                total_bytes: 100,
                speed_bps: 350,
            })
        );
    }

    #[test]
    fn ignores_a_speed_that_is_not_a_number() {
        let mut transfer = observed("a.flac", "InProgress");
        transfer.average_speed_bps = f64::NAN;

        let assessment = assess(&[expected("a.flac", 1)], &[transfer]);

        assert_eq!(
            assessment,
            Assessment::Running(Progress {
                bytes: 0,
                total_bytes: 1,
                speed_bps: 0,
            })
        );
    }
}
