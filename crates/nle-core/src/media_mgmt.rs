//! Media management: find offline media, relink it by searching a folder, and
//! consolidate (copy the media a project uses into one folder and repoint it).

use crate::asset::{AssetKind, MediaAsset};
use crate::clip::{Clip, ClipKind};
use crate::ids::AssetId;
use crate::project::Project;
use crate::time::Time;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Assets whose file isn't there (synthetic media never is, and isn't counted).
pub fn missing_assets(project: &Project) -> Vec<AssetId> {
    project.assets.iter().filter(|a| a.kind != AssetKind::Synthetic && !a.path.is_file()).map(|a| a.id).collect()
}

/// Assets referenced anywhere in any timeline (compound clips, storylines,
/// auditions and connected clips included).
pub fn used_assets(project: &Project) -> BTreeSet<AssetId> {
    fn walk(c: &Clip, out: &mut BTreeSet<AssetId>) {
        match &c.kind {
            ClipKind::Asset { asset } => {
                out.insert(*asset);
            }
            ClipKind::Compound(cc) => cc.spine.iter().for_each(|ch| walk(ch, out)),
            _ => {}
        }
        c.connected.iter().for_each(|ch| walk(ch, out));
        c.audition.iter().for_each(|ch| walk(ch, out));
    }
    let mut out = BTreeSet::new();
    project.all_timelines().flat_map(|t| t.spine.iter()).for_each(|c| walk(c, &mut out));
    out
}

/// Take the stream info from a fresh probe of an asset's file (after a relink
/// or an EDL / OTIO import), keeping the asset's id, name, path and tags.
/// Clips using it lose the video / audio they no longer have and learn the
/// media length.
pub fn refresh_asset(project: &mut Project, id: AssetId, probed: MediaAsset) {
    let Some(a) = project.asset_mut(id) else { return };
    // EDL imports keep absolute source timecode until the file's own start
    // timecode is known
    let tc_shift = if std::mem::take(&mut a.source_tc_pending) { probed.start_timecode.unwrap_or_default() } else { Time::ZERO };
    a.start_timecode = probed.start_timecode;
    a.kind = probed.kind;
    a.duration = probed.duration;
    a.video = probed.video;
    a.audio = probed.audio;
    let (has_v, has_a, dur) = (a.has_video(), a.has_audio(), a.duration);
    fn walk(c: &mut Clip, id: AssetId, f: &dyn Fn(&mut Clip)) {
        if matches!(c.kind, ClipKind::Asset { asset } if asset == id) {
            f(c);
        }
        if let ClipKind::Compound(cc) = &mut c.kind {
            cc.spine.iter_mut().for_each(|ch| walk(ch, id, f));
        }
        c.connected.iter_mut().for_each(|ch| walk(ch, id, f));
        c.audition.iter_mut().for_each(|ch| walk(ch, id, f));
    }
    let fix = |c: &mut Clip| {
        c.source_in = (c.source_in - tc_shift).max(Time::ZERO);
        c.av.video &= has_v;
        c.av.audio &= has_a;
        c.media_duration = Some(dur);
    };
    project.timeline.spine.iter_mut().chain(project.timelines.iter_mut().flat_map(|t| t.spine.iter_mut())).for_each(|c| walk(c, id, &fix));
}

#[derive(Debug, Default, PartialEq)]
pub struct RelinkReport {
    pub relinked: Vec<(AssetId, PathBuf)>,
    pub still_missing: Vec<AssetId>,
    /// Files looked at while searching.
    pub scanned: usize,
}

/// Most files a relink search looks at (protects against picking a drive root).
pub const RELINK_SCAN_LIMIT: usize = 500_000;

/// Look for every missing asset under `folder` (recursively) by file name,
/// case-insensitively. When several files share the name, the one whose
/// parent folders best match the old path wins. Assets whose stored path is a
/// bare name (from EDL / OTIO files without paths) are matched the same way.
pub fn relink_in_folder(project: &mut Project, folder: &Path) -> RelinkReport {
    let missing = missing_assets(project);
    let mut report = RelinkReport::default();
    if missing.is_empty() {
        return report;
    }
    let wanted: BTreeSet<String> = project.assets.iter().filter(|a| missing.contains(&a.id)).filter_map(|a| file_key(&a.path)).collect();
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                report.scanned += 1;
                if let Some(k) = file_key(&p).filter(|k| wanted.contains(k)) {
                    index.entry(k).or_default().push(p);
                }
            }
            if report.scanned >= RELINK_SCAN_LIMIT {
                stack.clear();
                break;
            }
        }
    }
    for a in project.assets.iter_mut().filter(|a| missing.contains(&a.id)) {
        let found = file_key(&a.path).and_then(|k| index.get(&k)).and_then(|cands| cands.iter().max_by_key(|c| shared_tail(&a.path, c)).cloned());
        match found {
            Some(p) => {
                a.path = p.clone();
                report.relinked.push((a.id, p));
            }
            None => report.still_missing.push(a.id),
        }
    }
    report
}

fn file_key(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase())
}

/// Number of trailing parent-folder names two paths share.
fn shared_tail(a: &Path, b: &Path) -> usize {
    let comps = |p: &Path| p.parent().map(|q| q.components().rev().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>()).unwrap_or_default();
    comps(a).iter().zip(comps(b).iter()).take_while(|(x, y)| x == y).count()
}

/// One file copy in a consolidate plan.
#[derive(Clone, Debug, PartialEq)]
pub struct CopyJob {
    pub asset: AssetId,
    pub from: PathBuf,
    pub to: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Default, PartialEq)]
pub struct ConsolidatePlan {
    pub copies: Vec<CopyJob>,
    /// Already inside the destination folder.
    pub in_place: Vec<AssetId>,
    /// Used but offline: can't be copied.
    pub missing: Vec<AssetId>,
}

impl ConsolidatePlan {
    pub fn total_bytes(&self) -> u64 {
        self.copies.iter().map(|c| c.bytes).sum()
    }
}

/// Plan copying the media into `dest`: the used assets, or every asset when
/// `include_unused`. Names that collide get ` (2)`, ` (3)`, … suffixes.
pub fn plan_consolidate(project: &Project, dest: &Path, include_unused: bool) -> ConsolidatePlan {
    let used = used_assets(project);
    let dest_canon = std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    let mut plan = ConsolidatePlan::default();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut by_source: HashMap<PathBuf, PathBuf> = HashMap::new();
    let assets: Vec<&MediaAsset> = project.assets.iter().filter(|a| a.kind != AssetKind::Synthetic && (include_unused || used.contains(&a.id))).collect();
    for a in assets {
        if !a.path.is_file() {
            plan.missing.push(a.id);
            continue;
        }
        let canon = std::fs::canonicalize(&a.path).unwrap_or_else(|_| a.path.clone());
        if canon.parent() == Some(dest_canon.as_path()) {
            plan.in_place.push(a.id);
            taken.insert(file_key(&a.path).unwrap_or_default());
            continue;
        }
        if let Some(to) = by_source.get(&canon) {
            // two assets on the same file share the copy
            plan.copies.push(CopyJob { asset: a.id, from: a.path.clone(), to: to.clone(), bytes: 0 });
            continue;
        }
        let stem = a.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "media".into());
        let ext = a.path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let mut n = 1;
        let to = loop {
            let name = if n == 1 { format!("{stem}{ext}") } else { format!("{stem} ({n}){ext}") };
            let candidate = dest.join(&name);
            if !taken.contains(&name.to_lowercase()) && !candidate.exists() {
                taken.insert(name.to_lowercase());
                break candidate;
            }
            n += 1;
        };
        let bytes = std::fs::metadata(&a.path).map(|m| m.len()).unwrap_or(0);
        by_source.insert(canon, to.clone());
        plan.copies.push(CopyJob { asset: a.id, from: a.path.clone(), to, bytes });
    }
    plan
}

/// Copy one planned file (skipped when an earlier job already wrote it).
/// Writes to a temporary name first so a cancelled copy never leaves a
/// truncated file under the final name.
pub fn copy_one(job: &CopyJob) -> std::io::Result<()> {
    if job.to.is_file() {
        return Ok(());
    }
    if let Some(d) = job.to.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = job.to.with_extension(format!("{}.partial", job.to.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()));
    std::fs::copy(&job.from, &tmp)?;
    std::fs::rename(&tmp, &job.to)
}

/// Point the copied assets at their new files.
pub fn apply_consolidate(project: &mut Project, done: &[CopyJob]) {
    for j in done {
        if let Some(a) = project.asset_mut(j.asset) {
            a.path = j.to.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Time;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rcp_media_mgmt_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn relink_prefers_the_matching_folder() {
        let root = tmpdir("relink");
        for sub in ["Day1/CamA", "Day2/CamA"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
            std::fs::write(root.join(sub).join("C0001.MP4"), b"x").unwrap();
        }
        let mut p = Project::new("t");
        let a = p.add_asset(MediaAsset::new("C0001.MP4", "Z:/old drive/Day2/CamA/c0001.mp4", Time::from_secs(1)));
        let b = p.add_asset(MediaAsset::new("gone.mov", "Z:/old drive/gone.mov", Time::from_secs(1)));
        let bare = p.add_asset(MediaAsset::new("C0001.MP4", "C0001.MP4", Time::from_secs(1)));
        let r = relink_in_folder(&mut p, &root);
        assert_eq!(r.still_missing, vec![b]);
        assert_eq!(p.asset(a).unwrap().path, root.join("Day2/CamA").join("C0001.MP4"));
        assert!(p.asset(bare).unwrap().path.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn consolidate_copies_used_media_and_renames_collisions() {
        let root = tmpdir("consolidate");
        let (d1, d2, dest) = (root.join("a"), root.join("b"), root.join("out"));
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(d1.join("clip.mov"), b"one").unwrap();
        std::fs::write(d2.join("clip.mov"), b"two!").unwrap();
        std::fs::write(d2.join("unused.wav"), b"u").unwrap();
        let mut p = Project::new("t");
        let a1 = MediaAsset::new("clip.mov", d1.join("clip.mov"), Time::from_secs(1));
        let a2 = MediaAsset::new("clip.mov", d2.join("clip.mov"), Time::from_secs(1));
        let a3 = MediaAsset::new("unused.wav", d2.join("unused.wav"), Time::from_secs(1));
        p.timeline.spine.push(Clip::from_asset(&a1));
        p.timeline.spine.push(Clip::from_asset(&a2));
        let (i1, i2, i3) = (p.add_asset(a1), p.add_asset(a2), p.add_asset(a3));
        let plan = plan_consolidate(&p, &dest, false);
        assert_eq!(plan.copies.len(), 2);
        assert_eq!(plan.total_bytes(), 7);
        for j in &plan.copies {
            copy_one(j).unwrap();
        }
        apply_consolidate(&mut p, &plan.copies);
        assert_eq!(std::fs::read(&p.asset(i1).unwrap().path).unwrap(), b"one");
        assert_eq!(std::fs::read(&p.asset(i2).unwrap().path).unwrap(), b"two!");
        assert_eq!(p.asset(i2).unwrap().path.file_name().unwrap(), "clip (2).mov");
        assert_eq!(p.asset(i3).unwrap().path, d2.join("unused.wav"), "unused media left alone");
        // running it again finds everything in place
        let again = plan_consolidate(&p, &dest, false);
        assert!(again.copies.is_empty() && again.in_place.len() == 2, "{again:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn other_timelines_count_as_used_and_get_refreshed() {
        let mut p = Project::new("t");
        let a = MediaAsset::new("b.mov", "b.mov", Time::from_secs(10));
        let i = p.add_asset(a.clone());
        let other = p.add_timeline("B-roll");
        p.switch_timeline(other);
        p.timeline.spine.push(Clip::from_asset(&a));
        p.switch_timeline(0);
        assert!(used_assets(&p).contains(&i), "an inactive timeline still uses its media");
        let mut probed = MediaAsset::new("b.mov", "b.mov", Time::from_secs(4));
        probed.video = None;
        refresh_asset(&mut p, i, probed);
        let c = &p.timeline_at(other).unwrap().spine[0];
        assert_eq!(c.media_duration, Some(Time::from_secs(4)));
        assert!(!c.av.video);
    }
}
