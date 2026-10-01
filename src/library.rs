//! The music library: scanning folders, reading tags, caching metadata, grouping and sorting.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{self, AtomicU64, AtomicUsize};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use lofty::config::{ParseOptions, ParsingMode};
use lofty::error::FileParseError;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagType};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::config::{LibraryConfig, expand_tilde};

pub const UNKNOWN_ARTIST: &str = "Unknown Artist";
pub const UNKNOWN_ALBUM: &str = "Unknown Album";
pub const UNKNOWN_GENRE: &str = "Unknown Genre";
pub const UNKNOWN_YEAR: &str = "Unknown Year";
/// Album artist shown for albums whose tracks have different artists and no album artist tag.
pub const VARIOUS_ARTISTS: &str = "Various Artists";

/// Index into `Library::tracks`. Only valid for the Library it came from (a rescan builds a new
/// Library; remap by path).
pub type TrackId = usize;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Track {
    #[serde(skip)]
    pub id: TrackId,
    pub path: PathBuf,
    /// Never empty: falls back to the file name (with "NN - " / "Artist - " prefixes stripped).
    pub title: String,
    /// Never empty: falls back to "Artist - Title" file names, then "Unknown Artist".
    pub artist: String,
    /// Never empty: falls back to the parent folder name, then "Unknown Album".
    pub album: String,
    /// Falls back to `artist`.
    pub album_artist: String,
    pub genre: String,
    pub year: Option<u32>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration: Duration,
    /// kbps
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    /// "MP3", "FLAC", "WAV", "AAC", "ALAC", "Vorbis", ...
    pub format: String,
    pub size: u64,
    /// Modification time (unix seconds): used as "date added".
    pub mtime: u64,
    pub has_art: bool,
    pub has_lyrics: bool,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    /// Parent folder relative to its library root ("LocalFiles/Jpop"); "." for files directly in a root.
    pub folder: String,
}

/// How the library tab groups tracks.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum BrowseMode {
    #[default]
    Folders,
    Artists,
    Albums,
    Genres,
    Years,
    /// One flat list of every track.
    Tracks,
}

impl BrowseMode {
    pub const ALL: [BrowseMode; 6] =
        [BrowseMode::Folders, BrowseMode::Artists, BrowseMode::Albums, BrowseMode::Genres, BrowseMode::Years, BrowseMode::Tracks];
    pub fn label(self) -> &'static str {
        match self {
            BrowseMode::Folders => "Folders",
            BrowseMode::Artists => "Artists",
            BrowseMode::Albums => "Albums",
            BrowseMode::Genres => "Genres",
            BrowseMode::Years => "Years",
            BrowseMode::Tracks => "All Tracks",
        }
    }
    pub fn next(self) -> BrowseMode {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum SortKey {
    /// Natural order for the context: album -> disc -> track number for albums/artists,
    /// folder order + file name for folders, title otherwise.
    #[default]
    Default,
    Title,
    Artist,
    Album,
    Duration,
    Year,
    /// By modification time (newest first when descending).
    Added,
    Plays,
    Path,
    Random,
}

impl SortKey {
    pub const ALL: [SortKey; 10] = [
        SortKey::Default,
        SortKey::Title,
        SortKey::Artist,
        SortKey::Album,
        SortKey::Duration,
        SortKey::Year,
        SortKey::Added,
        SortKey::Plays,
        SortKey::Path,
        SortKey::Random,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Default => "default",
            SortKey::Title => "title",
            SortKey::Artist => "artist",
            SortKey::Album => "album",
            SortKey::Duration => "duration",
            SortKey::Year => "year",
            SortKey::Added => "added",
            SortKey::Plays => "plays",
            SortKey::Path => "path",
            SortKey::Random => "random",
        }
    }
    pub fn next(self) -> SortKey {
        let i = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

/// One entry in the groups pane (an artist, an album, a folder, ...).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    pub name: String,
    /// Extra info shown dimmed, e.g. "2019 · 12 tracks · 48:02".
    pub detail: String,
    /// In the group's natural order (SortKey::Default).
    pub tracks: Vec<TrackId>,
}

#[derive(Clone, Debug, Default)]
pub struct Library {
    pub roots: Vec<PathBuf>,
    pub tracks: Vec<Track>,
    /// By `index_key` of the path.
    index: HashMap<PathBuf, TrackId>,
    generation: u64,
}

/// A path in NFC: macOS keeps the decomposed name a file was created with but opens it by either
/// spelling, so a typed (composed) path must find the scanned (often decomposed) track.
fn index_key(p: &Path) -> std::borrow::Cow<'_, Path> {
    match p.to_str() {
        Some(s) if !unicode_normalization::is_nfc(s) => PathBuf::from(s.nfc().collect::<String>()).into(),
        _ => p.into(),
    }
}

static GENERATIONS: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    GENERATIONS.fetch_add(1, atomic::Ordering::Relaxed)
}

impl Library {
    pub fn empty() -> Library {
        Library::default()
    }

    /// Takes ownership of scanned tracks, assigns ids in order and builds the path index.
    pub fn from_tracks(roots: Vec<PathBuf>, mut tracks: Vec<Track>) -> Library {
        for (i, t) in tracks.iter_mut().enumerate() {
            t.id = i;
        }
        let index = tracks.iter().map(|t| (index_key(&t.path).into_owned(), t.id)).collect();
        Library { roots, tracks, index, generation: next_generation() }
    }

    /// Identifies this track list: unique per `from_tracks`, renewed when `add_paths` adds tracks.
    /// Caches derived from the tracks (the search index) rebuild when it changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn get(&self, id: TrackId) -> Option<&Track> {
        self.tracks.get(id)
    }

    pub fn find(&self, path: &Path) -> Option<TrackId> {
        self.index.get(index_key(path).as_ref()).copied()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn all_ids(&self) -> Vec<TrackId> {
        (0..self.tracks.len()).collect()
    }

    /// Groups for a browse mode, sorted by name (years descending, "Unknown …" last).
    /// `Tracks` mode yields a single "All Tracks" group.
    ///
    /// Details and track order per mode:
    /// - Folders: one group per folder holding tracks, in tree order; "N tracks · time";
    ///   disc -> track number -> file name (playlist-style folders keep their numbering).
    /// - Artists: "N albums · M tracks"; album -> disc -> track -> title.
    /// - Albums: tracks with the same album in one folder form an album. Without album artist
    ///   tags, several artists label it with the artist of most tracks, else "Various Artists";
    ///   same album and album artist across folders (CD1/CD2) merge. "Artist · Year · N tracks · time".
    /// - Genres ("N artists · M tracks") and Years ("N albums · M tracks"): artist -> album order.
    /// - Tracks: by title.
    pub fn groups(&self, mode: BrowseMode) -> Vec<Group> {
        match mode {
            BrowseMode::Folders => self.folder_groups(),
            BrowseMode::Artists => self.artist_groups(),
            BrowseMode::Albums => self.album_groups(),
            BrowseMode::Genres => self.genre_groups(),
            BrowseMode::Years => self.year_groups(),
            BrowseMode::Tracks => {
                let ids = self.sorted(self.all_ids(), cmp_title_order);
                vec![Group { name: "All Tracks".into(), detail: self.count_time(&ids), tracks: ids }]
            }
        }
    }

    /// Sort ids in place. `plays` supplies play counts for SortKey::Plays. Stable; ties fall back to
    /// the default order.
    ///
    /// `Default` keeps the incoming order (a group's natural order, a playlist's order, ...) and
    /// `desc` just reverses it. Other keys compare naturally and case-insensitively ("Track 2" <
    /// "Track 10"); `desc` flips only the key itself, so an artist's albums and tracks stay in
    /// order and unknown artists/albums/years stay last. Artist sorts by album -> disc -> track
    /// within an artist, Album and Year by disc -> track. `Random` shuffles.
    pub fn sort(&self, ids: &mut [TrackId], key: SortKey, desc: bool, plays: &dyn Fn(TrackId) -> u32) {
        let dir = |o: Ordering| if desc { o.reverse() } else { o };
        match key {
            SortKey::Default if desc => ids.reverse(),
            SortKey::Default => {}
            SortKey::Random => ids.shuffle(&mut rand::rng()),
            SortKey::Plays => {
                let counts: HashMap<TrackId, u32> = ids.iter().map(|&id| (id, plays(id))).collect();
                ids.sort_by(|a, b| dir(counts[a].cmp(&counts[b])));
            }
            _ => ids.sort_by(|&a, &b| match (self.get(a), self.get(b)) {
                (Some(x), Some(y)) => match key {
                    SortKey::Title => dir(natural_cmp(&x.title, &y.title)),
                    SortKey::Artist => unknown_last(&x.artist, &y.artist, UNKNOWN_ARTIST, desc).then_with(|| cmp_album_order(x, y)),
                    SortKey::Album => unknown_last(&x.album, &y.album, UNKNOWN_ALBUM, desc)
                        .then_with(|| natural_cmp(&x.album_artist, &y.album_artist))
                        .then_with(|| cmp_disc_track(x, y)),
                    SortKey::Duration => dir(x.duration.cmp(&y.duration)),
                    SortKey::Year => {
                        (x.year.is_none().cmp(&y.year.is_none())).then_with(|| dir(x.year.cmp(&y.year))).then_with(|| cmp_album_order(x, y))
                    }
                    SortKey::Added => dir(x.mtime.cmp(&y.mtime)),
                    SortKey::Path => dir(natural_path_cmp(&x.path.to_string_lossy(), &y.path.to_string_lossy())),
                    SortKey::Default | SortKey::Random | SortKey::Plays => Ordering::Equal,
                },
                // Stale ids sort last.
                (x, y) => x.is_none().cmp(&y.is_none()),
            }),
        }
    }

    pub fn total_duration(&self, ids: &[TrackId]) -> Duration {
        ids.iter().filter_map(|i| self.tracks.get(*i)).map(|t| t.duration).sum()
    }

    /// Read files/folders that aren't in the library yet (CLI paths, `:add`), append them and return
    /// the ids of everything referenced by `paths` (existing or new), in path order.
    ///
    /// Paths may be relative or start with `~`. Folders are walked with the library settings
    /// (extensions, hidden, exclude, symlinks); a file named explicitly only needs a music
    /// extension. Files outside the library roots get `folder` relative to the argument's parent.
    pub fn add_paths(&mut self, paths: &[PathBuf], cfg: &LibraryConfig) -> Vec<TrackId> {
        let mut walker = Walker::new(cfg);
        let mut roots = Vec::new();
        for p in paths {
            let p = normalize_path(&p.to_str().filter(|s| s.starts_with('~')).map_or_else(|| p.clone(), expand_tilde));
            let root = match self.roots.iter().find(|r| p.starts_with(r)) {
                Some(r) => r.clone(),
                None => {
                    let base = if p.is_dir() { p.as_path() } else { p.parent().unwrap_or(&p) };
                    base.parent().unwrap_or(base).to_path_buf()
                }
            };
            roots.push(root);
            let start = walker.found.len();
            walker.seen.clear();
            walker.add(&p, roots.len() - 1);
            walker.found[start..].sort_by(|a, b| a.path.cmp(&b.path));
        }
        let mut new = HashSet::new();
        let jobs: Vec<Job> = walker
            .found
            .iter()
            .filter(|f| self.find(&f.path).is_none() && new.insert(&f.path))
            .map(|f| Job { path: &f.path, root: &roots[f.root], size: f.size, mtime: f.mtime })
            .collect();
        let added = read_all(&jobs, &|_| {});
        if !added.is_empty() {
            self.generation = next_generation();
        }
        for mut t in added {
            t.id = self.tracks.len();
            self.index.insert(index_key(&t.path).into_owned(), t.id);
            self.tracks.push(t);
        }
        walker.found.iter().filter_map(|f| self.find(&f.path)).collect()
    }

    fn sorted(&self, mut ids: Vec<TrackId>, cmp: fn(&Track, &Track) -> Ordering) -> Vec<TrackId> {
        ids.sort_by(|&a, &b| cmp(&self.tracks[a], &self.tracks[b]));
        ids
    }

    /// Track ids bucketed by `key` (unordered; callers sort the groups).
    fn bucket<K: std::hash::Hash + Eq>(&self, key: impl Fn(&Track) -> K) -> Vec<Vec<TrackId>> {
        let mut map: HashMap<K, Vec<TrackId>> = HashMap::new();
        for t in &self.tracks {
            map.entry(key(t)).or_default().push(t.id);
        }
        map.into_values().collect()
    }

    fn count_distinct(&self, ids: &[TrackId], key: impl Fn(&Track) -> String) -> usize {
        ids.iter().map(|&i| key(&self.tracks[i])).collect::<HashSet<_>>().len()
    }

    /// "12 tracks · 48:02"
    fn count_time(&self, ids: &[TrackId]) -> String {
        format!("{} · {}", plural(ids.len(), "track"), fmt_duration(self.total_duration(ids)))
    }

    fn folder_groups(&self) -> Vec<Group> {
        let mut groups: Vec<(String, Group)> = self
            .bucket(|t| t.path.parent().map(Path::to_path_buf))
            .into_iter()
            .map(|ids| {
                let ids = self.sorted(ids, cmp_folder_order);
                let t = &self.tracks[ids[0]];
                let name = match t.folder.as_str() {
                    "." => t.path.parent().and_then(Path::file_name).map_or_else(|| "/".into(), |n| n.to_string_lossy().into_owned()),
                    folder => folder.to_string(),
                };
                (t.folder.clone(), Group { name, detail: self.count_time(&ids), tracks: ids })
            })
            .collect();
        groups.sort_by(|(a, ga), (b, gb)| {
            ((a != ".").cmp(&(b != ".")))
                .then_with(|| natural_path_cmp(a, b))
                .then_with(|| self.tracks[ga.tracks[0]].path.cmp(&self.tracks[gb.tracks[0]].path))
        });
        groups.into_iter().map(|(_, g)| g).collect()
    }

    fn artist_groups(&self) -> Vec<Group> {
        let groups = self
            .bucket(|t| t.artist.to_lowercase())
            .into_iter()
            .map(|ids| {
                let ids = self.sorted(ids, cmp_album_order);
                let albums = self.count_distinct(&ids, |t| t.album.to_lowercase());
                let detail = format!("{} · {}", plural(albums, "album"), plural(ids.len(), "track"));
                Group { name: self.tracks[ids[0]].artist.clone(), detail, tracks: ids }
            })
            .collect();
        sort_named(groups, UNKNOWN_ARTIST)
    }

    fn album_groups(&self) -> Vec<Group> {
        // (album, album artist label) -> (label, ids)
        let mut albums: HashMap<(String, String), (String, Vec<TrackId>)> = HashMap::new();
        for ids in self.bucket(|t| (t.album.to_lowercase(), t.path.parent().map(Path::to_path_buf))) {
            let mut counts: HashMap<String, (usize, &str)> = HashMap::new();
            for &i in &ids {
                let artist = &self.tracks[i].album_artist;
                counts.entry(artist.to_lowercase()).or_insert((0, artist)).0 += 1;
            }
            // Without album artist tags, a folder album with several artists gets one label: the
            // artist of most of its tracks, else "Various Artists". Tagged album artists decide.
            let tagged = ids.iter().any(|&i| self.tracks[i].album_artist != self.tracks[i].artist);
            let shared = (counts.len() > 1 && !tagged).then(|| match counts.values().max() {
                Some(&(n, artist)) if n * 2 > ids.len() => artist,
                _ => VARIOUS_ARTISTS,
            });
            for i in ids {
                let t = &self.tracks[i];
                let label = shared.unwrap_or(&t.album_artist);
                albums.entry((t.album.to_lowercase(), label.to_lowercase())).or_insert_with(|| (label.to_string(), Vec::new())).1.push(i);
            }
        }
        let mut groups: Vec<(String, Group)> = albums
            .into_values()
            .map(|(label, ids)| {
                let ids = self.sorted(ids, cmp_album_order);
                let year = ids.iter().filter_map(|&i| self.tracks[i].year).min();
                let mut detail = label.clone();
                if let Some(y) = year {
                    detail.push_str(&format!(" · {y}"));
                }
                detail.push_str(&format!(" · {}", self.count_time(&ids)));
                (label, Group { name: self.tracks[ids[0]].album.clone(), detail, tracks: ids })
            })
            .collect();
        groups.sort_by(|(la, a), (lb, b)| {
            ((a.name == UNKNOWN_ALBUM).cmp(&(b.name == UNKNOWN_ALBUM)))
                .then_with(|| natural_cmp(&a.name, &b.name))
                .then_with(|| natural_cmp(la, lb))
                .then_with(|| a.tracks[0].cmp(&b.tracks[0]))
        });
        groups.into_iter().map(|(_, g)| g).collect()
    }

    fn genre_groups(&self) -> Vec<Group> {
        let groups = self
            .bucket(|t| t.genre.trim().to_lowercase())
            .into_iter()
            .map(|ids| {
                let ids = self.sorted(ids, cmp_artist_order);
                let genre = self.tracks[ids[0]].genre.trim();
                let artists = self.count_distinct(&ids, |t| t.artist.to_lowercase());
                let detail = format!("{} · {}", plural(artists, "artist"), plural(ids.len(), "track"));
                let name = if genre.is_empty() { UNKNOWN_GENRE.to_string() } else { genre.to_string() };
                Group { name, detail, tracks: ids }
            })
            .collect();
        sort_named(groups, UNKNOWN_GENRE)
    }

    fn year_groups(&self) -> Vec<Group> {
        let mut groups: Vec<(Option<u32>, Group)> = self
            .bucket(|t| t.year)
            .into_iter()
            .map(|ids| {
                let ids = self.sorted(ids, cmp_artist_order);
                let year = self.tracks[ids[0]].year;
                let albums = self.count_distinct(&ids, |t| t.album.to_lowercase());
                let detail = format!("{} · {}", plural(albums, "album"), plural(ids.len(), "track"));
                let name = year.map_or_else(|| UNKNOWN_YEAR.to_string(), |y| y.to_string());
                (year, Group { name, detail, tracks: ids })
            })
            .collect();
        groups.sort_by(|(a, _), (b, _)| a.is_none().cmp(&b.is_none()).then_with(|| b.cmp(a)));
        groups.into_iter().map(|(_, g)| g).collect()
    }
}

/// Natural, case-insensitive by name with `unknown` last.
fn sort_named(mut groups: Vec<Group>, unknown: &str) -> Vec<Group> {
    groups.sort_by(|a, b| {
        ((a.name == unknown).cmp(&(b.name == unknown)))
            .then_with(|| natural_cmp(&a.name, &b.name))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.tracks[0].cmp(&b.tracks[0]))
    });
    groups
}

pub fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

fn disc(t: &Track) -> u32 {
    t.disc_no.unwrap_or(1)
}

/// Disc, then track number (tracks without a number last).
fn cmp_disc_track(a: &Track, b: &Track) -> Ordering {
    disc(a).cmp(&disc(b)).then_with(|| a.track_no.is_none().cmp(&b.track_no.is_none())).then_with(|| a.track_no.cmp(&b.track_no))
}

fn file_name(t: &Track) -> std::borrow::Cow<'_, str> {
    t.path.file_name().unwrap_or_default().to_string_lossy()
}

fn cmp_folder_order(a: &Track, b: &Track) -> Ordering {
    cmp_disc_track(a, b).then_with(|| natural_cmp(&file_name(a), &file_name(b))).then_with(|| a.path.cmp(&b.path))
}

fn cmp_album_order(a: &Track, b: &Track) -> Ordering {
    natural_cmp(&a.album, &b.album).then_with(|| cmp_disc_track(a, b)).then_with(|| natural_cmp(&a.title, &b.title)).then_with(|| a.path.cmp(&b.path))
}

fn cmp_artist_order(a: &Track, b: &Track) -> Ordering {
    natural_cmp(&a.artist, &b.artist).then_with(|| cmp_album_order(a, b))
}

fn cmp_title_order(a: &Track, b: &Track) -> Ordering {
    natural_cmp(&a.title, &b.title)
        .then_with(|| natural_cmp(&a.artist, &b.artist))
        .then_with(|| natural_cmp(&a.album, &b.album))
        .then_with(|| a.path.cmp(&b.path))
}

/// `unknown` last in both directions, the rest natural (reversed when `desc`).
fn unknown_last(a: &str, b: &str, unknown: &str, desc: bool) -> Ordering {
    let o = natural_cmp(a, b);
    ((a == unknown).cmp(&(b == unknown))).then(if desc { o.reverse() } else { o })
}

/// Case-insensitive comparison where digit runs compare by value: "Track 2" < "Track 10",
/// "01" == "1". Strings equal under those rules compare Equal.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let ei = i + x[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            let ej = j + y[j..].iter().take_while(|c| c.is_ascii_digit()).count();
            let (n, m) = (a[i..ei].trim_start_matches('0'), b[j..ej].trim_start_matches('0'));
            let o = n.len().cmp(&m.len()).then_with(|| n.cmp(m));
            if o != Ordering::Equal {
                return o;
            }
            (i, j) = (ei, ej);
        } else {
            // Both indices are on char boundaries: they only ever advance by whole chars/digit runs.
            let c = a[i..].chars().next().unwrap_or_default();
            let d = b[j..].chars().next().unwrap_or_default();
            let o = lower(c).cmp(&lower(d));
            if o != Ordering::Equal {
                return o;
            }
            (i, j) = (i + c.len_utf8(), j + d.len_utf8());
        }
    }
    (x.len() - i).cmp(&(y.len() - j))
}

fn lower(c: char) -> char {
    if c.is_ascii() { c.to_ascii_lowercase() } else { c.to_lowercase().next().unwrap_or(c) }
}

/// Natural comparison of '/'-separated paths component by component (tree order: "A" < "A/B" < "A B").
fn natural_path_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.split('/'), b.split('/'));
    loop {
        match (x.next(), y.next()) {
            (Some(p), Some(q)) => match natural_cmp(p, q).then_with(|| p.cmp(q)) {
                Ordering::Equal => {}
                o => return o,
            },
            (p, q) => return p.is_some().cmp(&q.is_some()),
        }
    }
}

pub enum ScanMsg {
    Progress { done: usize, total: usize },
    Done(Library),
}

/// Scan `cfg.dirs` on a background thread. Uses the metadata cache at `cache` (unless `force` or
/// `!cfg.use_cache`) so unchanged files are not re-read; writes the updated cache when done.
pub fn spawn_scan(cfg: LibraryConfig, cache: PathBuf, force: bool) -> Receiver<ScanMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let lib = scan(&cfg, &cache, force, &|done, total| {
            let _ = tx.send(ScanMsg::Progress { done, total });
        });
        let _ = tx.send(ScanMsg::Done(lib));
    });
    rx
}

/// Bump when the Track layout or the metadata heuristics change: older caches are then ignored.
/// 2: text is NFC-normalized; MP4 audio lofty can't identify is "MP4" (no bitrate), not "AAC".
const CACHE_VERSION: u32 = 2;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Cache {
    version: u32,
    roots: Vec<PathBuf>,
    tracks: Vec<Track>,
}

#[derive(Serialize)]
struct CacheRef<'a> {
    version: u32,
    roots: &'a [PathBuf],
    tracks: Vec<&'a Track>,
}

/// The scan behind [`spawn_scan`], on the calling thread (file reading still uses every core).
///
/// Walks every root (deduplicated, `~` expanded) honoring the config, reuses cache entries whose
/// path, size and mtime match, reads the rest in parallel and reports `progress(done, total)`.
/// The cache keeps tracks shorter than `min_duration_secs` too, so changing it needs no re-read;
/// it is rewritten (atomically) only when something changed.
fn scan(cfg: &LibraryConfig, cache: &Path, force: bool, progress: &(dyn Fn(usize, usize) + Sync)) -> Library {
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in &cfg.dirs {
        let root = normalize_path(&expand_tilde(dir));
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let mut walker = Walker::new(cfg);
    for (i, root) in roots.iter().enumerate() {
        walker.add(root, i);
    }
    let mut found = walker.found;
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found.dedup_by(|a, b| a.path == b.path);
    let total = found.len();
    progress(0, total);

    let cached = if cfg.use_cache && !force { load_cache(cache) } else { Cache::default() };
    let (cached_len, same_roots) = (cached.tracks.len(), cached.roots == roots);
    let mut cached: HashMap<PathBuf, Track> = cached.tracks.into_iter().map(|t| (t.path.clone(), t)).collect();
    let mut slots: Vec<Option<Track>> = Vec::with_capacity(total);
    let mut jobs = Vec::new();
    let mut job_slots = Vec::new();
    for (i, f) in found.iter().enumerate() {
        match cached.remove(&f.path).filter(|t| t.size == f.size && t.mtime == f.mtime) {
            Some(mut t) => {
                t.folder = relative_folder(&f.path, &roots[f.root]);
                slots.push(Some(t));
            }
            None => {
                slots.push(None);
                jobs.push(Job { path: &f.path, root: &roots[f.root], size: f.size, mtime: f.mtime });
                job_slots.push(i);
            }
        }
    }
    let reused = total - jobs.len();
    progress(reused, total);
    let read = read_all(&jobs, &|done| {
        if done % 32 == 0 || reused + done == total {
            progress(reused + done, total);
        }
    });
    for (i, t) in job_slots.into_iter().zip(read) {
        slots[i] = Some(t);
    }
    let tracks: Vec<Track> = slots.into_iter().flatten().collect();
    if cfg.use_cache && (reused != total || cached_len != total || !same_roots) {
        let _ = save_cache(cache, &roots, &tracks);
    }
    let min = Duration::from_secs(cfg.min_duration_secs.into());
    Library::from_tracks(roots, tracks.into_iter().filter(|t| t.duration >= min).collect())
}

/// A cache that is missing, unreadable, corrupt or from another version is empty.
fn load_cache(path: &Path) -> Cache {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok()).filter(|c| c.version == CACHE_VERSION).unwrap_or_default()
}

/// Atomic write: a temp file next to the cache, renamed over it.
fn save_cache(path: &Path, roots: &[PathBuf], tracks: &[Track]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // JSON can't hold non-UTF-8 paths; such tracks are simply re-read next time.
    let cache = CacheRef { version: CACHE_VERSION, roots, tracks: tracks.iter().filter(|t| t.path.to_str().is_some()).collect() };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    // Unique per process and write, so overlapping scans never share a temp file.
    let tmp = path.with_file_name(format!(".{name}.{}-{}.tmp", std::process::id(), next_generation()));
    std::fs::write(&tmp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A file found by the walker.
struct Found {
    path: PathBuf,
    /// Index into the roots the walk was started from.
    root: usize,
    size: u64,
    mtime: u64,
}

/// Recursive folder walk honoring extensions / hidden / exclude / symlink settings.
struct Walker<'a> {
    cfg: &'a LibraryConfig,
    /// (device, inode) of folders and files already visited when following symlinks (loop guard).
    seen: HashSet<(u64, u64)>,
    found: Vec<Found>,
}

impl<'a> Walker<'a> {
    fn new(cfg: &'a LibraryConfig) -> Walker<'a> {
        Walker { cfg, seen: HashSet::new(), found: Vec::new() }
    }

    /// Adds a folder (recursively) or a single music file. `path` itself is never filtered as
    /// hidden/excluded: it was asked for explicitly.
    fn add(&mut self, path: &Path, root: usize) {
        let Ok(meta) = std::fs::metadata(path) else { return };
        if meta.is_dir() {
            self.seen.insert((meta.dev(), meta.ino()));
            self.walk(path, root);
        } else if meta.is_file() && is_audio(path, &self.cfg.extensions) {
            self.found.push(Found { path: path.to_path_buf(), root, size: meta.len(), mtime: mtime_secs(&meta) });
        }
    }

    fn walk(&mut self, dir: &Path, root: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let follow = self.cfg.follow_symlinks;
        for entry in entries.flatten() {
            if self.cfg.ignore_hidden && entry.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let path = entry.path();
            let Ok(mut kind) = entry.file_type() else { continue };
            let mut meta = None;
            if kind.is_symlink() {
                // Broken links are skipped too.
                let Some(m) = follow.then(|| std::fs::metadata(&path).ok()).flatten() else { continue };
                kind = m.file_type();
                meta = Some(m);
            }
            if kind.is_dir() {
                if self.excluded(&path, true) {
                    continue;
                }
                if follow {
                    let Some(m) = meta.or_else(|| entry.metadata().ok()) else { continue };
                    if !self.seen.insert((m.dev(), m.ino())) {
                        continue;
                    }
                }
                self.walk(&path, root);
            } else if kind.is_file() && is_audio(&path, &self.cfg.extensions) && !self.excluded(&path, false) {
                let Some(m) = meta.or_else(|| entry.metadata().ok()) else { continue };
                if follow && !self.seen.insert((m.dev(), m.ino())) {
                    continue;
                }
                self.found.push(Found { size: m.len(), mtime: mtime_secs(&m), path, root });
            }
        }
    }

    /// Folders are tested with a trailing '/' so "/Voice Memos/" excludes the folder itself.
    fn excluded(&self, path: &Path, dir: bool) -> bool {
        if self.cfg.exclude.is_empty() {
            return false;
        }
        let mut s = path.to_string_lossy().into_owned();
        if dir {
            s.push('/');
        }
        self.cfg.exclude.iter().any(|x| !x.is_empty() && s.contains(x.as_str()))
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    u64::try_from(meta.mtime()).unwrap_or(0)
}

/// Absolute, with "." and ".." resolved lexically (symlinks are kept, like the walker's paths).
fn normalize_path(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// All audio files under `path` (or `path` itself if it's an audio file), sorted, honoring
/// extensions / hidden / exclude / symlink settings.
#[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
pub fn collect_audio_files(path: &Path, cfg: &LibraryConfig) -> Vec<PathBuf> {
    let mut walker = Walker::new(cfg);
    walker.add(path, 0);
    let mut files: Vec<PathBuf> = walker.found.into_iter().map(|f| f.path).collect();
    files.sort();
    files
}

pub fn is_audio(path: &Path, extensions: &[String]) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

struct Job<'a> {
    path: &'a Path,
    root: &'a Path,
    size: u64,
    mtime: u64,
}

/// Reads `jobs` on every core (threads pull the next file from a shared counter, so slow files
/// don't pile up on one thread), keeping their order. `progress(done)` runs after each file.
fn read_all(jobs: &[Job], progress: &(dyn Fn(usize) + Sync)) -> Vec<Track> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(jobs.len().div_ceil(8));
    let (next, done) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let mut slots: Vec<Option<Track>> = std::iter::repeat_with(|| None).take(jobs.len()).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, atomic::Ordering::Relaxed);
                        let Some(job) = jobs.get(i) else { break };
                        out.push((i, build_track(job.path, job.root, job.size, job.mtime)));
                        progress(done.fetch_add(1, atomic::Ordering::Relaxed) + 1);
                    }
                    out
                })
            })
            .collect();
        for worker in workers {
            for (i, t) in worker.join().unwrap_or_default() {
                slots[i] = Some(t);
            }
        }
    });
    slots.into_iter().flatten().collect()
}

/// Read one file's metadata (tags + audio properties) with fallbacks so title/artist/album are
/// never empty. `root` is the library root it belongs to (for `folder`). None if unreadable.
///
/// Tags are merged per field: the format's primary tag first, ID3v1 last (it truncates and
/// often holds mis-decoded text). Unreadable/broken files keep file-name metadata. A title tag
/// repeating the artist ("ALI - LOST IN PARADISE" by ALI) loses that prefix.
#[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
pub fn read_track(path: &Path, root: &Path) -> Option<Track> {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    Some(build_track(path, root, meta.len(), mtime_secs(&meta)))
}

/// Metadata found in tags (all optional; fallbacks are applied by `build_track`).
#[derive(Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    year: Option<u32>,
    track_no: Option<u32>,
    disc_no: Option<u32>,
}

fn build_track(path: &Path, root: &Path, size: u64, mtime: u64) -> Track {
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    let mut t = Track { path: path.to_path_buf(), format: ext.to_uppercase(), size, mtime, folder: relative_folder(path, root), ..Track::default() };
    let tags = match probe(path) {
        Some((file, format)) => {
            let tags = read_tags(&file, &mut t);
            if format == "MP4" {
                // lofty estimated it from the whole file, video included
                t.bitrate = None;
            }
            t.format = format;
            tags
        }
        None => Tags::default(),
    };

    // NFC: macOS often stores names decomposed (ふ + ゛), while typed search text is composed
    let stem: String = path.file_stem().unwrap_or_default().to_string_lossy().nfc().collect();
    let name = parse_file_name(&stem);
    // The file name's "Artist - " part is trusted only when it agrees with the title tag (so a
    // title like "晩餐歌 - Bansanka" isn't split into a fake artist).
    let name_artist = name.artist.filter(|_| tags.title.as_deref().is_none_or(|title| title == name.title));
    t.artist = tags.artist.or(name_artist).unwrap_or_else(|| UNKNOWN_ARTIST.into());
    t.title = match tags.title {
        Some(title) => strip_artist_prefix(title, &t.artist),
        None if name.title.is_empty() => stem,
        None => name.title,
    };
    let folder_name = path.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().nfc().collect());
    t.album = tags.album.or(folder_name).unwrap_or_else(|| UNKNOWN_ALBUM.into());
    t.album_artist = tags.album_artist.unwrap_or_else(|| t.artist.clone());
    t.genre = tags.genre.unwrap_or_default();
    t.year = tags.year;
    t.track_no = tags.track_no.or(name.track_no);
    t.disc_no = tags.disc_no.or(name.disc_no);
    t
}

/// Parses tags + audio properties. MP4 is read as such to tell AAC from ALAC; files that fail in
/// the default mode get a second, more lenient try. Panics inside the parser count as failure.
fn probe(path: &Path) -> Option<(TaggedFile, String)> {
    let read = |mode: ParsingMode| -> Result<(TaggedFile, String), FileParseError> {
        let options = ParseOptions::new().parsing_mode(mode);
        let probe = Probe::open(path)?.options(options).guess_file_type()?;
        let kind = probe.file_type();
        if kind == Some(FileType::Mp4) {
            let file = Mp4File::read_from(&mut probe.into_inner(), options)?;
            let codec = match file.properties().codec() {
                Some(Mp4Codec::ALAC) => "ALAC",
                Some(Mp4Codec::FLAC) => "FLAC",
                Some(Mp4Codec::MP3) => "MP3",
                Some(Mp4Codec::AAC) => "AAC",
                // a sample entry lofty can't read (Opus, AC-3, …)
                _ => "MP4",
            };
            return Ok((file.into(), codec.into()));
        }
        let file = probe.read()?;
        let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
        let format = match kind {
            Some(FileType::Aac) => "AAC",
            Some(FileType::Aiff) => "AIFF",
            Some(FileType::Ape) => "APE",
            Some(FileType::Flac) => "FLAC",
            Some(FileType::Mpeg) if ext == "mp2" => "MP2",
            Some(FileType::Mpeg) if ext == "mp1" => "MP1",
            Some(FileType::Mpeg) => "MP3",
            Some(FileType::Mpc) => "Musepack",
            Some(FileType::Opus) => "Opus",
            Some(FileType::Vorbis) => "Vorbis",
            Some(FileType::Speex) => "Speex",
            Some(FileType::Wav) => "WAV",
            Some(FileType::WavPack) => "WavPack",
            Some(FileType::Custom(name)) => name,
            _ => return Ok((file, ext.to_uppercase())),
        };
        Ok((file, format.into()))
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(ParsingMode::BestAttempt).or_else(|_| read(ParsingMode::Relaxed)).ok()))
        .ok()
        .flatten()
}

/// Fills audio properties and tag flags into `t`; returns the text fields for fallback handling.
fn read_tags(file: &TaggedFile, t: &mut Track) -> Tags {
    let p = file.properties();
    t.duration = p.duration();
    t.bitrate = p.audio_bitrate().or(p.overall_bitrate()).filter(|&b| b > 0);
    t.sample_rate = p.sample_rate().filter(|&r| r > 0);
    t.channels = p.channels().filter(|&c| c > 0);

    let primary = file.primary_tag_type();
    let mut tags: Vec<&Tag> = file.tags().iter().collect();
    tags.sort_by_key(|tag| (tag.tag_type() == TagType::Id3v1, tag.tag_type() != primary));
    let first = |key: ItemKey| tags.iter().find_map(|tag| tag.get_strings(key).map(clean_text).find(|s| !s.is_empty()));
    // Multi-valued artists ("A\0B" in ID3v2.4, repeated Vorbis fields) are joined.
    let joined = |key: ItemKey| {
        tags.iter().find_map(|tag| Some(dedupe_list(&tag.get_strings(key).map(clean_text).collect::<Vec<_>>().join(", "))).filter(|s| !s.is_empty()))
    };
    let gain = |key: ItemKey| tags.iter().find_map(|tag| tag.get_strings(key).find_map(parse_gain));
    let number = |key: ItemKey| tags.iter().find_map(|tag| tag.get_strings(key).find_map(leading_number));

    t.has_art = tags.iter().any(|tag| tag.picture_count() > 0);
    t.has_lyrics =
        tags.iter().any(|tag| [ItemKey::Lyrics, ItemKey::UnsyncLyrics].into_iter().any(|key| tag.get_strings(key).any(|s| !s.trim().is_empty())));
    t.rg_track_gain = gain(ItemKey::ReplayGainTrackGain);
    t.rg_track_peak = gain(ItemKey::ReplayGainTrackPeak);
    t.rg_album_gain = gain(ItemKey::ReplayGainAlbumGain);
    t.rg_album_peak = gain(ItemKey::ReplayGainAlbumPeak);
    Tags {
        title: first(ItemKey::TrackTitle),
        artist: joined(ItemKey::TrackArtist),
        album: first(ItemKey::AlbumTitle),
        album_artist: joined(ItemKey::AlbumArtist),
        genre: first(ItemKey::Genre),
        year: [ItemKey::RecordingDate, ItemKey::Year, ItemKey::ReleaseDate, ItemKey::OriginalReleaseDate]
            .into_iter()
            .find_map(|key| tags.iter().find_map(|tag| tag.get_strings(key).find_map(parse_year))),
        track_no: number(ItemKey::TrackNumber),
        disc_no: number(ItemKey::DiscNumber),
    }
}

/// Control characters (newlines in titles, BOMs) become spaces; space runs collapse; trimmed;
/// NFC, so decomposed tags ("ふ" + U+3099) display, group and search like typed text ("ぶ").
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfc() {
        let c = if c.is_control() || c == '\u{feff}' { ' ' } else { c };
        if c != ' ' || !(out.is_empty() || out.ends_with(' ')) {
            out.push(c);
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out.nfc().collect()
}

/// "A, B, a, C," -> "A, B, C": repeated entries (common in YouTube Music tags) are dropped.
fn dedupe_list(s: &str) -> String {
    let mut seen = HashSet::new();
    let parts: Vec<&str> =
        s.trim_end_matches([',', ' ']).split(", ").map(str::trim).filter(|p| !p.is_empty() && seen.insert(p.to_lowercase())).collect();
    parts.join(", ")
}

/// "3/12" -> 3, "07" -> 7; 0 and garbage -> None.
fn leading_number(s: &str) -> Option<u32> {
    let s = s.trim();
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    s[..digits].parse().ok().filter(|&n| n > 0)
}

/// The first 4 digits of the first run of at least 4: "2021", "2021-05-01", "20210501", "05/01/2021".
fn parse_year(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let run = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if run >= 4 {
            return s[i..i + 4].parse().ok().filter(|&y| y > 0);
        }
        i += run.max(1);
    }
    None
}

/// "-6.54 dB" -> -6.54, "+1.2 dB" -> 1.2, "0.988" -> 0.988, "-6,54 dB" -> -6.54.
fn parse_gain(s: &str) -> Option<f32> {
    let s = s.trim();
    let s = s.strip_suffix("dB").or_else(|| s.strip_suffix("db")).or_else(|| s.strip_suffix("DB")).unwrap_or(s).trim();
    s.parse::<f32>().or_else(|_| s.replace(',', ".").parse()).ok().filter(|v| v.is_finite())
}

const DASHES: [&str; 3] = [" - ", " – ", " — "];

/// "ALI - LOST IN PARADISE" by "ALI" -> "LOST IN PARADISE".
fn strip_artist_prefix(title: String, artist: &str) -> String {
    let rest = title
        .get(..artist.len())
        .filter(|p| p.eq_ignore_ascii_case(artist))
        .and_then(|_| DASHES.iter().find_map(|d| title[artist.len()..].strip_prefix(*d)))
        .map(str::trim)
        .filter(|r| !r.is_empty());
    if let Some(rest) = rest {
        return rest.to_string();
    }
    title
}

/// What a file name says about a track (used when tags are missing).
#[derive(Clone, Debug, Default, PartialEq)]
struct NameInfo {
    track_no: Option<u32>,
    disc_no: Option<u32>,
    artist: Option<String>,
    /// Empty only if the name is.
    title: String,
}

/// Parses a file stem: "003 - King Gnu - 白日" -> track 3, artist "King Gnu", title "白日".
///
/// Leading numbers: "01 - ", "1 - ", "01. ", "01.", "01-", "01_", "1-02 " / "1.02 " (disc 1,
/// track 2) and a bare space after 2 digits or a zero-padded 3 ("07 Song", "007 Song" but not
/// "500 Miles High"); "NA - " (yt-dlp's missing index) is dropped. Then "Artist - Title" splits
/// on the first " - " (or en/em dash) with both sides non-empty. Names made of underscores
/// ("my_song") get spaces.
fn parse_file_name(stem: &str) -> NameInfo {
    let spaced;
    let mut s = stem.trim();
    if s.contains('_') && !s.contains(' ') {
        spaced = clean_text(&s.replace('_', " "));
        s = &spaced;
    }
    let mut info = NameInfo::default();
    if let Some((disc, track, rest)) = split_number_prefix(s) {
        (info.disc_no, info.track_no, s) = (disc, track, rest);
    }
    let split = DASHES
        .iter()
        .filter_map(|d| s.find(*d).map(|i| (i, d.len())))
        .min()
        .map(|(i, n)| (s[..i].trim(), s[i + n..].trim()))
        .filter(|(artist, title)| !artist.is_empty() && !title.is_empty());
    if let Some((artist, title)) = split {
        info.artist = Some(artist.to_string());
        s = title;
    }
    info.title = s.to_string();
    info
}

/// (disc, track, rest) for a leading track number; see [`parse_file_name`].
fn split_number_prefix(s: &str) -> Option<(Option<u32>, Option<u32>, &str)> {
    if let Some(rest) = s.strip_prefix("NA - ").map(str::trim_start).filter(|r| !r.is_empty()) {
        return Some((None, None, rest));
    }
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let n = digits(s);
    if n == 0 || n > 3 {
        return None;
    }
    let (num, after) = s.split_at(n);
    if n <= 2
        && let Some(r) = after.strip_prefix(['-', '.'])
        && digits(r) == 2
        && let Some(title) = strip_separator(&r[2..], true)
    {
        return Some((leading_number(num), leading_number(&r[..2]), title));
    }
    let bare_space = n == 2 || num.starts_with('0');
    strip_separator(after, bare_space).map(|title| (None, leading_number(num), title))
}

/// The rest after a track number, if a separator follows (a bare space only when `bare_space`).
fn strip_separator(s: &str, bare_space: bool) -> Option<&str> {
    let rest = if let Some(r) = DASHES.iter().chain(&[". "]).find_map(|d| s.strip_prefix(*d)) {
        r
    } else if let Some(r) = s.strip_prefix(['.', '-', '_']).filter(|r| !r.starts_with(|c: char| c.is_ascii_digit())) {
        r
    } else if bare_space {
        s.strip_prefix(' ')?
    } else {
        return None;
    };
    Some(rest.trim_start()).filter(|r| !r.is_empty())
}

fn relative_folder(path: &Path, root: &Path) -> String {
    match path.parent().and_then(|p| p.strip_prefix(root).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => ".".into(),
    }
}

/// "3:07", "1:02:03"
pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::TagExt;

    use super::*;

    /// A fresh, empty temp folder for one test.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbit-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tiny mono 8 kHz 16-bit PCM WAV with a LIST/INFO chunk ([("INAM", "Title"), ...]).
    fn wav(path: &Path, secs: u32, info: &[(&str, &str)]) {
        let data_len = secs * 16_000;
        let mut list = b"INFO".to_vec();
        for (id, value) in info {
            let mut v = value.as_bytes().to_vec();
            v.push(0);
            list.extend(id.as_bytes());
            list.extend((v.len() as u32).to_le_bytes());
            let odd = v.len() % 2 == 1;
            list.extend(v);
            if odd {
                list.push(0);
            }
        }
        let list_len = if info.is_empty() { 0 } else { 8 + list.len() as u32 };
        let mut out = b"RIFF".to_vec();
        out.extend((4 + 24 + 8 + data_len + list_len).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes()); // PCM
        out.extend(1u16.to_le_bytes()); // mono
        out.extend(8000u32.to_le_bytes());
        out.extend(16_000u32.to_le_bytes()); // bytes per second
        out.extend(2u16.to_le_bytes()); // block align
        out.extend(16u16.to_le_bytes()); // bits per sample
        out.extend(b"data");
        out.extend(data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        if !info.is_empty() {
            out.extend(b"LIST");
            out.extend((list.len() as u32).to_le_bytes());
            out.extend(list);
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn reads_riff_info_tags_and_properties() {
        let dir = temp("riff");
        let path = dir.join("CEREMONY/x.wav");
        let info = [("INAM", "白日"), ("IART", "King Gnu"), ("IPRD", "CEREMONY"), ("ICRD", "2019-02-22"), ("IPRT", "3"), ("IGNR", "J-Rock")];
        wav(&path, 2, &info);
        let t = read_track(&path, &dir).unwrap();
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.album.as_str()), ("白日", "King Gnu", "CEREMONY"));
        assert_eq!((t.album_artist.as_str(), t.genre.as_str()), ("King Gnu", "J-Rock"));
        assert_eq!((t.year, t.track_no, t.disc_no), (Some(2019), Some(3), None));
        assert_eq!((t.format.as_str(), t.duration), ("WAV", Duration::from_secs(2)));
        assert_eq!((t.sample_rate, t.channels, t.bitrate), (Some(8000), Some(1), Some(128)));
        assert_eq!((t.folder.as_str(), t.size), ("CEREMONY", std::fs::metadata(&path).unwrap().len()));
        assert!(t.mtime > 0 && !t.has_art && !t.has_lyrics && t.rg_track_gain.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_id3v2_extras() {
        let dir = temp("id3");
        let path = dir.join("x.wav");
        wav(&path, 1, &[]);
        let mut tag = Tag::new(TagType::Id3v2);
        for (key, value) in [
            (ItemKey::TrackTitle, "Song"),
            (ItemKey::TrackArtist, "Singer"),
            (ItemKey::AlbumTitle, "Record"),
            (ItemKey::AlbumArtist, "Band"),
            (ItemKey::TrackNumber, "5"),
            (ItemKey::DiscNumber, "2"),
            (ItemKey::RecordingDate, "2026-05-22"),
            (ItemKey::ReplayGainTrackGain, "-6.54 dB"),
            (ItemKey::ReplayGainTrackPeak, "0.988547"),
            (ItemKey::ReplayGainAlbumGain, "+1.20 dB"),
            (ItemKey::ReplayGainAlbumPeak, "1.0"),
            (ItemKey::UnsyncLyrics, "la la la"),
        ] {
            tag.insert_text(key, value.into());
        }
        tag.push_picture(Picture::unchecked(b"\x89PNG\r\n\x1a\n".to_vec()).pic_type(PictureType::CoverFront).mime_type(MimeType::Png).build());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();

        let t = read_track(&path, &dir).unwrap();
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.album.as_str(), t.album_artist.as_str()), ("Song", "Singer", "Record", "Band"));
        assert_eq!((t.track_no, t.disc_no, t.year), (Some(5), Some(2), Some(2026)));
        assert_eq!((t.rg_track_gain, t.rg_track_peak, t.rg_album_gain, t.rg_album_peak), (Some(-6.54), Some(0.988547), Some(1.2), Some(1.0)));
        assert!(t.has_art && t.has_lyrics);
        assert_eq!((t.folder.as_str(), t.format.as_str()), (".", "WAV"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_and_untagged_files_fall_back_to_names() {
        let dir = temp("broken");
        let garbage = dir.join("Jpop/003 - King Gnu - 白日.mp3");
        std::fs::create_dir_all(garbage.parent().unwrap()).unwrap();
        std::fs::write(&garbage, b"definitely not an mp3 \xff\xfb\x90 frame").unwrap();
        let t = read_track(&garbage, &dir).unwrap();
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.album.as_str(), t.album_artist.as_str()), ("白日", "King Gnu", "Jpop", "King Gnu"));
        assert_eq!((t.track_no, t.format.as_str(), t.folder.as_str()), (Some(3), "MP3", "Jpop"));

        let empty = dir.join("Jpop/01 - BETELGEUSE.wav");
        std::fs::write(&empty, b"").unwrap();
        let t = read_track(&empty, &dir).unwrap();
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.track_no, t.duration), ("BETELGEUSE", UNKNOWN_ARTIST, Some(1), Duration::ZERO));

        let untagged = dir.join("tmp/005 - YOASOBI「怪物」Official Music Video.wav");
        wav(&untagged, 1, &[]);
        let t = read_track(&untagged, &dir).unwrap();
        assert_eq!((t.title.as_str(), t.artist.as_str(), t.album.as_str()), ("YOASOBI「怪物」Official Music Video", UNKNOWN_ARTIST, "tmp"));
        assert_eq!((t.track_no, t.duration, t.format.as_str()), (Some(5), Duration::from_secs(1), "WAV"));

        assert!(read_track(&dir.join("missing.mp3"), &dir).is_none());
        assert!(read_track(&dir, &dir).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tags_and_file_names_combine() {
        let dir = temp("combine");
        // (file name, INFO tags, expected title, artist, track number)
        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str, &'a str, Option<u32>);
        let cases: [Case; 5] = [
            // A title repeating the artist loses the prefix.
            ("ALI.wav", &[("INAM", "ALI - LOST IN PARADISE"), ("IART", "ALI")], "LOST IN PARADISE", "ALI", None),
            // The name's "Artist - " disagrees with the title tag: not an artist.
            ("25 - 晩餐歌 - Bansanka.wav", &[("INAM", "晩餐歌 - Bansanka")], "晩餐歌 - Bansanka", UNKNOWN_ARTIST, Some(25)),
            // It agrees: the file name supplies the missing artist.
            ("002 - Official髭男dism - Pretender.wav", &[("INAM", "Pretender")], "Pretender", "Official髭男dism", Some(2)),
            // Track tags win over the file name's number.
            ("04 - Song.wav", &[("INAM", "Song"), ("IPRT", "9")], "Song", UNKNOWN_ARTIST, Some(9)),
            // Whitespace/control characters in tags are cleaned up.
            ("x.wav", &[("INAM", "  Two\nLines  "), ("IART", "Kenshi Yonezu  米津玄師")], "Two Lines", "Kenshi Yonezu 米津玄師", None),
        ];
        for (name, info, title, artist, track_no) in cases {
            let path = dir.join(name);
            wav(&path, 1, info);
            let t = read_track(&path, &dir).unwrap();
            assert_eq!((t.title.as_str(), t.artist.as_str(), t.track_no), (title, artist, track_no), "{name}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_name_heuristics() {
        // (stem, disc, track, artist, title)
        type Case<'a> = (&'a str, Option<u32>, Option<u32>, Option<&'a str>, &'a str);
        let cases: [Case; 20] = [
            ("001 - mosi mosi？ (楽音) ／ダズビー COVER", None, Some(1), None, "mosi mosi？ (楽音) ／ダズビー COVER"),
            ("003 - King Gnu - 白日", None, Some(3), Some("King Gnu"), "白日"),
            ("01 - BETELGEUSE", None, Some(1), None, "BETELGEUSE"),
            ("005 - YOASOBI「怪物」Official Music Video", None, Some(5), None, "YOASOBI「怪物」Official Music Video"),
            ("1 - Square (2017)", None, Some(1), None, "Square (2017)"),
            ("4 - 0+0", None, Some(4), None, "0+0"),
            ("01. Intro", None, Some(1), None, "Intro"),
            ("01.Intro", None, Some(1), None, "Intro"),
            ("07_Outro", None, Some(7), None, "Outro"),
            ("1-02 Song", Some(1), Some(2), None, "Song"),
            ("2-11 - Artist - Song", Some(2), Some(11), Some("Artist"), "Song"),
            ("07 Song", None, Some(7), None, "Song"),
            ("12 Song", None, Some(12), None, "Song"),
            ("007 Song", None, Some(7), None, "Song"),
            ("500 Miles High", None, None, None, "500 Miles High"),
            ("2.0", None, None, None, "2.0"),
            ("NA - [ᴘʟᴀʏʟɪꜱᴛ] 80년대 도쿄의 밤, 시티팝", None, None, None, "[ᴘʟᴀʏʟɪꜱᴛ] 80년대 도쿄의 밤, 시티팝"),
            ("Paul Kalkbrenner – Time To Dance (Official Video)", None, None, Some("Paul Kalkbrenner"), "Time To Dance (Official Video)"),
            ("my_cool__song", None, None, None, "my cool song"),
            ("활공", None, None, None, "활공"),
        ];
        for (stem, disc_no, track_no, artist, title) in cases {
            let want = NameInfo { disc_no, track_no, artist: artist.map(String::from), title: title.into() };
            assert_eq!(parse_file_name(stem), want, "{stem}");
        }
        assert_eq!(parse_file_name("01").title, "01");
        assert_eq!(parse_file_name(" - x").artist, None);
    }

    #[test]
    fn value_parsers() {
        assert_eq!(parse_gain("-6.54 dB"), Some(-6.54));
        assert_eq!(parse_gain(" +1.23 db "), Some(1.23));
        assert_eq!(parse_gain("-6,5 dB"), Some(-6.5));
        assert_eq!(parse_gain("0.988547"), Some(0.988547));
        assert_eq!(parse_gain("loud"), None);
        assert_eq!(parse_gain("NaN"), None);
        assert_eq!(parse_year("2022"), Some(2022));
        assert_eq!(parse_year("2021-05-01T10:00"), Some(2021));
        assert_eq!(parse_year("20260522"), Some(2026));
        assert_eq!(parse_year("05/01/1999"), Some(1999));
        assert_eq!(parse_year("0000"), None);
        assert_eq!(parse_year("'99"), None);
        assert_eq!(leading_number("03/12"), Some(3));
        assert_eq!(leading_number(" 029 "), Some(29));
        assert_eq!(leading_number("0"), None);
        assert_eq!(leading_number("A1"), None);
        assert_eq!(clean_text("\u{feff} a \t b\r\n"), "a b");
        assert_eq!(clean_text("\u{305f}\u{3075}\u{3099}\u{3093} \u{1109}\u{1161}"), "たぶん 사");
        assert_eq!(
            dedupe_list("IVE, RYAN JHUN, Gucci Caliente, THE WAVYS, Ryan Jhun, Gucci Caliente, THE WAVYS,"),
            "IVE, RYAN JHUN, Gucci Caliente, THE WAVYS"
        );
        assert_eq!(dedupe_list("Earth, Wind & Fire"), "Earth, Wind & Fire");
        assert_eq!(dedupe_list(", ,"), "");
        assert_eq!(normalize_path(Path::new("/a/b/../c/./d/")), PathBuf::from("/a/c/d"));
        assert_eq!(fmt_duration(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn natural_ordering() {
        let mut v = vec!["Track 10", "track 1", "Track 2", "track 02b", "Track", "Ａ", "a", "B", "001", "1"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["001", "1", "a", "B", "Track", "track 1", "Track 2", "track 02b", "Track 10", "Ａ"]);
        assert_eq!(natural_cmp("abc", "ABC"), Ordering::Equal);
        assert_eq!(natural_cmp("x9", "x10"), Ordering::Less);
        assert_eq!(natural_cmp("x", "x1"), Ordering::Less);
        assert_eq!(natural_cmp("98765432109876543210", "98765432109876543211"), Ordering::Less);
        assert_eq!(natural_path_cmp("A/B", "A B"), Ordering::Less);
        assert_eq!(natural_path_cmp("A", "A/B"), Ordering::Less);
        assert_eq!(natural_path_cmp("Disc 2/x", "Disc 10/x"), Ordering::Less);
    }

    fn track(path: &str, title: &str, artist: &str, album: &str) -> Track {
        Track {
            path: path.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            album_artist: artist.into(),
            duration: Duration::from_secs(60),
            folder: relative_folder(Path::new(path), Path::new("/m")),
            ..Track::default()
        }
    }

    fn sample() -> Library {
        let t = |path, title, artist, album, track_no, year, genre: &str| Track {
            track_no,
            year,
            genre: genre.into(),
            ..track(path, title, artist, album)
        };
        let tracks = vec![
            t("/m/Jpop/10 - Spica.mp3", "Spica", UNKNOWN_ARTIST, "Jpop", Some(10), None, ""),
            t("/m/Jpop/02 - One Voice.mp3", "One Voice", "Rokudenashi", "One Voice", Some(2), Some(2021), "J-Pop"),
            t("/m/Jpop/01 - BETELGEUSE.mp3", "BETELGEUSE", "Yuuri", "Ichi", Some(1), Some(2022), "J-Pop"),
            Track { disc_no: Some(2), ..t("/m/Album/CD2/01.mp3", "B1", "Band", "Double", Some(1), Some(2019), "Rock") },
            Track { disc_no: Some(1), ..t("/m/Album/CD1/01.mp3", "A1", "Band", "Double", Some(1), Some(2019), "rock") },
            t("/m/Comp/02.mp3", "C2", "Y", "Now", Some(2), Some(2020), "Pop"),
            t("/m/Comp/01.mp3", "C1", "X", "Now", Some(1), Some(2020), "Pop"),
            t("/m/loose.mp3", "Loose", "yuuri", "m", None, Some(2022), "J-Pop"),
        ];
        Library::from_tracks(vec!["/m".into()], tracks)
    }

    /// (name, detail, titles) per group.
    fn summary(lib: &Library, mode: BrowseMode) -> Vec<(String, String, Vec<String>)> {
        lib.groups(mode).into_iter().map(|g| (g.name, g.detail, g.tracks.iter().map(|&i| lib.tracks[i].title.clone()).collect())).collect()
    }

    fn names(lib: &Library, mode: BrowseMode) -> Vec<String> {
        lib.groups(mode).into_iter().map(|g| g.name).collect()
    }

    #[test]
    fn groups_by_every_mode() {
        let lib = sample();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        let folders = summary(&lib, BrowseMode::Folders);
        assert_eq!(folders.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["m", "Album/CD1", "Album/CD2", "Comp", "Jpop"]);
        assert_eq!(folders[4].1, "3 tracks · 3:00");
        assert_eq!(folders[4].2, s(&["BETELGEUSE", "One Voice", "Spica"]));

        let artists = summary(&lib, BrowseMode::Artists);
        assert_eq!(names(&lib, BrowseMode::Artists), ["Band", "Rokudenashi", "X", "Y", "Yuuri", UNKNOWN_ARTIST]);
        assert_eq!(artists[0].1, "1 album · 2 tracks");
        assert_eq!(artists[0].2, s(&["A1", "B1"]));
        assert_eq!(artists[4].1, "2 albums · 2 tracks");
        assert_eq!(artists[4].2, s(&["BETELGEUSE", "Loose"]));

        let albums = summary(&lib, BrowseMode::Albums);
        assert_eq!(names(&lib, BrowseMode::Albums), ["Double", "Ichi", "Jpop", "m", "Now", "One Voice"]);
        assert_eq!(albums[0].1, "Band · 2019 · 2 tracks · 2:00");
        assert_eq!(albums[0].2, s(&["A1", "B1"]));
        assert_eq!(albums[2].1, "Unknown Artist · 1 track · 1:00");
        assert_eq!(albums[4].1, "Various Artists · 2020 · 2 tracks · 2:00");
        assert_eq!(albums[4].2, s(&["C1", "C2"]));

        let genres = summary(&lib, BrowseMode::Genres);
        assert_eq!(names(&lib, BrowseMode::Genres), ["J-Pop", "Pop", "rock", UNKNOWN_GENRE]);
        assert_eq!(genres[0].1, "2 artists · 3 tracks");
        assert_eq!(genres[0].2, s(&["One Voice", "BETELGEUSE", "Loose"]));
        assert_eq!(genres[2].2, s(&["A1", "B1"]));

        let years = summary(&lib, BrowseMode::Years);
        assert_eq!(names(&lib, BrowseMode::Years), ["2022", "2021", "2020", "2019", UNKNOWN_YEAR]);
        assert_eq!(years[0].1, "2 albums · 2 tracks");

        let all = summary(&lib, BrowseMode::Tracks);
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].0.as_str(), all[0].1.as_str()), ("All Tracks", "8 tracks · 8:00"));
        assert_eq!(all[0].2, s(&["A1", "B1", "BETELGEUSE", "C1", "C2", "Loose", "One Voice", "Spica"]));

        for mode in BrowseMode::ALL {
            let groups = Library::empty().groups(mode);
            let want = if mode == BrowseMode::Tracks { vec!["All Tracks · 0 tracks · 0:00".to_string()] } else { vec![] };
            assert_eq!(groups.iter().map(|g| format!("{} · {}", g.name, g.detail)).collect::<Vec<_>>(), want);
        }
    }

    #[test]
    fn same_album_name_by_different_album_artists_stays_apart() {
        let tracks = vec![
            Track { album_artist: "A".into(), ..track("/m/Hits/1.mp3", "a", "A", "Greatest Hits") },
            Track { album_artist: "B".into(), ..track("/m/Hits/2.mp3", "b", "B feat. C", "Greatest Hits") },
        ];
        let lib = Library::from_tracks(vec![], tracks);
        let details: Vec<String> = lib.groups(BrowseMode::Albums).into_iter().map(|g| g.detail).collect();
        assert_eq!(details, ["A · 1 track · 1:00", "B · 1 track · 1:00"]);
    }

    #[test]
    fn untagged_folder_albums_take_the_majority_artist() {
        let album = |artists: &[&str]| {
            let tracks = artists.iter().enumerate().map(|(i, a)| track(&format!("/m/Daz/{i}.mp3"), "t", a, "Daz")).collect();
            let groups = Library::from_tracks(vec![], tracks).groups(BrowseMode::Albums);
            groups.into_iter().map(|g| g.detail).collect::<Vec<_>>()
        };
        assert_eq!(album(&["DAZBEE", "dazbee", "DAZBEE, 梟note"]), ["DAZBEE · 3 tracks · 3:00"]);
        assert_eq!(album(&["A", "A", "B", "C"]), ["Various Artists · 4 tracks · 4:00"]);
    }

    fn titles(lib: &Library, ids: &[TrackId]) -> Vec<String> {
        ids.iter().map(|&i| lib.tracks[i].title.clone()).collect()
    }

    #[test]
    fn sorts_by_every_key() {
        let mut tracks = vec![
            Track { duration: Duration::from_secs(200), mtime: 30, year: Some(2001), ..track("/m/b/Track 10.mp3", "Track 10", "beta", "Z") },
            Track { duration: Duration::from_secs(100), mtime: 10, year: None, ..track("/m/a/Track 2.mp3", "Track 2", UNKNOWN_ARTIST, "Y") },
            Track { duration: Duration::from_secs(300), mtime: 20, year: Some(1999), ..track("/m/a/track 1.mp3", "track 1", "Alpha", "X") },
        ];
        tracks[0].track_no = Some(1);
        let lib = Library::from_tracks(vec![], tracks);
        let plays = |id: TrackId| [5, 0, 9][id];
        let sorted = |key, desc| {
            let mut ids = lib.all_ids();
            lib.sort(&mut ids, key, desc, &plays);
            titles(&lib, &ids)
        };
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(sorted(SortKey::Default, false), s(&["Track 10", "Track 2", "track 1"]));
        assert_eq!(sorted(SortKey::Default, true), s(&["track 1", "Track 2", "Track 10"]));
        assert_eq!(sorted(SortKey::Title, false), s(&["track 1", "Track 2", "Track 10"]));
        assert_eq!(sorted(SortKey::Title, true), s(&["Track 10", "Track 2", "track 1"]));
        assert_eq!(sorted(SortKey::Artist, false), s(&["track 1", "Track 10", "Track 2"]));
        assert_eq!(sorted(SortKey::Artist, true), s(&["Track 10", "track 1", "Track 2"]), "unknown stays last");
        assert_eq!(sorted(SortKey::Album, false), s(&["track 1", "Track 2", "Track 10"]));
        assert_eq!(sorted(SortKey::Duration, false), s(&["Track 2", "Track 10", "track 1"]));
        assert_eq!(sorted(SortKey::Year, false), s(&["track 1", "Track 10", "Track 2"]));
        assert_eq!(sorted(SortKey::Year, true), s(&["Track 10", "track 1", "Track 2"]), "unknown stays last");
        assert_eq!(sorted(SortKey::Added, true), s(&["Track 10", "track 1", "Track 2"]));
        assert_eq!(sorted(SortKey::Plays, true), s(&["track 1", "Track 10", "Track 2"]));
        assert_eq!(sorted(SortKey::Path, false), s(&["track 1", "Track 2", "Track 10"]));
        let mut random = sorted(SortKey::Random, false);
        random.sort();
        assert_eq!(random, s(&["Track 10", "Track 2", "track 1"]));

        // Stable: equal keys keep the incoming order; stale ids go last.
        let lib = Library::from_tracks(vec![], (0..6).map(|i| track(&format!("/m/{i}.mp3"), "Same", "A", "B")).collect());
        let mut ids = vec![4, 99, 1, 5, 0, 3, 2];
        lib.sort(&mut ids, SortKey::Title, false, &|_| 0);
        assert_eq!(ids, [4, 1, 5, 0, 3, 2, 99]);
        lib.sort(&mut ids, SortKey::Plays, true, &|_| 0);
        assert_eq!(ids, [4, 1, 5, 0, 3, 2, 99]);
    }

    fn config(root: &Path) -> LibraryConfig {
        LibraryConfig { dirs: vec![root.to_string_lossy().into()], extensions: vec!["wav".into()], ..LibraryConfig::default() }
    }

    #[test]
    fn scan_honors_config_and_caches() {
        let dir = temp("scan");
        let root = dir.join("music");
        wav(&root.join("A/01 - One.wav"), 1, &[("INAM", "One"), ("IART", "Alpha")]);
        wav(&root.join("A/02 - Two.wav"), 3, &[("INAM", "Two"), ("IART", "Alpha")]);
        wav(&root.join("A/.hidden.wav"), 1, &[]);
        wav(&root.join(".secret/x.wav"), 1, &[]);
        wav(&root.join("Voice Memos/memo.wav"), 1, &[]);
        wav(&root.join("B/Loud.WAV"), 1, &[]);
        std::fs::write(root.join("B/notes.txt"), "not music").unwrap();
        let mut cfg = config(&root);
        cfg.exclude = vec!["/Voice Memos/".into()];
        cfg.dirs.push(format!("{}/../music/", root.display())); // duplicate root, differently spelled
        let cache = dir.join("cache/library.json");

        let progress = Mutex::new(Vec::new());
        let lib = scan(&cfg, &cache, false, &|done, total| progress.lock().unwrap().push((done, total)));
        let got: Vec<&str> = lib.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(got, ["One", "Two", "Loud"]);
        assert_eq!((lib.roots.len(), &lib.roots[0]), (1, &root));
        assert_eq!(lib.find(&root.join("A/02 - Two.wav")), Some(1));
        assert_eq!(progress.lock().unwrap().last(), Some(&(3, 3)));
        assert!(cache.exists());

        // Cached entries are reused as long as size + mtime match (prove it by editing the cache).
        let edit_cache = |from: &str, to: &str| {
            let text = std::fs::read_to_string(&cache).unwrap();
            std::fs::write(&cache, text.replace(from, to)).unwrap();
        };
        edit_cache("\"One\"", "\"Cached One\"");
        let lib = scan(&cfg, &cache, false, &|_, _| {});
        assert_eq!(lib.tracks[0].title, "Cached One");
        // ... unless forced,
        assert_eq!(scan(&cfg, &cache, true, &|_, _| {}).tracks[0].title, "One");
        // ... or the file changed.
        edit_cache("\"One\"", "\"Cached One\"");
        wav(&root.join("A/01 - One.wav"), 2, &[("INAM", "One"), ("IART", "Alpha")]);
        let lib = scan(&cfg, &cache, false, &|_, _| {});
        assert_eq!((lib.tracks[0].title.as_str(), lib.tracks[0].duration), ("One", Duration::from_secs(2)));

        // Deleted files drop out of the cache; a corrupt cache is ignored and rewritten.
        std::fs::remove_file(root.join("B/Loud.WAV")).unwrap();
        assert_eq!(scan(&cfg, &cache, false, &|_, _| {}).len(), 2);
        assert_eq!(load_cache(&cache).tracks.len(), 2);
        std::fs::write(&cache, "{ not json").unwrap();
        assert_eq!(scan(&cfg, &cache, false, &|_, _| {}).len(), 2);
        assert_eq!(load_cache(&cache).tracks.len(), 2);

        // min_duration hides short tracks but keeps them cached.
        cfg.min_duration_secs = 3;
        let lib = scan(&cfg, &cache, false, &|_, _| {});
        assert_eq!(lib.tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Two"]);
        assert_eq!(load_cache(&cache).tracks.len(), 2);

        // use_cache = false neither reads nor writes it.
        std::fs::remove_file(&cache).unwrap();
        cfg.use_cache = false;
        assert_eq!(scan(&cfg, &cache, false, &|_, _| {}).len(), 1);
        assert!(!cache.exists());

        // Hidden files are only skipped when asked to.
        cfg.ignore_hidden = false;
        cfg.min_duration_secs = 0;
        assert_eq!(scan(&cfg, &cache, false, &|_, _| {}).len(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn symlinks_are_followed_without_loops() {
        let dir = temp("symlinks");
        let root = dir.join("music");
        wav(&root.join("A/x.wav"), 1, &[]);
        wav(&dir.join("elsewhere/y.wav"), 1, &[]);
        std::os::unix::fs::symlink(&root, root.join("A/loop")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), root.join("linked")).unwrap();
        std::os::unix::fs::symlink(root.join("A/x.wav"), root.join("x-again.wav")).unwrap();
        std::os::unix::fs::symlink(dir.join("nowhere"), root.join("broken.wav")).unwrap();
        let mut cfg = config(&root);
        cfg.use_cache = false;
        let cache = dir.join("cache.json");
        assert_eq!(scan(&cfg, &cache, false, &|_, _| {}).len(), 1);
        cfg.follow_symlinks = true;
        let lib = scan(&cfg, &cache, false, &|_, _| {});
        // x.wav is reachable three ways (directly, via the loop, via x-again.wav): kept once.
        assert_eq!(lib.len(), 2);
        assert!(lib.find(&root.join("linked/y.wav")).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn add_paths_appends_new_files_and_reuses_known_ones() {
        let dir = temp("add");
        let root = dir.join("music");
        wav(&root.join("A/one.wav"), 1, &[("INAM", "One")]);
        wav(&dir.join("Downloads/single.wav"), 1, &[("INAM", "Single")]);
        wav(&dir.join("Downloads/Album/02.wav"), 1, &[("INAM", "Second")]);
        wav(&dir.join("Downloads/Album/01.wav"), 1, &[("INAM", "First")]);
        let mut cfg = config(&root);
        cfg.use_cache = false;
        let mut lib = scan(&cfg, &dir.join("cache.json"), false, &|_, _| {});
        assert_eq!(lib.len(), 1);

        let paths = [
            dir.join("Downloads/single.wav"),
            root.join("A/../A/one.wav"),
            dir.join("Downloads/Album"),
            dir.join("Downloads/nothing-here.wav"),
            dir.join("Downloads/single.wav"),
        ];
        let ids = lib.add_paths(&paths, &cfg);
        assert_eq!(titles(&lib, &ids), ["Single", "One", "First", "Second", "Single"]);
        assert_eq!(lib.len(), 4);
        assert_eq!(lib.tracks[ids[0]].folder, "Downloads");
        assert_eq!(lib.tracks[ids[2]].folder, "Album");
        assert_eq!(lib.tracks[ids[1]].folder, "A");
        assert!(lib.tracks.iter().enumerate().all(|(i, t)| t.id == i && lib.find(&t.path) == Some(i)));
        // Adding again changes nothing.
        assert_eq!(lib.add_paths(&paths, &cfg), ids);
        assert_eq!(lib.len(), 4);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn collects_audio_files_sorted() {
        let dir = temp("collect");
        wav(&dir.join("b/2.wav"), 1, &[]);
        wav(&dir.join("a.wav"), 1, &[]);
        wav(&dir.join(".x/3.wav"), 1, &[]);
        let cfg = config(&dir);
        assert_eq!(collect_audio_files(&dir, &cfg), [dir.join("a.wav"), dir.join("b/2.wav")]);
        assert_eq!(collect_audio_files(&dir.join("a.wav"), &cfg), [dir.join("a.wav")]);
        assert!(collect_audio_files(&dir.join("b/none.wav"), &cfg).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
