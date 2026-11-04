//! Custom audio filters contributed by plugins.
//!
//! Clients enable them through `filters.pluginFilters.<name>` on a player
//! update, exactly like Lavalink's `pluginFilters`:
//!
//! ```json
//! { "filters": { "pluginFilters": { "bitcrush": { "bits": 6 } } } }
//! ```

use serde_json::Value;

use crate::Failure;

/// Sample rate of every frame handed to a filter.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channels.
pub const CHANNELS: usize = 2;
/// One 20 ms frame: 960 samples per channel, interleaved L/R.
pub const FRAME_SAMPLES: usize = 960 * CHANNELS;

/// A running filter instance. The player builds one instance per deck (so two
/// exist during a crossfade), and may call [`FilterFactory::create`] once more
/// just to validate a configuration.
///
/// `process` runs on the real-time send path every 20 ms: it must not block,
/// allocate heavily or do I/O.
pub trait AudioFilter: Send {
    /// Process one interleaved stereo frame in place, values in `-1.0..=1.0`.
    fn process(&mut self, frame: &mut [f32]);

    /// A new configuration arrived for an already running filter. Return
    /// `false` to have the player rebuild the filter from the factory instead.
    fn reconfigure(&mut self, _config: &Value) -> bool {
        false
    }

    /// Clear internal state (called on seek and track change).
    fn reset(&mut self) {}
}

/// Builds [`AudioFilter`]s from client JSON.
pub trait FilterFactory: Send + Sync + 'static {
    /// The key under `pluginFilters` (`"bitcrush"`).
    fn name(&self) -> &str;

    /// Validate `config` and build an instance. Errors are returned to the
    /// client as `400 Bad Request`.
    fn create(&self, config: &Value) -> Result<Box<dyn AudioFilter>, Failure>;
}
