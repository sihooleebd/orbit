//! Persistent state (data_dir/state.json): the last session plus per-track statistics.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, Serializer};

use crate::app::Tab;
use crate::config::EqSettings;
use crate::library::BrowseMode;
use crate::queue::Repeat;
use crate::visualizer::VisMode;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct State {
    pub session: Session,
    /// Keyed by the track's absolute path (as a string, for JSON).
    #[serde(serialize_with = "sorted")]
    pub stats: HashMap<String, TrackStats>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Session {
    pub queue: Vec<PathBuf>,
    pub current: Option<usize>,
    pub position_secs: f64,
    /// None until a session is saved: playback.shuffle / playback.repeat apply.
    pub shuffle: Option<bool>,
    /// The shuffled play order (indices into `queue`), so a restart continues the pass.
    pub order: Vec<usize>,
    pub repeat: Option<Repeat>,
    pub volume: Option<u8>,
    pub muted: bool,
    pub speed: f32,
    pub eq: Option<EqSettings>,
    pub tab: Option<Tab>,
    pub browse_mode: Option<BrowseMode>,
    pub theme: Option<String>,
    /// ui.theme when `theme` was saved: once the config names another theme, that one wins.
    pub theme_base: Option<String>,
    pub vis_mode: Option<VisMode>,
    /// Per-track lyric offsets in ms, keyed by path.
    #[serde(serialize_with = "sorted")]
    pub lyrics_offsets: HashMap<String, i32>,
    pub command_history: Vec<String>,
    /// Runtime view toggles, like `tab`, `browse_mode` and `vis_mode` saved only when they differ
    /// from the config (None = follow the config).
    pub compact: Option<bool>,
    pub show_art: Option<bool>,
    pub show_lyrics: Option<bool>,
    pub mini_visualizer: Option<bool>,
    pub time_remaining: Option<bool>,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            queue: Vec::new(),
            current: None,
            position_secs: 0.0,
            shuffle: None,
            order: Vec::new(),
            repeat: None,
            volume: None,
            muted: false,
            speed: 1.0,
            eq: None,
            tab: None,
            browse_mode: None,
            theme: None,
            theme_base: None,
            vis_mode: None,
            lyrics_offsets: HashMap::new(),
            command_history: Vec::new(),
            compact: None,
            show_art: None,
            show_lyrics: None,
            mini_visualizer: None,
            time_remaining: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TrackStats {
    pub plays: u32,
    pub skips: u32,
    /// unix seconds, 0 = never
    pub last_played: u64,
    pub favorite: bool,
}

impl State {
    /// Missing file -> default. A corrupt file is moved aside to state.json.bak and defaults are used.
    /// When only parts are unreadable (a bad session field, one bad stats entry), the rest is kept —
    /// play counts and favorites survive — and the original is still backed up.
    pub fn load(path: &Path) -> State {
        let Ok(bytes) = std::fs::read(path) else { return State::default() };
        let (state, intact) = match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => State::salvage(&value),
            Err(_) => (State::default(), false),
        };
        if !intact {
            let mut bak = path.as_os_str().to_owned();
            bak.push(".bak");
            let _ = std::fs::rename(path, bak);
        }
        state
    }

    /// Deserialize, falling back to whatever parts are readable. The bool is false if anything was dropped.
    fn salvage(value: &serde_json::Value) -> (State, bool) {
        if let Ok(state) = State::deserialize(value) {
            return (state, true);
        }
        let session = value.get("session").and_then(|s| Session::deserialize(s).ok()).unwrap_or_default();
        let stats = value
            .get("stats")
            .and_then(|s| s.as_object())
            .map(|entries| entries.iter().filter_map(|(k, v)| Some((k.clone(), TrackStats::deserialize(v).ok()?))).collect())
            .unwrap_or_default();
        (State { session, stats }, false)
    }

    /// Atomic write (temp file + fsync + rename). Creates the parent folder.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn stats(&self, path: &Path) -> TrackStats {
        self.stats.get(&*path.to_string_lossy()).cloned().unwrap_or_default()
    }

    pub fn plays(&self, path: &Path) -> u32 {
        self.stats(path).plays
    }

    pub fn is_favorite(&self, path: &Path) -> bool {
        self.stats(path).favorite
    }

    /// plays += 1, last_played = now.
    pub fn record_play(&mut self, path: &Path) {
        let s = self.entry(path);
        s.plays = s.plays.saturating_add(1);
        s.last_played = unix_now().max(1);
    }

    pub fn record_skip(&mut self, path: &Path) {
        let s = self.entry(path);
        s.skips = s.skips.saturating_add(1);
    }

    /// Returns the new favorite state.
    #[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
    pub fn toggle_favorite(&mut self, path: &Path) -> bool {
        let on = !self.is_favorite(path);
        self.set_favorite(path, on);
        on
    }

    pub fn set_favorite(&mut self, path: &Path, on: bool) {
        let s = self.entry(path);
        s.favorite = on;
        if *s == TrackStats::default() {
            self.stats.remove(&*path.to_string_lossy());
        }
    }

    /// Drop statistics (and lyric offsets) of files that are gone; returns how many stats entries
    /// went. An entry stays if `keep(path)` (e.g. "still in the library"), if it's a favorite whose
    /// file still exists, or if its whole folder is missing — that may be an unmounted drive, which
    /// must not wipe play counts.
    pub fn prune(&mut self, keep: impl Fn(&Path) -> bool) -> usize {
        let folder_missing = |p: &Path| p.parent().is_some_and(|dir| !dir.exists());
        let before = self.stats.len();
        self.stats.retain(|k, s| {
            let p = Path::new(k);
            keep(p) || folder_missing(p) || (s.favorite && p.exists())
        });
        self.session.lyrics_offsets.retain(|k, _| keep(Path::new(k)) || folder_missing(Path::new(k)));
        before - self.stats.len()
    }

    fn entry(&mut self, path: &Path) -> &mut TrackStats {
        self.stats.entry(path.to_string_lossy().into_owned()).or_default()
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Serialize a map with its keys sorted, so state.json is stable and diffable.
fn sorted<V: Serialize, S: Serializer>(map: &HashMap<String, V>, s: S) -> Result<S::Ok, S::Error> {
    s.collect_map(map.iter().collect::<BTreeMap<_, _>>())
}

/// Replace `path` with `bytes` so readers only ever see the old or the new content: write a temp
/// file in the same folder, fsync it, rename it over the target (a symlinked target is followed,
/// so the link survives), then fsync the folder. Creates the folder if needed.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().ok_or_else(|| std::io::Error::other(format!("not a file path: {}", path.display())))?;
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.{}-{}.tmp", name.to_string_lossy(), std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let written = File::create_new(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &target));
    match written {
        Ok(()) => {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// A fresh temp folder for a test, removed on drop.
#[cfg(test)]
pub(crate) struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("orbit-test-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.0.join(p)
    }

    /// Everything in the folder (recursively), relative, sorted: catches stray temp files.
    pub fn listing(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                out.push(p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned());
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(&p, root, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &self.0, &mut out);
        out.sort();
        out
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> State {
        let mut s = State::default();
        s.session.queue = vec!["/m/a.mp3".into(), "/m/b.mp3".into()];
        s.session.current = Some(1);
        s.session.position_secs = 42.5;
        s.session.shuffle = Some(true);
        s.session.repeat = Some(Repeat::All);
        s.session.volume = Some(35);
        s.session.tab = Some(Tab::Queue);
        s.session.theme = Some("nord".into());
        s.session.lyrics_offsets.insert("/m/a.mp3".into(), -250);
        s.session.command_history = vec!["vol 30".into()];
        s.record_play(Path::new("/m/a.mp3"));
        s.set_favorite(Path::new("/m/b.mp3"), true);
        s
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = TempDir::new("state-missing");
        assert_eq!(State::load(&dir.join("state.json")), State::default());
        assert!(dir.listing().is_empty(), "loading creates nothing");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new("state-roundtrip");
        let path = dir.join("deep/er/state.json");
        let state = sample();
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);
        state.save(&path).unwrap();
        assert_eq!(dir.listing(), ["deep", "deep/er", "deep/er/state.json"], "no temp files left behind");
    }

    #[test]
    fn output_is_sorted_and_stable() {
        let dir = TempDir::new("state-stable");
        let path = dir.join("state.json");
        let mut state = State::default();
        for name in ["z", "a", "m", "b", "q"] {
            state.record_skip(Path::new(&format!("/m/{name}.mp3")));
        }
        state.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let keys: Vec<usize> = ["a", "b", "m", "q", "z"].iter().map(|n| text.find(&format!("/m/{n}.mp3")).unwrap()).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "stats keys sorted");
        state.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = TempDir::new("state-corrupt");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(State::load(&path), State::default());
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(dir.join("state.json.bak")).unwrap(), "{ this is not json");
    }

    #[test]
    fn partly_bad_file_keeps_what_it_can() {
        let dir = TempDir::new("state-salvage");
        let path = dir.join("state.json");
        let text = r#"{
            "session": { "volume": "loud" },
            "stats": {
                "/m/a.mp3": { "plays": 3, "favorite": true },
                "/m/b.mp3": { "plays": "many" },
                "/m/c.mp3": { "skips": 1, "some_future_field": [1, 2] }
            },
            "future_section": {}
        }"#;
        std::fs::write(&path, text).unwrap();
        let state = State::load(&path);
        assert_eq!(state.session, Session::default());
        assert_eq!(state.stats(Path::new("/m/a.mp3")), TrackStats { plays: 3, favorite: true, ..TrackStats::default() });
        assert_eq!(state.stats.len(), 2);
        assert_eq!(state.stats(Path::new("/m/c.mp3")).skips, 1);
        assert_eq!(std::fs::read_to_string(dir.join("state.json.bak")).unwrap(), text, "original backed up");
    }

    #[test]
    fn unknown_and_missing_fields_are_fine() {
        let dir = TempDir::new("state-lenient");
        let path = dir.join("state.json");
        std::fs::write(&path, r#"{"session": {"volume": 20, "new_thing": 1}}"#).unwrap();
        let state = State::load(&path);
        assert_eq!(state.session.volume, Some(20));
        assert_eq!(state.session.speed, 1.0);
        assert!(path.exists(), "an intact file is not moved");
    }

    #[test]
    fn save_follows_a_symlinked_state_file() {
        let dir = TempDir::new("state-symlink");
        let real = dir.join("dotfiles/state.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{}").unwrap();
        let link = dir.join("state.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let state = sample();
        state.save(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(State::load(&real), state);
    }

    #[test]
    fn statistics_and_favorites() {
        let mut s = State::default();
        let a = Path::new("/m/a.mp3");
        s.record_play(a);
        s.record_play(a);
        s.record_skip(a);
        let st = s.stats(a);
        assert_eq!((st.plays, st.skips), (2, 1));
        assert!(st.last_played.abs_diff(unix_now()) < 5);
        assert!(s.toggle_favorite(a));
        assert!(s.is_favorite(a));
        assert!(!s.toggle_favorite(a));
        assert_eq!(s.plays(a), 2, "unfavoriting keeps the counts");

        let b = Path::new("/m/b.mp3");
        s.set_favorite(b, true);
        s.set_favorite(b, false);
        assert!(!s.stats.contains_key("/m/b.mp3"), "empty entries are dropped");
        s.set_favorite(b, false);
        assert!(!s.stats.contains_key("/m/b.mp3"));
        assert_eq!(s.stats(Path::new("/nope")), TrackStats::default());
    }

    #[test]
    fn prune_drops_only_what_is_gone() {
        let dir = TempDir::new("state-prune");
        let file = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let (kept, fav, other) = (file("kept.mp3"), file("fav.mp3"), file("other.mp3"));
        let gone = dir.join("gone.mp3");
        let gone_fav = dir.join("gone-fav.mp3");
        let unmounted = PathBuf::from("/Volumes/orbit-test-not-mounted/x.mp3");
        let mut s = State::default();
        for p in [&kept, &fav, &other, &gone, &gone_fav, &unmounted] {
            s.record_play(p);
        }
        s.set_favorite(&fav, true);
        s.set_favorite(&gone_fav, true);
        s.session.lyrics_offsets.insert(gone.to_string_lossy().into_owned(), 100);
        s.session.lyrics_offsets.insert(kept.to_string_lossy().into_owned(), 100);

        let dropped = s.prune(|p| p == kept);
        assert_eq!(dropped, 3);
        let mut left: Vec<&String> = s.stats.keys().collect();
        left.sort();
        let mut expected = [kept.to_string_lossy().into_owned(), fav.to_string_lossy().into_owned(), unmounted.to_string_lossy().into_owned()];
        expected.sort();
        assert_eq!(left, expected.iter().collect::<Vec<_>>(), "kept, favorite that exists, unmounted drive");
        assert_eq!(s.session.lyrics_offsets.len(), 1);
    }

    #[test]
    fn write_atomic_replaces_content() {
        let dir = TempDir::new("atomic");
        let path = dir.join("f.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(dir.listing(), ["f.txt"]);
        assert!(write_atomic(&dir.join("f.txt/inside"), b"x").is_err(), "a file can't be a folder");
        assert_eq!(dir.listing(), ["f.txt"], "failed writes leave nothing behind");
    }
}
