//! Optional audio downloader: shells out to `yt-dlp` to fetch a URL as mp3 into
//! a chosen library folder, on a background thread. yt-dlp is an optional
//! external dependency — absent, the feature is simply unavailable.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// Message from the download worker thread.
pub enum DownloadMsg {
    /// Playlist progress: currently on item `done` of `total`.
    Progress { done: u32, total: u32 },
    /// The download finished; `ok` is true on a zero exit code.
    Done { ok: bool },
}

/// Parse a yt-dlp playlist progress line, e.g. "[download] Downloading item 5 of 25".
/// Returns `(done, total)`. Handles both "item N of M" and "video N of M".
pub fn parse_progress(line: &str) -> Option<(u32, u32)> {
    for marker in ["Downloading item ", "Downloading video "] {
        if let Some(rest) = line.split(marker).nth(1) {
            let mut it = rest.split_whitespace();
            let done: u32 = it.next()?.parse().ok()?;
            if it.next()? != "of" {
                continue;
            }
            let total: u32 = it.next()?.parse().ok()?;
            return Some((done, total));
        }
    }
    None
}

/// Build the exact `yt-dlp` argument list. Audio-only mp3, with metadata and
/// thumbnail embedding, into `<root>/<subfolder>/`, de-duplicated via a
/// per-root download archive. Pure — unit-tested.
pub fn command_args(root: &Path, subfolder: &str, url: &str) -> Vec<String> {
    let out_template = root
        .join(subfolder)
        .join("%(playlist_index)s - %(title)s.%(ext)s");
    let archive = root.join(".orbit_dl_archive");
    vec![
        "-x".into(),
        "--audio-format".into(),
        "mp3".into(),
        "--audio-quality".into(),
        "0".into(),
        "--embed-metadata".into(),
        "--embed-thumbnail".into(),
        "--convert-thumbnails".into(),
        "png".into(),
        "--add-metadata".into(),
        "--yes-playlist".into(),
        "--download-archive".into(),
        archive.to_string_lossy().into_owned(),
        "-o".into(),
        out_template.to_string_lossy().into_owned(),
        url.into(),
    ]
}

/// Whether a `yt-dlp` executable is reachable on `PATH`.
pub fn yt_dlp_available() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates = ["yt-dlp", "yt-dlp.exe"];
    std::env::split_paths(&paths).any(|dir| candidates.iter().any(|c| dir.join(c).is_file()))
}

/// Spawn a worker thread running `yt-dlp`. Returns a receiver of progress lines
/// terminated by a single `Done`.
pub fn spawn_download(root: PathBuf, subfolder: String, url: String) -> Receiver<DownloadMsg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let args = command_args(&root, &subfolder, &url);
        let mut child = match Command::new("yt-dlp")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                let _ = tx.send(DownloadMsg::Done { ok: false });
                return;
            }
        };
        // Parse playlist progress from stdout as it streams.
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some((done, total)) = parse_progress(&line) {
                    let _ = tx.send(DownloadMsg::Progress { done, total });
                }
            }
        }
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        let _ = tx.send(DownloadMsg::Done { ok });
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_args_builds_mp3_invocation() {
        let args = command_args(Path::new("/music"), "Lofi Mix", "https://x.test/p");

        // mp3 extraction flags present.
        assert!(args.iter().any(|a| a == "-x"));
        let fmt = args.iter().position(|a| a == "--audio-format").unwrap();
        assert_eq!(args[fmt + 1], "mp3");

        // Output template lands under <root>/<subfolder>/ with the index-title pattern.
        let o = args.iter().position(|a| a == "-o").unwrap();
        let template = &args[o + 1];
        assert!(template.contains("Lofi Mix"));
        assert!(template.ends_with("%(playlist_index)s - %(title)s.%(ext)s"));
        assert!(template.starts_with("/music/Lofi Mix/"));

        // Archive lives at the root.
        let ar = args.iter().position(|a| a == "--download-archive").unwrap();
        assert!(args[ar + 1].ends_with(".orbit_dl_archive"));

        // URL is last.
        assert_eq!(args.last().unwrap(), "https://x.test/p");
    }

    #[test]
    fn parse_progress_reads_item_counts() {
        assert_eq!(parse_progress("[download] Downloading item 5 of 25"), Some((5, 25)));
        assert_eq!(parse_progress("[download] Downloading video 1 of 3"), Some((1, 3)));
        assert_eq!(parse_progress("[download] Destination: foo.mp3"), None);
        assert_eq!(parse_progress("random noise"), None);
    }
}
