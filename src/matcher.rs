//! Matches "artist - title" from the player against `*.lrc` filenames in a folder.
//!
//! Comparison is fuzzy: case, punctuation and spacing are ignored, and common
//! decorations like "(feat. X)", "- Remastered 2011" or "[Live]" are tolerated.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

struct Entry {
    path: PathBuf,
    artist: Option<Variants>,
    title: Variants,
    /// Normalized whole filename stem, for "title - artist" swapped names.
    whole: String,
}

struct Variants {
    full: String,
    set: HashSet<String>,
}

#[derive(Default)]
pub struct Index {
    entries: Vec<Entry>,
}

impl Index {
    pub fn build(dir: &Path) -> Index {
        let mut entries = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let path = e.path();
                let is_lrc = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lrc"));
                if !is_lrc {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                let (artist, title) = match stem.split_once(" - ") {
                    Some((a, t)) => (Some(artist_variants(a)), title_variants(t)),
                    None => (None, title_variants(stem)),
                };
                entries.push(Entry { whole: norm(stem), path, artist, title });
            }
        }
        Index { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn find(&self, artist: &str, title: &str) -> Option<&Path> {
        let sa = artist_variants(artist);
        let st = title_variants(title);
        let swapped = format!("{}{}", st.full, sa.full);
        let mut best: Option<(u32, &Entry)> = None;
        for e in &self.entries {
            let score = if e.whole == swapped {
                12
            } else if e.title.set.is_disjoint(&st.set) {
                0
            } else {
                let exact_title = if e.title.full == st.full { 2 } else { 0 };
                match &e.artist {
                    Some(fa) if artists_match(fa, &sa) => 10 + exact_title,
                    Some(_) => 0, // same title, different artist: probably a cover, don't use
                    None => 3 + exact_title,
                }
            };
            if score > 0 && best.is_none_or(|(b, _)| score > b) {
                best = Some((score, e));
            }
        }
        best.map(|(_, e)| e.path.as_path())
    }
}

fn artists_match(a: &Variants, b: &Variants) -> bool {
    !a.set.is_disjoint(&b.set)
        || (!a.full.is_empty() && b.full.contains(&a.full))
        || (!b.full.is_empty() && a.full.contains(&b.full))
}

/// Lowercase and keep only letters/digits ("&" counts as "and").
fn norm(s: &str) -> String {
    s.replace('&', " and ")
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn variants(raw: &str, extra: impl IntoIterator<Item = String>) -> Variants {
    let full = norm(raw);
    let mut set: HashSet<String> = extra.into_iter().map(|s| norm(&s)).filter(|s| !s.is_empty()).collect();
    if !full.is_empty() {
        set.insert(full.clone());
    }
    Variants { full, set }
}

fn title_variants(t: &str) -> Variants {
    let mut extra = Vec::new();
    let no_brackets = strip_brackets(t);
    extra.push(no_brackets.clone());
    // "Song - Remastered 2011", "Song - Live at X"
    for s in [t, no_brackets.as_str()] {
        if let Some((head, _)) = s.split_once(" - ") {
            extra.push(head.to_owned());
        }
    }
    let lower = no_brackets.to_lowercase();
    for kw in [" feat.", " feat ", " ft.", " featuring "] {
        if let Some(i) = lower.find(kw) {
            extra.push(no_brackets[..i].to_owned());
        }
    }
    variants(t, extra)
}

fn artist_variants(a: &str) -> Variants {
    // Every individual artist of "A, B & C feat. D" is a variant.
    let mut parts = vec![a.to_owned()];
    let lower = a.to_lowercase();
    let mut pieces = vec![lower];
    for sep in [", ", " & ", " and ", " feat. ", " feat ", " ft. ", " featuring ", " x ", " vs. ", "; ", " / "] {
        pieces = pieces.iter().flat_map(|p| p.split(sep)).map(str::to_owned).collect();
    }
    parts.extend(pieces);
    variants(a, parts)
}

fn strip_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(names: &[&str]) -> Index {
        let entries = names
            .iter()
            .map(|n| {
                let (artist, title) = match n.split_once(" - ") {
                    Some((a, t)) => (Some(artist_variants(a)), title_variants(t)),
                    None => (None, title_variants(n)),
                };
                Entry { path: PathBuf::from(n), artist, title, whole: norm(n) }
            })
            .collect();
        Index { entries }
    }

    fn find<'a>(i: &'a Index, a: &str, t: &str) -> Option<&'a str> {
        i.find(a, t).and_then(|p| p.to_str())
    }

    #[test]
    fn matches_fuzzy() {
        let i = idx(&["panic at the disco - build god then we'll talk", "queen - bohemian rhapsody", "Yesterday"]);
        assert_eq!(find(&i, "Panic! At The Disco", "Build God, Then We'll Talk"), Some("panic at the disco - build god then we'll talk"));
        assert_eq!(find(&i, "Queen", "Bohemian Rhapsody - Remastered 2011"), Some("queen - bohemian rhapsody"));
        assert_eq!(find(&i, "The Beatles", "Yesterday - Remastered 2009"), Some("Yesterday"));
        assert_eq!(find(&i, "Some Cover Band", "Bohemian Rhapsody"), None);
        assert_eq!(find(&i, "Queen", "Another One Bites the Dust"), None);
    }

    #[test]
    fn matches_multi_artist_and_swapped() {
        let i = idx(&["daft punk - get lucky", "Starboy - The Weeknd"]);
        assert_eq!(find(&i, "Daft Punk, Pharrell Williams, Nile Rodgers", "Get Lucky (feat. Pharrell Williams)"), Some("daft punk - get lucky"));
        assert_eq!(find(&i, "The Weeknd", "Starboy"), Some("Starboy - The Weeknd"));
    }
}

#[cfg(test)]
mod example_tests {
    /// The bundled public-domain example must parse as plain text and match any recording.
    #[test]
    fn amazing_grace_example() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let l = crate::lrc::load(&dir.join("amazing grace.lrc")).unwrap();
        assert!(!l.synced, "the example should stay plain text so people can try sync mode on it");
        assert_eq!(l.lines.len(), 24);
        assert_eq!(l.lines[0].text, "Amazing grace! How sweet the sound");
        let idx = super::Index::build(&dir);
        for (artist, title) in [("Judy Collins", "Amazing Grace"), ("Some Choir", "Amazing Grace - Live"), ("X", "Amazing Grace (My Chains Are Gone)")] {
            assert!(idx.find(artist, title).is_some(), "{artist} - {title}");
        }
    }
}
