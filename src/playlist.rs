//! User playlists (M3U8 files in the data dir, plus any .m3u/.m3u8 found in music folders) and
//! smart playlists computed from play statistics.
//!
//! orbit writes extended M3U in UTF-8: `#EXTM3U`, a `#PLAYLIST:` title (so names survive file-name
//! sanitizing), then `#EXTINF:secs,Artist - Title` and the absolute path of each track. Playlists
//! found in music folders ("external") are never modified: editing or renaming one saves a copy in
//! the data dir, and deleting one only hides it from orbit.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::library::{Library, Track, TrackId};
use crate::state::{State, write_atomic};

/// In the playlists dir: music-folder playlists deleted, renamed or edited in orbit (the originals
/// stay untouched, so orbit remembers not to show them). One absolute path per line.
const HIDDEN_LIST: &str = ".hidden-external";
/// How deep to look for playlists inside music folders.
const MAX_DEPTH: usize = 16;
/// Directory entries to look at per music folder before giving up, so a huge `library.dirs` (the
/// whole home folder, say) can't stall startup.
/// ponytail: synchronous walk with a budget; finding playlists during the background library scan
/// would lift the ceiling.
const MAX_ENTRIES: usize = 200_000;
/// Longest file stem (in bytes) made from a playlist name.
const MAX_STEM: usize = 150;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playlist {
    pub name: String,
    /// The .m3u8 file backing it.
    pub path: PathBuf,
    pub tracks: Vec<PathBuf>,
    /// Imported from a music folder (read-only unless saved into the data dir).
    pub external: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Playlists {
    pub dir: PathBuf,
    /// Sorted by name (case-insensitive).
    pub lists: Vec<Playlist>,
}

impl Playlists {
    /// Load `dir/*.m3u8|*.m3u` (creating `dir` if missing) plus playlists found recursively in
    /// `music_dirs` (external). Names come from `#PLAYLIST:` or the file name; clashes get a
    /// " (folder)" or " (2)" suffix. Music-folder playlists deleted/replaced in orbit stay hidden.
    pub fn load(dir: &Path, music_dirs: &[PathBuf]) -> Playlists {
        let _ = std::fs::create_dir_all(dir);
        let mut pls = Playlists { dir: dir.to_path_buf(), lists: Vec::new() };
        for path in playlist_files(dir, 0) {
            pls.load_file(path, false);
        }
        let hidden: HashSet<PathBuf> = pls.hidden().into_iter().map(PathBuf::from).collect();
        let mut seen = HashSet::new();
        for root in music_dirs {
            for path in playlist_files(root, MAX_DEPTH) {
                if !path.starts_with(dir) && !hidden.contains(&path) && seen.insert(path.clone()) {
                    pls.load_file(path, true);
                }
            }
        }
        pls.lists.sort_by_cached_key(|p| sort_key(&p.name));
        pls
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        let name = name.trim();
        self.lists.iter().position(|p| same_name(&p.name, name))
    }

    /// Create an empty playlist (file written immediately). Err if the name is empty/taken/invalid.
    pub fn create(&mut self, name: &str) -> Result<usize, String> {
        self.create_with(name, Vec::new(), None)
    }

    /// Renames the file too. An external playlist is saved under the new name in the data dir.
    pub fn rename(&mut self, idx: usize, new_name: &str) -> Result<(), String> {
        let old = self.get(idx)?.clone();
        let name = self.check_name(new_name, Some(idx))?;
        let mut pl = Playlist { name, external: false, ..old.clone() };
        if old.external {
            pl.path = self.free_path(&pl.name, "m3u8", None);
            save(&pl, None, Some(&old.path))?;
            let _ = self.hide(&old.path); // best effort: the copy is saved either way
        } else {
            let ext = old.path.extension().map_or_else(|| "m3u8".into(), |e| e.to_string_lossy().into_owned());
            pl.path = self.free_path(&pl.name, &ext, Some(&old.path));
            if pl.path != old.path {
                match std::fs::rename(&old.path, &pl.path) {
                    Err(e) if e.kind() != ErrorKind::NotFound => return Err(format!("can't rename {}: {e}", old.path.display())),
                    _ => {}
                }
            }
            if let Err(e) = save(&pl, None, Some(&pl.path)) {
                if pl.path != old.path {
                    let _ = std::fs::rename(&pl.path, &old.path);
                }
                return Err(e);
            }
        }
        self.lists.remove(idx);
        self.insert(pl);
        Ok(())
    }

    /// Deletes the file too. An external playlist's file is left alone; it's just hidden.
    pub fn delete(&mut self, idx: usize) -> Result<(), String> {
        let pl = self.get(idx)?;
        if pl.external {
            self.hide(&pl.path)?;
        } else {
            match std::fs::remove_file(&pl.path) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(format!("can't delete {}: {e}", pl.path.display())),
                _ => {}
            }
        }
        self.lists.remove(idx);
        Ok(())
    }

    /// Append tracks and save (#EXTINF from `lib`). Returns how many were added (duplicates are allowed).
    pub fn add(&mut self, idx: usize, paths: &[PathBuf], lib: &Library) -> Result<usize, String> {
        if paths.is_empty() {
            return self.get(idx).map(|_| 0);
        }
        self.edit(idx, Some(lib), |tracks| {
            tracks.extend_from_slice(paths);
            Ok(paths.len())
        })
    }

    pub fn remove_track(&mut self, idx: usize, track_idx: usize) -> Result<(), String> {
        self.edit(idx, None, |tracks| {
            if track_idx >= tracks.len() {
                return Err("no such track in the playlist".into());
            }
            tracks.remove(track_idx);
            Ok(())
        })
    }

    pub fn move_track(&mut self, idx: usize, from: usize, to: usize) -> Result<(), String> {
        let len = self.get(idx)?.tracks.len();
        if from >= len || to >= len {
            return Err("no such track in the playlist".into());
        }
        if from == to {
            return Ok(());
        }
        self.edit(idx, None, |tracks| {
            let t = tracks.remove(from);
            tracks.insert(to, t);
            Ok(())
        })
    }

    /// Create or overwrite playlist `name` with `paths` (used by "save queue").
    pub fn save_as(&mut self, name: &str, paths: &[PathBuf], lib: &Library) -> Result<usize, String> {
        match self.find(name) {
            Some(idx) => self.edit(idx, Some(lib), |tracks| {
                *tracks = paths.to_vec();
                Ok(idx)
            }),
            None => self.create_with(name, paths.to_vec(), Some(lib)),
        }
    }

    // ---- internals ----

    fn get(&self, idx: usize) -> Result<&Playlist, String> {
        self.lists.get(idx).ok_or_else(|| "no such playlist".to_string())
    }

    fn create_with(&mut self, name: &str, tracks: Vec<PathBuf>, lib: Option<&Library>) -> Result<usize, String> {
        let name = self.check_name(name, None)?;
        let pl = Playlist { path: self.free_path(&name, "m3u8", None), name, tracks, external: false };
        save(&pl, lib, None)?;
        Ok(self.insert(pl))
    }

    /// Apply `change` to the tracks of playlist `idx` and save. An external playlist becomes a
    /// copy in the data dir (and the original is hidden): the music folder is never written.
    fn edit<R>(
        &mut self,
        idx: usize,
        lib: Option<&Library>,
        change: impl FnOnce(&mut Vec<PathBuf>) -> Result<R, String>,
    ) -> Result<R, String> {
        let old = self.get(idx)?.clone();
        let mut pl = old.clone();
        let out = change(&mut pl.tracks)?;
        if old.external {
            pl.path = self.free_path(&pl.name, "m3u8", None);
            pl.external = false;
        }
        save(&pl, lib, Some(&old.path))?;
        if old.external {
            let _ = self.hide(&old.path); // best effort: the copy is saved either way
        }
        self.lists[idx] = pl;
        Ok(out)
    }

    /// Insert keeping `lists` sorted; returns the new index.
    fn insert(&mut self, pl: Playlist) -> usize {
        let key = sort_key(&pl.name);
        let at = self.lists.partition_point(|p| sort_key(&p.name) <= key);
        self.lists.insert(at, pl);
        at
    }

    /// The trimmed name if it's usable for playlist `except` (None = a new playlist).
    fn check_name(&self, name: &str, except: Option<usize>) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            Err("playlist name can't be empty".into())
        } else if name.contains('/') {
            Err("playlist names can't contain \"/\"".into())
        } else if name.chars().any(char::is_control) {
            Err("playlist names can't contain control characters".into())
        } else if self.name_taken(name, except) {
            Err(format!("there is already a playlist called \"{name}\""))
        } else {
            Ok(name.to_string())
        }
    }

    /// Case-insensitive clash with another playlist or a smart playlist.
    fn name_taken(&self, name: &str, except: Option<usize>) -> bool {
        Smart::ALL.iter().any(|s| same_name(s.label(), name))
            || self.lists.iter().enumerate().any(|(i, p)| Some(i) != except && same_name(&p.name, name))
    }

    /// `base` if free, else `base (hint)`, `base (2)`, `base (3)`, …
    fn free_name(&self, base: &str, hint: Option<&str>) -> String {
        std::iter::once(base.to_string())
            .chain(hint.map(|h| format!("{base} ({h})")))
            .chain((2..).map(|n| format!("{base} ({n})")))
            .find(|n| !self.name_taken(n, None))
            .unwrap_or_default()
    }

    /// A path in `dir` for playlist `name` that no other file or playlist uses: `Name.ext`,
    /// `Name (2).ext`, … `own` (the playlist's current file) counts as free, so renaming "mix" to
    /// "Mix" keeps its file even on a case-insensitive file system.
    fn free_path(&self, name: &str, ext: &str, own: Option<&Path>) -> PathBuf {
        let stem = file_stem_for(name);
        (1..)
            .map(|n| self.dir.join(if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") }))
            .find(|p| {
                own.is_some_and(|o| p == o || same_file(p, o))
                    || (std::fs::symlink_metadata(p).is_err() && !self.lists.iter().any(|l| l.path == *p))
            })
            .unwrap_or_default()
    }

    fn load_file(&mut self, path: PathBuf, external: bool) {
        let Ok(m3u) = M3u::read(&path) else { return };
        let base = m3u.title.clone().unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
        let folder = path.parent().and_then(Path::file_name).map(|f| f.to_string_lossy().into_owned());
        let name = self.free_name(&base, folder.as_deref().filter(|_| external));
        self.lists.push(Playlist { name, tracks: m3u.paths(), path, external });
    }

    /// Paths listed in the hidden-externals file.
    fn hidden(&self) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.join(HIDDEN_LIST)).unwrap_or_default();
        text.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect()
    }

    /// Stop showing the music-folder playlist at `path` (its file is never touched).
    fn hide(&self, path: &Path) -> Result<(), String> {
        let mut hidden = self.hidden();
        let line = path.to_string_lossy().into_owned();
        if hidden.contains(&line) {
            return Ok(());
        }
        hidden.push(line);
        let list = self.dir.join(HIDDEN_LIST);
        write_atomic(&list, (hidden.join("\n") + "\n").as_bytes()).map_err(|e| format!("can't write {}: {e}", list.display()))
    }
}

/// Write `pl` to its file. #EXTINF info comes from `lib`, else from the same entry in `carry` (the
/// file being replaced), so edits made without the library at hand keep it.
fn save(pl: &Playlist, lib: Option<&Library>, carry: Option<&Path>) -> Result<(), String> {
    let known: HashMap<PathBuf, String> = carry
        .and_then(|p| M3u::read(p).ok())
        .map(|m| m.entries.into_iter().filter_map(|(p, info)| Some((p, info?))).collect())
        .unwrap_or_default();
    let info = |p: &Path| lib.and_then(|l| track_info(l, p)).or_else(|| known.get(p).cloned());
    write_atomic(&pl.path, render(Some(&pl.name), &pl.tracks, &info).as_bytes())
        .map_err(|e| format!("can't save {}: {e}", pl.path.display()))
}

/// Read an .m3u/.m3u8: comments skipped, relative paths resolved against the file's folder.
/// Handles `file://` URLs, a UTF-8 (or UTF-16) byte-order mark, CRLF, `~/` and Windows-style
/// separators in relative paths; other URLs (streams) are skipped. Paths come back absolute and
/// normalized (no `.`/`..`), matching how the library stores them.
#[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
pub fn read_m3u(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(M3u::read(path)?.paths())
}

/// Write an extended M3U8 (#EXTM3U, #EXTINF:secs,Artist - Title) with absolute paths; atomic.
/// Tracks the library doesn't know get no #EXTINF line.
#[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
pub fn write_m3u(path: &Path, tracks: &[PathBuf], lib: &Library) -> std::io::Result<()> {
    write_atomic(path, render(None, tracks, &|p| track_info(lib, p)).as_bytes())
}

/// A parsed playlist file.
struct M3u {
    /// From `#PLAYLIST:`.
    title: Option<String>,
    /// Resolved paths, each with the payload of the `#EXTINF:` line before it.
    entries: Vec<(PathBuf, Option<String>)>,
}

impl M3u {
    fn read(path: &Path) -> std::io::Result<M3u> {
        let text = decode(&std::fs::read(path)?);
        let path = std::path::absolute(path)?;
        Ok(M3u::parse(&text, path.parent().unwrap_or(Path::new("/"))))
    }

    fn parse(text: &str, base: &Path) -> M3u {
        let mut m3u = M3u { title: None, entries: Vec::new() };
        let mut info = None;
        for line in text.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(directive) = line.strip_prefix('#') {
                if let Some(v) = strip_prefix_ci(directive, "EXTINF:") {
                    info = Some(v.trim().to_string());
                } else if let Some(v) = strip_prefix_ci(directive, "PLAYLIST:") {
                    m3u.title = Some(v.trim().to_string()).filter(|t| !t.is_empty());
                }
            } else if let Some(path) = resolve(line, base) {
                m3u.entries.push((path, info.take()));
            } else {
                info = None;
            }
        }
        m3u
    }

    fn paths(self) -> Vec<PathBuf> {
        self.entries.into_iter().map(|(p, _)| p).collect()
    }
}

/// UTF-8 (BOM optional) or UTF-16 with a BOM; anything else that isn't UTF-8 is read as Latin-1
/// (legacy .m3u files).
fn decode(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], unit: fn([u8; 2]) -> u16| String::from_utf16_lossy(&b.as_chunks::<2>().0.iter().map(|&c| unit(c)).collect::<Vec<_>>());
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        String::from_utf8_lossy(rest).into_owned()
    } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        utf16(rest, u16::from_be_bytes)
    } else {
        String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| bytes.iter().map(|&b| char::from(b)).collect())
    }
}

/// One playlist entry as an absolute, normalized path. None for URLs that aren't `file:`.
fn resolve(entry: &str, base: &Path) -> Option<PathBuf> {
    let path = if let Some(rest) = strip_prefix_ci(entry, "file:") {
        let rest = rest.strip_prefix("//").map_or(rest, |r| r.strip_prefix("localhost").unwrap_or(r));
        if !rest.starts_with('/') {
            return None; // file://otherhost/...
        }
        PathBuf::from(OsString::from_vec(percent_decode(rest)))
    } else if has_scheme(entry) {
        return None;
    } else if entry.starts_with("~/") {
        crate::config::expand_tilde(entry)
    } else if entry.contains('\\') && !entry.contains('/') {
        base.join(entry.replace('\\', "/")) // made on Windows: "Artist\Album\01.mp3"
    } else {
        base.join(entry) // an absolute entry replaces `base`
    };
    Some(normalize(&path))
}

/// "http://…", "rtsp://…": a URL scheme before "://".
fn has_scheme(s: &str) -> bool {
    s.split_once("://").is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    })
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let hex = |i: usize| b.get(i).and_then(|c| char::from(*c).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Resolve `.` and `..` lexically (no file system access, like the paths the library stores).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// Extended M3U text: absolute paths, `info` gives each track's #EXTINF payload.
fn render(title: Option<&str>, tracks: &[PathBuf], info: &dyn Fn(&Path) -> Option<String>) -> String {
    let mut out = String::from("#EXTM3U\n");
    if let Some(title) = title {
        out += &format!("#PLAYLIST:{}\n", one_line(title));
    }
    for path in tracks {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.clone());
        let line = abs.to_string_lossy();
        if line.contains(['\n', '\r']) {
            continue; // not representable in M3U
        }
        if let Some(info) = info(path) {
            out += &format!("#EXTINF:{info}\n");
        }
        out += &line;
        out.push('\n');
    }
    out
}

/// "secs,Artist - Title" (secs = -1 when unknown).
fn track_info(lib: &Library, path: &Path) -> Option<String> {
    let t = lib.get(lib.find(path)?)?;
    let secs = if t.duration.is_zero() { -1 } else { t.duration.as_secs_f64().round() as i64 };
    Some(format!("{secs},{} - {}", one_line(&t.artist), one_line(&t.title)))
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// Playlist files in `dir` and its subfolders down to `depth` levels (hidden entries and symlinked
/// folders skipped, at most MAX_ENTRIES entries looked at), sorted.
fn playlist_files(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let is_playlist = |p: &Path| {
        p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("m3u8") || e.eq_ignore_ascii_case("m3u"))
    };
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0)];
    let mut budget = MAX_ENTRIES;
    while let Some((d, level)) = stack.pop().filter(|_| budget > 0) {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten().take(budget) {
            budget -= 1;
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    if level < depth {
                        stack.push((p, level + 1));
                    }
                }
                Ok(_) if is_playlist(&p) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// A portable file stem for a playlist name: characters invalid on common file systems become
/// '_', no leading dot (hidden file) or trailing dots/spaces, at most MAX_STEM bytes.
fn file_stem_for(name: &str) -> String {
    let mut stem: String = name.trim().chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    if stem.starts_with('.') {
        stem.replace_range(..1, "_");
    }
    let mut end = stem.len().min(MAX_STEM);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    stem.truncate(end);
    let stem = stem.trim_end_matches(['.', ' ']);
    if stem.is_empty() { "playlist".into() } else { stem.to_string() }
}

fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

fn sort_key(name: &str) -> (String, String) {
    (name.to_lowercase(), name.to_string())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Smart {
    Favorites,
    MostPlayed,
    RecentlyPlayed,
    RecentlyAdded,
    NeverPlayed,
    /// What sounds like your recent and most played tracks (filled in by `App`, see `radio`).
    Radio,
}

impl Smart {
    pub const ALL: [Smart; 6] =
        [Smart::Favorites, Smart::MostPlayed, Smart::RecentlyPlayed, Smart::RecentlyAdded, Smart::NeverPlayed, Smart::Radio];
    pub fn label(self) -> &'static str {
        match self {
            Smart::Favorites => "Favorites",
            Smart::MostPlayed => "Most Played",
            Smart::RecentlyPlayed => "Recently Played",
            Smart::RecentlyAdded => "Recently Added",
            Smart::NeverPlayed => "Never Played",
            Smart::Radio => "Radio",
        }
    }
}

/// Tracks for a smart playlist, best first, at most `limit` (0 = no limit).
/// Favorites: by artist, album, disc, track. Most Played: plays, then most recently played.
/// Recently Played: last played (played tracks only). Recently Added: newest file first.
/// Never Played: unplayed, newest first. Remaining ties keep library order.
pub fn smart_tracks(kind: Smart, lib: &Library, state: &State, limit: usize) -> Vec<TrackId> {
    let stats = |t: &Track| state.stats.get(&*t.path.to_string_lossy()).cloned().unwrap_or_default();
    let mut picked: Vec<&Track> = lib
        .tracks
        .iter()
        .filter(|t| {
            let s = stats(t);
            match kind {
                Smart::Favorites => s.favorite,
                Smart::MostPlayed => s.plays > 0,
                Smart::RecentlyPlayed => s.last_played > 0,
                Smart::RecentlyAdded => true,
                Smart::NeverPlayed => s.plays == 0,
                Smart::Radio => false,
            }
        })
        .collect();
    match kind {
        Smart::Favorites => picked.sort_by_cached_key(|t| {
            (t.artist.to_lowercase(), t.album.to_lowercase(), t.disc_no, t.track_no, t.title.to_lowercase(), t.id)
        }),
        Smart::MostPlayed => picked.sort_by_cached_key(|t| {
            let s = stats(t);
            (Reverse(s.plays), Reverse(s.last_played), t.id)
        }),
        Smart::RecentlyPlayed => picked.sort_by_cached_key(|t| (Reverse(stats(t).last_played), t.id)),
        Smart::RecentlyAdded | Smart::NeverPlayed | Smart::Radio => picked.sort_by_key(|t| (Reverse(t.mtime), t.id)),
    }
    let n = if limit == 0 { picked.len() } else { limit.min(picked.len()) };
    picked[..n].iter().map(|t| t.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TempDir;
    use std::time::Duration;

    fn track(path: &str, title: &str, artist: &str, album: &str, secs: u64, mtime: u64) -> Track {
        Track {
            path: path.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration: Duration::from_secs(secs),
            mtime,
            ..Track::default()
        }
    }

    fn library() -> Library {
        Library::from_tracks(
            vec!["/m".into()],
            vec![
                track("/m/a.mp3", "Alpha", "Zed", "Z1", 185, 100),
                track("/m/b.mp3", "Beta", "amy", "B1", 0, 300),
                track("/m/c.mp3", "Gamma", "Amy", "A1", 61, 200),
                track("/m/d.mp3", "Delta\nNewline", "Bob", "D1", 30, 400),
            ],
        )
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn paths(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn read_m3u_handles_the_usual_mess() {
        let dir = TempDir::new("m3u-read");
        let list = dir.join("lists/mix.m3u8");
        let text = "\u{feff}#EXTM3U\r\n\
            #PLAYLIST:My Mix\r\n\
            #EXTINF:185,Zed - Alpha\r\n\
            /m/a.mp3\r\n\
            \r\n\
            # a comment\r\n\
            song.mp3\r\n\
            ../up/./other.flac\r\n\
            sub\\win\\style.mp3\r\n\
            file:///m/with%20space%E3%81%82.mp3\r\n\
            file://localhost/m/local.mp3\r\n\
            FILE:/m/short.mp3\r\n\
            http://radio.example/stream\r\n\
            file://otherhost/share/x.mp3\r\n\
              /m/padded.mp3  \n\
            /m/../m/./dots.mp3";
        write(&list, text);
        let lists = dir.join("lists");
        assert_eq!(
            read_m3u(&list).unwrap(),
            vec![
                PathBuf::from("/m/a.mp3"),
                lists.join("song.mp3"),
                dir.join("up/other.flac"),
                lists.join("sub/win/style.mp3"),
                PathBuf::from("/m/with spaceあ.mp3"),
                PathBuf::from("/m/local.mp3"),
                PathBuf::from("/m/short.mp3"),
                PathBuf::from("/m/padded.mp3"),
                PathBuf::from("/m/dots.mp3"),
            ]
        );
        let m3u = M3u::read(&list).unwrap();
        assert_eq!(m3u.title.as_deref(), Some("My Mix"));
        assert_eq!(m3u.entries[0].1.as_deref(), Some("185,Zed - Alpha"));
        assert_eq!(m3u.entries[1].1, None, "#EXTINF belongs to one entry only");
        assert!(read_m3u(&dir.join("missing.m3u")).is_err());
    }

    #[test]
    fn read_m3u_decodes_utf16_and_latin1() {
        let dir = TempDir::new("m3u-enc");
        let utf16 = dir.join("u16.m3u");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("#EXTM3U\n/m/日本.mp3\n".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&utf16, bytes).unwrap();
        assert_eq!(read_m3u(&utf16).unwrap(), paths(&["/m/日本.mp3"]));
        let latin1 = dir.join("l1.m3u");
        std::fs::write(&latin1, b"/m/caf\xe9.mp3\n").unwrap();
        assert_eq!(read_m3u(&latin1).unwrap(), paths(&["/m/café.mp3"]));
    }

    #[test]
    fn write_m3u_round_trips_with_extinf() {
        let dir = TempDir::new("m3u-write");
        let lib = library();
        let path = dir.join("out/list.m3u8");
        let tracks = paths(&["/m/a.mp3", "/m/b.mp3", "/elsewhere/x.mp3", "/m/d.mp3"]);
        write_m3u(&path, &tracks, &lib).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "#EXTM3U\n#EXTINF:185,Zed - Alpha\n/m/a.mp3\n#EXTINF:-1,amy - Beta\n/m/b.mp3\n/elsewhere/x.mp3\n#EXTINF:30,Bob - Delta Newline\n/m/d.mp3\n"
        );
        assert_eq!(read_m3u(&path).unwrap(), tracks);
        write_m3u(&path, &tracks[..1], &lib).unwrap();
        assert_eq!(read_m3u(&path).unwrap(), tracks[..1]);
        assert_eq!(dir.listing(), ["out", "out/list.m3u8"], "atomic write leaves no temp files");
    }

    #[test]
    fn load_finds_user_and_external_playlists() {
        let dir = TempDir::new("pl-load");
        let data = dir.join("data/playlists");
        let music = dir.join("music");
        write(&data.join("zeta.m3u8"), "#EXTM3U\n/m/a.mp3\n");
        write(&data.join("Alpha.m3u"), "/m/b.mp3\n/m/c.mp3\n");
        write(&data.join("titled.m3u8"), "#EXTM3U\n#PLAYLIST:beta: the title\n");
        write(&data.join("notes.txt"), "not a playlist");
        write(&music.join("Jpop/best.m3u"), "01.mp3\n");
        write(&music.join("Kpop/deep/er/best.M3U8"), "02.mp3\n");
        write(&music.join("Kpop/Zeta.m3u"), "03.mp3\n");
        write(&music.join(".hidden/secret.m3u"), "x.mp3\n");
        write(&music.join("Jpop/song.mp3"), "");

        let pls = Playlists::load(&data, &[music.clone(), music.join("Jpop")]);
        let names: Vec<&str> = pls.lists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "best", "best (er)", "beta: the title", "zeta", "Zeta (Kpop)"]);
        let best = &pls.lists[1];
        assert!(best.external);
        assert_eq!(best.tracks, [music.join("Jpop/01.mp3")], "relative to the playlist's folder");
        assert!(!pls.lists[0].external);
        assert_eq!(pls.find("ALPHA"), Some(0));
        assert_eq!(pls.find(" zeta "), Some(4));
        assert_eq!(pls.find("nope"), None);

        let fresh = dir.join("new/dir");
        assert!(Playlists::load(&fresh, &[]).lists.is_empty());
        assert!(fresh.is_dir(), "the playlists dir is created");
    }

    #[test]
    fn create_rename_delete() {
        let dir = TempDir::new("pl-crud");
        let mut pls = Playlists::load(&dir.0, &[]);
        assert_eq!(pls.create("Road Trip"), Ok(0));
        assert_eq!(pls.create("chill"), Ok(0), "sorted case-insensitively");
        assert_eq!(pls.create("Zzz"), Ok(2));
        assert!(dir.join("Road Trip.m3u8").is_file());
        for bad in ["", "   ", "a/b", "ROAD TRIP", "favorites", "tab\there"] {
            assert!(pls.create(bad).is_err(), "{bad:?} should be rejected");
        }

        let i = pls.find("road trip").unwrap();
        pls.rename(i, "Highway").unwrap();
        assert!(!dir.join("Road Trip.m3u8").exists());
        assert!(dir.join("Highway.m3u8").is_file());
        assert_eq!(pls.lists.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["chill", "Highway", "Zzz"]);
        assert!(pls.rename(0, "zzz").is_err(), "taken");
        pls.rename(0, "Chill").unwrap();
        assert_eq!(pls.lists[0].name, "Chill", "case-only rename");
        assert_eq!(dir.listing(), ["Chill.m3u8", "Highway.m3u8", "Zzz.m3u8"]);

        pls.delete(pls.find("zzz").unwrap()).unwrap();
        assert!(!dir.join("Zzz.m3u8").exists());
        assert!(pls.delete(7).is_err());

        let again = Playlists::load(&dir.0, &[]);
        assert_eq!(again.lists.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Chill", "Highway"]);
    }

    #[test]
    fn names_that_need_sanitizing_survive_a_reload() {
        let dir = TempDir::new("pl-sanitize");
        let mut pls = Playlists::load(&dir.0, &[]);
        pls.create("mix: vol?1").unwrap();
        pls.create("mix_ vol_1").unwrap();
        pls.create(".hidden").unwrap();
        pls.create(&"長い名前".repeat(40)).unwrap();
        let mut files = dir.listing();
        files.retain(|f| !f.starts_with('長'));
        assert_eq!(files, ["_hidden.m3u8", "mix_ vol_1 (2).m3u8", "mix_ vol_1.m3u8"]);
        let again = Playlists::load(&dir.0, &[]);
        let mut names: Vec<String> = again.lists.iter().map(|p| p.name.clone()).collect();
        names.sort();
        assert!(names.contains(&"mix: vol?1".to_string()) && names.contains(&"mix_ vol_1".to_string()));
        assert!(names.contains(&".hidden".to_string()) && names.contains(&"長い名前".repeat(40)));
        assert!(again.lists.iter().all(|p| p.path.file_name().unwrap().len() <= MAX_STEM + ".m3u8".len()));
    }

    #[test]
    fn edits_persist_immediately() {
        let dir = TempDir::new("pl-edit");
        let lib = library();
        let mut pls = Playlists::load(&dir.0, &[]);
        let i = pls.create("list").unwrap();
        assert_eq!(pls.add(i, &paths(&["/m/a.mp3", "/m/b.mp3", "/m/a.mp3"]), &lib), Ok(3));
        assert!(std::fs::read_to_string(&pls.lists[i].path).unwrap().contains("#EXTINF:185,Zed - Alpha\n/m/a.mp3\n"), "added tracks get #EXTINF");
        assert_eq!(pls.add(i, &[], &lib), Ok(0));
        pls.move_track(i, 2, 0).unwrap();
        pls.remove_track(i, 1).unwrap();
        assert!(pls.remove_track(i, 5).is_err());
        assert!(pls.move_track(i, 0, 5).is_err());
        assert!(pls.add(9, &paths(&["/m/a.mp3"]), &lib).is_err());
        let expected = paths(&["/m/a.mp3", "/m/b.mp3"]);
        assert_eq!(pls.lists[i].tracks, expected);
        assert_eq!(Playlists::load(&dir.0, &[]).lists[0].tracks, expected);

        // save_as overwrites by name (any case) and writes #EXTINF from the library...
        assert_eq!(pls.save_as("LIST", &paths(&["/m/c.mp3", "/m/a.mp3"]), &lib), Ok(i));
        let text = std::fs::read_to_string(&pls.lists[i].path).unwrap();
        assert!(text.starts_with("#EXTM3U\n#PLAYLIST:list\n#EXTINF:61,Amy - Gamma\n/m/c.mp3\n"));
        // ...which later edits keep (a track the library doesn't know gets none)
        pls.add(i, &paths(&["/m/new.mp3"]), &lib).unwrap();
        let text = std::fs::read_to_string(&pls.lists[i].path).unwrap();
        assert!(text.contains("#EXTINF:61,Amy - Gamma\n/m/c.mp3\n#EXTINF:185,Zed - Alpha\n/m/a.mp3\n/m/new.mp3\n"));
        // and save_as creates new playlists too
        let j = pls.save_as("queue", &paths(&["/m/b.mp3"]), &lib).unwrap();
        assert_eq!(pls.lists[j].name, "queue");
        assert_eq!(dir.listing(), ["list.m3u8", "queue.m3u8"]);
    }

    #[test]
    fn external_playlists_are_never_modified() {
        let dir = TempDir::new("pl-external");
        let data = dir.join("data");
        let music = dir.join("music");
        let original = music.join("Jpop/mix.m3u");
        let original_text = "#EXTINF:10,A - B\n01.mp3\n02.mp3\n";
        write(&original, original_text);
        write(&music.join("Kpop/gone.m3u"), "x.mp3\n");
        write(&music.join("Kpop/old name.m3u"), "y.mp3\n");

        let mut pls = Playlists::load(&data, std::slice::from_ref(&music));
        let i = pls.find("mix").unwrap();
        assert!(pls.lists[i].external);
        pls.add(i, &paths(&["/m/a.mp3"]), &Library::empty()).unwrap();
        let pl = &pls.lists[i];
        assert!(!pl.external, "edited: now a regular playlist");
        assert_eq!(pl.path, data.join("mix.m3u8"));
        assert_eq!(std::fs::read_to_string(&original).unwrap(), original_text, "music folder untouched");
        let copy = std::fs::read_to_string(&pl.path).unwrap();
        assert!(copy.contains(&format!("#EXTINF:10,A - B\n{}\n", music.join("Jpop/01.mp3").display())), "{copy}");

        pls.delete(pls.find("gone").unwrap()).unwrap();
        assert!(music.join("Kpop/gone.m3u").exists(), "delete only hides it");
        pls.rename(pls.find("old name").unwrap(), "new name").unwrap();
        assert!(music.join("Kpop/old name.m3u").exists());

        let again = Playlists::load(&data, &[music]);
        let summary: Vec<(&str, bool)> = again.lists.iter().map(|p| (p.name.as_str(), p.external)).collect();
        assert_eq!(summary, [("mix", false), ("new name", false)], "no duplicates, deleted stays gone");
        assert_eq!(again.lists[0].tracks.len(), 3);
    }

    #[test]
    fn smart_playlists() {
        let lib = library();
        let mut state = State::default();
        let p = |s: &str| PathBuf::from(s);
        state.stats.insert("/m/a.mp3".into(), crate::state::TrackStats { plays: 5, last_played: 50, ..Default::default() });
        state.stats.insert("/m/b.mp3".into(), crate::state::TrackStats { plays: 5, last_played: 90, favorite: true, ..Default::default() });
        state.stats.insert("/m/c.mp3".into(), crate::state::TrackStats { plays: 1, last_played: 200, favorite: true, ..Default::default() });
        state.set_favorite(&p("/m/d.mp3"), true);
        let ids = |kind, limit| smart_tracks(kind, &lib, &state, limit);
        let path = |id: TrackId| lib.get(id).unwrap().path.clone();
        let named = |v: Vec<TrackId>| v.into_iter().map(|id| path(id).file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();

        assert_eq!(named(ids(Smart::Favorites, 0)), ["c.mp3", "b.mp3", "d.mp3"], "by artist (any case), then album");
        assert_eq!(named(ids(Smart::MostPlayed, 0)), ["b.mp3", "a.mp3", "c.mp3"], "ties: most recently played first");
        assert_eq!(named(ids(Smart::RecentlyPlayed, 0)), ["c.mp3", "b.mp3", "a.mp3"]);
        assert_eq!(named(ids(Smart::RecentlyAdded, 2)), ["d.mp3", "b.mp3"]);
        assert_eq!(named(ids(Smart::NeverPlayed, 0)), ["d.mp3"]);
        assert_eq!(ids(Smart::RecentlyAdded, 0).len(), 4, "0 = no limit");
        assert!(smart_tracks(Smart::Favorites, &Library::empty(), &state, 0).is_empty());
    }
}
