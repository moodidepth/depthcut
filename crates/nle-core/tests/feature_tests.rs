//! Effects stack, adjustment clips, animated titles, captions, markers and
//! loudness — how the new model pieces behave on a real timeline.

use nle_core::effects::{Effect, EffectKind};
use nle_core::eval::{evaluate, LayerSource, RenderNode, CAPTION_CLIP_ID};
use nle_core::*;

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

fn clip(name: &str, dur: i64) -> Clip {
    let mut a = MediaAsset::new(name, format!("/media/{name}.mp4"), secs(dur));
    a.video = Some(VideoStreamInfo {
        width: 1920,
        height: 1080,
        frame_rate: FrameRate::FPS_30,
        codec: "h264".into(),
        pix_fmt: "yuv420p".into(),
        bit_depth: 8,
        color_space: "bt709".into(),
        color_transfer: String::new(),
        color_primaries: String::new(),
        is_vfr: false,
        pts_table: Vec::new(),
    });
    a.audio = Some(AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "aac".into() });
    Clip::from_asset(&a)
}

#[test]
fn effects_resolve_into_layers_and_keyframes_survive_blade() {
    let mut tl = Timeline::default();
    let mut c = clip("A", 10);
    let mut glow = Effect::new(EffectKind::Glow);
    glow.params[2].value = 0.8;
    glow.params[2].add_keyframe(Time::ZERO); // intensity 0.8 at 0
    glow.params[2].curve.as_mut().unwrap().set_value_at(secs(10), 2.8);
    c.effects.push(glow);
    c.effects.push(Effect::new(EffectKind::FilmGrain));
    let id = tl.append(c).unwrap();

    let comp = evaluate(&tl, secs(5));
    let RenderNode::Layer(l) = &comp.nodes[0] else { panic!() };
    assert_eq!(l.effects.len(), 2);
    assert_eq!(l.effects[0].kind, EffectKind::Glow, "stack order kept");
    assert!((l.effects[0].get("Intensity") - 1.8).abs() < 1e-3);

    tl.blade_clip(id, secs(5)).unwrap();
    let right = &tl.spine[1];
    // the right half starts where the curve was at 5 s
    assert!((right.effects[0].params[2].at(Time::ZERO) - 1.8).abs() < 1e-6);
    assert!((right.effects[0].params[2].at(secs(5)) - 2.8).abs() < 1e-6);
}

#[test]
fn adjustment_motion_blur_pulls_in_the_previous_frame() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 4)).unwrap();
    let mut adj = Clip::adjustment(secs(2));
    adj.effects.push(Effect::new(EffectKind::MotionBlur));
    tl.connect(adj, secs(1), 1).unwrap();

    let comp = evaluate(&tl, secs(2));
    assert!(comp.needs_previous_frame());
    let prev = comp.prev.as_ref().expect("previous frame for motion vectors");
    assert_eq!(prev.time, secs(2) - tl.format.rate.frame_duration());
    // the previous frame's media is requested too
    assert_eq!(comp.media_requests().len(), 2);
    // outside the adjustment clip no extra work is done
    assert!(evaluate(&tl, Time::from_millis(3_500)).prev.is_none());

    // media layers with motion blur want the next source frame
    let mut tl2 = Timeline::default();
    let mut c = clip("B", 3);
    c.effects.push(Effect::new(EffectKind::MotionBlur));
    tl2.append(c).unwrap();
    assert_eq!(evaluate(&tl2, secs(1)).next_frame_requests().len(), 1);
}

#[test]
fn rate_conform_feeds_frame_synthesis() {
    let mut tl = Timeline::default();
    let mut c = clip("A", 3);
    c.rate_conform = FrameBlendMode::OpticalFlow;
    tl.append(c).unwrap();
    let comp = evaluate(&tl, secs(1));
    let RenderNode::Layer(l) = &comp.nodes[0] else { panic!() };
    assert!(matches!(l.source, LayerSource::Media { blend: FrameBlendMode::OpticalFlow, .. }));
    assert_eq!(comp.next_frame_requests().len(), 1);
}

#[test]
fn animated_title_layers_carry_their_state() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 5)).unwrap();
    let p = TitleParams { text: "Pop".into(), animation: TextAnimation::PopBounce, anim_duration: 0.5, ..Default::default() };
    tl.connect(Clip::title(p, secs(3)), secs(1), 1).unwrap();
    let at = |t: Time| {
        let comp = evaluate(&tl, t);
        comp.nodes.iter().find_map(|n| match n {
            RenderNode::Layer(l) => match &l.source {
                LayerSource::Title(_, a) => Some(*a),
                _ => None,
            },
            _ => None,
        })
    };
    assert!(at(secs(1)).unwrap().scale < 0.01);
    assert!((at(secs(3)).unwrap().scale - 1.0).abs() < 1e-3);
}

#[test]
fn captions_render_on_top_and_are_undoable() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 6)).unwrap();
    let words =
        [CaptionWord { start: secs(1), end: Time::from_millis(1_400), text: "Hello".into() }, CaptionWord { start: Time::from_millis(1_400), end: secs(2), text: "world".into() }];
    let track = CaptionTrack { segments: CaptionTrack::from_words(&words, 42, Time::from_millis(700)), ..Default::default() };
    let mut h = History::new(10);
    h.execute(&mut tl, Command::SetCaptions { captions: Box::new(track) }).unwrap();

    let comp = evaluate(&tl, Time::from_millis(1_500));
    let RenderNode::Layer(top) = comp.nodes.last().unwrap() else { panic!() };
    assert_eq!(top.clip, CAPTION_CLIP_ID);
    let LayerSource::Title(p, anim) = &top.source else { panic!() };
    assert_eq!(p.text, "HELLO WORLD");
    assert_eq!(anim.highlight_word, Some(1), "second word is being spoken");
    assert_eq!(evaluate(&tl, secs(3)).nodes.len(), 1, "no caption between segments");

    // the sync offset delays the caption and its word highlight together
    tl.captions.style.sync_offset_ms = 200;
    assert_eq!(evaluate(&tl, Time::from_millis(1_100)).nodes.len(), 1, "not yet shown");
    let comp = evaluate(&tl, Time::from_millis(1_500));
    let RenderNode::Layer(top) = comp.nodes.last().unwrap() else { panic!() };
    let LayerSource::Title(_, anim) = &top.source else { panic!() };
    assert_eq!(anim.highlight_word, Some(0), "first word at caption time 1.3 s");
    tl.captions.style.sync_offset_ms = 0;

    tl.captions.enabled = false;
    assert_eq!(evaluate(&tl, Time::from_millis(1_500)).nodes.len(), 1);
    tl.captions.enabled = true;
    h.undo(&mut tl);
    assert!(tl.captions.is_empty());
}

#[test]
fn beat_markers_snap_and_replace() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 10)).unwrap();
    let beats: Vec<Marker> = [500, 1_000, 1_500].iter().map(|ms| Marker::beat(Time::from_millis(*ms))).collect();
    let mut h = History::new(10);
    let markers = tl.with_markers(&beats, Time::from_millis(20));
    h.execute(&mut tl, Command::SetMarkers { markers }).unwrap();
    assert_eq!(tl.markers.len(), 3);
    let (t, snapped) = tl.snap(Time::from_millis(1_030), Time::from_millis(50), &[], None);
    assert!(snapped);
    assert_eq!(t, Time::from_millis(1_000));
    // re-analysing replaces instead of duplicating
    let again: Vec<Marker> = [505, 1_000].iter().map(|ms| Marker::beat(Time::from_millis(*ms))).collect();
    let merged = tl.with_markers(&again, Time::from_millis(20));
    assert_eq!(merged.len(), 3);
    h.undo(&mut tl);
    assert!(tl.markers.is_empty());
}

#[test]
fn insert_gap_command() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 4)).unwrap();
    let mut h = History::new(10);
    h.execute(&mut tl, Command::InsertGap { at: secs(2), duration: secs(3) }).unwrap();
    assert_eq!(tl.duration(), secs(7));
    assert!(tl.spine[1].is_gap());
    assert!(h.execute(&mut tl, Command::InsertGap { at: secs(1), duration: Time::ZERO }).is_err());
}

#[test]
fn loudness_normalize_and_credits() {
    let mut a = AudioParams { loudness_gain_db: 6.0, ..Default::default() };
    assert!((a.gain_at(Time::ZERO, secs(1)) - 1.0).abs() < 1e-6, "off by default");
    a.loudness_normalize = true;
    assert!((a.gain_at(Time::ZERO, secs(1)) - 10f32.powf(6.0 / 20.0)).abs() < 1e-4);

    let cc0 = Attribution { title: "Rain".into(), creator: "x".into(), license: "CC0 1.0".into(), ..Default::default() };
    assert!(!cc0.requires_credit());
    let by = Attribution {
        title: "Thunder".into(),
        creator: "Jane".into(),
        license: "CC BY 4.0".into(),
        provider: "Freesound".into(),
        source_url: "https://freesound.org/s/1".into(),
        ..Default::default()
    };
    assert!(by.requires_credit());
    let line = by.credit_line();
    assert!(line.contains("\"Thunder\" by Jane (Freesound), licensed under CC BY 4.0"), "{line}");
}

#[test]
fn projects_from_before_these_features_still_load() {
    // a minimal project JSON without effects, markers, captions or the new title/audio fields
    let mut p = Project::new("Old");
    p.timeline.append(clip("A", 2)).unwrap();
    p.timeline.connect(Clip::title(TitleParams::default(), secs(1)), Time::ZERO, 1).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&p.to_json().unwrap()).unwrap();
    let tl = v["timeline"].as_object_mut().unwrap();
    tl.remove("markers");
    tl.remove("captions");
    let spine = tl["spine"].as_array_mut().unwrap();
    for c in spine.iter_mut() {
        let o = c.as_object_mut().unwrap();
        o.remove("effects");
        o.remove("rate_conform");
        for k in ["loudness_normalize", "loudness_gain_db", "vocal_clarity"] {
            o["audio"].as_object_mut().unwrap().remove(k);
        }
        for cc in o["connected"].as_array_mut().unwrap() {
            let t = cc["kind"]["Title"].as_object_mut().unwrap();
            for k in ["animation", "anim_duration", "outline_enabled", "outline_color", "outline_width", "highlight_color", "word_times"] {
                t.remove(k);
            }
        }
    }
    let back = Project::from_json(&v.to_string()).expect("old project loads");
    assert!(back.timeline.markers.is_empty() && back.timeline.captions.is_empty());
    let ClipKind::Title(t) = &back.timeline.spine[0].connected[0].kind else { panic!() };
    assert_eq!(t.animation, TextAnimation::None);
    assert!((t.anim_duration - 0.6).abs() < 1e-6);
}

// ---- Research-driven upgrades: roll edits, J/L split edits, expressive easing ----

#[test]
fn roll_edit_moves_the_edit_point_without_changing_length() {
    let mut tl = Timeline::default();
    let a = tl.append(clip("A", 10)).unwrap();
    let b = tl.append(clip("B", 10)).unwrap();
    tl.trim_end(a, secs(-3)).unwrap(); // A: 7 s, 3 s of media slack after
    tl.trim_start(b, secs(2)).unwrap(); // B: 8 s, source_in 2 s
    let total = tl.duration();
    assert_eq!(total, secs(15));

    let mut h = History::default();
    h.execute(&mut tl, Command::Roll { left: a, delta: secs(1) }).unwrap();
    assert_eq!(tl.duration(), total, "roll is length-neutral");
    assert_eq!(tl.spine[0].duration, secs(8));
    assert_eq!(tl.spine[1].duration, secs(7));
    assert_eq!(tl.spine[1].source_in, secs(3), "incoming clip's in-point follows the edit");

    // Clamped by A's media end (only 2 s of slack left).
    tl.roll(a, secs(10)).unwrap();
    assert_eq!(tl.spine[0].duration, secs(10));
    assert_eq!(tl.duration(), total);
    assert!(tl.roll(a, secs(1)).is_err(), "no media left -> AtBoundary");

    // Clamped by B's handle (source_in 5 s) going left.
    tl.roll(a, secs(-100)).unwrap();
    assert_eq!(tl.spine[1].source_in, Time::ZERO);
    assert_eq!(tl.spine[0].duration, secs(5));
    assert_eq!(tl.duration(), total);

    // Last clip has no right neighbour.
    assert!(tl.roll(b, secs(1)).is_err());

    // Undo restores the original edit point.
    h.undo(&mut tl);
    assert_eq!(tl.spine[0].duration, secs(7));
    assert_eq!(h.undo_label(), None);
}

#[test]
fn split_edit_extends_audio_into_neighbours_clamped_to_media() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 10)).unwrap();
    let b = tl.append(clip("B", 10)).unwrap();
    tl.trim_start(b, secs(2)).unwrap(); // B at 10 s, source_in 2 s
    tl.trim_end(b, secs(-3)).unwrap(); //  B 5 s long, 3 s media after

    let mut h = History::default();
    h.execute(&mut tl, Command::SetAudioSplit { id: b, lead: secs(5), tail: secs(1) }).unwrap();
    assert_eq!(tl.spine[1].audio_split(), (secs(2), secs(1)), "lead clamped to available pre-roll");

    let spans = nle_core::eval::audio_spans(&tl);
    let sb = spans.iter().find(|s| s.clip == b).unwrap();
    assert_eq!(sb.start, secs(8), "J-cut: B's audio starts 2 s before its picture");
    assert_eq!(sb.end(), secs(16), "L-cut: B's audio runs 1 s past its picture");
    assert_eq!(sb.source_time(secs(8)), Time::ZERO, "source stays in sync with picture");
    assert!(sb.gain_at(secs(8)) > 0.99, "no gap/mute in the lead region");
    assert_eq!(tl.duration(), secs(15), "split edits never move picture");

    h.undo(&mut tl);
    assert_eq!(tl.spine[1].audio_split(), (Time::ZERO, Time::ZERO));
}

#[test]
fn split_edit_fades_cover_the_extended_range() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 10)).unwrap();
    let b = tl.append(clip("B", 10)).unwrap();
    tl.trim_start(b, secs(4)).unwrap();
    tl.set_audio_split(b, secs(2), Time::ZERO).unwrap();
    let mut c = tl.spine[1].clone();
    c.audio.fade_in = secs(2);
    tl.spine[1] = c;
    let sb = nle_core::eval::audio_spans(&tl).into_iter().find(|s| s.clip == b).unwrap();
    assert!(sb.gain_at(sb.start) < 0.05, "fade-in begins at the J-cut head, not the picture cut");
    assert!(sb.gain_at(secs(10)) > 0.95, "fully up by the picture cut");
}

#[test]
fn expressive_easing_presets() {
    use nle_core::keyframe::Interpolation as I;
    for i in [I::Whip, I::Overshoot, I::Bounce] {
        assert!(I::ALL.contains(&i));
        assert!(i.ease(0.0).abs() < 1e-9 && (i.ease(1.0) - 1.0).abs() < 1e-9, "{i:?} endpoints");
    }
    // Whip is slower than ease-in-out early and faster in the middle.
    assert!(I::Whip.ease(0.15) < I::EaseInOut.ease(0.15));
    // Overshoot goes past the target before settling.
    let peak = (1..100).map(|k| I::Overshoot.ease(k as f64 / 100.0)).fold(0.0, f64::max);
    assert!(peak > 1.05 && peak < 1.2, "overshoot peak {peak}");
    // Bounce touches the target (1.0) at the first rebound and never exceeds it.
    assert!((I::Bounce.ease(1.0 / 2.75) - 1.0).abs() < 1e-9);
    assert!((0..=100).all(|k| I::Bounce.ease(k as f64 / 100.0) <= 1.0 + 1e-9));
}

#[test]
fn polish_audio_is_role_aware_and_one_undo_step() {
    use nle_core::audio_polish::{polish_audio_command, MUSIC_BED_DB};
    let mut tl = Timeline::default();
    let voice = tl.append(clip("Voice", 6)).unwrap();
    let mut bed = clip("Bed", 6);
    bed.audio.role = AudioRole::Music;
    let bed = tl.append(bed).unwrap();
    let mut sfx = clip("Sfx", 6);
    sfx.audio.role = AudioRole::Effects;
    let sfx = tl.append(sfx).unwrap();

    let cmd = polish_audio_command(&tl, &mut |_| Some(-20.0)).expect("changes");
    let mut h = History::default();
    h.execute(&mut tl, cmd).unwrap();
    assert_eq!(h.undo_len(), 1);
    assert_eq!(h.undo_label(), Some("Polish Audio"));

    let v = tl.clip(voice).unwrap();
    assert!(v.audio.loudness_normalize && (v.audio.loudness_gain_db - 6.0).abs() < 1e-6 && v.audio.vocal_clarity);
    let b = tl.clip(bed).unwrap();
    assert!(b.audio.ducking.enabled && !b.audio.vocal_clarity && b.audio.volume_db.value == MUSIC_BED_DB);
    let s = tl.clip(sfx).unwrap();
    assert!(s.audio.loudness_normalize && !s.audio.vocal_clarity && !s.audio.ducking.enabled);

    // Idempotent: a second run changes nothing.
    assert!(polish_audio_command(&tl, &mut |_| Some(-20.0)).is_none());
    h.undo(&mut tl);
    assert!(!tl.clip(voice).unwrap().audio.vocal_clarity);
}

#[test]
fn interchange_urls_keep_synthetic_paths_and_windows_drives() {
    use nle_core::fcpxml::{path_to_url, url_to_path};
    use std::path::{Path, PathBuf};
    let demo = "synthetic://Golden Hour?hue=0.08";
    assert_eq!(url_to_path(&path_to_url(Path::new(demo))), PathBuf::from(demo));
    // what earlier builds exported for demo media
    assert_eq!(url_to_path("file:///synthetic://Golden%20Hour%3Fhue%3D0.08"), PathBuf::from(demo));
    let win = r"C:\Media\Ocean Drive.mp4";
    let url = path_to_url(Path::new(win));
    assert_eq!(url, "file:///C:/Media/Ocean%20Drive.mp4");
    assert_eq!(url_to_path(&url), PathBuf::from("C:/Media/Ocean Drive.mp4"));
    assert_eq!(url_to_path(&path_to_url(Path::new("/home/me/a b.mov"))), PathBuf::from("/home/me/a b.mov"));
}
