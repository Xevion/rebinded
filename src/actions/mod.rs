//! Action execution layer
//!
//! Translates high-level actions (MediaPlayPause, BrowserBack, etc.) into
//! platform-specific API calls or simulated key presses.

mod keys;
mod media;

use crate::config::Action;
use anyhow::Result;
use tracing::debug;

/// Execute an action
pub async fn execute(action: &Action) -> Result<()> {
    debug!(?action, "executing action");

    match action {
        Action::MediaPlayPause => media::play_pause().await,
        Action::MediaNext => media::next_track().await,
        Action::MediaPrevious => media::prev_track().await,
        Action::MediaStop => media::stop().await,
        Action::BrowserBack => keys::browser_back().await,
        Action::BrowserForward => keys::browser_forward().await,
        Action::Passthrough => {
            // Passthrough is handled at the hook level, not here
            Ok(())
        }
        Action::Block => {
            // Block means do nothing
            Ok(())
        }
    }
}
