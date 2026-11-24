//! AAF export: the timeline as an Advanced Authoring Format composition for
//! Avid Media Composer, Pro Tools and other AAF readers.
//!
//! The file starts from `assets/aaf_template.aaf`, an empty AAF made once
//! with pyaaf2 (`scripts/make_aaf_template.py`). It holds the meta-dictionary
//! (class and type definitions), the dictionary's data definitions and the
//! weak-reference table. This module adds the mobs:
//!
//! * a composition mob with one picture slot per video track and one mono
//!   sound slot per audio channel of each audio track,
//! * per media file a master mob and a file source mob that points at the
//!   file through a network locator (`file:///…`), so the reader links the
//!   original media.
//!
//! Cuts only: dissolves, speed changes, effects, titles and generators are
//! not carried (titles / generators become filler); [`AafReport`] counts
//! what was left out. Edit units are timeline frames on every slot.

use crate::asset::MediaAsset;
use crate::clip::ClipKind;
use crate::ids::AssetId;
use crate::interchange::{self, LaneTrack};
use crate::project::Project;
use crate::time::{FrameRate, Time};
use cfb::CompoundFile;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use uuid::Uuid;

const TEMPLATE: &[u8] = include_bytes!("../assets/aaf_template.aaf");
const CONTENT: &str = "/Header-2/Content-3b03";

const CLS_COMPOSITION_MOB: &str = "0d010101-0101-3500-060e-2b3402060101";
const CLS_MASTER_MOB: &str = "0d010101-0101-3600-060e-2b3402060101";
const CLS_SOURCE_MOB: &str = "0d010101-0101-3700-060e-2b3402060101";
const CLS_TIMELINE_SLOT: &str = "0d010101-0101-3b00-060e-2b3402060101";
const CLS_SEQUENCE: &str = "0d010101-0101-0f00-060e-2b3402060101";
const CLS_SOURCE_CLIP: &str = "0d010101-0101-1100-060e-2b3402060101";
const CLS_FILLER: &str = "0d010101-0101-0900-060e-2b3402060101";
const CLS_IMPORT_DESCRIPTOR: &str = "0d010101-0101-4a00-060e-2b3402060101";
const CLS_NETWORK_LOCATOR: &str = "0d010101-0101-3200-060e-2b3402060101";

/// Data definitions as stored (AUID, little-endian fields).
const DD_PICTURE: [u8; 16] = [0x02, 0x02, 0x03, 0x01, 0x00, 0x01, 0x00, 0x00, 0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01];
const DD_SOUND: [u8; 16] = [0x02, 0x02, 0x03, 0x01, 0x00, 0x02, 0x00, 0x00, 0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01];
/// Index of Header/Dictionary/DataDefinitions in the template's
/// "referenced properties" table, and the DefinitionObject key pid.
const DATADEF_TABLE_INDEX: u16 = 2;
const DATADEF_KEY_PID: u16 = 0x1b01;

const SF_DATA: u16 = 0x82;
const SF_STRONG: u16 = 0x22;
const SF_STRONG_VECTOR: u16 = 0x32;
const SF_STRONG_SET: u16 = 0x3a;
const SF_WEAK: u16 = 0x02;

#[derive(Debug, thiserror::Error)]
pub enum AafError {
    #[error("nothing to export: the timeline has no media clips")]
    Empty,
    #[error("AAF write failed: {0}")]
    Io(#[from] std::io::Error),
}

/// What the export carried and what it left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AafReport {
    pub clips: usize,
    pub video_tracks: usize,
    pub audio_tracks: usize,
    /// Titles, generators, compounds: written as filler.
    pub not_media: usize,
    pub dissolves_dropped: usize,
    pub speed_changes_dropped: usize,
}

impl AafReport {
    /// "12 clips on 2 video / 3 audio tracks; left out: …"
    pub fn summary(&self) -> String {
        let mut left = Vec::new();
        if self.not_media > 0 {
            left.push(format!("{} title / generator clip(s) as gaps", self.not_media));
        }
        if self.dissolves_dropped > 0 {
            left.push(format!("{} dissolve(s) as cuts", self.dissolves_dropped));
        }
        if self.speed_changes_dropped > 0 {
            left.push(format!("{} speed change(s)", self.speed_changes_dropped));
        }
        let mut s = format!("{} clip(s) on {} video / {} audio track(s)", self.clips, self.video_tracks, self.audio_tracks);
        if !left.is_empty() {
            s += &format!("; not carried: {}", left.join(", "));
        }
        s
    }
}

// ----- object model written to the file

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Picture,
    Sound,
}

impl Kind {
    fn datadef(self) -> [u8; 16] {
        match self {
            Kind::Picture => DD_PICTURE,
            Kind::Sound => DD_SOUND,
        }
    }
}

type MobId = [u8; 32];
const NO_MOB: MobId = [0; 32];

enum Component {
    Filler {
        len: i64,
    },
    /// `mob` = NO_MOB ends the source chain (the file itself).
    Source {
        len: i64,
        start: i64,
        mob: MobId,
        slot: u32,
    },
}

impl Component {
    fn len(&self) -> i64 {
        match self {
            Component::Filler { len } | Component::Source { len, .. } => *len,
        }
    }
}

enum Segment {
    Sequence(Vec<Component>),
    Clip(Component),
}

struct Slot {
    id: u32,
    kind: Kind,
    segment: Segment,
}

struct Mob {
    class: &'static str,
    id: MobId,
    name: String,
    slots: Vec<Slot>,
    /// File source mobs: where the media is.
    url: Option<String>,
}

// ----- property encoding

struct Props(Vec<(u16, u16, Vec<u8>)>);

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

impl Props {
    fn new() -> Self {
        Props(Vec::new())
    }
    fn data(mut self, pid: u16, v: impl Into<Vec<u8>>) -> Self {
        self.0.push((pid, SF_DATA, v.into()));
        self
    }
    fn reference(mut self, pid: u16, form: u16, name: &str) -> Self {
        self.0.push((pid, form, utf16z(name)));
        self
    }
    fn datadef(mut self, kind: Kind) -> Self {
        let mut v = Vec::with_capacity(21);
        v.extend(DATADEF_TABLE_INDEX.to_le_bytes());
        v.extend(DATADEF_KEY_PID.to_le_bytes());
        v.push(16);
        v.extend(kind.datadef());
        self.0.push((0x0201, SF_WEAK, v));
        self
    }
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![0x4c, 0x20];
        out.extend((self.0.len() as u16).to_le_bytes());
        for (pid, form, v) in &self.0 {
            out.extend(pid.to_le_bytes());
            out.extend(form.to_le_bytes());
            out.extend((v.len() as u16).to_le_bytes());
        }
        for (_, _, v) in &self.0 {
            out.extend(v);
        }
        out
    }
}

/// AAF TimeStamp: year (i16), month, day, hour, minute, second, 1/100 s.
fn timestamp(unix: u64) -> Vec<u8> {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // days since 1970-01-01 → civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i16;
    let mut v = year.to_le_bytes().to_vec();
    v.extend([month, day, (secs / 3600) as u8, (secs / 60 % 60) as u8, (secs % 60) as u8, 0]);
    v
}

/// A SMPTE UMID mob id with a material number from `seed`.
fn mob_id(seed: u64) -> MobId {
    fn mix(mut z: u64) -> u64 {
        z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    let mut id = [0u8; 32];
    id[..16].copy_from_slice(&[0x06, 0x0a, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x05, 0x01, 0x01, 0x0f, 0x20, 0x13, 0x00, 0x00, 0x00]);
    id[16..24].copy_from_slice(&mix(seed).to_le_bytes());
    id[24..].copy_from_slice(&mix(seed ^ 0x5bd1_e995).to_le_bytes());
    id
}

struct Writer {
    cf: CompoundFile<Cursor<Vec<u8>>>,
    stamp: Vec<u8>,
    rate: (i32, i32),
}

impl Writer {
    fn storage(&mut self, path: &str, class: &str) -> std::io::Result<()> {
        self.cf.create_storage(path)?;
        let clsid = Uuid::parse_str(class).map_err(|e| std::io::Error::other(e.to_string()))?;
        self.cf.set_storage_clsid(path, clsid)
    }

    fn stream(&mut self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.cf.create_stream(path)?.write_all(bytes)
    }

    fn props(&mut self, dir: &str, p: Props) -> std::io::Result<()> {
        self.stream(&format!("{dir}/properties"), &p.encode())
    }

    /// Strong-reference vector index: count, next free key, last free key, keys.
    fn vector_index(&mut self, dir: &str, name: &str, count: usize) -> std::io::Result<()> {
        let mut v = Vec::new();
        v.extend((count as u32).to_le_bytes());
        v.extend((count as u32).to_le_bytes());
        v.extend(u32::MAX.to_le_bytes());
        for k in 0..count as u32 {
            v.extend(k.to_le_bytes());
        }
        self.stream(&format!("{dir}/{name} index"), &v)
    }

    fn component(&mut self, path: &str, kind: Kind, c: &Component) -> std::io::Result<()> {
        match c {
            Component::Filler { len } => {
                self.storage(path, CLS_FILLER)?;
                self.props(path, Props::new().datadef(kind).data(0x0202, len.to_le_bytes()))
            }
            Component::Source { len, start, mob, slot } => {
                self.storage(path, CLS_SOURCE_CLIP)?;
                let p = Props::new().datadef(kind).data(0x0202, len.to_le_bytes()).data(0x1201, start.to_le_bytes()).data(0x1101, mob.to_vec()).data(0x1102, slot.to_le_bytes());
                self.props(path, p)
            }
        }
    }

    fn mob(&mut self, key: usize, m: &Mob) -> std::io::Result<()> {
        let dir = format!("{CONTENT}/Mobs-1901{{{key:x}}}");
        self.storage(&dir, m.class)?;
        let mut p = Props::new().data(0x4402, utf16z(&m.name)).data(0x4401, m.id.to_vec()).data(0x4405, self.stamp.clone()).data(0x4404, self.stamp.clone()).reference(
            0x4403,
            SF_STRONG_VECTOR,
            "Slots-4403",
        );
        if m.url.is_some() {
            p = p.reference(0x4701, SF_STRONG, "EssenceDescription-4701");
        }
        self.props(&dir, p)?;
        if let Some(url) = &m.url {
            let desc = format!("{dir}/EssenceDescription-4701");
            self.storage(&desc, CLS_IMPORT_DESCRIPTOR)?;
            self.props(&desc, Props::new().reference(0x2f01, SF_STRONG_VECTOR, "Locator-2f01"))?;
            self.vector_index(&desc, "Locator-2f01", 1)?;
            let loc = format!("{desc}/Locator-2f01{{0}}");
            self.storage(&loc, CLS_NETWORK_LOCATOR)?;
            self.props(&loc, Props::new().data(0x4001, utf16z(url)))?;
        }
        self.vector_index(&dir, "Slots-4403", m.slots.len())?;
        for (i, s) in m.slots.iter().enumerate() {
            let sdir = format!("{dir}/Slots-4403{{{i:x}}}");
            self.storage(&sdir, CLS_TIMELINE_SLOT)?;
            let mut rate = self.rate.0.to_le_bytes().to_vec();
            rate.extend(self.rate.1.to_le_bytes());
            let p = Props::new().data(0x4801, s.id.to_le_bytes()).data(0x4802, utf16z("")).data(0x4b02, 0i64.to_le_bytes()).data(0x4b01, rate).reference(
                0x4803,
                SF_STRONG,
                "Segment-4803",
            );
            self.props(&sdir, p)?;
            let seg = format!("{sdir}/Segment-4803");
            match &s.segment {
                Segment::Clip(c) => self.component(&seg, s.kind, c)?,
                Segment::Sequence(items) => {
                    self.storage(&seg, CLS_SEQUENCE)?;
                    let len: i64 = items.iter().map(Component::len).sum();
                    self.props(&seg, Props::new().datadef(s.kind).data(0x0202, len.to_le_bytes()).reference(0x1001, SF_STRONG_VECTOR, "Components-1001"))?;
                    self.vector_index(&seg, "Components-1001", items.len())?;
                    for (j, c) in items.iter().enumerate() {
                        self.component(&format!("{seg}/Components-1001{{{j:x}}}"), s.kind, c)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(mut self, mobs: &[Mob]) -> std::io::Result<Vec<u8>> {
        for (k, m) in mobs.iter().enumerate() {
            self.mob(k, m)?;
        }
        // the content storage's mob set: local key, ref count, mob id
        let mut idx = Vec::new();
        idx.extend((mobs.len() as u32).to_le_bytes());
        idx.extend((mobs.len() as u32).to_le_bytes());
        idx.extend(u32::MAX.to_le_bytes());
        idx.extend(0x4401u16.to_le_bytes());
        idx.push(32);
        for (k, m) in mobs.iter().enumerate() {
            idx.extend((k as u32).to_le_bytes());
            idx.extend(1u32.to_le_bytes());
            idx.extend(m.id);
        }
        self.stream(&format!("{CONTENT}/Mobs-1901 index"), &idx)?;
        self.props(CONTENT, Props::new().reference(0x1901, SF_STRONG_SET, "Mobs-1901"))?;
        self.cf.flush()?;
        Ok(self.cf.into_inner().into_inner())
    }
}

/// Media of one file: master mob and the slots it offers.
struct Media {
    master: MobId,
    picture: Option<u32>,
    /// Sound slot per channel (mono slots, at most two).
    sound: Vec<u32>,
}

fn frames(t: Time, rate: FrameRate) -> i64 {
    t.frames_round(rate)
}

/// Export the open timeline. `seed` makes the mob ids (use the clock).
pub fn export(project: &Project, seed: u64, unix_time: u64) -> Result<(Vec<u8>, AafReport), AafError> {
    let rate = project.timeline.format.rate;
    let lanes = interchange::lanes(project);
    let mut report = AafReport::default();
    let mut seeds = seed;
    let mut next_id = || {
        seeds = seeds.wrapping_add(1);
        mob_id(seeds)
    };

    // master + file mobs per used media file
    let mut media: BTreeMap<AssetId, Media> = BTreeMap::new();
    let mut mobs = Vec::new();
    let used = lanes.iter().flat_map(|t| &t.items).filter_map(|i| match i.clip.kind {
        ClipKind::Asset { asset } => Some(asset),
        _ => None,
    });
    for id in used {
        if media.contains_key(&id) {
            continue;
        }
        let Some(a) = project.asset(id) else { continue };
        let (m, file, master) = media_mobs(a, rate, &mut next_id);
        media.insert(id, m);
        mobs.push(file);
        mobs.push(master);
    }
    if media.is_empty() {
        return Err(AafError::Empty);
    }

    let mut slots = Vec::new();
    let video: Vec<&LaneTrack> = lanes.iter().filter(|t| t.items.iter().any(|i| i.clip.av.video)).collect();
    for t in &video {
        let items = sequence(t, Kind::Picture, 0, rate, &media, &mut report);
        slots.push(Slot { id: slots.len() as u32 + 1, kind: Kind::Picture, segment: Segment::Sequence(items) });
    }
    let mut audio: Vec<&LaneTrack> = lanes.iter().filter(|t| t.items.iter().any(|i| i.clip.has_audio())).collect();
    audio.sort_by_key(|t| (t.lane != 0, t.lane > 0, t.lane.abs()));
    for t in &audio {
        let channels = t.items.iter().filter_map(|i| media_of(i.clip, &media)).map(|m| m.sound.len()).max().unwrap_or(1).max(1);
        for ch in 0..channels {
            let items = sequence(t, Kind::Sound, ch, rate, &media, &mut report);
            slots.push(Slot { id: slots.len() as u32 + 1, kind: Kind::Sound, segment: Segment::Sequence(items) });
        }
    }
    report.video_tracks = video.len();
    report.audio_tracks = audio.len();
    report.clips = lanes.iter().flat_map(|t| &t.items).filter(|i| media_of(i.clip, &media).is_some()).count();
    for t in &lanes {
        report.dissolves_dropped += (0..t.items.len()).filter(|&i| interchange::dissolve_after(&t.items, i).is_some()).count();
        report.speed_changes_dropped += t.items.iter().filter(|i| (interchange::clip_speed(i.clip).0 - 1.0).abs() > 1e-6).count();
    }
    let name = if project.timeline.name.is_empty() { project.name.clone() } else { project.timeline.name.clone() };
    mobs.insert(0, Mob { class: CLS_COMPOSITION_MOB, id: next_id(), name, slots, url: None });

    let cf = CompoundFile::open(Cursor::new(TEMPLATE.to_vec()))?;
    let w = Writer { cf, stamp: timestamp(unix_time), rate: (rate.num as i32, rate.den as i32) };
    Ok((w.finish(&mobs)?, report))
}

fn media_of<'a>(clip: &crate::clip::Clip, media: &'a BTreeMap<AssetId, Media>) -> Option<&'a Media> {
    match clip.kind {
        ClipKind::Asset { asset } => media.get(&asset),
        _ => None,
    }
}

/// The master mob and file source mob of a media file.
fn media_mobs(a: &MediaAsset, rate: FrameRate, next_id: &mut impl FnMut() -> MobId) -> (Media, Mob, Mob) {
    let len = frames(a.duration, rate).max(1);
    let file_id = next_id();
    let master_id = next_id();
    let mut kinds = Vec::new();
    if a.has_video() {
        kinds.push(Kind::Picture);
    }
    if a.has_audio() {
        let ch = a.audio.as_ref().map_or(1, |x| x.channels.clamp(1, 2));
        kinds.extend(std::iter::repeat_n(Kind::Sound, ch as usize));
    }
    let mut m = Media { master: master_id, picture: None, sound: Vec::new() };
    let mut file_slots = Vec::new();
    let mut master_slots = Vec::new();
    for (i, k) in kinds.iter().enumerate() {
        let id = i as u32 + 1;
        match k {
            Kind::Picture => m.picture = Some(id),
            Kind::Sound => m.sound.push(id),
        }
        file_slots.push(Slot { id, kind: *k, segment: Segment::Clip(Component::Source { len, start: 0, mob: NO_MOB, slot: 0 }) });
        master_slots.push(Slot { id, kind: *k, segment: Segment::Clip(Component::Source { len, start: 0, mob: file_id, slot: id }) });
    }
    let url = crate::fcpxml::path_to_url(&a.path);
    let file = Mob { class: CLS_SOURCE_MOB, id: file_id, name: a.name.clone(), slots: file_slots, url: Some(url) };
    let master = Mob { class: CLS_MASTER_MOB, id: master_id, name: a.name.clone(), slots: master_slots, url: None };
    (m, file, master)
}

/// One track as filler and source clips. `channel` picks the sound slot.
fn sequence(t: &LaneTrack, kind: Kind, channel: usize, rate: FrameRate, media: &BTreeMap<AssetId, Media>, report: &mut AafReport) -> Vec<Component> {
    let mut out = Vec::new();
    let mut at = 0i64;
    let gap = |out: &mut Vec<Component>, len: i64| {
        if len <= 0 {
            return;
        }
        match out.last_mut() {
            Some(Component::Filler { len: l }) => *l += len,
            _ => out.push(Component::Filler { len }),
        }
    };
    for it in &t.items {
        let wanted = match kind {
            Kind::Picture => it.clip.av.video,
            Kind::Sound => it.clip.has_audio(),
        };
        if !wanted {
            continue;
        }
        let (s, e) = (frames(it.start, rate), frames(it.end(), rate));
        if e <= s || s < at {
            continue;
        }
        gap(&mut out, s - at);
        let slot = media_of(it.clip, media).and_then(|m| match kind {
            Kind::Picture => m.picture.map(|p| (m.master, p)),
            Kind::Sound => m.sound.get(channel).or(m.sound.last()).map(|&p| (m.master, p)),
        });
        match slot {
            Some((mob, slot)) => out.push(Component::Source { len: e - s, start: frames(it.clip.source_in, rate), mob, slot }),
            None => {
                if kind == Kind::Picture || channel == 0 {
                    report.not_media += usize::from(!matches!(it.clip.kind, ClipKind::Asset { .. }) && (kind == Kind::Picture || !it.clip.av.video));
                }
                gap(&mut out, e - s);
            }
        }
        at = e;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AudioStreamInfo, MediaAsset, VideoStreamInfo};
    use crate::clip::{Clip, TitleParams, Transition, TransitionKind};
    use std::io::Read;

    fn asset(name: &str) -> MediaAsset {
        let mut a = MediaAsset::new(name, format!("D:/Footage/Day 1/{name}"), Time::from_secs(30));
        a.video = Some(VideoStreamInfo {
            width: 1920,
            height: 1080,
            frame_rate: FrameRate::FPS_24,
            codec: "prores".into(),
            pix_fmt: "yuv422p10le".into(),
            bit_depth: 10,
            color_space: "bt709".into(),
            color_transfer: String::new(),
            color_primaries: String::new(),
            is_vfr: false,
            pts_table: Vec::new(),
        });
        a.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "pcm_s24le".into() });
        a
    }

    fn project() -> Project {
        let mut p = Project::new("Doc Cut");
        p.timeline.format = crate::timeline::VideoFormat { width: 1920, height: 1080, rate: FrameRate::FPS_24 };
        let a = asset("A 001.mov");
        let b = asset("B002.mov");
        let mut music = MediaAsset::new("score.wav", "D:/Audio/score.wav", Time::from_secs(60));
        music.kind = crate::asset::AssetKind::Audio;
        music.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 1, codec: "pcm_s16le".into() });
        let mut ca = Clip::from_asset_range(&a, Time::from_secs(3), Time::from_secs(5));
        ca.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, Time::from_frames(12, FrameRate::FPS_24)));
        let cb = Clip::from_asset_range(&b, Time::from_secs(1), Time::from_secs(4));
        p.timeline.spine.push(ca);
        p.timeline.spine.push(cb);
        p.timeline.connect(Clip::from_asset_range(&music, Time::ZERO, Time::from_secs(6)), Time::from_secs(2), -1).unwrap();
        p.timeline.connect(Clip::title(TitleParams::default(), Time::from_secs(2)), Time::from_secs(1), 1).unwrap();
        p.add_asset(a);
        p.add_asset(b);
        p.add_asset(music);
        p
    }

    fn read(cf: &mut CompoundFile<Cursor<Vec<u8>>>, path: &str) -> Vec<u8> {
        let mut v = Vec::new();
        cf.open_stream(path).unwrap().read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn writes_mobs_into_the_template() {
        let p = project();
        let (bytes, report) = export(&p, 42, 1_795_000_000).unwrap();
        // `scripts/validate_aaf.py` reads this with pyaaf2
        if let Some(dir) = std::env::var_os("RCP_AAF_DUMP") {
            std::fs::write(std::path::PathBuf::from(dir).join("rcp_export.aaf"), &bytes).unwrap();
        }
        assert_eq!(report.clips, 3);
        assert_eq!((report.video_tracks, report.audio_tracks), (2, 2));
        assert_eq!(report.not_media, 1, "{report:?}");
        assert_eq!(report.dissolves_dropped, 1);

        let mut cf = CompoundFile::open(Cursor::new(bytes)).unwrap();
        let idx = read(&mut cf, &format!("{CONTENT}/Mobs-1901 index"));
        // composition + (file + master) × 3 files
        assert_eq!(u32::from_le_bytes(idx[..4].try_into().unwrap()), 7);
        let comp = cf.entry(format!("{CONTENT}/Mobs-1901{{0}}")).unwrap();
        assert_eq!(comp.clsid().to_string(), CLS_COMPOSITION_MOB);
        // V1, V2 (title lane: filler only), A1 L/R (storyline), A2 (mono music)
        let slots = read(&mut cf, &format!("{CONTENT}/Mobs-1901{{0}}/Slots-4403 index"));
        assert_eq!(u32::from_le_bytes(slots[..4].try_into().unwrap()), 5);
        let props = read(&mut cf, &format!("{CONTENT}/Mobs-1901{{0}}/Slots-4403{{0}}/Segment-4803/properties"));
        // sequence length: 5 s + 4 s at 24 fps
        let n = u16::from_le_bytes([props[2], props[3]]) as usize;
        let mut off = 4 + 6 * n;
        let mut len = None;
        for e in 0..n {
            let pid = u16::from_le_bytes([props[4 + e * 6], props[5 + e * 6]]);
            let size = u16::from_le_bytes([props[8 + e * 6], props[9 + e * 6]]) as usize;
            if pid == 0x0202 {
                len = Some(i64::from_le_bytes(props[off..off + 8].try_into().unwrap()));
            }
            off += size;
        }
        assert_eq!(len, Some(216));
    }

    #[test]
    fn timestamps_are_civil_dates() {
        // 2026-11-24 22:39:27 UTC
        assert_eq!(timestamp(1_795_559_967), vec![0xea, 0x07, 11, 24, 22, 39, 27, 0]);
        assert_eq!(timestamp(0)[..4], [0xb2, 0x07, 1, 1]);
    }

    #[test]
    fn an_empty_timeline_is_refused() {
        assert!(matches!(export(&Project::new("x"), 1, 0), Err(AafError::Empty)));
    }

    #[test]
    fn report_summary_names_what_was_left_out() {
        let r = AafReport { clips: 3, video_tracks: 1, audio_tracks: 1, not_media: 1, dissolves_dropped: 2, speed_changes_dropped: 0 };
        let s = r.summary();
        assert!(s.contains("3 clip(s)") && s.contains("2 dissolve(s)") && s.contains("title"), "{s}");
    }
}
