//! Timeline interchange (EDL / OTIO / FCPXML files) and media management
//! (relink missing media, consolidate media into one folder) for the File menu.

use crate::app::{App, ProjectSwitch};
use eframe::egui;
use nle_core::media_mgmt::{self, ConsolidatePlan, CopyJob, RelinkReport};
use nle_core::{edl, fcpxml, otio, AssetId, MediaAsset, Project};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

enum JobMsg {
    Relinked(RelinkReport),
    Copied { done: Vec<CopyJob>, errors: Vec<String>, cancelled: bool },
}

/// A relink search or consolidate copy running on a worker thread.
pub struct MediaJob {
    rx: Receiver<JobMsg>,
    label: &'static str,
    /// Bytes copied so far (consolidate).
    progress: Arc<AtomicU64>,
    total: u64,
    cancel: Arc<AtomicBool>,
}

impl MediaJob {
    pub fn label(&self) -> String {
        if self.total > 0 {
            let p = self.progress.load(Ordering::Relaxed) as f64 / self.total as f64;
            format!("{} {:.0}%", self.label, (p * 100.0).min(100.0))
        } else {
            format!("{}…", self.label)
        }
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Timeline file formats the File menu reads.
pub fn is_timeline_file(p: &Path) -> bool {
    p.extension().is_some_and(|e| ["fcpxml", "edl", "otio"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

impl App {
    pub fn import_interchange_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Timelines (EDL, OpenTimelineIO, FCPXML)", &["edl", "otio", "fcpxml", "xml"])
            .add_filter("CMX3600 EDL", &["edl"])
            .add_filter("OpenTimelineIO", &["otio"])
            .pick_file()
        else {
            return;
        };
        self.load_timeline_file(&path);
    }

    /// Replace the project with a timeline read from an FCPXML, EDL or OTIO file.
    pub fn load_timeline_file(&mut self, path: &Path) {
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let rate = self.project.timeline.format.rate;
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return self.status(format!("Could not read {}: {e}", path.display())),
        };
        let (res, needs_probe): (Result<Project, String>, bool) = match ext.as_str() {
            "edl" => (edl::import(&text, rate).map_err(|e| e.to_string()), true),
            "otio" => (otio::import(&text).map_err(|e| e.to_string()), true),
            _ => (fcpxml::import(&text).map_err(|e| e.to_string()), false),
        };
        let mut p = match res {
            Ok(p) => p,
            Err(e) => return self.status(format!("Import of {} failed: {e}", path.display())),
        };
        if ext == "edl" {
            // EDLs don't name their timeline size: keep the current one
            p.timeline.format = self.project.timeline.format;
        }
        self.set_project(p, None);
        let mut msg = format!("Imported {}", path.display());
        if needs_probe {
            // EDL / OTIO files carry no stream info: read it from the media
            let base = path.parent().map(Path::to_path_buf);
            let mut offline = 0;
            for a in self.project.assets.clone() {
                // paths relative to the timeline file
                let full = match (&base, a.path.is_relative()) {
                    (Some(b), true) if b.join(&a.path).is_file() => b.join(&a.path),
                    _ => a.path.clone(),
                };
                if full.is_file() {
                    if let Some(m) = self.project.asset_mut(a.id) {
                        m.path = full.clone();
                    }
                    self.library.reprobe(a.id, full);
                } else {
                    offline += 1;
                }
            }
            if ext == "edl" {
                msg.push_str(&format!(" at {:.3} fps", rate.fps()));
            }
            if offline > 0 {
                msg.push_str(&format!(" — {offline} media file(s) offline: File ▸ Relink Missing Media…"));
            }
            self.assets_changed = true;
        }
        self.status(msg);
    }

    /// Fresh stream info for an asset (after an import or relink).
    pub fn apply_reprobe(&mut self, id: AssetId, probed: MediaAsset) {
        media_mgmt::refresh_asset(&mut self.project, id, probed);
        self.assets_changed = true;
        self.library.forget_asset(id);
        self.frames.forget(id);
        if let Some(a) = self.project.asset(id).cloned() {
            self.library.ensure(&a);
        }
        if let Some(p) = &mut self.preview {
            p.renderer.clear_cache();
            p.last = None;
        }
    }

    pub fn export_edl(&mut self) {
        let project = self.full_project().into_owned();
        let Some(path) = rfd::FileDialog::new().add_filter("CMX3600 EDL", &["edl"]).set_file_name(format!("{}.edl", project.name)).save_file() else {
            return;
        };
        let tracks = edl::export_tracks(&project);
        let mut written = Vec::new();
        for (i, (name, text)) in tracks.iter().enumerate() {
            // V1 gets the chosen name; other tracks get a _V2 / _A2 suffix
            let p = if i == 0 {
                path.clone()
            } else {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                path.with_file_name(format!("{stem}_{name}.edl"))
            };
            if let Err(e) = nle_core::autosave::write_atomic(&p, text.as_bytes()) {
                return self.status(format!("EDL export failed: {e}"));
            }
            written.push(p);
        }
        let extra = if written.len() > 1 { format!(" (+{} more track file(s))", written.len() - 1) } else { String::new() };
        self.status(format!("Exported {}{extra}", path.display()));
    }

    pub fn export_otio(&mut self) {
        let project = self.full_project().into_owned();
        let Some(path) = rfd::FileDialog::new().add_filter("OpenTimelineIO", &["otio"]).set_file_name(format!("{}.otio", project.name)).save_file() else {
            return;
        };
        match nle_core::autosave::write_atomic(&path, otio::export(&project).as_bytes()) {
            Ok(()) => self.status(format!("Exported {}", path.display())),
            Err(e) => self.status(format!("OTIO export failed: {e}")),
        }
    }

    pub fn relink_dialog(&mut self) {
        if self.media_job.is_some() {
            return;
        }
        let missing = media_mgmt::missing_assets(&self.project);
        if missing.is_empty() {
            return self.status("No media is missing");
        }
        let Some(folder) = rfd::FileDialog::new().set_title(format!("Folder to search for {} missing file(s)", missing.len())).pick_folder() else {
            return;
        };
        let mut snapshot = self.project.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let report = media_mgmt::relink_in_folder(&mut snapshot, &folder);
            let _ = tx.send(JobMsg::Relinked(report));
        });
        self.media_job = Some(MediaJob { rx, label: "Searching for missing media", progress: Arc::default(), total: 0, cancel: Arc::default() });
    }

    pub fn consolidate_dialog(&mut self) {
        if self.media_job.is_some() {
            return;
        }
        let Some(dest) = rfd::FileDialog::new().set_title("Copy the project's media into…").pick_folder() else {
            return;
        };
        let plan: ConsolidatePlan = media_mgmt::plan_consolidate(&self.full_project(), &dest, false);
        if plan.copies.is_empty() {
            let missing = if plan.missing.is_empty() { String::new() } else { format!(" ({} offline file(s) skipped)", plan.missing.len()) };
            return self.status(format!("Nothing to copy: the used media is already in {}{missing}", dest.display()));
        }
        let free_hint = format!("{:.1} GB", plan.total_bytes() as f64 / 1e9);
        let (tx, rx) = channel();
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let total = plan.total_bytes().max(1);
        let (pr, ca) = (progress.clone(), cancel.clone());
        let skipped = plan.missing.len();
        std::thread::spawn(move || {
            let mut done = Vec::new();
            let mut errors = Vec::new();
            if skipped > 0 {
                errors.push(format!("{skipped} offline file(s) not copied"));
            }
            for job in plan.copies {
                if ca.load(Ordering::Relaxed) {
                    let _ = tx.send(JobMsg::Copied { done, errors, cancelled: true });
                    return;
                }
                match media_mgmt::copy_one(&job) {
                    Ok(()) => {
                        pr.fetch_add(job.bytes, Ordering::Relaxed);
                        done.push(job);
                    }
                    Err(e) => errors.push(format!("{}: {e}", job.from.display())),
                }
            }
            let _ = tx.send(JobMsg::Copied { done, errors, cancelled: false });
        });
        self.status(format!("Copying {free_hint} of media to {}", dest.display()));
        self.media_job = Some(MediaJob { rx, label: "Consolidating media", progress, total, cancel });
    }

    /// Collect a finished relink / consolidate job.
    pub fn poll_media_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.media_job else { return };
        let msg = match job.rx.try_recv() {
            Ok(m) => m,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.media_job = None;
                return self.status("Media job stopped unexpectedly");
            }
        };
        self.media_job = None;
        match msg {
            JobMsg::Relinked(r) => {
                for (id, path) in &r.relinked {
                    if let Some(a) = self.project.asset_mut(*id) {
                        a.path = path.clone();
                    }
                    self.library.reprobe(*id, path.clone());
                }
                self.assets_changed |= !r.relinked.is_empty();
                let still = if r.still_missing.is_empty() { String::new() } else { format!(", {} still missing", r.still_missing.len()) };
                self.status(format!("Relinked {} file(s) ({} files searched){still}", r.relinked.len(), r.scanned));
            }
            JobMsg::Copied { done, errors, cancelled } => {
                media_mgmt::apply_consolidate(&mut self.project, &done);
                for j in &done {
                    self.frames.forget(j.asset);
                }
                self.assets_changed |= !done.is_empty();
                let mut m = format!("Consolidated {} file(s)", done.len());
                if cancelled {
                    m.push_str(" — cancelled; the rest still point at their old location");
                }
                if let Some(e) = errors.first() {
                    m.push_str(&format!(" — {} problem(s), first: {e}", errors.len()));
                }
                self.status(m);
            }
        }
    }

    /// Path of a dropped timeline file, opened like File ▸ Import.
    pub fn open_dropped_timeline(&mut self, path: PathBuf) {
        self.request_switch(ProjectSwitch::DroppedTimeline(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_files_are_recognised() {
        assert!(is_timeline_file(Path::new("a/cut.EDL")));
        assert!(is_timeline_file(Path::new("cut.otio")));
        assert!(is_timeline_file(Path::new("cut.fcpxml")));
        assert!(!is_timeline_file(Path::new("clip.mov")));
    }
}
