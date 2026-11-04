//! Track analysis for Wavyzon, ported from Cadence: BPM, meter, downbeats,
//! phrase grid, Camelot key, sections, vocals, intro/outro mix regions and
//! EBU R128 loudness, packaged as a [`Fingerprint`] compatible with greed's
//! automix.
//!
//! - [`Analyzer`] is the streaming analyser. The player attaches one to a
//!   deck as a live tap ([`Analyzer::feed`] for each 20 ms frame,
//!   [`Analyzer::finish`] at the end).
//! - [`Offline`] analyses queued tracks in the background with a bounded
//!   worker pool and stores results in a [`Registry`].
//! - [`Registry::loudness`] is the lookup used for loudness normalisation.

mod analyzer;
pub mod dsp;
mod fingerprint;
mod offline;
pub mod structure;
#[cfg(test)]
mod tests_support;

pub use analyzer::{Analyzer, OnDone};
pub use fingerprint::{
    ANALYSIS_VERSION, ANALYZER_VERSION, EnergyPoint, Fingerprint, Harmony, LoudnessResult, MixRegions, Onset,
    REGISTRY_SIZE, Registry, Rhythm, Section, TempoPoint, VocalActivity,
};
pub use offline::{FAIL_RETRY, MAX_TRACK_LENGTH, OFFLINE_TIMEOUT, Offline, Opener, Stats};
pub use structure::{FLOOR_LUFS, Loudness};
