//! The Magnetic Timeline: primary storyline, connected lanes, filmstrips,
//! min/max peak waveforms, transitions, retime editor, keyframe animation lane,
//! skimming, snapping, the A / T / P / B / R tools, step-in editing of
//! compound clips and storylines, auditions and the role-grouped audio lanes.

use crate::app::{drag_key, App, EditMode, Tool};
use crate::inspector::clip_accent;
use crate::{theme, track_headers};
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use nle_core::captions::CaptionTrack;
use nle_core::speed::{MAX_VELOCITY, MIN_VELOCITY};
use nle_core::{Action, AssetId, AudioRole, Clip, ClipId, ClipKind, Command, FrameBlendMode, Interpolation, Marker, MarkerKind, Param, Retime, Time, Timeline};
use std::borrow::Cow;

const RULER_H: f32 = 26.0;
const CAPTIONS_H: f32 = 24.0;
const SPINE_H: f32 = 78.0;
const LANE_H: f32 = 50.0;
const ANIM_H: f32 = 30.0;
const RETIME_H: f32 = 34.0;
const LANE_GAP: f32 = 4.0;
/// Header strip of a role band in the audio-lanes view.
const ROLE_HEADER_H: f32 = 16.0;
/// Height of a collapsed role band's clip strip.
const ROLE_COLLAPSED_H: f32 = 14.0;
const EDGE_PX: f32 = 7.0;
/// Hit width of a caption block's trim handles.
const CAP_EDGE_PX: f32 = 8.0;
/// History coalescing key for caption-bar drags (one undo step per drag).
const CAPTION_BAR_KEY: u64 = 0xCC00_0002;

#[derive(Clone, Debug)]
pub enum Drag {
    Scrub,
    Move {
        id: ClipId,
        grab: Time,
    },
    /// Position tool move: no ripple, gap left behind, overwrite at the target.
    Position {
        id: ClipId,
        grab: Time,
    },
    Trim {
        id: ClipId,
        start_edge: bool,
        origin: Time,
    },
    Slip {
        id: ClipId,
        origin: Time,
    },
    Slide {
        id: ClipId,
        origin: Time,
    },
    /// Roll edit (Trim tool on a storyline edit point): moves the cut between
    /// `left` and its right neighbour without changing timeline length.
    Roll {
        left: ClipId,
        edit: Time,
        origin: Time,
    },
    /// J/L split edit (Alt-drag a clip edge): extends audio past the picture cut.
    Split {
        id: ClipId,
        start_edge: bool,
        origin: Time,
    },
    Range {
        anchor: Time,
    },
    Key {
        id: ClipId,
        key_time: Time,
        origin: Time,
    },
    VelocityKey {
        id: ClipId,
        index: usize,
        lane_top: f32,
        lane_height: f32,
    },
    /// Trim a caption block edge in the captions row; `base` is the track
    /// at drag start (the drag is re-applied to it every frame).
    CaptionTrim {
        index: usize,
        start_edge: bool,
        origin: Time,
        base: Box<CaptionTrack>,
    },
    /// Slide a caption block (with its words) in the captions row.
    CaptionSlide {
        index: usize,
        origin: Time,
        base: Box<CaptionTrack>,
    },
}

pub struct TimelineState {
    /// Pixels per second.
    pub pps: f32,
    /// Seconds at the left edge.
    pub scroll: f32,
    pub vscroll: f32,
    pub drag: Option<Drag>,
    pub range: Option<(Time, Time)>,
    pub show_animation: bool,
    pub show_retime: bool,
    pub fit_pending: bool,
    /// Clip under the pointer (target of split when nothing is selected).
    pub hovered_clip: Option<ClipId>,
    /// Also snap to loud audio transients while magnetic snapping is on.
    pub snap_audio_peaks: bool,
    /// Audio-peak snap points for the drag in progress (timeline times).
    snap_extra: Vec<Time>,
    /// Pointer is over the captions row (C / Ctrl+B then split captions).
    pub hover_captions: bool,
    view_width: f32,
    context_clip: Option<ClipId>,
    /// Keyframe diamond selected in the Animation lane (clip, clip-local time): Delete removes it.
    pub selected_key: Option<(ClipId, Time)>,
    /// Diamond the right-click (ease) menu is open for.
    key_menu: Option<(ClipId, Time)>,
    /// Audio-lanes view: connected clips below the storyline grouped by role.
    pub audio_lanes: bool,
    /// Role bands folded to a thin strip in the audio-lanes view.
    pub collapsed_roles: Vec<AudioRole>,
}

/// One role band of the audio-lanes view.
struct RoleGroup {
    role: AudioRole,
    rows: usize,
    /// (index into the placed clips, sub-row inside the band)
    slots: Vec<(usize, usize)>,
    collapsed: bool,
    top: f32,
}

impl TimelineState {
    /// Extra snap targets: `also` (usually the playhead) plus audio peaks.
    fn snap_targets(&self, also: Time) -> Vec<Time> {
        let mut v = Vec::with_capacity(self.snap_extra.len() + 1);
        v.push(also);
        v.extend_from_slice(&self.snap_extra);
        v
    }
}

/// Most audio-peak snap points gathered per frame (beyond this the timeline
/// is zoomed out so far that peaks are closer together than the snap radius).
const MAX_PEAK_SNAPS: usize = 4_096;

/// Timeline times of loud audio transients inside the visible range, for
/// clips that are not being moved. Only computed while a drag is active.
fn audio_peak_snaps(app: &App, placed: &[(Clip, Time, i32, Option<ClipId>)], visible: (Time, Time)) -> Vec<Time> {
    let mut out = Vec::new();
    if !app.project.timeline.snapping || !app.tl.snap_audio_peaks {
        return out;
    }
    // A clip being moved carries its audio with it: its old peaks are stale.
    let moving = match &app.tl.drag {
        Some(Drag::Move { id, .. }) | Some(Drag::Position { id, .. }) | Some(Drag::Slip { id, .. }) | Some(Drag::Slide { id, .. }) => Some(*id),
        Some(_) => None,
        None => return out,
    };
    for (c, start, _, _) in placed {
        // Retimed clips have no linear source→timeline mapping; skip them.
        if Some(c.id) == moving || !c.has_audio() || c.retime.is_some() {
            continue;
        }
        let Some(tr) = c.asset_id().and_then(|a| app.library.transients.get(&a)) else { continue };
        let local0 = (visible.0 - *start).max(Time::ZERO);
        let local1 = (visible.1 - *start).min(c.duration);
        if local1 <= local0 {
            continue;
        }
        let (s0, s1) = (c.source_in + local0, c.source_in + local1);
        let first = tr.partition_point(|t| *t < s0);
        for t in tr[first..].iter().take_while(|t| **t <= s1) {
            out.push(*start + (*t - c.source_in));
            if out.len() >= MAX_PEAK_SNAPS {
                return out;
            }
        }
    }
    out
}

impl Default for TimelineState {
    fn default() -> Self {
        TimelineState {
            pps: 60.0,
            scroll: 0.0,
            vscroll: 0.0,
            drag: None,
            range: None,
            show_animation: false,
            show_retime: false,
            fit_pending: true,
            hovered_clip: None,
            snap_audio_peaks: true,
            snap_extra: Vec::new(),
            hover_captions: false,
            view_width: 1000.0,
            context_clip: None,
            selected_key: None,
            key_menu: None,
            audio_lanes: false,
            collapsed_roles: Vec::new(),
        }
    }
}

impl TimelineState {
    pub fn zoom_by(&mut self, k: f32) {
        self.pps = (self.pps * k).clamp(1.0, 4000.0);
    }
}

struct Row {
    lane: i32,
    top: f32,
    height: f32,
}

struct Hit {
    id: ClipId,
    rect: Rect,
    start: Time,
    lane: i32,
}

/// Geometry helpers for one frame.
struct Geo {
    left: f32,
    scroll: f32,
    pps: f32,
}

impl Geo {
    fn x(&self, t: Time) -> f32 {
        self.left + (t.as_secs_f64() as f32 - self.scroll) * self.pps
    }
    fn t(&self, x: f32) -> Time {
        Time::from_secs_f64(((x - self.left) / self.pps + self.scroll).max(0.0) as f64)
    }
}

const OVERVIEW_H: f32 = 18.0;

/// Resolve Cut-page style overview: the whole edit at a fixed scale with the
/// zoomed viewport outlined. Click or drag to jump the detail timeline there,
/// so long montages never need scroll-wheel hunting.
fn overview(app: &mut App, ui: &mut egui::Ui) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), OVERVIEW_H), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(0x13, 0x13, 0x15));
    let tl = &app.project.timeline;
    let extent = tl.extent().max(Time::from_secs(10)).as_secs_f64() as f32 * 1.02;
    let x_of = |t: f32| rect.left() + (t / extent) * rect.width();
    let mid = rect.center().y;
    for p in tl.placed_clips() {
        let (x0, x1) = (x_of(p.start.as_secs_f64() as f32), x_of((p.start + p.clip.duration).as_secs_f64() as f32));
        let (y0, y1) = match p.lane {
            0 => (mid - 4.0, mid + 4.0),
            l if l > 0 => (rect.top() + 2.0, mid - 5.0),
            _ => (mid + 5.0, rect.bottom() - 2.0),
        };
        let col = if p.clip.enabled { clip_accent(p.clip) } else { theme::GAP };
        painter.rect_filled(Rect::from_min_max(Pos2::new(x0, y0), Pos2::new((x1 - 0.5).max(x0 + 1.0), y1)), CornerRadius::same(1), col.gamma_multiply(0.8));
    }
    for m in &tl.markers {
        let x = x_of(m.time.as_secs_f64() as f32);
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.top() + 4.0)], Stroke::new(1.0_f32, theme::SNAP));
    }
    let span = app.tl.view_width.max(1.0) / app.tl.pps.max(1e-3);
    let vp = Rect::from_min_max(Pos2::new(x_of(app.tl.scroll), rect.top() + 1.0), Pos2::new(x_of(app.tl.scroll + span).min(rect.right()), rect.bottom() - 1.0));
    painter.rect_filled(vp, CornerRadius::same(2), Color32::from_white_alpha(18));
    painter.rect_stroke(vp, CornerRadius::same(2), Stroke::new(1.0_f32, theme::TEXT_DIM), StrokeKind::Inside);
    let px = x_of(app.playhead.as_secs_f64() as f32);
    painter.line_segment([Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())], Stroke::new(1.5_f32, theme::PLAYHEAD));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if let Some(p) = resp.interact_pointer_pos().filter(|_| resp.clicked() || resp.dragged()) {
        let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) * extent;
        app.tl.scroll = (t - span * 0.5).max(0.0);
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    toolbar(app, ui);
    overview(app, ui);
    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let split = (full.left() + track_headers::WIDTH).min(full.right());
    let column = Rect::from_min_max(full.min, Pos2::new(split, full.bottom()));
    let area = Rect::from_min_max(Pos2::new(split, full.top()), full.max);
    let resp = ui.interact(area, ui.id().with("timeline-lanes"), Sense::click_and_drag());
    let painter = ui.painter_at(area);
    painter.rect_filled(area, CornerRadius::ZERO, theme::BG);

    let rate = app.project.timeline.format.rate;
    let frame = app.project.timeline.frame();
    app.tl.view_width = area.width();
    let extent = app.project.timeline.extent().max(Time::from_secs(10));
    if app.tl.fit_pending {
        app.tl.pps = ((area.width() - 60.0) / extent.as_secs_f64() as f32).clamp(1.0, 4000.0);
        app.tl.scroll = 0.0;
        app.tl.fit_pending = false;
    }

    // ----- scroll & zoom
    if resp.hovered() {
        let (delta, zoom, pointer) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
        if zoom != 1.0 {
            let px = pointer.map(|p| p.x).unwrap_or(area.center().x);
            let t_at = (px - area.left()) / app.tl.pps + app.tl.scroll;
            app.tl.zoom_by(zoom);
            app.tl.scroll = (t_at - (px - area.left()) / app.tl.pps).max(0.0);
        }
        if delta.x != 0.0 {
            app.tl.scroll = (app.tl.scroll - delta.x / app.tl.pps).max(0.0);
        }
        if delta.y != 0.0 {
            app.tl.vscroll -= delta.y;
        }
    }
    let geo = Geo { left: area.left(), scroll: app.tl.scroll, pps: app.tl.pps };

    // follow the playhead during playback
    if app.playback.playing {
        let x = geo.x(app.playhead);
        let span = area.width() / app.tl.pps;
        if x > area.right() - 30.0 || x < area.left() {
            app.tl.scroll = (app.playhead.as_secs_f64() as f32 - span * 0.1).max(0.0);
        }
    }
    let geo = Geo { left: area.left(), scroll: app.tl.scroll, pps: app.tl.pps };

    // ----- rows
    let placed: Vec<(Clip, Time, i32, Option<ClipId>)> = app.project.timeline.placed_clips().into_iter().map(|p| (p.clip.clone(), p.start, p.lane, p.parent)).collect();
    // a lane with a switch on stays listed when it has no clips left
    let used = || placed.iter().map(|p| p.2).chain(app.project.timeline.tracks.iter().map(|t| t.lane));
    let max_lane = used().max().unwrap_or(0).max(0) + 1;
    let min_lane = used().min().unwrap_or(0).min(0) - 1;
    let anim = if app.tl.show_animation { ANIM_H } else { 0.0 };
    let retime_extra = if app.tl.show_retime { RETIME_H + 2.0 } else { 0.0 };
    let has_captions = !app.project.timeline.captions.is_empty();
    let header_h = RULER_H + if has_captions { CAPTIONS_H } else { 0.0 };
    // Audio-lanes view: connected clips below the storyline grouped into
    // collapsible bands by role, packed into as few sub-rows as they fit.
    let role_mode = app.tl.audio_lanes;
    let mut role_groups: Vec<RoleGroup> = Vec::new();
    if role_mode {
        for role in AudioRole::ALL {
            let mut members: Vec<usize> = (0..placed.len()).filter(|&i| placed[i].2 < 0 && placed[i].0.audio.role == role).collect();
            if members.is_empty() {
                continue;
            }
            members.sort_by_key(|&i| placed[i].1);
            let mut ends: Vec<Time> = Vec::new();
            let mut slots = Vec::with_capacity(members.len());
            for i in members {
                let (c, start, ..) = &placed[i];
                let row = match ends.iter().position(|e| *e <= *start) {
                    Some(r) => r,
                    None => {
                        ends.push(Time::ZERO);
                        ends.len() - 1
                    }
                };
                ends[row] = *start + c.duration;
                slots.push((i, row));
            }
            let collapsed = app.tl.collapsed_roles.contains(&role);
            role_groups.push(RoleGroup { role, rows: ends.len(), slots, collapsed, top: 0.0 });
        }
    }
    let lane_step = LANE_H + anim + retime_extra + LANE_GAP;
    let group_h = |g: &RoleGroup| ROLE_HEADER_H + if g.collapsed { ROLE_COLLAPSED_H + LANE_GAP } else { g.rows as f32 * lane_step };
    let roles_h: f32 = role_groups.iter().map(group_h).sum();
    // in the role view the lanes below the storyline become the bands, plus one free lane to drop onto
    let lanes: Vec<i32> = (min_lane..=max_lane).rev().filter(|l| !role_mode || *l >= 0 || *l == min_lane).collect();
    let mut rows = Vec::new();
    let content_h: f32 = lanes.iter().map(|&l| if l == 0 { SPINE_H } else { LANE_H } + anim + retime_extra + LANE_GAP).sum::<f32>() + roles_h;
    let visible_h = area.height() - header_h;
    // centre the storyline when everything fits, otherwise allow scrolling
    let max_v = (content_h - visible_h).max(0.0);
    app.tl.vscroll = app.tl.vscroll.clamp(-visible_h * 0.3, max_v + 40.0);
    let above_h: f32 = (1..=max_lane).map(|_| LANE_H + anim + retime_extra + LANE_GAP).sum();
    let spine_target = area.top() + header_h + (visible_h * 0.42 - SPINE_H * 0.5).max(above_h.min(visible_h * 0.5));
    let mut y = if content_h <= visible_h { spine_target - above_h } else { area.top() + header_h + 6.0 - app.tl.vscroll };
    for &lane in &lanes {
        if role_mode && lane == min_lane {
            for g in &mut role_groups {
                g.top = y;
                y += group_h(&*g);
            }
        }
        let h = if lane == 0 { SPINE_H } else { LANE_H } + anim + retime_extra;
        rows.push(Row { lane, top: y, height: h });
        y += h + LANE_GAP;
    }
    let row_of = |lane: i32| rows.iter().find(|r| r.lane == lane);
    // (top, height, collapsed) of a below-storyline clip in the role view
    let role_slot = |i: usize| -> Option<(f32, f32, bool)> {
        role_groups.iter().find_map(|g| {
            g.slots.iter().find(|s| s.0 == i).map(|&(_, r)| {
                let top = g.top + ROLE_HEADER_H;
                if g.collapsed {
                    (top, ROLE_COLLAPSED_H, true)
                } else {
                    (top + r as f32 * lane_step, LANE_H + anim + retime_extra, false)
                }
            })
        })
    };
    let lane_at = |py: f32| -> i32 {
        if let (Some(first), Some(last)) = (role_groups.first(), role_groups.last()) {
            if py >= first.top && py < last.top + group_h(last) {
                return min_lane;
            }
        }
        for r in &rows {
            if py >= r.top - LANE_GAP * 0.5 && py < r.top + r.height + LANE_GAP * 0.5 {
                return r.lane;
            }
        }
        if py < rows.first().map(|r| r.top).unwrap_or(0.0) {
            max_lane
        } else {
            min_lane
        }
    };

    // storyline band
    if let Some(r) = row_of(0) {
        let band = Rect::from_min_max(Pos2::new(area.left(), r.top + retime_extra - 2.0), Pos2::new(area.right(), r.top + r.height + 2.0));
        painter.rect_filled(band, CornerRadius::ZERO, Color32::from_rgb(0x1D, 0x1D, 0x20));
    }
    // role bands (click a header to fold it)
    let mut role_headers: Vec<(Rect, AudioRole)> = Vec::new();
    for g in &role_groups {
        let band = Rect::from_min_max(Pos2::new(area.left(), g.top), Pos2::new(area.right(), g.top + group_h(g) - LANE_GAP));
        painter.rect_filled(band, CornerRadius::ZERO, Color32::from_rgb(0x19, 0x1B, 0x1F));
        let head = Rect::from_min_size(band.min, Vec2::new(area.width(), ROLE_HEADER_H));
        painter.rect_filled(head, CornerRadius::ZERO, theme::AUDIO.gamma_multiply(0.18));
        let arrow = if g.collapsed { "▸" } else { "▾" };
        let n = g.slots.len();
        painter.text(
            head.left_center() + Vec2::new(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            format!("{arrow} {}  ({n})", g.role.label()),
            egui::FontId::proportional(11.0),
            theme::TEXT,
        );
        role_headers.push((head, g.role));
    }

    // ----- clips
    let mut hits: Vec<Hit> = Vec::new();
    let sel: Vec<ClipId> = app.selection.clone();
    let spine_row = row_of(0).map(|r| (r.top + retime_extra, r.height - retime_extra)).unwrap_or((area.center().y, SPINE_H));
    // spine first, then connected lanes above/below
    let mut order: Vec<usize> = (0..placed.len()).collect();
    order.sort_by_key(|&i| (placed[i].2 != 0, placed[i].2.abs()));
    for &i in &order {
        let (clip, start, lane, parent) = &placed[i];
        let slot = if role_mode && *lane < 0 { role_slot(i) } else { row_of(*lane).map(|r| (r.top, r.height, false)) };
        let Some((row_top, row_h, collapsed)) = slot else { continue };
        let x0 = geo.x(*start);
        let x1 = geo.x(*start + clip.duration);
        if x1 < area.left() - 2.0 || x0 > area.right() + 2.0 {
            // still record for hit testing consistency (off-screen)
            continue;
        }
        let rect = if collapsed {
            Rect::from_min_max(Pos2::new(x0, row_top), Pos2::new(x1.max(x0 + 2.0), row_top + row_h))
        } else {
            Rect::from_min_max(Pos2::new(x0, row_top + retime_extra), Pos2::new(x1.max(x0 + 2.0), row_top + row_h - anim))
        };
        // connection line to the storyline, at the connection point (Ctrl+Alt+click moves it)
        if parent.is_some() {
            let cx = geo.x(*start + clip.anchor_clamped()).clamp(x0 + 1.0, (x1 - 1.0).max(x0 + 1.0));
            let (sy0, sh) = spine_row;
            let (a, b) = if *lane > 0 { (rect.bottom(), sy0) } else { (sy0 + sh, rect.top()) };
            painter.line_segment([Pos2::new(cx, a), Pos2::new(cx, b)], Stroke::new(1.0_f32, theme::TEXT_DIM));
            painter.circle_filled(Pos2::new(cx, b), 2.5, theme::TEXT_DIM);
        }
        draw_clip(app, &painter, rect, clip, sel.contains(&clip.id));
        draw_duck_overlay(app, &painter, rect, clip, *start);
        let track = app.project.timeline.track(*lane);
        if track.hidden || track.locked {
            let shade = if track.hidden { Color32::from_black_alpha(130) } else { Color32::from_black_alpha(60) };
            painter.rect_filled(rect, CornerRadius::same(4), shade);
            if track.locked && rect.width() > 24.0 {
                painter.text(rect.right_bottom() - Vec2::new(4.0, 3.0), egui::Align2::RIGHT_BOTTOM, "🔒", egui::FontId::proportional(11.0), theme::SNAP);
            }
        }
        // J/L split edits: audio extends past the picture edges.
        let (lead, tail) = clip.audio_split();
        if lead.is_positive() || tail.is_positive() {
            let bar = split_bar_rect(rect, geo.x(*start - lead), geo.x(*start + clip.duration + tail));
            painter.rect_filled(bar, CornerRadius::same(2), theme::AUDIO.gamma_multiply(0.55));
        }
        if app.tl.show_animation && !collapsed {
            let arect = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() + 1.0), Pos2::new(rect.right(), rect.bottom() + anim - 1.0));
            let sel = app.tl.selected_key.filter(|(id, _)| *id == clip.id).map(|(_, t)| t);
            draw_anim_lane(&painter, arect, clip, &geo, *start, sel);
        }
        hits.push(Hit { id: clip.id, rect, start: *start, lane: *lane });
    }

    // ----- transitions (spine edit points)
    let mut tr_hits: Vec<(Rect, ClipId)> = Vec::new();
    if let Some(r) = row_of(0) {
        let starts = app.project.timeline.spine_starts();
        for (i, c) in app.project.timeline.spine.iter().enumerate() {
            let end = starts[i] + c.duration;
            let mut draw = |tr: &nle_core::Transition, centre: Time, align_start: bool| {
                let w = (tr.duration.as_secs_f64() as f32 * geo.pps).max(10.0);
                let x = geo.x(centre);
                let x0 = if align_start { x } else { x - w * 0.5 };
                let rect = Rect::from_min_size(Pos2::new(x0, r.top + 2.0), Vec2::new(w, (r.height - anim) * 0.42));
                painter.rect_filled(rect, CornerRadius::same(4), Color32::from_rgba_unmultiplied(20, 20, 24, 225));
                painter.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0_f32, Color32::from_gray(150)), StrokeKind::Inside);
                painter.line_segment([rect.left_top(), rect.right_bottom()], Stroke::new(0.7_f32, Color32::from_gray(110)));
                painter.line_segment([rect.left_bottom(), rect.right_top()], Stroke::new(0.7_f32, Color32::from_gray(110)));
                if w > 60.0 {
                    painter.text(rect.center(), egui::Align2::CENTER_CENTER, tr.kind.label(), egui::FontId::proportional(10.0), theme::TEXT);
                }
                tr_hits.push((rect, c.id));
            };
            if let Some(tr) = &c.transition_out {
                draw(tr, end, false);
            }
            if let Some(tr) = &c.transition_in {
                match i.checked_sub(1).map(|j| &app.project.timeline.spine[j]).filter(|prev| !prev.is_gap()) {
                    Some(prev) if prev.transition_out.is_some() => {} // the previous clip's wins
                    Some(_) => draw(tr, starts[i], false),
                    None => draw(tr, starts[i], true),
                }
            }
        }
    }

    // ----- ruler & captions row
    let ruler = Rect::from_min_size(area.min, Vec2::new(area.width(), RULER_H));
    draw_ruler(&painter, ruler, &geo, rate, &app.project.timeline.markers);
    let cap_rect = has_captions.then(|| {
        let cr = Rect::from_min_size(Pos2::new(area.left(), ruler.bottom()), Vec2::new(area.width(), CAPTIONS_H));
        draw_captions_row(&painter, cr, &geo, app);
        cr
    });

    // ----- range selection
    if let Some((s, e)) = app.tl.range {
        let r = Rect::from_min_max(Pos2::new(geo.x(s), area.top() + header_h), Pos2::new(geo.x(e), area.bottom()));
        painter.rect_filled(r, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(255, 214, 10, 28));
        painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.5_f32, theme::SELECTION), StrokeKind::Inside);
        painter.text(r.left_top() + Vec2::new(4.0, 4.0), egui::Align2::LEFT_TOP, crate::widgets::secs_label(e - s), egui::FontId::proportional(11.0), theme::SELECTION);
    }
    // a lone In or Out mark (I / O)
    for (t, label, align) in [(app.source.record.mark_in, "IN", egui::Align2::LEFT_TOP), (app.source.record.mark_out, "OUT", egui::Align2::RIGHT_TOP)] {
        if let Some(t) = t {
            let x = geo.x(t);
            painter.line_segment([Pos2::new(x, area.top() + header_h), Pos2::new(x, area.bottom())], Stroke::new(1.5_f32, theme::SELECTION));
            let dx = if label == "IN" { 4.0 } else { -4.0 };
            painter.text(Pos2::new(x + dx, area.top() + header_h + 4.0), align, label, egui::FontId::proportional(10.0), theme::SELECTION);
        }
    }

    // ----- interaction
    let pointer = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
    let mods = ui.input(|i| i.modifiers);
    let snap_threshold = Time::from_secs_f64((8.0 / geo.pps) as f64);
    let hit_at = |p: Pos2| hits.iter().rev().find(|h| h.rect.contains(p));
    let mut snap_line: Option<Time> = None;
    app.tl.hovered_clip = resp.hover_pos().and_then(&hit_at).map(|h| h.id);
    app.tl.snap_extra = audio_peak_snaps(app, &placed, (geo.t(area.left()), geo.t(area.right())));

    // skimming
    app.skimmer = None;
    if let Some(p) = resp.hover_pos() {
        if app.skimming && app.tl.drag.is_none() && p.y > area.top() + header_h {
            app.skimmer = Some(geo.t(p.x).snap_to_frame(rate));
        }
    }

    // blade hover indicator
    if app.tool == Tool::Blade {
        if let Some(h) = pointer.and_then(&hit_at) {
            let t = geo.t(pointer.map(|p| p.x).unwrap_or(0.0)).snap_to_frame(rate);
            let x = geo.x(t);
            painter.line_segment([Pos2::new(x, h.rect.top()), Pos2::new(x, h.rect.bottom())], Stroke::new(2.0_f32, theme::PLAYHEAD));
        }
    }

    // retime bars & interactive 10%–800% velocity curve lane (Ctrl+R)
    let mut retime_click: Option<(ClipId, usize, Rect)> = None;
    let mut vel_handles: Vec<(ClipId, usize, Pos2, Rect)> = Vec::new();
    if app.tl.show_retime {
        for h in &hits {
            let Some(c) = app.project.timeline.clip(h.id) else { continue };
            // folded role bands have no room for a retime lane
            if h.rect.height() < ROLE_COLLAPSED_H + 1.0 || (c.retime.is_none() && !sel.contains(&h.id)) {
                continue;
            }
            if !matches!(c.kind, ClipKind::Asset { .. } | ClipKind::Compound(_)) {
                continue;
            }
            let r = c.retime.clone().unwrap_or_else(|| Retime::uniform(c.duration, 1.0));
            let lane_r = Rect::from_min_max(Pos2::new(h.rect.left(), h.rect.top() - RETIME_H - 1.0), Pos2::new(h.rect.right(), h.rect.top() - 1.0));
            if r.has_velocity_curve() || (r.segments.len() == 1 && r.segments[0].speed >= 0.0) {
                draw_velocity_lane(&painter, lane_r, c, &r, &geo, h.start, &mut vel_handles);
                if resp.clicked() && pointer.is_some_and(|p| lane_r.contains(p)) {
                    let near_handle = vel_handles.iter().any(|(hid, _, pos, _)| *hid == h.id && pointer.is_some_and(|p| p.distance(*pos) < 8.0));
                    if !near_handle {
                        retime_click = Some((h.id, 0, lane_r));
                    }
                }
            } else {
                let starts = r.segment_starts();
                for (i, seg) in r.segments.iter().enumerate() {
                    let x0 = geo.x(h.start + starts[i]);
                    let x1 = geo.x(h.start + starts[i] + seg.duration);
                    let br = Rect::from_min_max(Pos2::new(x0, lane_r.top() + RETIME_H * 0.4), Pos2::new(x1, lane_r.bottom()));
                    let (col, label) = retime_style(seg.speed);
                    painter.rect_filled(br, CornerRadius::same(3), col);
                    if seg.ramp > Time::ZERO {
                        let rx = geo.x(h.start + starts[i] + seg.ramp);
                        painter.line_segment([Pos2::new(x0, br.bottom()), Pos2::new(rx, br.top())], Stroke::new(1.0_f32, Color32::from_black_alpha(160)));
                    }
                    if br.width() > 70.0 {
                        painter.text(br.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(10.0), Color32::BLACK);
                    }
                    if resp.clicked() && pointer.is_some_and(|p| br.contains(p)) {
                        retime_click = Some((h.id, i, br));
                    }
                }
            }
        }
    }

    // ----- drags
    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or_default();
        let t = geo.t(origin.x);
        app.tl.drag = if origin.y < ruler.bottom() {
            Some(Drag::Scrub)
        } else if let Some((index, edge)) = cap_rect.and_then(|cr| caption_hit(app, cr, &geo, origin)) {
            app.captions.selected_seg = Some(index);
            app.selection.clear();
            let base = Box::new(app.project.timeline.captions.clone());
            Some(match edge {
                Some(start_edge) => Drag::CaptionTrim { index, start_edge, origin: t, base },
                None => Drag::CaptionSlide { index, origin: t, base },
            })
        } else if let Some((id, idx, _, lr)) = vel_handles.iter().rev().find(|(_, _, pos, _)| origin.distance(*pos) <= 9.0) {
            Some(Drag::VelocityKey { id: *id, index: *idx, lane_top: lr.top(), lane_height: lr.height() })
        } else if let Some(k) = anim_key_hit(app, &hits, origin, &geo, anim) {
            if let Drag::Key { id, key_time, .. } = k {
                app.tl.selected_key = Some((id, key_time));
            }
            Some(k)
        } else if let Some(h) = hit_at(origin) {
            let locked = app.project.timeline.is_locked(h.lane) && app.tool != Tool::Range;
            let near_start = (origin.x - h.rect.left()).abs() < EDGE_PX;
            let near_end = (origin.x - h.rect.right()).abs() < EDGE_PX;
            let roll = if matches!(app.tool, Tool::Trim) && !mods.alt && h.lane == 0 { roll_target(&app.project.timeline, h, near_start, near_end) } else { None };
            match app.tool {
                _ if locked => {
                    app.status(format!("{} is locked", track_headers::lane_name(h.lane)));
                    None
                }
                Tool::Select | Tool::Trim if (near_start || near_end) && mods.alt => Some(Drag::Split { id: h.id, start_edge: near_start && !near_end, origin: t }),
                Tool::Trim if roll.is_some() => roll.map(|(left, edit)| Drag::Roll { left, edit, origin: t }),
                Tool::Select | Tool::Trim if near_start || near_end => Some(Drag::Trim { id: h.id, start_edge: near_start && !near_end, origin: t }),
                Tool::Select => {
                    if !app.selection.contains(&h.id) {
                        app.selection = vec![h.id];
                    }
                    Some(Drag::Move { id: h.id, grab: t - h.start })
                }
                Tool::Position => {
                    if !app.selection.contains(&h.id) {
                        app.selection = vec![h.id];
                    }
                    Some(Drag::Position { id: h.id, grab: t - h.start })
                }
                Tool::Trim if mods.alt => Some(Drag::Slide { id: h.id, origin: t }),
                Tool::Trim => Some(Drag::Slip { id: h.id, origin: t }),
                Tool::Range => Some(Drag::Range { anchor: t }),
                Tool::Blade => None,
            }
        } else if matches!(app.tool, Tool::Range | Tool::Select) && !cap_rect.is_some_and(|cr| cr.contains(origin)) {
            Some(Drag::Range { anchor: t })
        } else {
            None
        };
        if matches!(app.tl.drag, Some(Drag::Scrub)) {
            app.stop();
        }
    }

    if let (Some(d), Some(p)) = (app.tl.drag.clone(), pointer) {
        let raw_t = geo.t(p.x);
        let tl = &app.project.timeline;
        match d {
            Drag::Scrub => {
                let (t, s) = tl.snap(raw_t, snap_threshold, &app.tl.snap_extra, None);
                app.scrub_to(t.snap_to_frame(rate));
                if s {
                    snap_line = Some(t);
                }
            }
            Drag::Move { id, grab } | Drag::Position { id, grab } => {
                if let Some(h) = hits.iter().find(|h| h.id == id) {
                    let mut start = raw_t - grab;
                    let dur = tl.clip(id).map(|c| c.duration).unwrap_or_default();
                    let (s1, ok1) = tl.snap(start, snap_threshold, &app.tl.snap_targets(app.playhead), Some(id));
                    if ok1 {
                        start = s1;
                        snap_line = Some(s1);
                    } else {
                        let (e1, ok2) = tl.snap(start + dur, snap_threshold, &app.tl.snap_targets(app.playhead), Some(id));
                        if ok2 {
                            start = e1 - dur;
                            snap_line = Some(e1);
                        }
                    }
                    let mut lane = lane_at(p.y);
                    if role_mode && lane < 0 && h.lane < 0 {
                        // inside the role bands a clip keeps its own lane
                        lane = h.lane;
                    }
                    let ghost_row = row_of(lane).map(|r| (r.top, r.height - anim)).unwrap_or((h.rect.top(), h.rect.height()));
                    let gx = geo.x(start.max(Time::ZERO));
                    let ghost = Rect::from_min_size(Pos2::new(gx, ghost_row.0), Vec2::new(h.rect.width(), ghost_row.1));
                    painter.rect_filled(ghost, CornerRadius::same(4), Color32::from_rgba_unmultiplied(255, 255, 255, 30));
                    painter.rect_stroke(ghost, CornerRadius::same(4), Stroke::new(1.5_f32, Color32::WHITE), StrokeKind::Inside);
                    if lane == 0 && h.lane == 0 && matches!(d, Drag::Move { .. }) {
                        // magnetic reorder: show the insertion point
                        let idx = tl.drop_index(raw_t);
                        let ix = geo.x(tl.spine_start(idx));
                        painter.line_segment([Pos2::new(ix, ghost.top() - 4.0), Pos2::new(ix, ghost.bottom() + 4.0)], Stroke::new(3.0_f32, theme::VIDEO));
                    }
                }
            }
            Drag::Trim { id, start_edge, origin } => {
                if let Some(h) = hits.iter().find(|h| h.id == id) {
                    let edge = if start_edge { h.start } else { h.start + tl.clip(id).map(|c| c.duration).unwrap_or_default() };
                    let (mut target, s) = tl.snap(edge + (raw_t - origin), snap_threshold, &app.tl.snap_targets(app.playhead), Some(id));
                    target = target.snap_to_frame(rate);
                    if s {
                        snap_line = Some(target);
                    }
                    let x = geo.x(target);
                    painter.line_segment([Pos2::new(x, h.rect.top() - 6.0), Pos2::new(x, h.rect.bottom() + 6.0)], Stroke::new(3.0_f32, theme::SELECTION));
                    let d = target - edge;
                    painter.text(
                        Pos2::new(x, h.rect.top() - 8.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{:+.2}s", d.as_secs_f64()),
                        egui::FontId::proportional(11.0),
                        theme::SELECTION,
                    );
                    app.skimmer = Some(target);
                }
            }
            Drag::Slip { id, origin } | Drag::Slide { id, origin } => {
                if let Some(h) = hits.iter().find(|h| h.id == id) {
                    let d = (raw_t - origin).snap_to_frame(rate);
                    let what = if matches!(app.tl.drag, Some(Drag::Slip { .. })) { "Slip" } else { "Slide" };
                    painter.rect_stroke(h.rect, CornerRadius::same(4), Stroke::new(2.0_f32, theme::SNAP), StrokeKind::Inside);
                    painter.text(
                        h.rect.center_top() - Vec2::new(0.0, 4.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{what} {:+.2}s", d.as_secs_f64()),
                        egui::FontId::proportional(11.0),
                        theme::SNAP,
                    );
                }
            }
            Drag::Roll { edit, origin, .. } => {
                let target = roll_drag_target(app, edit, origin, raw_t, snap_threshold);
                if target != (edit + (raw_t - origin)).snap_to_frame(rate) {
                    snap_line = Some(target);
                }
                let (top, bottom) = hits.iter().filter(|h| h.lane == 0).fold((f32::MAX, f32::MIN), |(a, b), h| (a.min(h.rect.top()), b.max(h.rect.bottom())));
                if top < bottom {
                    let x = geo.x(target);
                    painter.line_segment([Pos2::new(x, top - 6.0), Pos2::new(x, bottom + 6.0)], Stroke::new(3.0_f32, theme::SNAP));
                    painter.text(
                        Pos2::new(x, top - 8.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("Roll {:+.2}s", (target - edit).as_secs_f64()),
                        egui::FontId::proportional(11.0),
                        theme::SNAP,
                    );
                }
                app.skimmer = Some(target);
            }
            Drag::Split { id, start_edge, origin } => {
                if let (Some(h), Some(c)) = (hits.iter().find(|h| h.id == id), tl.clip(id)) {
                    let (lead, tail) = split_preview(c, start_edge, (raw_t - origin).snap_to_frame(rate));
                    let bar = split_bar_rect(h.rect, geo.x(h.start - lead), geo.x(h.start + c.duration + tail));
                    painter.rect_filled(bar, CornerRadius::same(2), theme::AUDIO.gamma_multiply(0.75));
                    let label = if start_edge { format!("J-cut {:.2}s", lead.as_secs_f64()) } else { format!("L-cut {:.2}s", tail.as_secs_f64()) };
                    let x = if start_edge { bar.left() } else { bar.right() };
                    painter.text(Pos2::new(x, h.rect.top() - 4.0), egui::Align2::CENTER_BOTTOM, label, egui::FontId::proportional(11.0), theme::AUDIO);
                }
            }
            Drag::Range { anchor } => {
                let (t, s) = tl.snap(raw_t, snap_threshold, &app.tl.snap_targets(app.playhead), None);
                if s {
                    snap_line = Some(t);
                }
                let (a, b) = if t < anchor { (t, anchor) } else { (anchor, t) };
                app.tl.range = Some((a.snap_to_frame(rate), b.snap_to_frame(rate)));
            }
            Drag::CaptionTrim { index, .. } | Drag::CaptionSlide { index, .. } => {
                if let (Some((tr, line)), Some(cr)) = (caption_drag_result(app, &d, raw_t, snap_threshold), cap_rect) {
                    if line.is_some() {
                        snap_line = line;
                    }
                    if let Some(seg) = tr.segments.get(index) {
                        let off = tr.sync_offset();
                        let (s, e) = (seg.start + off, seg.end + off);
                        if let Drag::CaptionTrim { start_edge, .. } = d {
                            let edge = if start_edge { s } else { e };
                            caption_edge_bar(&painter, cr, geo.x(edge));
                            app.skimmer = Some(edge);
                        } else {
                            let r = Rect::from_min_max(Pos2::new(geo.x(s), cr.top() + 2.0), Pos2::new(geo.x(e), cr.bottom() - 2.0));
                            painter.rect_stroke(r, CornerRadius::same(3), Stroke::new(2.0_f32, theme::SNAP), StrokeKind::Inside);
                        }
                        caption_tooltip(&painter, cr, geo.x(s), geo.x(e), s, e - s);
                    }
                    if tr != app.project.timeline.captions {
                        app.exec_key(Command::SetCaptions { captions: Box::new(tr) }, Some(CAPTION_BAR_KEY));
                    }
                }
                let resize = matches!(d, Drag::CaptionTrim { .. });
                ui.ctx().set_cursor_icon(if resize { egui::CursorIcon::ResizeHorizontal } else { egui::CursorIcon::Grabbing });
            }
            Drag::Key { .. } => {
                let x = p.x;
                painter.line_segment([Pos2::new(x, area.top() + RULER_H), Pos2::new(x, area.bottom())], Stroke::new(1.0_f32, theme::SNAP));
            }
            Drag::VelocityKey { id, index, lane_top, lane_height } => {
                if let Some(c) = app.project.timeline.clip(id).cloned() {
                    let clip_start = app.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
                    let mut r = c.retime.clone().unwrap_or_else(|| Retime::uniform(c.duration, 1.0));
                    let y_norm = (1.0 - ((p.y - (lane_top + 4.0)) / (lane_height - 8.0).max(4.0))).clamp(0.0, 1.0);
                    let new_speed = vel_norm_to_speed(y_norm);
                    let local_t = (raw_t - clip_start).clamp(Time::ZERO, c.duration);
                    if let Some(cv) = r.velocity_curve.as_mut() {
                        let key_count = cv.keys.len();
                        if let Some(k) = cv.keys.get_mut(index) {
                            let is_endpoint = index == 0 || index + 1 == key_count;
                            k.value = new_speed;
                            if !is_endpoint {
                                k.time = local_t;
                            }
                        }
                        cv.keys.sort_by_key(|k| k.time);
                    } else {
                        r.set_velocity_point(local_t, new_speed);
                    }
                    r.blend = FrameBlendMode::OpticalFlow;
                    app.exec_key(Command::SetRetime { id, retime: Some(r) }, Some(drag_key(0x52, id)));
                    painter.text(
                        Pos2::new(p.x, lane_top - 4.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{:.0}%", new_speed * 100.0),
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(0x40, 0xD2, 0xFF),
                    );
                }
            }
        }
    }

    if app.hold_connections && matches!(app.tl.drag, Some(Drag::Move { .. } | Drag::Position { .. } | Drag::Trim { .. })) {
        painter.text(
            Pos2::new(area.left() + 8.0, area.top() + header_h + 4.0),
            egui::Align2::LEFT_TOP,
            "` held: connected clips stay where they are",
            egui::FontId::proportional(11.0),
            theme::SNAP,
        );
    }

    if resp.drag_stopped() {
        if let (Some(d), Some(p)) = (app.tl.drag.take(), pointer) {
            finish_drag(app, d, p, &geo, &hits, &lane_at, snap_threshold);
        }
        app.history.seal();
    }

    // ----- clicks
    if resp.clicked() {
        if let Some(p) = pointer {
            let t = geo.t(p.x).snap_to_frame(rate);
            let key_click = anim_key_hit(app, &hits, p, &geo, anim);
            app.tl.selected_key = None;
            if let Some(Drag::Key { id, key_time, .. }) = key_click {
                app.tl.selected_key = Some((id, key_time));
                app.selection = vec![id];
                let start = app.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
                app.seek(start + key_time);
            } else if let Some((id, seg, _)) = retime_click {
                app.selection = vec![id];
                ui.memory_mut(|m| m.data.insert_temp(egui::Id::new("retime-seg"), (id, seg, p)));
                ui.memory_mut(|m| m.open_popup(egui::Id::new("retime-popup")));
            } else if p.y < ruler.bottom() {
                app.seek(t);
            } else if let Some(cr) = cap_rect.filter(|cr| cr.contains(p)) {
                let tr = &app.project.timeline.captions;
                let off = tr.sync_offset();
                let hit = caption_hit(app, cr, &geo, p).map(|(i, _)| i);
                let target = hit.and_then(|i| tr.segments.get(i)).map_or(t, |s| s.start + off);
                app.captions.selected_seg = hit;
                if hit.is_some() {
                    app.selection.clear();
                }
                app.seek(target);
            } else if let Some(&(_, role)) = role_headers.iter().find(|(r, _)| r.contains(p)) {
                let folded = &mut app.tl.collapsed_roles;
                match folded.iter().position(|r| *r == role) {
                    Some(i) => {
                        folded.remove(i);
                    }
                    None => folded.push(role),
                }
            } else if let Some((_, id)) = tr_hits.iter().find(|(r, _)| r.contains(p)) {
                app.selection = vec![*id];
                app.inspector_tab = crate::app::InspectorTab::TitlesTransitions;
            } else if let Some(h) = hit_at(p) {
                let id = h.id;
                let connected = h.lane != 0;
                app.captions.selected_seg = None;
                match app.tool {
                    // Ctrl+Alt+click: move the connection point under the pointer
                    _ if mods.command && mods.alt && connected => {
                        app.selection = vec![id];
                        app.exec(Command::SetConnectionPoint { id, at: t });
                    }
                    Tool::Blade => {
                        if mods.shift {
                            app.exec(Command::BladeAll { at: t });
                        } else {
                            app.exec(Command::BladeClip { id, at: t });
                        }
                    }
                    _ => {
                        if mods.command || mods.shift {
                            if let Some(i) = app.selection.iter().position(|s| *s == id) {
                                app.selection.remove(i);
                            } else {
                                app.selection.push(id);
                            }
                        } else {
                            app.selection = vec![id];
                        }
                    }
                }
            } else {
                app.selection.clear();
                app.captions.selected_seg = None;
                app.tl.range = None;
                app.seek(t);
            }
        }
    }
    if resp.double_clicked() {
        if let Some(p) = pointer {
            if let Some(cr) = cap_rect.filter(|cr| cr.contains(p)) {
                let t = geo.t(p.x).snap_to_frame(rate);
                let tr = &app.project.timeline.captions;
                let hit = caption_hit(app, cr, &geo, p).map(|(i, _)| i);
                let target = hit.and_then(|i| tr.segments.get(i)).map_or(t, |s| s.start + tr.sync_offset());
                app.captions.selected_seg = hit.or(app.captions.selected_seg);
                app.seek(target);
                app.show_captions = true;
            } else if let Some(h) = hit_at(p) {
                let t = geo.t(p.x).snap_to_frame(rate);
                let id = h.id;
                if app.project.timeline.clip(id).is_some_and(|c| matches!(c.kind, ClipKind::Compound(_))) {
                    // open the compound clip / storyline (breadcrumb or Ctrl+[ steps back out)
                    app.seek(t);
                    app.step_into(id);
                } else {
                    app.selection = vec![id];
                    app.seek(t);
                }
            }
        }
    }
    if resp.secondary_clicked() {
        app.tl.key_menu = match pointer.and_then(|p| anim_key_hit(app, &hits, p, &geo, anim)) {
            Some(Drag::Key { id, key_time, .. }) => Some((id, key_time)),
            _ => None,
        };
        if app.tl.key_menu.is_some() {
            app.tl.selected_key = app.tl.key_menu;
        }
        app.tl.context_clip = pointer.and_then(&hit_at).map(|h| h.id);
        if let Some(id) = app.tl.context_clip {
            if !app.selection.contains(&id) {
                app.selection = vec![id];
            }
        }
    }
    let ctx_clip = app.tl.context_clip;
    resp.context_menu(|ui| match app.tl.key_menu {
        Some(key) => key_context_menu(app, ui, key),
        None => context_menu(app, ui, ctx_clip),
    });
    retime_popup(app, ui, &resp);

    // ----- drop from the browser
    if let Some(asset) = resp.dnd_hover_payload::<AssetId>() {
        if let Some(p) = pointer {
            let (lane, at) = drop_target(app, p, &geo, &lane_at, snap_threshold);
            let x = geo.x(at);
            let (top, h) = row_of(lane).map(|r| (r.top, r.height - anim)).unwrap_or((area.top() + header_h, SPINE_H));
            let len = app.project.asset(*asset).map(|a| a.duration).unwrap_or(Time::from_secs(5));
            let ghost = Rect::from_min_size(Pos2::new(x, top), Vec2::new(len.as_secs_f64() as f32 * geo.pps, h));
            painter.rect_filled(ghost, CornerRadius::same(4), theme::VIDEO.gamma_multiply(0.25));
            painter.line_segment([Pos2::new(x, top - 6.0), Pos2::new(x, top + h + 6.0)], Stroke::new(3.0_f32, theme::VIDEO));
        }
    }
    if let Some(asset) = resp.dnd_release_payload::<AssetId>() {
        if let Some(p) = pointer {
            let (lane, at) = drop_target(app, p, &geo, &lane_at, snap_threshold);
            drop_asset(app, *asset, lane, at);
        }
    }
    if let Some(preset_name) = resp.dnd_hover_payload::<String>() {
        if let Some(p) = pointer {
            if let Some(h) = hit_at(p) {
                painter.rect_stroke(h.rect, CornerRadius::same(4), Stroke::new(2.5_f32, theme::SNAP), StrokeKind::Inside);
                painter.text(
                    h.rect.center_top() - Vec2::new(0.0, 4.0),
                    egui::Align2::CENTER_BOTTOM,
                    format!("⚡ Drop preset: {preset_name}"),
                    egui::FontId::proportional(11.0),
                    theme::SNAP,
                );
            }
        }
    }
    if let Some(preset_name) = resp.dnd_release_payload::<String>() {
        if let Some(p) = pointer {
            if let Some(h) = hit_at(p) {
                app.apply_montage_preset_by_name(h.id, &preset_name);
            }
        }
    }

    // ----- snap indicator, skimmer, playhead
    if let Some(t) = snap_line {
        let x = geo.x(t);
        painter.line_segment([Pos2::new(x, area.top()), Pos2::new(x, area.bottom())], Stroke::new(1.5_f32, theme::SNAP));
    }
    if let Some(s) = app.skimmer {
        if !app.playback.playing {
            let x = geo.x(s);
            painter.line_segment([Pos2::new(x, ruler.top()), Pos2::new(x, area.bottom())], Stroke::new(1.0_f32, theme::SKIMMER));
        }
    }
    let px = geo.x(app.playhead);
    painter.line_segment([Pos2::new(px, ruler.top() + 6.0), Pos2::new(px, area.bottom())], Stroke::new(1.5_f32, theme::PLAYHEAD));
    let tri = vec![Pos2::new(px - 6.0, ruler.top() + 2.0), Pos2::new(px + 6.0, ruler.top() + 2.0), Pos2::new(px, ruler.top() + 10.0)];
    painter.add(egui::Shape::convex_polygon(tri, theme::PLAYHEAD, Stroke::NONE));

    // cursor feedback
    app.tl.hover_captions = resp.hover_pos().is_some_and(|p| cap_rect.is_some_and(|cr| cr.contains(p)));
    if let (Some(p), Some(cr), None) = (resp.hover_pos(), cap_rect, &app.tl.drag) {
        if let Some((i, edge)) = caption_hit(app, cr, &geo, p) {
            let tr = &app.project.timeline.captions;
            let off = tr.sync_offset();
            if let Some(seg) = tr.segments.get(i) {
                let (s, e) = (seg.start + off, seg.end + off);
                if let Some(start_edge) = edge {
                    caption_edge_bar(&painter, cr, geo.x(if start_edge { s } else { e }));
                    caption_tooltip(&painter, cr, geo.x(s), geo.x(e), s, e - s);
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                } else {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
            }
        }
    }
    if let Some(p) = resp.hover_pos() {
        if let Some(h) = hit_at(p) {
            let near_edge = (p.x - h.rect.left()).abs() < EDGE_PX || (p.x - h.rect.right()).abs() < EDGE_PX;
            let icon = match app.tool {
                Tool::Blade => egui::CursorIcon::Crosshair,
                Tool::Position => egui::CursorIcon::Move,
                _ if near_edge => egui::CursorIcon::ResizeColumn,
                Tool::Trim => egui::CursorIcon::ResizeHorizontal,
                Tool::Range => egui::CursorIcon::Text,
                Tool::Select => egui::CursorIcon::Default,
            };
            ui.ctx().set_cursor_icon(icon);
        }
    }
    // in the role view the negative lanes are folded into the bands
    let strips: Vec<(i32, Rect)> = rows
        .iter()
        .filter(|r| !role_mode || r.lane >= 0)
        .map(|r| (r.lane, Rect::from_min_max(Pos2::new(column.left(), r.top + retime_extra), Pos2::new(column.right(), r.top + r.height - anim))))
        .collect();
    track_headers::show(app, ui, column, area.top() + header_h, &strips);
    let _ = frame;
}

// ---------------------------------------------------------------------------

fn toolbar(app: &mut App, ui: &mut egui::Ui) {
    egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.horizontal(|ui| {
            for t in Tool::ALL {
                let r = ui.selectable_label(app.tool == t, egui::RichText::new(t.icon()).size(15.0)).on_hover_text(app.keymap.tip(t.label(), t.action()));
                if r.clicked() {
                    app.tool = t;
                }
            }
            ui.separator();
            let asset = app.browser.selected;
            for (mode, label, tip) in [
                (EditMode::Connect, "⤒", "Connect to primary storyline (Q)"),
                (EditMode::Insert, "⤓", "Insert (W)"),
                (EditMode::Append, "⇥", "Append to storyline (E)"),
                (EditMode::Overwrite, "⬇", "Overwrite (D)"),
            ] {
                if ui.add_enabled(asset.is_some(), egui::Button::new(egui::RichText::new(label).size(14.0))).on_hover_text(tip).clicked() {
                    if let Some(a) = asset {
                        app.add_asset_to_timeline(a, mode);
                    }
                }
            }
            ui.separator();
            if ui.add_enabled(app.history.can_undo(), egui::Button::new("↶")).on_hover_text(app.keymap.tip("Undo", Action::Undo)).clicked() {
                app.undo();
            }
            if ui.add_enabled(app.history.can_redo(), egui::Button::new("↷")).on_hover_text(app.keymap.tip("Redo", Action::Redo)).clicked() {
                app.redo();
            }
            ui.separator();
            if app.focus.is_empty() {
                timeline_switcher(app, ui);
            } else {
                breadcrumb(app, ui);
            }
            let rate = app.project.timeline.format.rate;
            ui.label(egui::RichText::new(nle_core::format_timecode(app.project.timeline.extent(), rate)).monospace().color(theme::TEXT_DIM));
            if !app.selection.is_empty() {
                ui.label(egui::RichText::new(format!("{} selected", app.selection.len())).color(theme::SELECTION));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut z = app.tl.pps.ln();
                if ui
                    .add(egui::Slider::new(&mut z, 0.0..=8.3).show_value(false))
                    .on_hover_text(format!("Zoom (Ctrl+scroll, {} to fit)", app.keymap.hint(Action::ZoomToFit)))
                    .changed()
                {
                    app.tl.pps = z.exp();
                }
                if ui.small_button("Fit").clicked() {
                    app.tl.fit_pending = true;
                }
                ui.separator();
                let snap = app.project.timeline.snapping;
                if ui
                    .add_enabled(snap, egui::SelectableLabel::new(snap && app.tl.snap_audio_peaks, "♪ Peaks"))
                    .on_hover_text("Also snap to loud audio transients (gunshots, hits) while dragging")
                    .on_disabled_hover_text("Turn snapping on to snap to audio peaks")
                    .clicked()
                {
                    app.tl.snap_audio_peaks = !app.tl.snap_audio_peaks;
                }
                let badge = egui::RichText::new(if snap { "🧲 Snap: ON" } else { "🧲 Snap: OFF" }).strong().color(if snap { theme::SNAP } else { theme::TEXT_DIM });
                if ui.selectable_label(snap, badge).on_hover_text(format!("Magnetic snapping ({})", app.keymap.hint(Action::ToggleSnapping))).clicked() {
                    app.project.timeline.snapping = !snap;
                }
                if ui.selectable_label(app.skimming, "⇋ Skim").on_hover_text(format!("Skimming ({})", app.keymap.hint(Action::ToggleSkimming))).clicked() {
                    app.skimming = !app.skimming;
                }
                ui.toggle_value(&mut app.show_captions, "CC Captions").on_hover_text("Auto captions & Whisper transcription");
                let lanes_tip = format!("Group the audio below the storyline into Dialogue / Music / SFX / General lanes ({})", app.keymap.hint(Action::ToggleAudioLanes));
                ui.toggle_value(&mut app.tl.audio_lanes, "Audio Lanes").on_hover_text(lanes_tip);
                ui.toggle_value(&mut app.tl.show_retime, "Retime").on_hover_text(app.keymap.tip("Retime editor", Action::ToggleRetime));
                ui.toggle_value(&mut app.tl.show_animation, "Animation").on_hover_text(app.keymap.tip("Video animation lane", Action::ToggleAnimation));
            });
        });
    });
}

/// The open timeline's name; a menu opens the project's other timelines.
fn timeline_switcher(app: &mut App, ui: &mut egui::Ui) {
    let active = app.project.active_timeline;
    let names: Vec<String> = app.project.all_timelines().map(|t| t.name.clone()).collect();
    let mut open = None;
    let mut new = false;
    let title = egui::RichText::new(format!("{} ⏷", app.project.timeline.name)).strong();
    let tip = format!("Timelines in this project ({} / {} to cycle)", app.keymap.hint(Action::PrevTimeline), app.keymap.hint(Action::NextTimeline));
    ui.menu_button(title, |ui| {
        for (i, n) in names.iter().enumerate() {
            if ui.radio(i == active, n).clicked() {
                open = Some(i);
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.button(app.keymap.tip("New Timeline", Action::NewTimeline)).clicked() {
            new = true;
            ui.close_menu();
        }
    })
    .response
    .on_hover_text(tip);
    if let Some(i) = open {
        app.switch_timeline(i);
    }
    if new {
        app.new_timeline();
    }
}

/// "Project ▸ Compound ▸ Storyline" while stepped into containers; each
/// crumb steps back out to that level.
fn breadcrumb(app: &mut App, ui: &mut egui::Ui) {
    let names: Vec<String> = app.focus.levels().iter().map(|f| f.name.clone()).collect();
    let mut out: Option<usize> = None;
    let back_tip = format!("Back out one level ({})", app.keymap.hint(Action::StepOut));
    if ui.button("⬅").on_hover_text(back_tip).clicked() {
        out = Some(1);
    }
    if ui.link(app.root_timeline_name().to_owned()).clicked() {
        out = Some(names.len());
    }
    for (i, n) in names.iter().enumerate() {
        ui.label(egui::RichText::new("▸").color(theme::TEXT_DIM));
        if i + 1 == names.len() {
            ui.label(egui::RichText::new(n).strong().color(theme::SELECTION));
        } else if ui.link(n).clicked() {
            out = Some(names.len() - 1 - i);
        }
    }
    if let Some(levels) = out {
        app.step_out(levels);
    }
}

fn retime_style(speed: f64) -> (Color32, String) {
    if speed == 0.0 {
        (Color32::from_rgb(0xFF, 0x45, 0x3A), "Hold".into())
    } else if speed < 0.0 {
        (Color32::from_rgb(0xBF, 0x5A, 0xF2), format!("Reverse ({:.0}%)", -speed * 100.0))
    } else if (speed - 1.0).abs() < 1e-3 {
        (Color32::from_rgb(0x30, 0xD1, 0x58), "Normal (100%)".into())
    } else if speed < 1.0 {
        (Color32::from_rgb(0xFF, 0x9F, 0x0A), format!("Slow ({:.0}%)", speed * 100.0))
    } else {
        (Color32::from_rgb(0x0A, 0x84, 0xFF), format!("Fast ({:.0}%)", speed * 100.0))
    }
}

fn draw_ruler(p: &egui::Painter, r: Rect, geo: &Geo, rate: nle_core::FrameRate, markers: &[Marker]) {
    let p = p.with_clip_rect(r.intersect(p.clip_rect()));
    p.rect_filled(r, CornerRadius::ZERO, theme::PANEL);
    p.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0_f32, theme::BORDER));
    let fd = rate.frame_duration().as_secs_f64();
    let steps = [fd, 2.0 * fd, 5.0 * fd, 10.0 * fd, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];
    let major = steps.iter().copied().find(|s| *s as f32 * geo.pps >= 90.0).unwrap_or(3600.0);
    let minor = major / 5.0;
    let t0 = geo.scroll as f64;
    let t1 = t0 + (r.width() / geo.pps) as f64;
    let mut k = (t0 / minor).floor() as i64;
    loop {
        let t = k as f64 * minor;
        if t > t1 {
            break;
        }
        let x = geo.x(Time::from_secs_f64(t.max(0.0)));
        let is_major = (k % 5) == 0;
        let h = if is_major { 10.0 } else { 5.0 };
        p.line_segment([Pos2::new(x, r.bottom() - h), Pos2::new(x, r.bottom())], Stroke::new(1.0_f32, if is_major { theme::TEXT_DIM } else { theme::BORDER }));
        if is_major && t >= 0.0 {
            let tc = nle_core::format_timecode(Time::from_secs_f64(t), rate);
            p.text(Pos2::new(x + 3.0, r.top() + 3.0), egui::Align2::LEFT_TOP, tc, egui::FontId::monospace(10.0), theme::TEXT_DIM);
        }
        k += 1;
    }
    for m in markers {
        let x = geo.x(m.time);
        if x < r.left() - 8.0 || x > r.right() + 8.0 {
            continue;
        }
        match m.kind {
            MarkerKind::Beat | MarkerKind::SubBass | MarkerKind::Snare => {
                let col = match m.kind {
                    MarkerKind::SubBass => Color32::from_rgb(0xFF, 0x6E, 0x30),
                    MarkerKind::Snare => Color32::from_rgb(0x40, 0xD2, 0xFF),
                    _ => theme::SNAP,
                };
                p.line_segment([Pos2::new(x, r.bottom() - 10.0), Pos2::new(x, r.bottom())], Stroke::new(1.2_f32, col));
                let cy = r.bottom() - 6.0;
                let d = 3.0;
                p.add(egui::Shape::convex_polygon(vec![Pos2::new(x, cy - d), Pos2::new(x + d, cy), Pos2::new(x, cy + d), Pos2::new(x - d, cy)], col, Stroke::NONE));
            }
            MarkerKind::Standard | MarkerKind::Kill => {
                let col = m.color.map(|c| Color32::from_rgb(c[0], c[1], c[2])).unwrap_or_else(|| {
                    if m.kind == MarkerKind::Kill {
                        Color32::from_rgb(0xFF, 0x45, 0x3A)
                    } else {
                        Color32::from_rgb(0x32, 0xAD, 0xE6)
                    }
                });
                p.line_segment([Pos2::new(x, r.top() + 2.0), Pos2::new(x, r.bottom())], Stroke::new(1.5_f32, col));
                let flag = vec![Pos2::new(x, r.top() + 2.0), Pos2::new(x + 8.0, r.top() + 5.0), Pos2::new(x, r.top() + 9.0)];
                p.add(egui::Shape::convex_polygon(flag, col, Stroke::NONE));
                if !m.name.is_empty() && x + 12.0 < r.right() {
                    p.text(Pos2::new(x + 10.0, r.top() + 2.0), egui::Align2::LEFT_TOP, &m.name, egui::FontId::proportional(9.5), col);
                }
            }
        }
    }
}

fn draw_captions_row(p: &egui::Painter, r: Rect, geo: &Geo, app: &App) {
    let cp = p.with_clip_rect(r.intersect(p.clip_rect()));
    cp.rect_filled(r, CornerRadius::ZERO, Color32::from_rgb(0x1B, 0x1E, 0x24));
    cp.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0_f32, theme::BORDER));
    let track = &app.project.timeline.captions;
    let enabled = track.enabled;
    let cc_col = if enabled { Color32::from_rgb(0x5E, 0xCE, 0xFF) } else { theme::TEXT_DIM };
    // blocks are drawn where they play (sync offset applied)
    let off = track.sync_offset();
    let ct = track.caption_time(app.playhead);
    for (i, seg) in track.segments.iter().enumerate() {
        let x0 = geo.x(seg.start + off);
        let x1 = geo.x(seg.end + off).max(x0 + 4.0);
        if x1 < r.left() - 2.0 || x0 > r.right() + 2.0 {
            continue;
        }
        let sr = Rect::from_min_max(Pos2::new(x0, r.top() + 3.0), Pos2::new(x1, r.bottom() - 3.0));
        let active = ct >= seg.start && ct < seg.end;
        let selected = app.captions.selected_seg == Some(i);
        let fill = if enabled { Color32::from_rgb(0x28, 0x3A, 0x4E) } else { theme::GAP };
        let stroke = if selected {
            Stroke::new(2.0_f32, theme::SELECTION)
        } else if active {
            Stroke::new(1.5_f32, cc_col)
        } else {
            Stroke::new(1.0_f32, theme::BORDER)
        };
        cp.rect_filled(sr, CornerRadius::same(3), fill);
        // word boundaries
        if sr.width() > 24.0 {
            for w in seg.words.iter().skip(1) {
                let x = geo.x(w.start + off);
                cp.line_segment([Pos2::new(x, sr.top() + 4.0), Pos2::new(x, sr.bottom() - 4.0)], Stroke::new(1.0_f32, Color32::from_white_alpha(28)));
            }
        }
        cp.rect_stroke(sr, CornerRadius::same(3), stroke, StrokeKind::Inside);
        if sr.width() > 16.0 {
            let sp = cp.with_clip_rect(sr.shrink2(Vec2::new(4.0, 0.0)).intersect(cp.clip_rect()));
            sp.text(
                Pos2::new(sr.left() + 5.0, sr.center().y),
                egui::Align2::LEFT_CENTER,
                seg.text.replace('\n', " "),
                egui::FontId::proportional(10.5),
                if enabled { theme::TEXT } else { theme::TEXT_DIM },
            );
        }
    }
    let badge = Rect::from_min_size(Pos2::new(r.left() + 4.0, r.top() + 3.0), Vec2::new(24.0, r.height() - 6.0));
    cp.rect_filled(badge, CornerRadius::same(3), Color32::from_rgba_unmultiplied(16, 18, 22, 210));
    cp.text(badge.center(), egui::Align2::CENTER_CENTER, "CC", egui::FontId::proportional(10.0), cc_col);
}

/// Caption block under `p` in the captions row `cr`: `(index, Some(start_edge))`
/// within [`CAP_EDGE_PX`] of an edge (nearest edge wins), `(index, None)` on its body.
fn caption_hit(app: &App, cr: Rect, geo: &Geo, p: Pos2) -> Option<(usize, Option<bool>)> {
    if !cr.contains(p) {
        return None;
    }
    let tr = &app.project.timeline.captions;
    let off = tr.sync_offset();
    let mut edge: Option<(usize, bool, f32)> = None;
    let mut body = None;
    for (i, s) in tr.segments.iter().enumerate() {
        let (x0, x1) = (geo.x(s.start + off), geo.x(s.end + off));
        if x0 > p.x + CAP_EDGE_PX {
            break;
        }
        for (start_edge, x) in [(true, x0), (false, x1)] {
            let d = (p.x - x).abs();
            if d < CAP_EDGE_PX && edge.is_none_or(|(_, _, best)| d < best) {
                edge = Some((i, start_edge, d));
            }
        }
        if p.x >= x0 && p.x < x1 {
            body = Some(i);
        }
    }
    edge.map(|(i, s, _)| (i, Some(s))).or(body.map(|i| (i, None)))
}

/// Snap targets for caption-bar drags: playhead, audio peaks and the other
/// blocks' edges (timeline times).
fn caption_snap_targets(app: &App, base: &CaptionTrack, index: usize) -> Vec<Time> {
    let off = base.sync_offset();
    let mut v = app.tl.snap_targets(app.playhead);
    for (i, s) in base.segments.iter().enumerate() {
        if i + 1 == index || i == index + 1 {
            v.push(s.start + off);
            v.push(s.end + off);
        }
    }
    v
}

/// The caption track that results from caption drag `d` with the pointer at
/// `raw_t`, plus the snapped timeline time (for the snap line), if any.
fn caption_drag_result(app: &App, d: &Drag, raw_t: Time, snap: Time) -> Option<(CaptionTrack, Option<Time>)> {
    let tl = &app.project.timeline;
    let rate = tl.format.rate;
    let fit = app.captions.word_fit;
    match d {
        Drag::CaptionTrim { index, start_edge, origin, base } => {
            let seg = base.segments.get(*index)?;
            let off = base.sync_offset();
            let edge = if *start_edge { seg.start } else { seg.end } + off;
            let (target, snapped) = tl.snap(edge + (raw_t - *origin), snap, &caption_snap_targets(app, base, *index), None);
            let target = if snapped { target } else { target.snap_to_frame(rate) };
            let mut tr = (**base).clone();
            tr.trim_segment(*index, *start_edge, target - off, fit);
            Some((tr, snapped.then_some(target)))
        }
        Drag::CaptionSlide { index, origin, base } => {
            let seg = base.segments.get(*index)?;
            let off = base.sync_offset();
            let (s0, e0) = (seg.start + off, seg.end + off);
            let targets = caption_snap_targets(app, base, *index);
            let delta = raw_t - *origin;
            let (s1, ok1) = tl.snap(s0 + delta, snap, &targets, None);
            let (e1, ok2) = tl.snap(e0 + delta, snap, &targets, None);
            let (delta, line) = if ok1 {
                (s1 - s0, Some(s1))
            } else if ok2 {
                (e1 - e0, Some(e1))
            } else {
                ((s0 + delta).snap_to_frame(rate) - s0, None)
            };
            let mut tr = (**base).clone();
            tr.slide_segment(*index, delta);
            Some((tr, line))
        }
        _ => None,
    }
}

/// Highlight bar on a caption block edge at `x`.
fn caption_edge_bar(p: &egui::Painter, cr: Rect, x: f32) {
    let bar = Rect::from_min_max(Pos2::new(x - 1.5, cr.top() + 1.0), Pos2::new(x + 1.5, cr.bottom() - 1.0));
    p.rect_filled(bar, CornerRadius::same(1), theme::SELECTION);
}

/// `mm:ss.cc` for the caption tooltip.
fn caption_tc(t: Time) -> String {
    let cs = (t.as_secs_f64().max(0.0) * 100.0).round() as i64;
    format!("{:02}:{:02}.{:02}", cs / 6000, (cs / 100) % 60, cs % 100)
}

/// Live "Start | Duration" tooltip under a caption block spanning `x0..x1`.
fn caption_tooltip(p: &egui::Painter, cr: Rect, x0: f32, x1: f32, start: Time, dur: Time) {
    let text = format!("Start: {} | Duration: {:.2}s", caption_tc(start), dur.as_secs_f64());
    let font = egui::FontId::proportional(11.0);
    let galley = p.layout_no_wrap(text, font, theme::TEXT);
    let size = galley.size() + Vec2::new(10.0, 6.0);
    let cx = ((x0 + x1) * 0.5).clamp(cr.left() + size.x * 0.5 + 2.0, (cr.right() - size.x * 0.5 - 2.0).max(cr.left() + size.x * 0.5 + 2.0));
    let r = Rect::from_center_size(Pos2::new(cx, cr.bottom() + 4.0 + size.y * 0.5), size);
    p.rect_filled(r, CornerRadius::same(4), Color32::from_rgba_unmultiplied(12, 14, 18, 235));
    p.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0_f32, theme::SELECTION), StrokeKind::Inside);
    p.galley(r.min + Vec2::new(5.0, 3.0), galley, theme::TEXT);
}

fn draw_clip(app: &App, p: &egui::Painter, rect: Rect, c: &Clip, selected: bool) {
    let p = p.with_clip_rect(rect.intersect(p.clip_rect()));
    let accent = clip_accent(c);
    let radius = CornerRadius::same(4);
    match &c.kind {
        ClipKind::Gap => {
            p.rect_filled(rect, radius, theme::GAP.gamma_multiply(0.6));
            if rect.width() > 30.0 {
                p.text(rect.center(), egui::Align2::CENTER_CENTER, "Gap", egui::FontId::proportional(11.0), theme::TEXT_DIM);
            }
        }
        ClipKind::Title(tp) => {
            p.rect_filled(rect, radius, theme::clip_fill(theme::TITLE));
            p.text(
                rect.left_center() + Vec2::new(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("T  {}", tp.text.lines().next().unwrap_or("")),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
        ClipKind::Adjustment => {
            p.rect_filled(rect, radius, theme::clip_fill(theme::TITLE));
            let active_fx = c.effects.iter().filter(|e| e.enabled).count();
            let label = if active_fx > 0 { format!("fx  {} ({active_fx})", c.name) } else { format!("fx  {}", c.name) };
            if rect.width() > 28.0 {
                p.text(rect.left_center() + Vec2::new(8.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(12.0), Color32::WHITE);
            }
        }
        _ => {
            let has_v = c.has_video();
            let has_a = c.has_audio() && !c.audio.muted;
            let split = if has_v && has_a {
                rect.top() + rect.height() * 0.64
            } else if has_v {
                rect.bottom()
            } else {
                rect.top()
            };
            let vrect = Rect::from_min_max(rect.min, Pos2::new(rect.right(), split));
            let arect = Rect::from_min_max(Pos2::new(rect.left(), split), rect.max);
            if has_v {
                p.rect_filled(vrect, radius, theme::clip_fill(accent));
                if let Some(asset) = c.asset_id() {
                    if let Some(strip) = app.library.filmstrips.get(&asset) {
                        let tw = (vrect.height() * strip.aspect).max(8.0);
                        let mut x = vrect.left();
                        let pps = rect.width() / c.duration.as_secs_f64().max(1e-6) as f32;
                        while x < vrect.right() && x < p.clip_rect().right() {
                            if x + tw >= p.clip_rect().left() {
                                let local = Time::from_secs_f64((((x - vrect.left()) + tw * 0.5) / pps).max(0.0) as f64).min(c.duration);
                                if let Some(tex) = strip.at(c.source_time(local)) {
                                    let tr = Rect::from_min_size(Pos2::new(x, vrect.top()), Vec2::new(tw, vrect.height()));
                                    p.image(tex.id(), tr, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::from_gray(225));
                                }
                            }
                            x += tw;
                        }
                    }
                }
                if matches!(c.kind, ClipKind::Compound(_)) {
                    p.rect_filled(vrect, radius, Color32::from_rgba_unmultiplied(10, 132, 255, 40));
                }
            }
            if has_a || (!has_v && c.has_audio()) {
                p.rect_filled(arect, radius, theme::clip_fill(theme::AUDIO));
                if let Some(pk) = c.asset_id().and_then(|a| app.library.peaks.get(&a)) {
                    draw_peaks(&p, pk, c, rect, arect);
                    // volume line
                    let pps = pps_of(rect, c).max(1e-3);
                    let vy = |x: f32| {
                        let t = Time::from_secs_f64(((x - rect.left()) / pps).max(0.0) as f64).min(c.duration);
                        let db = c.audio.volume_db.at(t) as f32;
                        Pos2::new(x, arect.top() + 3.0 + (1.0 - ((db + 60.0) / 72.0).clamp(0.0, 1.0)) * (arect.height() - 6.0))
                    };
                    let (x0, x1) = (arect.left().max(p.clip_rect().left()), arect.right().min(p.clip_rect().right()));
                    let steps = if c.audio.volume_db.is_animated() { ((x1 - x0) / 4.0).ceil().max(1.0) as usize } else { 1 };
                    if x1 > x0 {
                        let line = (0..=steps).map(|i| vy(x0 + (x1 - x0) * i as f32 / steps as f32)).collect();
                        p.add(egui::Shape::line(line, Stroke::new(1.0_f32, Color32::from_white_alpha(140))));
                    }
                }
                // fade handles
                let fi = c.audio.fade_in.as_secs_f64() as f32 * pps_of(rect, c);
                let fo = c.audio.fade_out.as_secs_f64() as f32 * pps_of(rect, c);
                if fi > 1.0 {
                    p.line_segment([arect.left_bottom(), Pos2::new(arect.left() + fi, arect.top())], Stroke::new(1.0_f32, Color32::WHITE));
                }
                if fo > 1.0 {
                    p.line_segment([Pos2::new(arect.right() - fo, arect.top()), arect.right_bottom()], Stroke::new(1.0_f32, Color32::WHITE));
                }
            }
        }
    }
    if let ClipKind::Compound(comp) = &c.kind {
        if comp.storyline {
            // secondary storyline: grey bar along the top and the cuts between its clips
            p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width(), 3.0)), CornerRadius::ZERO, Color32::from_gray(170));
            let pps = pps_of(rect, c);
            let mut acc = Time::ZERO;
            for inner in &comp.spine {
                acc += inner.duration;
                let x = rect.left() + (acc - c.source_in).as_secs_f64() as f32 * pps;
                if x > rect.left() + 1.0 && x < rect.right() - 1.0 {
                    p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.5_f32, Color32::from_black_alpha(200)));
                }
            }
        }
    }
    // name label
    if rect.width() > 24.0 && !c.is_gap() && !matches!(c.kind, ClipKind::Title(_) | ClipKind::Adjustment) {
        let mut name = c.name.clone();
        if c.is_storyline() {
            name = format!("☰ {name}");
        } else if matches!(c.kind, ClipKind::Compound(_)) {
            name = format!("⧉ {name}");
        }
        if !c.audition.is_empty() {
            name = format!("{name}  [audition: {} takes]", c.audition.len() + 1);
        }
        if let Some(r) = &c.retime {
            let s = r.speed_at(Time::ZERO);
            if r.segments.len() > 1 {
                name = format!("{name}  ⏱ ramp");
            } else if (s - 1.0).abs() > 1e-3 {
                name = format!("{name}  ⏱ {:.0}%", s * 100.0);
            }
        }
        let g = p.layout_no_wrap(name, egui::FontId::proportional(11.0), Color32::WHITE);
        let bg = Rect::from_min_size(rect.min + Vec2::new(2.0, 2.0), g.size() + Vec2::new(8.0, 2.0));
        p.rect_filled(bg, CornerRadius::same(3), Color32::from_black_alpha(130));
        p.galley(bg.min + Vec2::new(4.0, 1.0), g, Color32::WHITE);
    }
    if !c.enabled {
        p.rect_filled(rect, radius, Color32::from_black_alpha(150));
    }
    let border = if selected { Stroke::new(2.0_f32, theme::SELECTION) } else { Stroke::new(1.0_f32, Color32::from_black_alpha(160)) };
    p.rect_stroke(rect, radius, border, StrokeKind::Inside);
}

/// Duck overlay on a ducked (Music) clip's waveform: cyan shading for the
/// depth the music is pulled down under voice, orange line for the gain.
fn draw_duck_overlay(app: &App, p: &egui::Painter, rect: Rect, c: &Clip, start: Time) {
    if !c.has_audio() || c.audio.muted || !c.enabled {
        return;
    }
    let Some(curve) = app.duck_curve_for(c) else { return };
    let split = if c.has_video() { rect.top() + rect.height() * 0.64 } else { rect.top() };
    let arect = Rect::from_min_max(Pos2::new(rect.left(), split), rect.max);
    let p = p.with_clip_rect(arect.intersect(p.clip_rect()));
    let (x0, x1) = (arect.left().max(p.clip_rect().left()), arect.right().min(p.clip_rect().right()));
    if x1 <= x0 || arect.height() < 4.0 {
        return;
    }
    let pps = pps_of(rect, c) as f64;
    let range = nle_core::DuckingSettings::AMOUNT_RANGE_DB;
    let floor_db = *range.end();
    let y_of = |g: f32| {
        let db = (-nle_core::clip::gain_to_db(g.max(1e-6))).clamp(0.0, floor_db);
        arect.top() + 2.0 + db / floor_db * (arect.height() - 4.0)
    };
    let cyan = Color32::from_rgba_unmultiplied(0, 200, 255, 60);
    let orange = Color32::from_rgb(255, 150, 40);
    const STEP: f32 = 2.0;
    let mut pts = Vec::with_capacity(((x1 - x0) / STEP) as usize + 2);
    let mut x = x0;
    while x <= x1 {
        let t = start.as_secs_f64() + (x - rect.left()) as f64 / pps;
        let g = curve.min_in(t, t + STEP as f64 / pps);
        let y = y_of(g);
        if y > arect.top() + 2.5 {
            p.rect_filled(Rect::from_min_max(Pos2::new(x, arect.top()), Pos2::new(x + STEP, y)), 0.0, cyan);
        }
        pts.push(Pos2::new(x, y));
        x += STEP;
    }
    if pts.iter().any(|q| q.y > arect.top() + 2.5) {
        p.add(egui::Shape::line(pts, Stroke::new(1.5_f32, orange)));
    }
}

fn pps_of(rect: Rect, c: &Clip) -> f32 {
    rect.width() / c.duration.as_secs_f64().max(1e-6) as f32
}

/// Animated video and effect parameters of a clip, for the animation lane.
fn video_params(c: &Clip) -> Vec<(Cow<'static, str>, &Param)> {
    let v = &c.video;
    let col = &c.color;
    let mut out = vec![
        (Cow::Borrowed("Opacity"), &v.opacity),
        (Cow::Borrowed("Position X"), &v.position_x),
        (Cow::Borrowed("Position Y"), &v.position_y),
        (Cow::Borrowed("Scale X"), &v.scale_x),
        (Cow::Borrowed("Scale Y"), &v.scale_y),
        (Cow::Borrowed("Rotation"), &v.rotation),
        (Cow::Borrowed("Anchor X"), &v.anchor_x),
        (Cow::Borrowed("Anchor Y"), &v.anchor_y),
        (Cow::Borrowed("Crop L"), &v.crop_left),
        (Cow::Borrowed("Crop R"), &v.crop_right),
        (Cow::Borrowed("Crop T"), &v.crop_top),
        (Cow::Borrowed("Crop B"), &v.crop_bottom),
        (Cow::Borrowed("Exposure"), &col.exposure),
        (Cow::Borrowed("Brightness"), &col.brightness),
        (Cow::Borrowed("Contrast"), &col.contrast),
        (Cow::Borrowed("Saturation"), &col.saturation),
        (Cow::Borrowed("Temperature"), &col.temperature),
        (Cow::Borrowed("Tint"), &col.tint),
        (Cow::Borrowed("Volume"), &c.audio.volume_db),
        (Cow::Borrowed("Pan"), &c.audio.pan),
    ];
    for e in &c.effects {
        let defs = e.kind.params();
        for (d, p) in defs.iter().zip(&e.params) {
            out.push((Cow::Owned(format!("{}: {}", e.kind.label(), d.name)), p));
        }
    }
    out
}

fn key_times(c: &Clip) -> Vec<Time> {
    let mut ts: Vec<Time> = video_params(c).iter().filter_map(|(_, p)| p.curve.as_ref()).flat_map(|cv| cv.keys.iter().map(|k| k.time)).collect();
    ts.sort();
    ts.dedup();
    ts
}

fn draw_anim_lane(p: &egui::Painter, r: Rect, c: &Clip, geo: &Geo, start: Time, selected: Option<Time>) {
    if c.is_gap() {
        return;
    }
    p.rect_filled(r, CornerRadius::same(3), Color32::from_rgb(0x1A, 0x1A, 0x1E));
    p.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.0_f32, theme::BORDER), StrokeKind::Inside);
    // opacity curve
    let n = ((r.width() / 3.0) as usize).clamp(2, 400);
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let u = i as f32 / n as f32;
            let t = c.duration.mul_f64(u as f64);
            let v = c.video.opacity.at(t) as f32;
            Pos2::new(r.left() + u * r.width(), r.bottom() - 3.0 - v.clamp(0.0, 1.0) * (r.height() - 6.0))
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.0_f32, theme::VIDEO.gamma_multiply(0.8))));
    let animated: Vec<String> = video_params(c).into_iter().filter(|(_, p)| p.is_animated()).map(|(n, _)| n.into_owned()).collect();
    if r.width() > 80.0 {
        let label = if animated.is_empty() { "No keyframes".to_string() } else { animated.join(", ") };
        p.text(r.left_top() + Vec2::new(4.0, 2.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(9.0), theme::TEXT_DIM);
    }
    for t in key_times(c) {
        let x = geo.x(start + t);
        let cy = r.center().y + 4.0;
        let sel = selected == Some(t);
        let d = if sel { 6.5 } else { 5.0 };
        p.add(egui::Shape::convex_polygon(
            vec![Pos2::new(x, cy - d), Pos2::new(x + d, cy), Pos2::new(x, cy + d), Pos2::new(x - d, cy)],
            if sel { Color32::WHITE } else { theme::SNAP },
            Stroke::new(if sel { 1.5_f32 } else { 1.0_f32 }, if sel { theme::SNAP } else { Color32::BLACK }),
        ));
    }
}

fn anim_key_hit(app: &App, hits: &[Hit], pos: Pos2, geo: &Geo, anim: f32) -> Option<Drag> {
    if anim <= 0.0 {
        return None;
    }
    for h in hits {
        let lane = Rect::from_min_max(Pos2::new(h.rect.left(), h.rect.bottom()), Pos2::new(h.rect.right(), h.rect.bottom() + anim));
        if !lane.contains(pos) {
            continue;
        }
        let c = app.project.timeline.clip(h.id)?;
        for t in key_times(c) {
            if (geo.x(h.start + t) - pos.x).abs() < 6.0 {
                return Some(Drag::Key { id: h.id, key_time: t, origin: geo.t(pos.x) });
            }
        }
    }
    None
}

/// Right-click menu on a keyframe diamond: ease of the segment leaving it, delete.
fn key_context_menu(app: &mut App, ui: &mut egui::Ui, (id, t): (ClipId, Time)) {
    use nle_core::zoom_punch::{interp_near, set_interp_near};
    let Some(clip) = app.project.timeline.clip(id).cloned() else { return };
    let tol = Time(1);
    let current = interp_near(&clip, t, tol);
    ui.label(egui::RichText::new(format!("Keyframe @ {}", nle_core::format_timecode(t, app.project.timeline.format.rate))).small().color(theme::TEXT_DIM));
    ui.separator();
    ui.label(egui::RichText::new("Ease to next key").small().color(theme::TEXT_DIM));
    for interp in Interpolation::ALL {
        if ui.radio(current == Some(interp), interp.label()).clicked() {
            let mut c = clip.clone();
            if set_interp_near(&mut c, t, tol, interp) {
                app.exec(Command::UpdateClip { clip: Box::new(c) });
            }
            ui.close_menu();
        }
    }
    ui.separator();
    if ui.button("Delete Keyframe").clicked() {
        app.delete_selected_keyframe(id, t);
        ui.close_menu();
    }
}

fn move_keys(c: &mut Clip, from: Time, to: Time, tol: Time) {
    let mut f = |p: &mut Param| {
        if let Some(cv) = &mut p.curve {
            if let Some(i) = cv.key_near(from, tol) {
                let mut k = cv.keys.remove(i);
                k.time = to;
                cv.insert(k);
            }
        }
    };
    c.for_each_param_mut(&mut f);
}

/// How tall the waveform is drawn at clip time `t`: what the mixer applies.
fn wave_gain(c: &Clip, t: Time) -> f32 {
    c.audio.gain_at(t, c.duration)
}

/// Width of one waveform column in points.
const WAVE_COL: f32 = 2.0;

/// Mirrored min/max waveform for `c` inside `arect`. Columns map through
/// `source_time`, so only the trimmed source range is shown, and only the
/// columns inside the painter's clip rect (the visible timeline viewport)
/// are generated — cost is bounded by screen width, not clip length. All
/// bars go into a single mesh (one draw call per clip).
fn draw_peaks(p: &egui::Painter, pk: &nle_media::PeakBuffer, c: &Clip, rect: Rect, arect: Rect) {
    let x0 = arect.left().max(p.clip_rect().left());
    let x1 = arect.right().min(p.clip_rect().right());
    let half = (arect.height() - 4.0) * 0.5;
    if pk.is_empty() || x1 <= x0 || half < 1.0 || rect.width() <= 0.0 {
        return;
    }
    let pps = rect.width() / c.duration.as_secs_f64().max(1e-6) as f32;
    let mid = arect.center().y;
    let color = theme::AUDIO.gamma_multiply(0.9);
    // Align columns to the clip's left edge so bars do not shimmer while scrolling.
    let mut x = rect.left() + ((x0 - rect.left()) / WAVE_COL).floor() * WAVE_COL;
    let local = |x: f32| Time::from_secs_f64(((x - rect.left()) / pps).max(0.0) as f64).min(c.duration);
    // Perceptual curve: quiet game audio stays visible, peaks still stand out.
    let shape = |v: f32| v.signum() * v.abs().min(1.0).powf(0.7);
    let mut mesh = egui::Mesh::default();
    let mut s_prev = c.source_time(local(x)).as_secs_f64();
    while x < x1 {
        let l0 = local(x);
        let s_next = c.source_time(local(x + WAVE_COL)).as_secs_f64();
        let (lo, hi) = pk.range(s_prev.min(s_next), s_prev.max(s_next));
        s_prev = s_next;
        let g = wave_gain(c, l0);
        let top = mid - shape(hi * g) * half;
        let bottom = (mid - shape(lo * g) * half).max(top + 1.0);
        let left = x.max(x0);
        let right = (x + WAVE_COL - 0.5).min(x1);
        if right > left {
            mesh.add_colored_rect(Rect::from_min_max(Pos2::new(left, top), Pos2::new(right, bottom)), color);
        }
        x += WAVE_COL;
    }
    p.add(egui::Shape::mesh(mesh));
}

fn finish_drag(app: &mut App, d: Drag, p: Pos2, geo: &Geo, hits: &[Hit], lane_at: &dyn Fn(f32) -> i32, snap: Time) {
    let rate = app.project.timeline.format.rate;
    let raw_t = geo.t(p.x);
    let tl = &app.project.timeline;
    match d {
        Drag::Scrub | Drag::Range { .. } | Drag::VelocityKey { .. } => {}
        Drag::CaptionTrim { .. } | Drag::CaptionSlide { .. } => {
            // final pointer position (the live preview already applied it under the same key)
            if let Some((tr, _)) = caption_drag_result(app, &d, raw_t, snap) {
                if tr != app.project.timeline.captions {
                    app.exec_key(Command::SetCaptions { captions: Box::new(tr) }, Some(CAPTION_BAR_KEY));
                }
            }
        }
        Drag::Move { id, grab } | Drag::Position { id, grab } => {
            let Some(h) = hits.iter().find(|h| h.id == id) else { return };
            let mut lane = lane_at(p.y);
            if app.tl.audio_lanes && lane < 0 && h.lane < 0 {
                lane = h.lane;
            }
            let dur = tl.clip(id).map(|c| c.duration).unwrap_or_default();
            let mut start = (raw_t - grab).max(Time::ZERO);
            let (s1, ok) = tl.snap(start, snap, &app.tl.snap_targets(app.playhead), Some(id));
            if ok {
                start = s1;
            } else {
                let (e1, ok2) = tl.snap(start + dur, snap, &app.tl.snap_targets(app.playhead), Some(id));
                if ok2 {
                    start = (e1 - dur).max(Time::ZERO);
                }
            }
            let start = start.snap_to_frame(rate);
            let moved_px = (geo.x(start) - h.rect.left()).abs();
            if moved_px < 3.0 && lane == h.lane {
                return;
            }
            let cmd = if matches!(d, Drag::Position { .. }) {
                Command::PositionMove { id, at: start, lane }
            } else if h.lane == 0 && lane == 0 {
                Command::MoveClip { id, at: raw_t, lane: 0 }
            } else {
                Command::MoveClip { id, at: start, lane }
            };
            app.exec(hold_if_grave(app, h, cmd));
        }
        Drag::Trim { id, start_edge, origin } => {
            let Some(h) = hits.iter().find(|h| h.id == id) else { return };
            let dur = tl.clip(id).map(|c| c.duration).unwrap_or_default();
            let edge = if start_edge { h.start } else { h.start + dur };
            let (target, _) = tl.snap(edge + (raw_t - origin), snap, &app.tl.snap_targets(app.playhead), Some(id));
            let delta = target.snap_to_frame(rate) - edge;
            if delta.is_zero() {
                return;
            }
            let cmd = if start_edge { Command::TrimStart { id, delta } } else { Command::TrimEnd { id, delta } };
            app.exec(hold_if_grave(app, h, cmd));
        }
        Drag::Roll { left, edit, origin } => {
            let delta = roll_drag_target(app, edit, origin, raw_t, snap) - edit;
            if !delta.is_zero() {
                app.exec(Command::Roll { left, delta });
            }
        }
        Drag::Split { id, start_edge, origin } => {
            let Some(c) = tl.clip(id) else { return };
            let before = c.audio_split();
            let (lead, tail) = split_preview(c, start_edge, (raw_t - origin).snap_to_frame(rate));
            if (lead, tail) != before {
                app.exec(Command::SetAudioSplit { id, lead, tail });
            }
        }
        Drag::Slip { id, origin } => {
            let delta = (raw_t - origin).snap_to_frame(rate);
            if !delta.is_zero() {
                app.exec(Command::Slip { id, delta: Time(-delta.0) });
            }
        }
        Drag::Slide { id, origin } => {
            let delta = (raw_t - origin).snap_to_frame(rate);
            if !delta.is_zero() {
                app.exec(Command::Slide { id, delta });
            }
        }
        Drag::Key { id, key_time, origin } => {
            let Some(mut c) = tl.clip(id).cloned() else { return };
            let to = (key_time + (raw_t - origin)).snap_to_frame(rate).clamp(Time::ZERO, c.duration);
            if to != key_time {
                move_keys(&mut c, key_time, to, tl.frame());
                if app.exec(Command::UpdateClip { clip: Box::new(c) }) {
                    app.tl.selected_key = Some((id, to));
                }
            }
        }
    }
}

/// Storyline edit point under a Trim-tool grab: `(left clip, edit time)` when
/// the grabbed edge has a neighbour on the other side.
fn roll_target(tl: &Timeline, h: &Hit, near_start: bool, near_end: bool) -> Option<(ClipId, Time)> {
    let idx = tl.spine.iter().position(|c| c.id == h.id)?;
    if near_end && idx + 1 < tl.spine.len() {
        Some((h.id, h.start + tl.spine[idx].duration))
    } else if near_start && idx > 0 {
        Some((tl.spine[idx - 1].id, h.start))
    } else {
        None
    }
}

/// Snapped, frame-aligned roll target. The edit point's own edges move with
/// the roll, so they are not snap targets — only the playhead/markers/peaks.
fn roll_drag_target(app: &App, edit: Time, origin: Time, raw_t: Time, snap: Time) -> Time {
    let tl = &app.project.timeline;
    let want = edit + (raw_t - origin);
    if !tl.snapping {
        return want.snap_to_frame(tl.format.rate);
    }
    // the cut being rolled is an edit point itself: snapping to it would pin it
    let points = tl.snap_points(None).into_iter().filter(|t| *t != edit).chain(app.tl.snap_targets(app.playhead));
    let best = points.filter(|t| (*t - want).abs() <= snap).min_by_key(|t| (*t - want).abs());
    best.unwrap_or(want).snap_to_frame(tl.format.rate)
}

/// New `(lead, tail)` for an Alt-drag of a clip edge by `d`, clamped to media.
fn split_preview(c: &Clip, start_edge: bool, d: Time) -> (Time, Time) {
    let (lead, tail) = c.audio_split();
    let mut probe = c.clone();
    if start_edge {
        probe.audio.audio_lead = (lead - d).max(Time::ZERO);
    } else {
        probe.audio.audio_tail = (tail + d).max(Time::ZERO);
    }
    probe.audio_split()
}

fn split_bar_rect(clip: Rect, x0: f32, x1: f32) -> Rect {
    Rect::from_min_max(Pos2::new(x0, clip.bottom() - 5.0), Pos2::new(x1.max(x0 + 1.0), clip.bottom() - 1.0))
}

fn drop_target(app: &App, p: Pos2, geo: &Geo, lane_at: &dyn Fn(f32) -> i32, snap: Time) -> (i32, Time) {
    let tl = &app.project.timeline;
    let raw = geo.t(p.x);
    let lane = if tl.spine.is_empty() { 0 } else { lane_at(p.y) };
    if lane == 0 {
        let idx = tl.drop_index(raw);
        (0, tl.spine_start(idx))
    } else {
        let (t, _) = tl.snap(raw, snap, &app.tl.snap_targets(app.playhead), None);
        (lane, t.snap_to_frame(tl.format.rate))
    }
}

fn drop_asset(app: &mut App, asset: AssetId, lane: i32, at: Time) {
    let Some(a) = app.project.asset(asset).cloned() else { return };
    // a browser clip with In / Out marks brings only the marked part
    let clip = app.marked_clip(&a);
    let cmd = if lane == 0 {
        if at >= app.project.timeline.duration() {
            Command::Append { clip }
        } else {
            Command::Insert { at, clip }
        }
    } else {
        Command::Connect { clip, at, lane }
    };
    app.browser.selected = Some(asset);
    app.exec(cmd);
}

/// While ` is held, an edit of a storyline clip leaves its connected clips
/// where they are on the timeline instead of carrying them along.
fn hold_if_grave(app: &App, h: &Hit, cmd: Command) -> Command {
    if app.hold_connections && h.lane == 0 {
        Command::HoldConnections { ids: vec![h.id], cmd: Box::new(cmd) }
    } else {
        cmd
    }
}

fn context_menu(app: &mut App, ui: &mut egui::Ui, clip: Option<ClipId>) {
    let Some(id) = clip else {
        if ui.add_enabled(app.can_paste(), egui::Button::new("Paste at Playhead")).clicked() {
            app.paste_clips();
            ui.close_menu();
        }
        if ui.button("Add Basic Title").clicked() {
            app.add_title(false);
            ui.close_menu();
        }
        if ui.button("Add Lower Third").clicked() {
            app.add_title(true);
            ui.close_menu();
        }
        if ui.button("Add Adjustment Clip").clicked() {
            app.add_adjustment_clip();
            ui.close_menu();
        }
        if ui.button("Insert Gap (2 s)").clicked() {
            app.insert_gap();
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Add Marker").clicked() {
            app.add_marker();
            ui.close_menu();
        }
        if ui.button("Generate Captions…").clicked() {
            app.show_captions = true;
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Zoom to Fit").clicked() {
            app.tl.fit_pending = true;
            ui.close_menu();
        }
        return;
    };
    let spine = nle_core::command::is_spine(&app.project.timeline, id);
    // the menu acts on the whole selection when opened on part of it
    let select = |app: &mut App| {
        if !app.selection.contains(&id) {
            app.selection = vec![id];
        }
    };
    let (linkable, linked) =
        app.project.timeline.clip(id).map_or((false, false), |c| (c.av.video && c.av.audio && c.link.is_none() && matches!(c.kind, ClipKind::Asset { .. }), c.link.is_some()));
    if ui.button("Cut").clicked() {
        select(app);
        app.cut_selection();
        ui.close_menu();
    }
    if ui.button("Copy").clicked() {
        select(app);
        app.copy_selection();
        ui.close_menu();
    }
    if ui.add_enabled(app.can_paste(), egui::Button::new("Paste at Playhead")).clicked() {
        app.paste_clips();
        ui.close_menu();
    }
    if ui.button("Duplicate").clicked() {
        select(app);
        app.duplicate_selection();
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Delete").clicked() {
        select(app);
        app.delete_clips(false);
        ui.close_menu();
    }
    if ui.add_enabled(spine, egui::Button::new("Replace with Gap")).clicked() {
        select(app);
        app.delete_clips(true);
        ui.close_menu();
    }
    if ui.button("Blade at Playhead").clicked() {
        let at = app.playhead;
        app.exec(Command::BladeClip { id, at });
        ui.close_menu();
    }
    ui.separator();
    if ui.add_enabled(linkable, egui::Button::new("Detach Audio")).clicked() {
        select(app);
        app.detach_audio();
        ui.close_menu();
    }
    if ui.add_enabled(linked, egui::Button::new("Unlink Audio and Video")).clicked() {
        select(app);
        app.unlink_audio();
        ui.close_menu();
    }
    let pair = !linked && app.selection.len() == 2 && app.selection.contains(&id);
    if ui.add_enabled(pair, egui::Button::new("Link Audio and Video")).on_hover_text("Select one video clip and one audio clip").clicked() {
        app.link_audio();
        ui.close_menu();
    }
    ui.separator();
    if ui.button("New Compound Clip").clicked() {
        app.make_compound();
        ui.close_menu();
    }
    let compound = app.project.timeline.clip(id).is_some_and(|c| matches!(c.kind, ClipKind::Compound(_)));
    if ui.add_enabled(compound, egui::Button::new("Break Apart Clip Items")).clicked() {
        app.break_apart();
        ui.close_menu();
    }
    if ui.add_enabled(compound, egui::Button::new("Open in Timeline")).on_hover_text("Edit its contents (double-click; Ctrl+[ backs out)").clicked() {
        app.step_into(id);
        ui.close_menu();
    }
    if ui.add_enabled(!spine, egui::Button::new("Create Storyline")).on_hover_text("Group the selected connected clips into a storyline").clicked() {
        if !app.selection.contains(&id) {
            app.selection = vec![id];
        }
        app.make_storyline();
        ui.close_menu();
    }
    if ui.add_enabled(!spine, egui::Button::new("Move Connection Point to Playhead")).on_hover_text("Or Ctrl+Alt+click the connected clip where it should attach").clicked() {
        let at = app.playhead;
        app.exec(Command::SetConnectionPoint { id, at });
        ui.close_menu();
    }
    let takes = app.project.timeline.clip(id).map_or(0, |c| c.audition.len());
    ui.menu_button("Audition", |ui| {
        if ui.button("Duplicate as Audition").clicked() {
            app.selection = vec![id];
            app.duplicate_as_audition();
            ui.close_menu();
        }
        if ui.add_enabled(app.browser.selected.is_some(), egui::Button::new("Add Browser Clip as Take")).clicked() {
            app.add_browser_clip_to_audition(id);
            ui.close_menu();
        }
        for (label, step) in [("Next Pick", 1), ("Previous Pick", -1)] {
            if ui.add_enabled(takes > 0, egui::Button::new(label)).clicked() {
                app.selection = vec![id];
                app.cycle_audition(step);
                ui.close_menu();
            }
        }
        if ui.add_enabled(takes > 0, egui::Button::new("Finalize Audition")).clicked() {
            app.selection = vec![id];
            app.finalize_audition();
            ui.close_menu();
        }
    });
    if ui.add_enabled(spine, egui::Button::new("Lift from Storyline")).clicked() {
        let tl = &app.project.timeline;
        if let (Some(at), Some(c)) = (tl.clip_start(id), tl.clip(id)) {
            let lane = tl.free_lane(1, at, c.duration);
            app.exec(Command::LiftToLane { id, lane });
        }
        ui.close_menu();
    }
    if ui.add_enabled(!spine, egui::Button::new("Overwrite to Primary Storyline")).clicked() {
        if let Some(at) = app.project.timeline.clip_start(id) {
            app.exec(Command::MoveClip { id, at, lane: 0 });
        }
        ui.close_menu();
    }
    if ui.button("Enable / Disable").clicked() {
        app.exec(Command::ToggleEnabled { id });
        ui.close_menu();
    }
    ui.separator();
    ui.menu_button("Add Transition", |ui| {
        for (edge, at_end) in [("At Start", false), ("At End", true)] {
            ui.menu_button(edge, |ui| {
                for k in nle_core::TransitionKind::ALL {
                    let label = if k == nle_core::TransitionKind::DipToBlack { "Fade to Black (Dip to Black)" } else { k.label() };
                    if ui.add_enabled(spine, egui::Button::new(label)).clicked() {
                        app.selection = vec![id];
                        app.add_transition_at(k, at_end);
                        ui.close_menu();
                    }
                }
                ui.separator();
                let audible = app.project.timeline.clip(id).is_some_and(|c| c.has_audio());
                if ui.add_enabled(audible, egui::Button::new("Audio Crossfade")).clicked() {
                    app.selection = vec![id];
                    app.add_audio_crossfade(at_end);
                    ui.close_menu();
                }
            });
        }
    });
    ui.menu_button("Retime", |ui| app.retime_menu(ui));
    let has_audio = app.project.timeline.clip(id).is_some_and(|c| c.has_audio() && c.asset_id().is_some());
    if ui.add_enabled(has_audio && app.beat_job.is_none(), egui::Button::new("Analyze Beats")).clicked() {
        app.selection = vec![id];
        app.analyze_selected_beats();
        ui.close_menu();
    }
    if ui.button("Add Marker").clicked() {
        app.add_marker();
        ui.close_menu();
    }
}

fn retime_popup(app: &mut App, ui: &mut egui::Ui, resp: &egui::Response) {
    let popup = egui::Id::new("retime-popup");
    if !ui.memory(|m| m.is_popup_open(popup)) {
        return;
    }
    let Some((id, seg, pos)) = ui.memory(|m| m.data.get_temp::<(ClipId, usize, Pos2)>(egui::Id::new("retime-seg"))) else { return };
    let mut close = false;
    let area = egui::Area::new(popup).order(egui::Order::Foreground).fixed_pos(pos + Vec2::new(0.0, 6.0)).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_min_width(170.0);
            ui.label(egui::RichText::new(format!("Segment {}", seg + 1)).small().color(theme::TEXT_DIM));
            for (l, s) in [("Slow 25%", 0.25), ("Slow 50%", 0.5), ("Normal 100%", 1.0), ("Fast 200%", 2.0), ("Fast 400%", 4.0)] {
                if ui.button(l).clicked() {
                    set_segment(app, id, seg, |r| r.set_segment_speed(seg, s));
                    close = true;
                }
            }
            ui.separator();
            if ui.button("Ease into segment (ramp)").clicked() {
                set_segment(app, id, seg, |r| {
                    let d = r.segments.get(seg).map(|s| s.duration.div_i64(2)).unwrap_or_default();
                    r.set_ramp(seg, d);
                });
                close = true;
            }
            if ui.button("Split segment at playhead").clicked() {
                let local = app.local_time(id);
                set_segment(app, id, seg, |r| {
                    r.split_segment(local);
                });
                close = true;
            }
            ui.separator();
            if ui.button("Add Velocity Point at Playhead").clicked() {
                let local = app.local_time(id);
                set_segment(app, id, seg, |r| {
                    let sp = r.speed_at(local);
                    r.set_velocity_point(local, sp);
                    r.blend = FrameBlendMode::OpticalFlow;
                });
                close = true;
            }
            ui.menu_button("Fragmovie Velocity Presets", |ui| {
                for name in Retime::PRESET_NAMES {
                    if ui.button(name).clicked() {
                        if let Some(c) = app.project.timeline.clip(id) {
                            if let Some(r) = Retime::velocity_preset(name, c.duration) {
                                app.exec(Command::SetRetime { id, retime: Some(r) });
                            }
                        }
                        close = true;
                        ui.close_menu();
                    }
                }
            });
            ui.separator();
            app.retime_menu(ui);
        });
    });
    if close || (resp.clicked_elsewhere() && area.response.clicked_elsewhere()) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ui.memory_mut(|m| m.close_popup());
    }
}

fn vel_speed_to_norm(speed: f64) -> f32 {
    let ratio = (speed.clamp(MIN_VELOCITY, MAX_VELOCITY) / MIN_VELOCITY).ln();
    let span = (MAX_VELOCITY / MIN_VELOCITY).ln();
    (ratio / span).clamp(0.0, 1.0) as f32
}

fn vel_norm_to_speed(norm: f32) -> f64 {
    let span = (MAX_VELOCITY / MIN_VELOCITY).ln();
    (MIN_VELOCITY * (norm.clamp(0.0, 1.0) as f64 * span).exp()).clamp(MIN_VELOCITY, MAX_VELOCITY)
}

fn draw_velocity_lane(p: &egui::Painter, r: Rect, c: &Clip, retime: &Retime, geo: &Geo, start: Time, handles: &mut Vec<(ClipId, usize, Pos2, Rect)>) {
    if r.width() < 8.0 {
        return;
    }
    let cp = p.with_clip_rect(r.intersect(p.clip_rect()));
    cp.rect_filled(r, CornerRadius::same(3), Color32::from_rgb(0x13, 0x18, 0x20));
    cp.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.0_f32, Color32::from_rgb(0x28, 0x45, 0x60)), StrokeKind::Inside);

    let y_of = |speed: f64| {
        let u = vel_speed_to_norm(speed);
        r.bottom() - 4.0 - u * (r.height() - 8.0).max(4.0)
    };
    let base_y = y_of(1.0);
    cp.line_segment([Pos2::new(r.left(), base_y), Pos2::new(r.right(), base_y)], Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 45)));

    let n = ((r.width() / 4.0) as usize).clamp(4, 160);
    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let frac = i as f64 / n as f64;
        let t = c.duration.mul_f64(frac);
        let x = geo.x(start + t);
        let y = y_of(retime.speed_at(t));
        pts.push(Pos2::new(x, y));
    }
    cp.add(egui::Shape::line(pts, Stroke::new(1.75_f32, Color32::from_rgb(0x40, 0xD2, 0xFF))));

    if let Some(cv) = &retime.velocity_curve {
        for (idx, k) in cv.keys.iter().enumerate() {
            let kx = geo.x(start + k.time);
            let ky = y_of(k.value);
            let pos = Pos2::new(kx, ky);
            cp.circle_filled(pos, 4.0, Color32::from_rgb(0xFF, 0xD6, 0x0A));
            cp.circle_stroke(pos, 4.0, Stroke::new(1.0_f32, Color32::BLACK));
            handles.push((c.id, idx, pos, r));
        }
    } else if r.width() > 36.0 {
        let mid_pos = Pos2::new(r.center().x, y_of(retime.speed_at(Time::ZERO)));
        cp.circle_filled(mid_pos, 3.5, theme::SNAP);
        handles.push((c.id, 0, mid_pos, r));
    }
    if r.width() > 90.0 {
        let cur_pct = retime.speed_at(Time::ZERO) * 100.0;
        let label = if retime.has_velocity_curve() { "Velocity 10–800%".to_string() } else { format!("Speed {:.0}%", cur_pct) };
        cp.text(r.left_top() + Vec2::new(4.0, 2.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(9.5), Color32::from_rgb(0x8E, 0xE8, 0xFF));
    }
}

fn set_segment(app: &mut App, id: ClipId, _seg: usize, f: impl FnOnce(&mut Retime)) {
    let Some(c) = app.project.timeline.clip(id) else { return };
    let mut r = c.retime.clone().unwrap_or_else(|| Retime::uniform(c.duration, 1.0));
    f(&mut r);
    app.exec(Command::SetRetime { id, retime: Some(r) });
}

#[cfg(test)]
mod wave_tests {
    use super::{wave_gain, Clip, Time};

    #[test]
    fn the_waveform_applies_clip_volume_once() {
        let mut c = Clip::gap(Time::from_secs(4));
        c.audio.volume_db.value = -20.0;
        assert!((wave_gain(&c, Time::from_secs(2)) - 0.1).abs() < 1e-3);
    }
}
