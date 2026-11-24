//! Media organisation: bins, ratings, metadata, keyword collections and
//! smart collections (the parts of a project that sit next to the timelines).
//!
//! * A [`Bin`] is a folder. Bins nest through `parent`; an asset sits in at
//!   most one bin ([`MediaAsset::bin`](crate::MediaAsset)), `None` = the root.
//! * Keywords are the asset's `tags`. Every distinct keyword is a *keyword
//!   collection* (FCP) — a virtual folder, nothing is stored for it.
//! * A [`Rating`] is favourite / rejected / none (FCP's F / Delete / U).
//! * A [`SmartCollection`] is a saved search: rules that are matched against
//!   each asset whenever the browser is drawn.

use crate::asset::{AssetKind, MediaAsset};
use crate::ids::AssetId;
use crate::time::Time;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};

/// Favourite / rejected / unrated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Rating {
    #[default]
    None,
    Favorite,
    Rejected,
}

impl Rating {
    pub const ALL: [Rating; 3] = [Rating::None, Rating::Favorite, Rating::Rejected];

    pub fn label(self) -> &'static str {
        match self {
            Rating::None => "Unrated",
            Rating::Favorite => "Favorite",
            Rating::Rejected => "Rejected",
        }
    }
}

/// Logging metadata typed in by the editor (Resolve's metadata panel /
/// FCP's info inspector). Empty strings are "not set".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetMetadata {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scene: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub take: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reel: String,
    /// Camera name / angle label (multicam groups angles by it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub camera: String,
}

impl AssetMetadata {
    pub fn is_empty(&self) -> bool {
        *self == AssetMetadata::default()
    }

    /// All fields with their labels, in display order.
    pub fn fields_mut(&mut self) -> [(&'static str, &mut String); 6] {
        [
            ("Description", &mut self.description),
            ("Notes", &mut self.notes),
            ("Scene", &mut self.scene),
            ("Take", &mut self.take),
            ("Reel", &mut self.reel),
            ("Camera", &mut self.camera),
        ]
    }

    pub fn fields(&self) -> [(&'static str, &str); 6] {
        [("Description", &self.description), ("Notes", &self.notes), ("Scene", &self.scene), ("Take", &self.take), ("Reel", &self.reel), ("Camera", &self.camera)]
    }

    /// Case-insensitive search over every field (`q` already lower-case).
    pub fn contains_lower(&self, q: &str) -> bool {
        self.fields().iter().any(|(_, v)| v.to_lowercase().contains(q))
    }
}

/// A folder in the media browser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bin {
    pub id: u64,
    pub name: String,
    /// Containing bin (`None` = top level).
    #[serde(default)]
    pub parent: Option<u64>,
}

/// What a smart-collection rule compares the media kind with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KindFilter {
    /// Has a picture (video or still).
    Video,
    /// Has sound and no picture.
    AudioOnly,
    Still,
}

impl KindFilter {
    pub const ALL: [KindFilter; 3] = [KindFilter::Video, KindFilter::AudioOnly, KindFilter::Still];

    pub fn label(self) -> &'static str {
        match self {
            KindFilter::Video => "Video",
            KindFilter::AudioOnly => "Audio only",
            KindFilter::Still => "Still image",
        }
    }

    pub fn matches(self, a: &MediaAsset) -> bool {
        match self {
            KindFilter::Video => a.has_video() && a.kind != AssetKind::Image,
            KindFilter::AudioOnly => !a.has_video() && a.audio.is_some(),
            KindFilter::Still => a.kind == AssetKind::Image,
        }
    }
}

/// One condition of a smart collection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SmartRule {
    /// Name, keywords, codec or metadata contain the text (case-insensitive).
    Text(String),
    /// Has (or, with `has: false`, lacks) the keyword.
    Keyword {
        keyword: String,
        has: bool,
    },
    Kind(KindFilter),
    Rating(Rating),
    /// At least this long.
    MinDuration(Time),
    /// At most this long.
    MaxDuration(Time),
    /// Picture at least this many lines tall (1080, 2160, ...).
    MinHeight(u32),
    /// Frame rate at least this (e.g. 50 to find high-frame-rate footage).
    MinFrameRate(f64),
    /// Video codec name contains this (e.g. "hevc", "prores").
    Codec(String),
    /// Used (or, with `false`, unused) in any timeline of the project.
    Used(bool),
    /// Inside this bin or any bin below it.
    InBin(u64),
}

impl SmartRule {
    /// Short human description, e.g. "Keyword is #kill".
    pub fn describe(&self) -> String {
        match self {
            SmartRule::Text(t) => format!("Text contains \u{201c}{t}\u{201d}"),
            SmartRule::Keyword { keyword, has: true } => format!("Has keyword #{keyword}"),
            SmartRule::Keyword { keyword, has: false } => format!("Lacks keyword #{keyword}"),
            SmartRule::Kind(k) => format!("Kind is {}", k.label()),
            SmartRule::Rating(r) => format!("Rating is {}", r.label()),
            SmartRule::MinDuration(t) => format!("At least {:.1} s", t.as_secs_f64()),
            SmartRule::MaxDuration(t) => format!("At most {:.1} s", t.as_secs_f64()),
            SmartRule::MinHeight(h) => format!("At least {h} lines"),
            SmartRule::MinFrameRate(f) => format!("At least {f} fps"),
            SmartRule::Codec(c) => format!("Codec contains \u{201c}{c}\u{201d}"),
            SmartRule::Used(true) => "Used in a timeline".into(),
            SmartRule::Used(false) => "Not used in any timeline".into(),
            SmartRule::InBin(_) => "In bin".into(),
        }
    }
}

/// A saved search over the project's media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmartCollection {
    pub id: u64,
    pub name: String,
    /// `true`: every rule must match; `false`: any rule.
    #[serde(default = "yes")]
    pub match_all: bool,
    #[serde(default)]
    pub rules: Vec<SmartRule>,
}

fn yes() -> bool {
    true
}

/// What rule evaluation needs to know beyond the asset itself.
pub struct MatchContext<'a> {
    /// Assets placed in any timeline of the project.
    pub used: &'a HashSet<AssetId>,
    pub bins: &'a [Bin],
}

impl SmartCollection {
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        SmartCollection { id, name: name.into(), match_all: true, rules: Vec::new() }
    }

    /// An empty collection matches everything.
    pub fn matches(&self, a: &MediaAsset, cx: &MatchContext) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        let mut hits = self.rules.iter().map(|r| rule_matches(r, a, cx));
        if self.match_all {
            hits.all(|h| h)
        } else {
            hits.any(|h| h)
        }
    }
}

pub fn rule_matches(rule: &SmartRule, a: &MediaAsset, cx: &MatchContext) -> bool {
    match rule {
        SmartRule::Text(t) => a.matches(t),
        SmartRule::Keyword { keyword, has } => a.tags.iter().any(|k| k.eq_ignore_ascii_case(keyword)) == *has,
        SmartRule::Kind(k) => k.matches(a),
        SmartRule::Rating(r) => a.rating() == *r,
        SmartRule::MinDuration(t) => a.span() >= *t,
        SmartRule::MaxDuration(t) => a.span() <= *t,
        SmartRule::MinHeight(h) => a.video.as_ref().is_some_and(|v| v.height >= *h),
        SmartRule::MinFrameRate(f) => a.video.as_ref().is_some_and(|v| v.frame_rate.fps() + 1e-3 >= *f),
        SmartRule::Codec(c) => {
            let c = c.trim().to_lowercase();
            a.video.as_ref().is_some_and(|v| v.codec.to_lowercase().contains(&c)) || a.audio.as_ref().is_some_and(|au| au.codec.to_lowercase().contains(&c))
        }
        SmartRule::Used(u) => cx.used.contains(&a.id) == *u,
        SmartRule::InBin(b) => a.bin.is_some_and(|ab| bin_is_within(cx.bins, ab, *b)),
    }
}

/// `bin` is `ancestor` or nested somewhere below it.
pub fn bin_is_within(bins: &[Bin], bin: u64, ancestor: u64) -> bool {
    let mut cur = Some(bin);
    let mut guard = 0;
    while let Some(b) = cur {
        if b == ancestor {
            return true;
        }
        guard += 1;
        if guard > bins.len() + 1 {
            return false; // a cycle in a hand-edited file
        }
        cur = bins.iter().find(|x| x.id == b).and_then(|x| x.parent);
    }
    false
}

/// Bins, smart collections and the id counter for both.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Library {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<Bin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub smart: Vec<SmartCollection>,
    #[serde(default)]
    pub next_id: u64,
}

impl Library {
    pub fn is_empty(&self) -> bool {
        self.bins.is_empty() && self.smart.is_empty()
    }

    fn fresh_id(&mut self) -> u64 {
        let used = self.bins.iter().map(|b| b.id).chain(self.smart.iter().map(|s| s.id)).max().unwrap_or(0);
        self.next_id = self.next_id.max(used) + 1;
        self.next_id
    }

    pub fn bin(&self, id: u64) -> Option<&Bin> {
        self.bins.iter().find(|b| b.id == id)
    }

    /// New bin under `parent` (which must exist, else it goes to the top).
    pub fn add_bin(&mut self, name: impl Into<String>, parent: Option<u64>) -> u64 {
        let parent = parent.filter(|p| self.bin(*p).is_some());
        let id = self.fresh_id();
        self.bins.push(Bin { id, name: name.into(), parent });
        id
    }

    pub fn rename_bin(&mut self, id: u64, name: impl Into<String>) -> bool {
        match self.bins.iter_mut().find(|b| b.id == id) {
            Some(b) => {
                b.name = name.into();
                true
            }
            None => false,
        }
    }

    /// Move `id` under `parent`. Refuses to move a bin into itself or below itself.
    pub fn move_bin(&mut self, id: u64, parent: Option<u64>) -> bool {
        if self.bin(id).is_none() || parent.is_some_and(|p| self.bin(p).is_none() || bin_is_within(&self.bins, p, id)) {
            return false;
        }
        if let Some(b) = self.bins.iter_mut().find(|b| b.id == id) {
            b.parent = parent;
        }
        true
    }

    /// Delete a bin. Its child bins and its assets move up to its parent.
    pub fn remove_bin(&mut self, id: u64, assets: &mut [MediaAsset]) -> bool {
        let Some(parent) = self.bin(id).map(|b| b.parent) else { return false };
        self.bins.retain(|b| b.id != id);
        for b in &mut self.bins {
            if b.parent == Some(id) {
                b.parent = parent;
            }
        }
        for a in assets.iter_mut() {
            if a.bin == Some(id) {
                a.bin = parent;
            }
        }
        for s in &mut self.smart {
            s.rules.retain(|r| *r != SmartRule::InBin(id));
        }
        true
    }

    /// Child bins of `parent`, sorted by name.
    pub fn children(&self, parent: Option<u64>) -> Vec<&Bin> {
        let mut v: Vec<&Bin> = self.bins.iter().filter(|b| b.parent == parent).collect();
        v.sort_by_key(|b| b.name.to_lowercase());
        v
    }

    /// "Parent / Child" path of a bin.
    pub fn bin_path(&self, id: u64) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(id);
        while let Some(b) = cur.and_then(|c| self.bin(c)) {
            parts.push(b.name.clone());
            if parts.len() > self.bins.len() {
                break;
            }
            cur = b.parent;
        }
        parts.reverse();
        parts.join(" / ")
    }

    pub fn add_smart(&mut self, name: impl Into<String>) -> u64 {
        let id = self.fresh_id();
        self.smart.push(SmartCollection::new(id, name));
        id
    }

    pub fn smart(&self, id: u64) -> Option<&SmartCollection> {
        self.smart.iter().find(|s| s.id == id)
    }

    pub fn smart_mut(&mut self, id: u64) -> Option<&mut SmartCollection> {
        self.smart.iter_mut().find(|s| s.id == id)
    }

    pub fn remove_smart(&mut self, id: u64) -> bool {
        let n = self.smart.len();
        self.smart.retain(|s| s.id != id);
        self.smart.len() != n
    }

    /// Ready-made smart collections like FCP's defaults (added on request).
    pub fn add_default_smart_collections(&mut self) {
        let presets: [(&str, Vec<SmartRule>); 4] = [
            ("All Video", vec![SmartRule::Kind(KindFilter::Video)]),
            ("Audio Only", vec![SmartRule::Kind(KindFilter::AudioOnly)]),
            ("Favorites", vec![SmartRule::Rating(Rating::Favorite)]),
            ("Unused", vec![SmartRule::Used(false)]),
        ];
        for (name, rules) in presets {
            if self.smart.iter().any(|s| s.name == name) {
                continue;
            }
            let id = self.fresh_id();
            self.smart.push(SmartCollection { id, name: name.into(), match_all: true, rules });
        }
    }
}

/// Every distinct keyword in the project (keyword collections), sorted.
pub fn keywords(assets: &[MediaAsset]) -> BTreeSet<String> {
    assets.iter().flat_map(|a| a.tags.iter().cloned()).collect()
}

/// Which slice of the media the browser shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Collection {
    #[default]
    All,
    /// A bin and everything nested in it.
    Bin(u64),
    /// Assets outside every bin.
    Unfiled,
    Keyword(String),
    Smart(u64),
    Favorites,
    Rejected,
}

impl Collection {
    pub fn matches(&self, a: &MediaAsset, lib: &Library, cx: &MatchContext) -> bool {
        match self {
            Collection::All => true,
            Collection::Bin(b) => a.bin.is_some_and(|ab| bin_is_within(&lib.bins, ab, *b)),
            Collection::Unfiled => a.bin.is_none(),
            Collection::Keyword(k) => a.tags.iter().any(|t| t == k),
            Collection::Smart(id) => lib.smart(*id).is_some_and(|s| s.matches(a, cx)),
            Collection::Favorites => a.rating() == Rating::Favorite,
            Collection::Rejected => a.rating() == Rating::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AudioStreamInfo, VideoStreamInfo};
    use crate::time::FrameRate;

    fn video(name: &str, secs: i64, h: u32, fps: FrameRate) -> MediaAsset {
        let mut a = MediaAsset::new(name, name, Time::from_secs(secs));
        a.video = Some(VideoStreamInfo {
            width: h * 16 / 9,
            height: h,
            frame_rate: fps,
            codec: "hevc".into(),
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: String::new(),
            color_transfer: String::new(),
            color_primaries: String::new(),
            is_vfr: false,
            pts_table: Vec::new(),
        });
        a
    }

    fn audio(name: &str, secs: i64) -> MediaAsset {
        let mut a = MediaAsset::new(name, name, Time::from_secs(secs));
        a.kind = AssetKind::Audio;
        a.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "aac".into() });
        a
    }

    #[test]
    fn bins_nest_and_delete_moves_contents_up() {
        let mut lib = Library::default();
        let a = lib.add_bin("Clips", None);
        let b = lib.add_bin("Kills", Some(a));
        let c = lib.add_bin("AWP", Some(b));
        assert_eq!(lib.bin_path(c), "Clips / Kills / AWP");
        assert!(bin_is_within(&lib.bins, c, a));
        assert!(!bin_is_within(&lib.bins, a, c));
        // no cycles
        assert!(!lib.move_bin(a, Some(c)));
        assert!(!lib.move_bin(a, Some(a)));
        assert!(lib.move_bin(c, None));
        assert!(lib.move_bin(c, Some(b)));

        let mut assets = vec![video("x", 3, 1080, FrameRate::FPS_60)];
        assets[0].bin = Some(b);
        lib.smart.push(SmartCollection { id: 99, name: "s".into(), match_all: true, rules: vec![SmartRule::InBin(b)] });
        assert!(lib.remove_bin(b, &mut assets));
        assert_eq!(assets[0].bin, Some(a));
        assert_eq!(lib.bin(c).unwrap().parent, Some(a));
        assert!(lib.smart(99).unwrap().rules.is_empty());
        assert!(!lib.remove_bin(b, &mut assets));
    }

    #[test]
    fn ids_never_repeat() {
        let mut lib = Library::default();
        let a = lib.add_bin("a", None);
        let s = lib.add_smart("s");
        lib.remove_bin(a, &mut []);
        let b = lib.add_bin("b", None);
        assert!(a != s && b != s && b != a);
        // a file that only stored the bins still gets fresh ids
        let mut loaded = Library { bins: vec![Bin { id: 40, name: "x".into(), parent: None }], ..Default::default() };
        assert_eq!(loaded.add_bin("y", None), 41);
    }

    #[test]
    fn smart_rules() {
        let mut lib = Library::default();
        let bin = lib.add_bin("B", None);
        let mut clip = video("kill_awp.mp4", 12, 2160, FrameRate::FPS_60);
        clip.tags = vec!["Kill".into()];
        clip.set_rating(Rating::Favorite);
        clip.bin = Some(bin);
        clip.meta.scene = "Mirage A site".into();
        let song = audio("song.mp3", 180);
        let used: HashSet<AssetId> = [clip.id].into_iter().collect();
        let cx = MatchContext { used: &used, bins: &lib.bins };

        let r = |rule: SmartRule, a: &MediaAsset| rule_matches(&rule, a, &cx);
        assert!(r(SmartRule::Text("mirage".into()), &clip), "metadata is searched");
        assert!(r(SmartRule::Keyword { keyword: "kill".into(), has: true }, &clip));
        assert!(r(SmartRule::Keyword { keyword: "kill".into(), has: false }, &song));
        assert!(r(SmartRule::Kind(KindFilter::Video), &clip) && !r(SmartRule::Kind(KindFilter::Video), &song));
        assert!(r(SmartRule::Kind(KindFilter::AudioOnly), &song));
        assert!(r(SmartRule::Rating(Rating::Favorite), &clip) && r(SmartRule::Rating(Rating::None), &song));
        assert!(r(SmartRule::MinDuration(Time::from_secs(60)), &song) && !r(SmartRule::MinDuration(Time::from_secs(60)), &clip));
        assert!(r(SmartRule::MaxDuration(Time::from_secs(60)), &clip));
        assert!(r(SmartRule::MinHeight(2160), &clip) && !r(SmartRule::MinHeight(2160), &song));
        assert!(r(SmartRule::MinFrameRate(59.94), &clip));
        assert!(r(SmartRule::Codec("HEVC".into()), &clip) && r(SmartRule::Codec("aac".into()), &song));
        assert!(r(SmartRule::Used(true), &clip) && r(SmartRule::Used(false), &song));
        assert!(r(SmartRule::InBin(bin), &clip) && !r(SmartRule::InBin(bin), &song));

        let mut s = SmartCollection::new(1, "4K kills");
        assert!(s.matches(&song, &cx), "no rules = everything");
        s.rules = vec![SmartRule::MinHeight(2160), SmartRule::Kind(KindFilter::AudioOnly)];
        assert!(!s.matches(&clip, &cx));
        s.match_all = false;
        assert!(s.matches(&clip, &cx) && s.matches(&song, &cx));
    }

    #[test]
    fn collections_and_ratings() {
        let mut lib = Library::default();
        let bin = lib.add_bin("B", None);
        let sub = lib.add_bin("Sub", Some(bin));
        let mut a = video("a", 1, 1080, FrameRate::FPS_30);
        a.bin = Some(sub);
        a.tags = vec!["intro".into()];
        let mut b = audio("b", 1);
        b.set_rating(Rating::Rejected);
        let used = HashSet::new();
        let cx = MatchContext { used: &used, bins: &lib.bins };
        assert!(Collection::Bin(bin).matches(&a, &lib, &cx), "a parent bin shows nested media");
        assert!(!Collection::Unfiled.matches(&a, &lib, &cx) && Collection::Unfiled.matches(&b, &lib, &cx));
        assert!(Collection::Keyword("intro".into()).matches(&a, &lib, &cx));
        assert!(Collection::Rejected.matches(&b, &lib, &cx) && !Collection::Favorites.matches(&b, &lib, &cx));
        assert_eq!(keywords(&[a.clone(), b.clone()]).into_iter().collect::<Vec<_>>(), vec!["intro".to_string()]);

        // the legacy `favorite` flag and the rating stay in step
        b.set_rating(Rating::Favorite);
        assert!(b.favorite && !b.rejected);
        b.set_rating(Rating::None);
        assert!(!b.favorite && !b.rejected && b.rating() == Rating::None);

        lib.add_default_smart_collections();
        lib.add_default_smart_collections();
        assert_eq!(lib.smart.len(), 4, "defaults are added once");
    }
}
