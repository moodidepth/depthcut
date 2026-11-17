//! Online Audio & SFX Browser: Openverse (CC0 / CC-BY) & Internet Archive
//! search, streaming preview, streamed download & timeline insertion, plus
//! optional `yt-dlp` URL import.

use crate::app::App;
use crate::theme;
use eframe::egui::{self, Color32};
use nle_core::{AssetId, Attribution, Clip, Command, Time, Timeline};
use nle_media::audio::AudioPreview;
use nle_media::jobs::Job;
use nle_media::online::{block_on, download, license_badge, search_blocking, ytdlp, Provider, SearchResult};
use nle_media::{paths, probe};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserTab {
    #[default]
    ProjectMedia,
    MontagePresets,
    OnlineAudio,
}

pub struct OnlineState {
    pub tab: BrowserTab,
    pub query: String,
    pub use_openverse: bool,
    pub use_archive: bool,
    pub results: Vec<SearchResult>,
    pub warnings: Vec<String>,
    pub search_job: Option<Job<(Vec<SearchResult>, Vec<String>)>>,
    pub download_job: Option<Job<(PathBuf, Option<Attribution>)>>,
    pub download_id: Option<String>,
    pub preview_url_job: Option<Job<(String, String)>>,
    pub preview_resolving_id: Option<String>,
    pub ytdlp_url: String,
    pub ytdlp_path: Option<PathBuf>,
    pub preview: AudioPreview,
}

impl Default for OnlineState {
    fn default() -> Self {
        OnlineState {
            tab: BrowserTab::ProjectMedia,
            query: String::new(),
            use_openverse: true,
            use_archive: true,
            results: Vec::new(),
            warnings: Vec::new(),
            search_job: None,
            download_job: None,
            download_id: None,
            preview_url_job: None,
            preview_resolving_id: None,
            ytdlp_url: String::new(),
            ytdlp_path: ytdlp::find(),
            preview: AudioPreview::new(),
        }
    }
}

pub fn poll(app: &mut App, ctx: &egui::Context) {
    app.online.preview.poll();
    if app.online.preview.is_playing() {
        ctx.request_repaint_after(Duration::from_millis(120));
    }

    if let Some(job) = app.online.preview_url_job.as_ref() {
        if let Some(res) = job.take() {
            app.online.preview_url_job = None;
            app.online.preview_resolving_id = None;
            match res {
                Ok((id, url)) => app.online.preview.play(id, url),
                Err(e) => app.status(format!("Could not stream preview: {e}")),
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    if let Some(job) = app.online.search_job.as_ref() {
        if let Some(res) = job.take() {
            app.online.search_job = None;
            match res {
                Ok((hits, warns)) => {
                    let count = hits.len();
                    app.online.results = hits;
                    app.online.warnings = warns;
                    app.status(format!("Found {count} online audio tracks"));
                }
                Err(e) => {
                    app.online.warnings = vec![e];
                }
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    if let Some(job) = app.online.download_job.as_ref() {
        if let Some(res) = job.take() {
            app.online.download_job = None;
            app.online.download_id = None;
            match res {
                Ok((path, attr)) => add_downloaded_audio(app, path, attr),
                Err(e) => app.status(format!("Download failed: {e}")),
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn free_audio_lane_below(tl: &Timeline, at: Time, dur: Time) -> i32 {
    let placed = tl.placed_clips();
    (1..=32)
        .map(|l| -l)
        .find(|&lane| !placed.iter().any(|p| p.lane == lane && p.start < at + dur && p.end() > at))
        .unwrap_or(-1)
}

fn free_video_lane_above(tl: &Timeline, at: Time, dur: Time) -> i32 {
    let placed = tl.placed_clips();
    (1..=32)
        .find(|&lane| !placed.iter().any(|p| p.lane == lane && p.start < at + dur && p.end() > at))
        .unwrap_or(1)
}

fn add_downloaded_audio(app: &mut App, path: PathBuf, attr: Option<Attribution>) {
    match probe::probe(&path) {
        Ok(mut asset) => {
            asset.attribution = attr;
            let name = asset.name.clone();
            let id: AssetId = app.project.add_asset(asset);
            app.assets_changed = true;
            if let Some(a) = app.project.asset(id).cloned() {
                app.library.ensure(&a);
                app.browser.selected = Some(id);
                let clip = Clip::from_asset(&a);
                let cmd = if app.project.timeline.spine.is_empty() {
                    Command::Append { clip }
                } else {
                    let lane = if a.has_video() {
                        free_video_lane_above(&app.project.timeline, app.playhead, clip.duration)
                    } else {
                        free_audio_lane_below(&app.project.timeline, app.playhead, clip.duration)
                    };
                    Command::Connect { clip, at: app.playhead, lane }
                };
                app.exec(cmd);
                app.status(format!("Added {name} to project & timeline"));
            }
        }
        Err(e) => app.status(format!("Could not probe downloaded media: {e}")),
    }
}

fn start_search(app: &mut App) {
    let query = app.online.query.trim().to_string();
    if query.is_empty() {
        return;
    }
    let mut providers = Vec::new();
    if app.online.use_openverse {
        providers.push(Provider::Openverse);
    }
    if app.online.use_archive {
        providers.push(Provider::InternetArchive);
    }
    if providers.is_empty() {
        app.online.warnings = vec!["Enable Openverse or Internet Archive to search".into()];
        return;
    }
    app.online.warnings.clear();
    app.online.search_job = Some(Job::spawn("online-search", move |jctx| {
        jctx.set_progress(0.2, "Searching online libraries…");
        if jctx.cancelled() {
            return Err("Cancelled".into());
        }
        Ok(search_blocking(&query, &providers))
    }));
}

fn license_color(badge: &str) -> Color32 {
    match badge {
        "CC0" | "PD" => theme::AUDIO,
        "Unknown" => theme::PLAYHEAD,
        _ => theme::SNAP,
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let mut trigger_search = false;
    ui.horizontal(|ui| {
        let edit = ui.add(
            egui::TextEdit::singleline(&mut app.online.query)
                .hint_text("Search CC0 / CC-BY music & SFX…")
                .desired_width((ui.available_width() - 68.0).max(120.0)),
        );
        if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            trigger_search = true;
        }
        if ui
            .add_enabled(app.online.search_job.is_none() && !app.online.query.trim().is_empty(), egui::Button::new("Search"))
            .clicked()
        {
            trigger_search = true;
        }
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut app.online.use_openverse, "Openverse");
        ui.checkbox(&mut app.online.use_archive, "Internet Archive");
        if app.online.preview.is_playing() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⏹ Stop Preview").clicked() {
                    app.online.preview.stop();
                }
            });
        }
    });
    if trigger_search {
        start_search(app);
    }

    if let Some(job) = &app.online.search_job {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Searching…").small().color(theme::TEXT_DIM));
            if ui.small_button("Cancel").clicked() {
                job.cancel();
            }
        });
    }
    if let Some(err) = app.online.preview.error() {
        ui.label(egui::RichText::new(err).small().color(theme::PLAYHEAD));
    }
    for w in &app.online.warnings {
        ui.label(egui::RichText::new(w).small().color(theme::SNAP));
    }

    if let Some(job) = &app.online.download_job {
        let (frac, msg) = job.progress();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_width((ui.available_width() - 72.0).max(80.0))
                    .text(format!("{msg} ({:.0}%)", frac * 100.0)),
            );
            if ui.small_button("Cancel").clicked() {
                job.cancel();
            }
        });
    }

    ui.add_space(4.0);
    egui::CollapsingHeader::new("Import URL (yt-dlp)").default_open(false).show(ui, |ui| {
        ui.horizontal(|ui| {
            match &app.online.ytdlp_path {
                Some(p) => {
                    ui.label(egui::RichText::new(format!("yt-dlp: {}", p.display())).small().color(theme::AUDIO));
                }
                None => {
                    ui.label(egui::RichText::new("yt-dlp not found (place in ./tools or PATH)").small().color(theme::TEXT_DIM));
                }
            }
            if ui.small_button("↻").on_hover_text("Rescan for yt-dlp").clicked() {
                app.online.ytdlp_path = ytdlp::find();
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut app.online.ytdlp_url)
                    .hint_text("https://…")
                    .desired_width((ui.available_width() - 104.0).max(90.0)),
            );
            let can_dl = app.online.ytdlp_path.is_some()
                && app.online.download_job.is_none()
                && !app.online.ytdlp_url.trim().is_empty();
            if ui.add_enabled(can_dl, egui::Button::new("Download & Add")).clicked() {
                let url = app.online.ytdlp_url.trim().to_string();
                let dir = paths::downloads_dir();
                app.online.preview.stop();
                app.online.download_id = Some(url.clone());
                app.online.download_job = Some(Job::spawn("ytdlp", move |jctx| {
                    jctx.set_progress(0.0, "Downloading via yt-dlp…");
                    let c = jctx.cancel_flag();
                    let p = ytdlp::download_audio(&url, &dir, |f| jctx.set_fraction(f), c)?;
                    Ok((p, None))
                }));
            }
        });
    });

    ui.separator();

    let results = app.online.results.clone();
    let busy_dl = app.online.download_job.is_some();
    egui::ScrollArea::vertical().id_salt("online-results").auto_shrink([false, false]).show(ui, |ui| {
        if results.is_empty() {
            ui.label(
                egui::RichText::new("Search royalty-free Creative Commons audio on Openverse (Freesound, Jamendo, Wikimedia) & Internet Archive.")
                    .small()
                    .color(theme::TEXT_DIM),
            );
            return;
        }
        for res in &results {
            let badge = license_badge(&res.license);
            let badge_col = license_color(&badge);
            egui::Frame::new()
                .fill(theme::BG)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 115.0).max(80.0));
                            ui.label(egui::RichText::new(&res.title).strong());
                            let mut meta = String::new();
                            if !res.creator.is_empty() {
                                meta.push_str(&res.creator);
                                meta.push_str(" · ");
                            }
                            meta.push_str(&res.source);
                            if let Some(secs) = res.duration_secs {
                                let s = secs.round() as u64;
                                meta.push_str(&format!(" · {}:{:02}", s / 60, s % 60));
                            }
                            ui.label(egui::RichText::new(meta).small().color(theme::TEXT_DIM));
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&badge).small().strong().color(badge_col))
                                    .on_hover_text(&res.license_name);
                                if !res.landing_url.is_empty() {
                                    ui.hyperlink_to(egui::RichText::new("Source").small(), &res.landing_url);
                                }
                            });
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add_enabled(!busy_dl, egui::Button::new("⬇ Add")).on_hover_text("Download, add attribution, and insert at playhead").clicked() {
                                let item = res.clone();
                                app.online.download_id = Some(item.id.clone());
                                app.online.download_job = Some(Job::spawn("online-download", move |jctx| {
                                    jctx.set_progress(0.0, format!("Downloading {}…", item.title));
                                    let cancel = jctx.cancel_flag();
                                    let attr = Some(item.attribution());
                                    let path = download::fetch_result(
                                        &item,
                                        |done, total| {
                                            if let Some(t) = total.filter(|&t| t > 0) {
                                                jctx.set_fraction((done as f64 / t as f64) as f32);
                                            }
                                        },
                                        cancel,
                                    )?;
                                    Ok((path, attr))
                                }));
                            }
                            let is_playing = app.online.preview.is_playing_id(&res.id);
                            let resolving = app.online.preview_resolving_id.as_deref() == Some(res.id.as_str());
                            let btn_text = if is_playing {
                                "⏹"
                            } else if resolving {
                                "⏳"
                            } else {
                                "▶"
                            };
                            if ui.add_enabled(!resolving, egui::Button::new(btn_text)).on_hover_text(if is_playing { "Stop preview" } else { "Stream preview" }).clicked() {
                                if is_playing {
                                    app.online.preview.stop();
                                } else {
                                    if app.playback.playing {
                                        app.stop();
                                    }
                                    if let Some(url) = &res.audio_url {
                                        app.online.preview.play(&res.id, url);
                                    } else {
                                        let item = res.clone();
                                        let id = item.id.clone();
                                        app.online.preview_resolving_id = Some(id.clone());
                                        app.online.preview_url_job = Some(Job::spawn("preview-url", move |_| {
                                            let url = block_on(item.resolve_audio_url())?;
                                            Ok((id, url))
                                        }));
                                    }
                                }
                            }
                        });
                    });
                });
            ui.add_space(3.0);
        }
    });
}
