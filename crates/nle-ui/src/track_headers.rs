//! The column left of the timeline: each lane's name with its eye, mute,
//! solo and lock switches.

use crate::app::App;
use crate::theme;
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use nle_core::tracks::TrackState;
use nle_core::Command;

pub const WIDTH: f32 = 96.0;
const BUTTON: Vec2 = Vec2::new(19.0, 18.0);

/// Lanes are numbered outwards from the storyline: V1 is the storyline.
pub fn lane_name(lane: i32) -> String {
    match lane {
        0 => "V1 Main".into(),
        l if l > 0 => format!("V{}", l + 1),
        l => format!("A{}", -l),
    }
}

/// One switch: its glyph, whether it is engaged, its colour then, and its tip.
type Switch = (&'static str, bool, Color32, &'static str);

fn switches(t: &TrackState) -> [Switch; 4] {
    [
        ("👁", t.hidden, theme::PLAYHEAD, if t.hidden { "Show this track" } else { "Hide this track's picture" }),
        ("M", t.muted, theme::PLAYHEAD, if t.muted { "Unmute this track" } else { "Mute this track" }),
        ("S", t.solo, theme::SELECTION, if t.solo { "End solo" } else { "Solo: only soloed tracks are heard, in export too" }),
        ("🔒", t.locked, theme::SNAP, if t.locked { "Unlock this track" } else { "Lock this track against edits" }),
    ]
}

/// "A1: muted, locked"
fn flags(t: &TrackState) -> String {
    let set: Vec<&str> =
        [(t.hidden, "hidden"), (t.muted, "muted"), (t.solo, "soloed"), (t.locked, "locked"), (t.target, "targeted")].into_iter().filter(|f| f.0).map(|f| f.1).collect();
    format!("{}: {}", lane_name(t.lane), set.join(", "))
}

/// Draw the column. `rows` are the lanes with their clip strips, in screen
/// coordinates; `top` is where the lanes start below the ruler.
pub fn show(app: &mut App, ui: &mut egui::Ui, column: Rect, top: f32, rows: &[(i32, Rect)]) {
    let id = ui.id().with("track-headers");
    let painter = ui.painter_at(column);
    painter.rect_filled(column, CornerRadius::ZERO, theme::PANEL);
    painter.line_segment([column.right_top(), column.right_bottom()], Stroke::new(1.0_f32, theme::BORDER));
    painter.text(column.left_top() + Vec2::new(6.0, 7.0), Align2::LEFT_TOP, "Tracks", FontId::proportional(11.0), theme::TEXT_DIM);
    // the role view folds the audio lanes away, and their switches with them
    let folded: Vec<String> = app.project.timeline.tracks.iter().filter(|t| **t != TrackState::new(t.lane) && !rows.iter().any(|(lane, _)| *lane == t.lane)).map(flags).collect();
    if !folded.is_empty() {
        let at = Rect::from_min_size(Pos2::new(column.right() - 26.0, column.top() + 4.0), Vec2::new(22.0, 16.0));
        let tip = format!("Not shown in this view, still in effect:\n{}", folded.join("\n"));
        ui.interact(at, id.with("folded"), Sense::hover()).on_hover_text(tip);
        painter.text(at.center(), Align2::CENTER_CENTER, "⚠", FontId::proportional(12.0), theme::PLAYHEAD);
    }

    let body = Rect::from_min_max(Pos2::new(column.left(), top), column.max);
    if ui.interact(body, id, Sense::hover()).contains_pointer() {
        app.tl.vscroll -= ui.input(|i| i.smooth_scroll_delta.y);
    }
    let clipped = painter.with_clip_rect(body);
    // track switches live on the project's timeline, not inside containers
    let editable = app.focus.is_empty();
    let mut change = None;
    for &(lane, strip) in rows {
        let row = Rect::from_min_max(Pos2::new(column.left(), strip.top()), Pos2::new(column.right(), strip.bottom()));
        if !row.intersects(body) {
            continue;
        }
        clipped.line_segment([row.left_bottom(), row.right_bottom()], Stroke::new(1.0_f32, theme::BORDER));
        clipped.text(row.left_top() + Vec2::new(6.0, 4.0), Align2::LEFT_TOP, lane_name(lane), FontId::proportional(11.0), theme::TEXT);
        if !editable {
            continue;
        }
        let state = app.project.timeline.track(lane);
        // track target: source edits and Lift / Extract go here
        let pill = Rect::from_min_size(Pos2::new(row.right() - 26.0, row.top() + 3.0), Vec2::new(21.0, 15.0));
        if pill.intersect(body).is_positive() {
            let tip = if state.target {
                "Targeted: Insert / Overwrite / Connect from the source land on this track; Lift ( ; ) and Extract ( ' ) cut it. Click to untarget"
            } else {
                "Target this track for source edits and Lift / Extract"
            };
            let r = ui.interact(pill.intersect(body), id.with((lane, "target")), Sense::click()).on_hover_text(tip);
            let fill = if state.target {
                theme::SELECTION
            } else if r.hovered() {
                Color32::from_white_alpha(30)
            } else {
                Color32::from_white_alpha(10)
            };
            clipped.rect_filled(pill, CornerRadius::same(3), fill);
            clipped.text(pill.center(), Align2::CENTER_CENTER, "◎", FontId::proportional(11.0), if state.target { Color32::BLACK } else { theme::TEXT_DIM });
            if r.clicked() {
                change = Some(TrackState { target: !state.target, ..state });
            }
        }
        for (n, (glyph, on, colour, tip)) in switches(&state).into_iter().enumerate() {
            let min = Pos2::new(row.left() + 5.0 + n as f32 * (BUTTON.x + 3.0), row.top() + 22.0);
            let button = Rect::from_min_size(min, BUTTON);
            let visible = button.intersect(body);
            if !visible.is_positive() {
                continue;
            }
            let r = ui.interact(visible, id.with((lane, n)), Sense::click()).on_hover_text(tip);
            let fill = if on {
                colour
            } else if r.hovered() {
                Color32::from_white_alpha(30)
            } else {
                Color32::from_white_alpha(10)
            };
            clipped.rect_filled(button, CornerRadius::same(3), fill);
            let ink = if on { Color32::BLACK } else { theme::TEXT_DIM };
            clipped.text(button.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(11.5), ink);
            if n == 0 && on {
                clipped.line_segment([button.left_bottom(), button.right_top()], Stroke::new(1.5_f32, ink));
            }
            if r.clicked() {
                let mut track = state;
                let flag = match n {
                    0 => &mut track.hidden,
                    1 => &mut track.muted,
                    2 => &mut track.solo,
                    _ => &mut track.locked,
                };
                *flag = !*flag;
                change = Some(track);
            }
        }
    }
    if let Some(track) = change {
        app.exec(Command::SetTrack { track });
    }
}

#[cfg(test)]
mod tests {
    use super::lane_name;

    #[test]
    fn lanes_are_numbered_outwards_from_the_storyline() {
        assert_eq!([2, 1, 0, -1, -2].map(lane_name), ["V3", "V2", "V1 Main", "A1", "A2"].map(String::from));
    }
}
