//! Search over the whole library (title, artist, album, file name): substring, else fuzzy.
//!
//! Each track's haystack is `"{title} {artist} {album} {file stem}"`. Tracks whose words all match
//! within the tags part, `"{title} {artist} {album}"`, are scored and highlighted there; the rest
//! are file-name hits: a file named "06 - Toumeina Wakusei.mp3" is found by its romanized name even
//! when its title tag is "透明な惑星". File-name hits rank after equally scored tag hits.
//! Haystacks are built once per library (see `Library::generation`), so typing stays fast.
//!
//! Query syntax (fzf-style, from nucleo): space-separated words must all match, in any field and
//! any order. A plain word matches as a substring; only when no track matches that way are the
//! plain words matched fuzzily (letters in order, gaps allowed: typos, abbreviations). `^word` =
//! prefix, `word$` = suffix, `!word` = must not match (tags or file name), `'word` = substring.
//! Field filters apply one word to one field, with the same syntax after the colon:
//! `title:`/`t:`, `artist:`/`a:`, `album:`/`al:`, `genre:`/`g:`, `path:`/`p:`, and `year:`/`y:`
//! taking `2021`, `2010-2019`, `2015-` or `-1999`.
//!
//! Matching ignores case, Latin diacritics (é = e), character width (ＡＢＣ = ABC, full-width
//! spaces separate words), katakana vs hiragana (ヨルシカ = よるしか) and Unicode normalization
//! (decomposed file names match composed input).

use nucleo_matcher::pattern::{Atom, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str, Utf32String};
use unicode_normalization::UnicodeNormalization;

use crate::library::{Library, Track, TrackId};

pub struct Searcher {
    matcher: Matcher,
    /// One haystack per track of the library generation below.
    hays: Vec<Hay>,
    generation: Option<u64>,
    /// Scratch buffer for genre/path filters.
    field: Vec<char>,
}

/// One result, best first.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: TrackId,
    pub score: u32,
    /// Char indices into `format!("{} {} {}", title, artist, album)` that matched (for highlighting).
    /// Sorted and unique; words that only matched the file name (or a genre/path/year filter)
    /// have nothing to highlight.
    pub matched: Vec<u32>,
}

/// A track's folded `"{title} {artist} {album} {file stem}"`, one char per char of the original
/// (so match indices are char indices, not grapheme indices).
struct Hay {
    text: Utf32String,
    /// Char lengths of the title, artist and album at the start of `text`.
    title: usize,
    artist: usize,
    album: usize,
}

impl Hay {
    fn new(t: &Track) -> Hay {
        // tags are NFC already (library.rs); file names on macOS are often decomposed
        let stem: String = t.path.file_stem().unwrap_or_default().to_string_lossy().nfc().collect();
        let text = format!("{} {} {} {stem}", t.title, t.artist, t.album);
        let text = if text.is_ascii() { Utf32String::Ascii(text.into_boxed_str()) } else { Utf32String::Unicode(text.chars().map(fold).collect()) };
        Hay { text, title: t.title.chars().count(), artist: t.artist.chars().count(), album: t.album.chars().count() }
    }

    /// Length of `"{title} {artist} {album}"`, the part `Hit::matched` indexes.
    fn tags_len(&self) -> usize {
        self.title + self.artist + self.album + 2
    }
}

impl Searcher {
    pub fn new() -> Searcher {
        let mut config = Config::DEFAULT;
        // A small bonus for matches near the start: titles win over artist/album matches.
        config.prefer_prefix = true;
        Searcher { matcher: Matcher::new(config), hays: Vec::new(), generation: None, field: Vec::new() }
    }

    /// Up to `limit` best matches (0 = no limit). An empty query returns nothing. Case- and
    /// width-insensitive; multiple words must all match (in any field). See the module docs for
    /// the query syntax.
    pub fn search(&mut self, lib: &Library, query: &str, limit: usize) -> Vec<Hit> {
        let mut query = Query::parse(query);
        if query.is_empty() {
            return Vec::new();
        }
        if self.generation != Some(lib.generation()) || self.hays.len() != lib.len() {
            self.hays = lib.tracks.iter().map(Hay::new).collect();
            self.generation = Some(lib.generation());
        }
        let Searcher { matcher, hays, field, .. } = self;
        let mut score_all = |q: &Query| -> Vec<(u32, bool, usize)> {
            lib.tracks
                .iter()
                .zip(hays.iter())
                .enumerate()
                .filter_map(|(i, (t, hay))| eval(matcher, field, q, t, hay, None).map(|(score, by_name)| (score, by_name, i)))
                .collect()
        };
        let mut hits = score_all(&query);
        if hits.is_empty()
            && let Some(fuzzy) = query.fuzzy.take()
        {
            query.words = fuzzy;
            hits = score_all(&query);
        }
        hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        if limit > 0 {
            hits.truncate(limit);
        }
        hits.into_iter()
            .map(|(score, _, i)| {
                let mut matched = Vec::new();
                eval(matcher, field, &query, &lib.tracks[i], &hays[i], Some(&mut matched));
                matched.sort_unstable();
                matched.dedup();
                Hit { id: lib.tracks[i].id, score, matched }
            })
            .collect()
    }
}

/// Scores one track: `Some((score, matched only thanks to the file name))`. With `out`, also
/// collects highlight indices into the tags part.
fn eval(matcher: &mut Matcher, field: &mut Vec<char>, q: &Query, t: &Track, hay: &Hay, mut out: Option<&mut Vec<u32>>) -> Option<(u32, bool)> {
    if let Some((lo, hi)) = q.years
        && !t.year.is_some_and(|y| (lo..=hi).contains(&y))
    {
        return None;
    }
    let (mut score, mut by_name) = (0, false);
    if !q.words.atoms.is_empty() {
        // The full haystack first: most tracks fail here, after a single match.
        let full = q.words.score(hay.text.slice(..), matcher)?;
        let start = out.as_deref().map_or(0, Vec::len);
        let tags = hay.text.slice(..hay.tags_len());
        match run_pattern(&q.words, tags, matcher, out.as_deref_mut()) {
            Some(s) => score = s,
            None => {
                (score, by_name) = (full, true);
                if let Some(out) = out.as_deref_mut() {
                    out.truncate(start);
                    q.words.indices(hay.text.slice(..), matcher, out);
                    let tags_len = hay.tags_len() as u32;
                    out.retain(|&i| i < tags_len);
                }
            }
        }
    }
    for (f, atom) in &q.fields {
        let path;
        let (h, offset) = match f {
            Field::Title => (hay.text.slice(..hay.title), Some(0)),
            Field::Artist => (hay.text.slice(hay.title + 1..hay.title + 1 + hay.artist), Some(hay.title + 1)),
            Field::Album => {
                let at = hay.title + hay.artist + 2;
                (hay.text.slice(at..at + hay.album), Some(at))
            }
            Field::Genre => (haystack(&t.genre, field), None),
            Field::Path => {
                path = t.path.to_string_lossy();
                (haystack(&path, field), None)
            }
        };
        score += u32::from(match out.as_deref_mut() {
            Some(out) => {
                let start = out.len();
                let s = atom.indices(h, matcher, out)?;
                match offset {
                    Some(offset) => out[start..].iter_mut().for_each(|i| *i += offset as u32),
                    None => out.truncate(start),
                }
                s
            }
            None => atom.score(h, matcher)?,
        });
    }
    Some((score, by_name))
}

fn run_pattern(p: &Pattern, h: Utf32Str<'_>, matcher: &mut Matcher, out: Option<&mut Vec<u32>>) -> Option<u32> {
    match out {
        Some(out) => p.indices(h, matcher, out),
        None => p.score(h, matcher),
    }
}

/// `s` as matcher input: ASCII as is, anything else folded into `buf` one char per char.
fn haystack<'a>(s: &'a str, buf: &'a mut Vec<char>) -> Utf32Str<'a> {
    if s.is_ascii() {
        return Utf32Str::Ascii(s.as_bytes());
    }
    buf.clear();
    buf.extend(s.chars().map(fold));
    Utf32Str::Unicode(buf)
}

/// One-to-one folding applied to haystacks and queries: full-width ASCII -> ASCII, ideographic
/// space -> space, katakana -> hiragana, "⧸" (yt-dlp's slash in file names) -> "/".
fn fold(c: char) -> char {
    let shifted = |delta: u32| char::from_u32(c as u32 - delta).unwrap_or(c);
    match c {
        '\u{3000}' => ' ',
        '\u{ff01}'..='\u{ff5e}' => shifted(0xfee0),
        '\u{30a1}'..='\u{30f6}' => shifted(0x60),
        '\u{29f8}' => '/',
        _ => c,
    }
}

enum Field {
    Title,
    Artist,
    Album,
    Genre,
    Path,
}

struct Query {
    /// The words, matched against the haystack (plain words as substrings).
    words: Pattern,
    /// The words with plain ones matched fuzzily, if there are plain ones: tried when `words`
    /// finds nothing.
    fuzzy: Option<Pattern>,
    fields: Vec<(Field, Atom)>,
    /// Inclusive year range.
    years: Option<(u32, u32)>,
}

impl Query {
    fn parse(input: &str) -> Query {
        let folded: String = input.nfc().map(fold).collect();
        let (mut words, mut fields, mut years) = (String::new(), Vec::new(), None);
        for token in folded.split(' ').filter(|t| !t.is_empty()) {
            if let Some((key, value)) = token.split_once(':') {
                let field = match key.to_ascii_lowercase().as_str() {
                    "t" | "title" => Some(Field::Title),
                    "a" | "artist" => Some(Field::Artist),
                    "al" | "album" => Some(Field::Album),
                    "g" | "genre" => Some(Field::Genre),
                    "p" | "path" => Some(Field::Path),
                    // A filter still being typed ("a:") constrains nothing yet.
                    "y" | "year" if value.is_empty() => continue,
                    "y" | "year" => match parse_years(value) {
                        Some(range) => {
                            years = Some(range);
                            continue;
                        }
                        None => None,
                    },
                    _ => None,
                };
                if let Some(field) = field {
                    let atom = Atom::parse(value, CaseMatching::Ignore, Normalization::Smart);
                    if !atom.needle_text().is_empty() {
                        fields.push((field, atom));
                    }
                    continue;
                }
            }
            words.push_str(token);
            words.push(' ');
        }
        let plain = |w: &str| !w.starts_with(['\'', '^', '!', '\\']) && !w.ends_with('$');
        let exact: Vec<String> = words.split_whitespace().map(|w| if plain(w) { format!("'{w}") } else { w.to_string() }).collect();
        let fuzzy = words.split_whitespace().any(plain).then(|| Pattern::parse(&words, CaseMatching::Ignore, Normalization::Smart));
        Query { words: Pattern::parse(&exact.join(" "), CaseMatching::Ignore, Normalization::Smart), fuzzy, fields, years }
    }

    fn is_empty(&self) -> bool {
        self.words.atoms.is_empty() && self.fields.is_empty() && self.years.is_none()
    }
}

/// "2021" | "2010-2019" | "2015-" | "-1999" -> inclusive range.
fn parse_years(s: &str) -> Option<(u32, u32)> {
    let bound = |s: &str, open: u32| if s.is_empty() { Some(open) } else { s.parse().ok() };
    match s.split_once('-') {
        Some(("", "")) => None,
        Some((lo, hi)) => Some((bound(lo, 0)?, bound(hi, u32::MAX)?)),
        None => s.parse().ok().map(|y| (y, y)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn track(path: &str, title: &str, artist: &str, album: &str, year: u32, genre: &str) -> Track {
        Track {
            path: path.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            album_artist: artist.into(),
            year: Some(year),
            genre: genre.into(),
            ..Track::default()
        }
    }

    fn lib() -> Library {
        Library::from_tracks(
            vec![],
            vec![
                track("/m/Lemon.mp3", "Lemon", "米津玄師", "STRAY SHEEP", 2020, "J-Pop"),
                track("/m/Lemonade.mp3", "Lemonade", "Beyoncé", "Lemonade", 2016, "R&B"),
                track("/m/tmp/005 - YOASOBI「怪物」.mp3", "怪物", "YOASOBI", "怪物", 2021, "J-Pop"),
                track("/m/tmp/035 - YOASOBI「群青」.mp3", "群青", "YOASOBI", "THE BOOK", 2020, "J-Pop"),
                track("/m/Jpop/ただ君に晴れ.mp3", "ただ君に晴れ", "ヨルシカ", "負け犬にアンコールはいらない", 2018, "Rock"),
                track("/m/Jpop/06 - Toumeina Wakusei.mp3", "透明な惑星", "SennaRin", "SAIHATE", 2022, "Anime"),
                track("/m/Jpop/45 - Mela!.mp3", "Ｍｅｌａ！", "緑黄色社会", "Mela!", 2020, "J-Pop"),
                track("/m/live/Lemon (Live).mp3", "Lemon (Live)", "米津玄師", "Live", 2019, "J-Pop"),
                track("/m/Lemon Tree.mp3", "Mystery", "Nobody", "Untitled", 1995, "Pop"),
            ],
        )
    }

    fn ids(lib: &Library, query: &str) -> Vec<TrackId> {
        Searcher::new().search(lib, query, 100).iter().map(|h| h.id).collect()
    }

    /// The haystack chars at the hit's `matched` indices.
    fn highlighted(lib: &Library, hit: &Hit) -> String {
        let t = &lib.tracks[hit.id];
        let hay: Vec<char> = format!("{} {} {}", t.title, t.artist, t.album).chars().collect();
        hit.matched.iter().map(|&i| hay[i as usize]).collect()
    }

    #[test]
    fn empty_queries_find_nothing() {
        let lib = lib();
        for q in ["", "   ", "\u{3000}", "!"] {
            assert!(ids(&lib, q).is_empty(), "{q:?}");
        }
    }

    #[test]
    fn ranks_best_first_and_highlights() {
        let lib = lib();
        let hits = Searcher::new().search(&lib, "lemon", 100);
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), [0, 1, 7, 8]);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(hits[0].matched, [0, 1, 2, 3, 4]);
        // Only found through its file name "Lemon Tree": nothing to highlight in the tags.
        assert!(hits[3].matched.is_empty());

        let hit = &Searcher::new().search(&lib, "yoasobi 怪物", 10)[0];
        assert_eq!(hit.id, 2);
        let text = highlighted(&lib, hit);
        assert!(text.contains("YOASOBI") && text.chars().count() == 9, "{text}");
        assert!(hit.matched.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    }

    #[test]
    fn words_must_all_match_in_any_order() {
        let lib = lib();
        assert_eq!(ids(&lib, "yoasobi 怪物"), [2]);
        assert_eq!(ids(&lib, "怪物 yoasobi"), [2]);
        assert_eq!(ids(&lib, "yoasobi\u{3000}群青"), [3]);
        assert!(ids(&lib, "yoasobi lemon").is_empty());
        assert_eq!(ids(&lib, "^lemona"), [1]);
        assert_eq!(ids(&lib, "'sheep"), [0]);
    }

    #[test]
    fn plain_words_match_as_substrings_else_fuzzily() {
        let lib = lib();
        // "tree" is in one file name; fuzzily it would also match "STRAY SHEEP"
        assert_eq!(ids(&lib, "tree"), [8]);
        // no substring anywhere: letters in order still find something (typos, abbreviations)
        assert_eq!(ids(&lib, "lmn"), [0, 1, 7, 8]);
        assert_eq!(ids(&lib, "たぶん"), Vec::<TrackId>::new());
    }

    #[test]
    fn folds_width_kana_case_and_accents() {
        let lib = lib();
        assert_eq!(ids(&lib, "ｙｏａｓｏｂｉ"), [2, 3]);
        assert_eq!(ids(&lib, "YoAsObI"), [2, 3]);
        assert_eq!(ids(&lib, "よるしか"), [4]);
        assert_eq!(ids(&lib, "ヨルシカ"), [4]);
        assert_eq!(ids(&lib, "beyonce"), [1]);
        let hit = &Searcher::new().search(&lib, "mela", 10)[0];
        assert_eq!((hit.id, highlighted(&lib, hit).as_str()), (6, "Ｍｅｌａ"));
    }

    #[test]
    fn falls_back_to_the_file_name() {
        let lib = lib();
        let hits = Searcher::new().search(&lib, "toumeina", 10);
        assert_eq!((hits.len(), hits[0].id, hits[0].matched.len()), (1, 5, 0));
        // Words can split between tags and file name; highlights cover the tags part only.
        let hit = &Searcher::new().search(&lib, "wakusei sennarin", 10)[0];
        assert_eq!((hit.id, highlighted(&lib, hit).as_str()), (5, "SennaRin"));
        // Exclusions still apply to the tags.
        assert_eq!(ids(&lib, "lemon !live"), [0, 1, 8]);
    }

    #[test]
    fn field_filters() {
        let lib = lib();
        assert_eq!(ids(&lib, "a:yoasobi"), [2, 3]);
        assert_eq!(ids(&lib, "artist:yoasobi 群青"), [3]);
        assert!(ids(&lib, "t:yoasobi").is_empty());
        assert_eq!(ids(&lib, "al:lemonade"), [1]);
        assert_eq!(ids(&lib, "G:rock"), [4]);
        assert_eq!(ids(&lib, "p:live"), [7]);
        assert_eq!(ids(&lib, "a:米津 !live"), [0]);
        assert_eq!(ids(&lib, "a:米津 al:!live"), [0]);
        assert_eq!(ids(&lib, "y:2020"), [0, 3, 6]);
        assert_eq!(ids(&lib, "year:2019-2020"), [0, 3, 6, 7]);
        assert_eq!(ids(&lib, "y:2021-"), [2, 5]);
        assert_eq!(ids(&lib, "y:-2000"), [8]);
        assert_eq!(ids(&lib, "y:2020 lemon"), [0]);
        assert!(ids(&lib, "year:abc").is_empty(), "not a year: a plain word");
        assert_eq!(ids(&lib, "yoasobi a:"), [2, 3], "an unfinished filter changes nothing");
        assert_eq!(ids(&lib, "yoasobi y:"), [2, 3]);
        assert!(ids(&lib, "a: y:").is_empty());

        let hit = &Searcher::new().search(&lib, "a:yoasobi", 10)[0];
        assert_eq!(highlighted(&lib, hit), "YOASOBI");
        let hit = &Searcher::new().search(&lib, "al:book", 10)[0];
        assert_eq!((hit.id, highlighted(&lib, hit).as_str()), (3, "BOOK"));
        // Genre/path/year filters narrow the results but have nothing to highlight.
        let hits = Searcher::new().search(&lib, "g:j-pop 群青", 10);
        assert_eq!((hits.len(), hits[0].id, highlighted(&lib, &hits[0]).as_str()), (1, 3, "群青"));
    }

    #[test]
    fn limits_results() {
        let lib = lib();
        let mut s = Searcher::new();
        assert_eq!(s.search(&lib, "e", 2).len(), 2);
        let all = s.search(&lib, "e", 0).len();
        assert_eq!(all, s.search(&lib, "e", 1000).len());
        assert!(all > 2);
    }

    #[test]
    fn fast_on_large_libraries() {
        let artists = ["YOASOBI", "米津玄師", "Official髭男dism", "Mrs. GREEN APPLE", "ヨルシカ", "Ado", "King Gnu", "Aimer"];
        let tracks = (0..5000)
            .map(|i| {
                let artist = artists[i % artists.len()];
                let path = format!("/m/Folder {}/{:03} - {artist} - Song {i}.mp3", i / 100, i % 100);
                track(&path, &format!("Song {i} 曲 {}", i * 7), artist, &format!("Album {}", i / 12), 2000 + (i % 25) as u32, "J-Pop")
            })
            .collect();
        let lib = Library::from_tracks(vec![], tracks);
        let mut s = Searcher::new();
        let t0 = Instant::now();
        s.search(&lib, "x", 200);
        eprintln!("first search over 5000 tracks (builds the haystacks): {:?}", t0.elapsed());
        for q in ["song 4242", "yoasobi", "曲 77", "a:ado album 3", "y:2010-2012 king", "zzzz", "e"] {
            s.search(&lib, q, 200); // warm up
            let t0 = Instant::now();
            let hits = s.search(&lib, q, 200);
            let took = t0.elapsed();
            eprintln!("search {q:?} over 5000 tracks: {} hits in {took:?}", hits.len());
            assert!(took.as_millis() < 100, "{q:?} took {took:?}");
        }
    }
}
