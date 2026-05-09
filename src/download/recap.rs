//! Per-run highlights for the friendly summary card.
//!
//! Tracks three "interesting facts" about each sync cycle: the largest
//! file downloaded, the oldest by EXIF date, and the album whose
//! newest-asset capture date is the most recent. The first two are
//! straightforward folds; the album highlight uses a small map keyed by
//! pass label because iCloud doesn't expose an album creation timestamp,
//! so "newest album" is operationalised as "album whose newest asset is
//! newest" (i.e. the album that just gained the latest-captured photo).
//!
//! Off mode never observes anything: the renderer guards on
//! `Mode::is_friendly()` and the structures here are tiny enough that
//! the dead-code path in off mode allocates only a handful of bytes per
//! cycle (Default::default()).

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use chrono::{DateTime, Local};

/// One asset observation: just enough to render a recap line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecapAsset {
    /// Display-only filename (basename of the on-disk path).
    pub filename: String,
    /// Asset bytes as reported by the version size selected for download.
    pub bytes: u64,
    /// Local-time capture date pulled off the iCloud asset; preserved
    /// here so the rendered line can say "1998-06-15" without having to
    /// re-query the state DB.
    pub created_local: DateTime<Local>,
}

/// Aggregated highlights for a single sync run. Folded across streaming
/// and cleanup passes via `merge`. Accumulating across watch cycles is
/// not done — each cycle resets so the recap reads as "this cycle".
#[derive(Debug, Default, Clone)]
pub struct RunRecap {
    /// Largest file by `bytes`.
    pub biggest: Option<RecapAsset>,
    /// Earliest by `created_local` — the "oldest backfilled photo".
    pub oldest: Option<RecapAsset>,
    /// Per-album newest-asset tracker. Reduced to a single highlight by
    /// `top_album`; held as a map so multi-pass runs accumulate without
    /// losing per-album resolution.
    pub albums: HashMap<String, RecapAsset>,
}

impl RunRecap {
    /// Record one successful download under the given pass label. Takes
    /// ownership of `asset` so the final move into the album map avoids
    /// the dead clone the `entry(..).and_modify(..).or_insert(asset)`
    /// shape would force (or_insert consumes its arg even when the entry
    /// existed). At most two clones per call (biggest, oldest); typical
    /// case is one or zero.
    pub fn observe(&mut self, pass_label: &str, asset: RecapAsset) {
        if self
            .biggest
            .as_ref()
            .is_none_or(|cur| asset.bytes > cur.bytes)
        {
            self.biggest = Some(asset.clone());
        }
        if self
            .oldest
            .as_ref()
            .is_none_or(|cur| asset.created_local < cur.created_local)
        {
            self.oldest = Some(asset.clone());
        }
        match self.albums.entry(pass_label.to_string()) {
            Entry::Occupied(mut e) => {
                if asset.created_local > e.get().created_local {
                    *e.get_mut() = asset;
                }
            }
            Entry::Vacant(e) => {
                e.insert(asset);
            }
        }
    }

    /// Combine two recaps (e.g. streaming pass result + cleanup pass
    /// result for one cycle). `other` wins ties on biggest because the
    /// later pass observed it last; deterministic enough for display.
    pub fn merge(&mut self, other: RunRecap) {
        if let Some(b) = other.biggest {
            if self.biggest.as_ref().is_none_or(|cur| b.bytes >= cur.bytes) {
                self.biggest = Some(b);
            }
        }
        if let Some(o) = other.oldest {
            if self
                .oldest
                .as_ref()
                .is_none_or(|cur| o.created_local <= cur.created_local)
            {
                self.oldest = Some(o);
            }
        }
        for (label, asset) in other.albums {
            self.albums
                .entry(label)
                .and_modify(|cur| {
                    if asset.created_local > cur.created_local {
                        *cur = asset.clone();
                    }
                })
                .or_insert(asset);
        }
    }

    /// Return the `(album_label, newest_asset)` whose newest asset has
    /// the most recent `created_local`. `None` when no albums were
    /// observed (e.g. unfiled-only run with the unfiled label still
    /// present, in which case it's returned as the sole entry).
    pub fn top_album(&self) -> Option<(&str, &RecapAsset)> {
        self.albums
            .iter()
            .max_by_key(|(_, asset)| asset.created_local)
            .map(|(label, asset)| (label.as_str(), asset))
    }

    /// True when nothing was observed; render path uses this to suppress
    /// the recap section entirely on no-op cycles.
    pub fn is_empty(&self) -> bool {
        self.biggest.is_none() && self.oldest.is_none() && self.albums.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn asset(name: &str, bytes: u64, year: i32) -> RecapAsset {
        RecapAsset {
            filename: name.to_string(),
            bytes,
            created_local: Local.with_ymd_and_hms(year, 6, 1, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn observe_tracks_biggest() {
        let mut r = RunRecap::default();
        r.observe("Family", asset("a.jpg", 100, 2020));
        r.observe("Family", asset("b.mov", 500, 2020));
        r.observe("Family", asset("c.jpg", 250, 2020));
        assert_eq!(r.biggest.as_ref().unwrap().filename, "b.mov");
        assert_eq!(r.biggest.as_ref().unwrap().bytes, 500);
    }

    #[test]
    fn observe_tracks_oldest_by_created_date() {
        let mut r = RunRecap::default();
        r.observe("Family", asset("recent.jpg", 100, 2024));
        r.observe("Family", asset("ancient.jpg", 100, 1998));
        r.observe("Family", asset("middle.jpg", 100, 2010));
        assert_eq!(r.oldest.as_ref().unwrap().filename, "ancient.jpg");
    }

    #[test]
    fn observe_tracks_per_album_newest_asset() {
        let mut r = RunRecap::default();
        r.observe("Family", asset("a.jpg", 100, 2010));
        r.observe("Family", asset("b.jpg", 100, 2024));
        r.observe("Travel", asset("c.jpg", 100, 2020));
        let family = &r.albums["Family"];
        assert_eq!(family.filename, "b.jpg", "Family must keep the newer asset");
        let travel = &r.albums["Travel"];
        assert_eq!(travel.filename, "c.jpg");
    }

    #[test]
    fn top_album_picks_most_recent_capture() {
        let mut r = RunRecap::default();
        r.observe("Family", asset("a.jpg", 100, 2010));
        r.observe("Travel", asset("b.jpg", 100, 2024));
        r.observe("Pets", asset("c.jpg", 100, 2018));
        let (label, asset) = r.top_album().unwrap();
        assert_eq!(label, "Travel");
        assert_eq!(asset.filename, "b.jpg");
    }

    #[test]
    fn top_album_none_when_empty() {
        let r = RunRecap::default();
        assert!(r.top_album().is_none());
        assert!(r.is_empty());
    }

    #[test]
    fn merge_keeps_global_extremes() {
        let mut a = RunRecap::default();
        a.observe("Family", asset("a.jpg", 200, 2010));
        let mut b = RunRecap::default();
        b.observe("Travel", asset("b.jpg", 500, 2024));
        b.observe("Travel", asset("c.jpg", 100, 1998));
        a.merge(b);
        assert_eq!(a.biggest.as_ref().unwrap().filename, "b.jpg");
        assert_eq!(a.oldest.as_ref().unwrap().filename, "c.jpg");
        assert_eq!(a.albums.len(), 2);
    }

    #[test]
    fn merge_per_album_keeps_newer_asset() {
        let mut a = RunRecap::default();
        a.observe("Family", asset("old.jpg", 100, 2010));
        let mut b = RunRecap::default();
        b.observe("Family", asset("new.jpg", 100, 2024));
        a.merge(b);
        assert_eq!(a.albums["Family"].filename, "new.jpg");
    }
}
