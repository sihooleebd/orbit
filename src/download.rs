//! `download <url>`: fetch audio as mp3 into a library folder with `yt-dlp` (when it's installed),
//! on a background thread.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};

pub enum DownloadMsg {
    /// Playlist progress: on item `done` of `total`.
    Progress(u32, u32),
    /// Finished: Ok, or yt-dlp's error.
    Done(Result<(), String>),
}

/// "[download] Downloading item 5 of 25" (or "video 5 of 25") -> (5, 25).
fn parse_progress(line: &str) -> Option<(u32, u32)> {
    let rest = line.split_once("Downloading item ").or_else(|| line.split_once("Downloading video "))?.1;
    let mut it = rest.split_whitespace();
    let done = it.next()?.parse().ok()?;
    (it.next()? == "of").then_some(())?;
    Some((done, it.next()?.parse().ok()?))
}

/// mp3 with tags and cover art into `<root>/<folder>/`; an archive at the root skips what was
/// downloaded before.
fn args(root: &Path, folder: &str, url: &str) -> Vec<String> {
    let out = root.join(folder).join("%(playlist_index)s - %(title)s.%(ext)s");
    let archive = root.join(".orbit_dl_archive");
    let flags = ["-x", "--audio-format", "mp3", "--audio-quality", "0", "--embed-metadata", "--embed-thumbnail"];
    let flags = flags.into_iter().chain(["--convert-thumbnails", "png", "--yes-playlist", "--newline", "--download-archive"]);
    let mut v: Vec<String> = flags.map(String::from).collect();
    v.extend([archive.to_string_lossy().into_owned(), "-o".into(), out.to_string_lossy().into_owned()]);
    // "--": a URL starting with "-" must not be read as an option
    v.extend(["--".into(), url.into()]);
    v
}

pub fn available() -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|dir| dir.join("yt-dlp").is_file()))
}

pub fn spawn(root: PathBuf, folder: String, url: String) -> Receiver<DownloadMsg> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("download".into()).spawn(move || {
        let child = Command::new("yt-dlp").args(args(&root, &folder, &url)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(DownloadMsg::Done(Err(format!("can't run yt-dlp: {e}"))));
                return;
            }
        };
        // stderr on its own thread so neither pipe fills up and stalls yt-dlp
        let stderr = child.stderr.take().map(|err| {
            std::thread::spawn(move || BufReader::new(err).lines().map_while(Result::ok).filter(|l| l.starts_with("ERROR")).last())
        });
        for line in child.stdout.take().map(|out| BufReader::new(out).lines().map_while(Result::ok)).into_iter().flatten() {
            if let Some((done, total)) = parse_progress(&line) {
                let _ = tx.send(DownloadMsg::Progress(done, total));
            }
        }
        let error = stderr.and_then(|h| h.join().ok().flatten());
        let result = match child.wait() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(error.unwrap_or_else(|| format!("yt-dlp failed ({s})"))),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(DownloadMsg::Done(result));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_into_the_folder_url_last() {
        let a = args(Path::new("/music"), "Lofi Mix", "-https://x.test/p");
        let after = |flag: &str| &a[a.iter().position(|x| x == flag).unwrap() + 1];
        assert!(a.contains(&"-x".to_string()));
        assert_eq!(after("--audio-format"), "mp3");
        assert_eq!(after("-o"), "/music/Lofi Mix/%(playlist_index)s - %(title)s.%(ext)s");
        assert_eq!(after("--download-archive"), "/music/.orbit_dl_archive");
        assert_eq!(a[a.len() - 2..], ["--", "-https://x.test/p"]);
    }

    #[test]
    fn reads_playlist_progress() {
        assert_eq!(parse_progress("[download] Downloading item 5 of 25"), Some((5, 25)));
        assert_eq!(parse_progress("[download] Downloading video 1 of 3"), Some((1, 3)));
        assert_eq!(parse_progress("[download] Destination: foo.mp3"), None);
        assert_eq!(parse_progress("[download] Downloading item x of 3"), None);
    }
}
