//! Media control actions
//!
//! Platform implementations:
//! - Windows: SendInput with VK_MEDIA_* keys, or Windows.Media.SystemMediaTransportControls
//! - Linux: playerctl (D-Bus MPRIS), or direct D-Bus calls

use anyhow::Result;
use tracing::warn;

pub async fn play_pause() -> Result<()> {
    #[cfg(windows)]
    {
        // TODO: Use SendInput with VK_MEDIA_PLAY_PAUSE (0xB3)
        // Or use Windows Runtime SystemMediaTransportControls for more control
        warn!("media play/pause not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // TODO: Use playerctl or direct D-Bus MPRIS call
        // playerctl play-pause
        // Or: dbus-send --print-reply --dest=org.mpris.MediaPlayer2.* /org/mpris/MediaPlayer2 org.mpris.MediaPlayer2.Player.PlayPause
        warn!("media play/pause not implemented on Linux");
    }

    Ok(())
}

pub async fn next_track() -> Result<()> {
    #[cfg(windows)]
    {
        // VK_MEDIA_NEXT_TRACK = 0xB0
        warn!("media next not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // playerctl next
        warn!("media next not implemented on Linux");
    }

    Ok(())
}

pub async fn prev_track() -> Result<()> {
    #[cfg(windows)]
    {
        // VK_MEDIA_PREV_TRACK = 0xB1
        warn!("media prev not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // playerctl previous
        warn!("media prev not implemented on Linux");
    }

    Ok(())
}

pub async fn stop() -> Result<()> {
    #[cfg(windows)]
    {
        // VK_MEDIA_STOP = 0xB2
        warn!("media stop not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // playerctl stop
        warn!("media stop not implemented on Linux");
    }

    Ok(())
}
