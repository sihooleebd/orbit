//! OS media-control integration: hardware media keys + the system "Now Playing"
//! panel (macOS Control Center / lock screen, MPRIS on Linux, SMTC on Windows),
//! via [`souvlaki`].

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

/// Commands coming back from the OS media controls.
pub enum RemoteCmd {
    PlayPause,
    Play,
    Pause,
    Next,
    Prev,
    Stop,
    SeekForward,
    SeekBackward,
}

pub struct Remote {
    controls: MediaControls,
    rx: Receiver<RemoteCmd>,
}

impl Remote {
    pub fn new() -> Option<Self> {
        let config = PlatformConfig {
            display_name: "Orbit",
            dbus_name: "orbit",
            // Windows SMTC needs a window handle; use the console window.
            hwnd: console_hwnd(),
        };
        let mut controls = MediaControls::new(config).ok()?;
        let (tx, rx) = mpsc::channel();
        controls
            .attach(move |event: MediaControlEvent| {
                let cmd = match event {
                    MediaControlEvent::Toggle => RemoteCmd::PlayPause,
                    MediaControlEvent::Play => RemoteCmd::Play,
                    MediaControlEvent::Pause => RemoteCmd::Pause,
                    MediaControlEvent::Next => RemoteCmd::Next,
                    MediaControlEvent::Previous => RemoteCmd::Prev,
                    MediaControlEvent::Stop => RemoteCmd::Stop,
                    MediaControlEvent::Seek(SeekDirection::Forward)
                    | MediaControlEvent::SeekBy(SeekDirection::Forward, _) => RemoteCmd::SeekForward,
                    MediaControlEvent::Seek(SeekDirection::Backward)
                    | MediaControlEvent::SeekBy(SeekDirection::Backward, _) => RemoteCmd::SeekBackward,
                    _ => return,
                };
                let _ = tx.send(cmd);
            })
            .ok()?;
        Some(Self { controls, rx })
    }

    /// Pump pending OS events and return any commands received.
    pub fn poll(&mut self) -> Vec<RemoteCmd> {
        pump_runloop();
        self.rx.try_iter().collect()
    }

    /// Publish the currently-playing item to the OS.
    pub fn set_now_playing(
        &mut self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        paused: bool,
        pos: Duration,
        dur: Option<Duration>,
    ) {
        let _ = self.controls.set_metadata(MediaMetadata {
            title: Some(title),
            artist: Some(artist),
            album,
            duration: dur,
            ..Default::default()
        });
        let progress = Some(MediaPosition(pos));
        let playback = if paused {
            MediaPlayback::Paused { progress }
        } else {
            MediaPlayback::Playing { progress }
        };
        let _ = self.controls.set_playback(playback);
    }

    pub fn set_stopped(&mut self) {
        let _ = self.controls.set_playback(MediaPlayback::Stopped);
    }
}

/// On macOS, MPRemoteCommandCenter callbacks are delivered on the main run loop.
/// Our event loop blocks in a syscall, so we briefly drain the run loop here.
#[cfg(target_os = "macos")]
fn pump_runloop() {
    use core_foundation_sys::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};
    const HANDLED_SOURCE: i32 = 2;
    unsafe {
        for _ in 0..32 {
            let res = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1);
            if res != HANDLED_SOURCE {
                break;
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn pump_runloop() {}

/// The console window handle, needed by Windows SMTC.
#[cfg(windows)]
fn console_hwnd() -> Option<*mut c_void> {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    let h = unsafe { GetConsoleWindow() } as *mut c_void;
    if h.is_null() {
        None
    } else {
        Some(h)
    }
}

#[cfg(not(windows))]
fn console_hwnd() -> Option<*mut c_void> {
    None
}
