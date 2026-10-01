//! OS media controls: hardware media keys and the system "Now Playing" panel (macOS Control
//! Center / lock screen, MPRIS on Linux), via [`souvlaki`].

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};

use crate::command::{Command, SeekTarget};

pub struct Media {
    controls: MediaControls,
    rx: Receiver<Command>,
}

impl Media {
    /// None when the platform service isn't there (no D-Bus session, ...): orbit runs without it.
    pub fn new(seek_step: Duration) -> Option<Media> {
        let config = PlatformConfig { display_name: "Orbit", dbus_name: "orbit", hwnd: None };
        let mut controls = MediaControls::new(config).ok()?;
        let (tx, rx) = mpsc::channel();
        controls
            .attach(move |event| {
                let cmd = match event {
                    MediaControlEvent::Toggle => Command::Toggle,
                    MediaControlEvent::Play => Command::Play(None),
                    MediaControlEvent::Pause => Command::Pause,
                    MediaControlEvent::Next => Command::Next,
                    MediaControlEvent::Previous => Command::Prev,
                    MediaControlEvent::Stop => Command::Stop,
                    MediaControlEvent::Seek(SeekDirection::Forward) => Command::Seek(SeekTarget::Forward(seek_step)),
                    MediaControlEvent::Seek(SeekDirection::Backward) => Command::Seek(SeekTarget::Backward(seek_step)),
                    MediaControlEvent::SeekBy(SeekDirection::Forward, d) => Command::Seek(SeekTarget::Forward(d)),
                    MediaControlEvent::SeekBy(SeekDirection::Backward, d) => Command::Seek(SeekTarget::Backward(d)),
                    MediaControlEvent::SetPosition(MediaPosition(d)) => Command::Seek(SeekTarget::Absolute(d)),
                    _ => return,
                };
                let _ = tx.send(cmd);
            })
            .ok()?;
        Some(Media { controls, rx })
    }

    /// Commands from media keys and the Now Playing panel since the last call.
    pub fn poll(&mut self) -> Vec<Command> {
        pump_runloop();
        self.rx.try_iter().collect()
    }

    /// Publish the playing track (None = stopped).
    pub fn publish(&mut self, track: Option<(&str, &str, &str)>, paused: bool, pos: Duration, duration: Option<Duration>) {
        let Some((title, artist, album)) = track else {
            let _ = self.controls.set_playback(MediaPlayback::Stopped);
            return;
        };
        let album = Some(album).filter(|a| !a.is_empty());
        let _ = self.controls.set_metadata(MediaMetadata { title: Some(title), artist: Some(artist), album, duration, ..Default::default() });
        let progress = Some(MediaPosition(pos));
        let _ = self.controls.set_playback(if paused { MediaPlayback::Paused { progress } } else { MediaPlayback::Playing { progress } });
    }
}

/// macOS delivers MPRemoteCommandCenter callbacks on the main run loop, which our event loop
/// (blocked in poll(2)) never runs: drain it briefly on every tick.
#[cfg(target_os = "macos")]
fn pump_runloop() {
    use core_foundation_sys::runloop::{CFRunLoopRunInMode, kCFRunLoopDefaultMode};
    const HANDLED_SOURCE: i32 = 2;
    for _ in 0..32 {
        // SAFETY: runs the current thread's run loop for at most one source, without waiting
        if unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1) } != HANDLED_SOURCE {
            break;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn pump_runloop() {}
