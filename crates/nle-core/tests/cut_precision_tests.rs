use nle_core::clip::*;
use nle_core::command::{Command, History};
use nle_core::project::Project;
use nle_core::time::{FrameRate, Time};
use nle_core::timeline::{EditError, Timeline, VideoFormat};
use nle_core::*;

fn asset(dur: Time) -> MediaAsset {
    let mut a = MediaAsset::new("test", "test.mp4", dur);
    a.video = Some(VideoStreamInfo {
        width: 1920,
        height: 1080,
        frame_rate: FrameRate::FPS_60,
        codec: "h264".into(),
        pix_fmt: "yuv420p".into(),
        bit_depth: 8,
        color_space: "bt709".into(),
        color_transfer: "".into(),
        color_primaries: "".into(),
        is_vfr: false,
        pts_table: Vec::new(),
    });
    a.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "aac".into() });
    a
}

#[test]
fn time_frame_round_trip() {
    let rates = [FrameRate::FPS_60, FrameRate::FPS_120, FrameRate::FPS_240, FrameRate::new(165, 1)];
    for rate in rates {
        for n in (0..100000).step_by(997) {
            let t = Time::from_frames(n, rate);
            assert_eq!(t.frames_floor(rate), n);
            assert_eq!(t.frames_round(rate), n);
        }
    }
    for rate in [FrameRate::FPS_60, FrameRate::FPS_120, FrameRate::FPS_240] {
        let frame_ticks = rate.frame_ticks();
        for n in 0..100 {
            assert_eq!(Time::from_frames(n, rate).ticks(), n * frame_ticks);
        }
    }
}

#[test]
fn time_sample_round_trip() {
    for rate in [48000, 44100] {
        for n in (0..5_000_000).step_by(12345) {
            let t = Time::from_samples(n, rate);
            assert_eq!(t.samples_round(rate), n);
        }
    }
}

#[test]
fn high_fps_blade_precision() {
    let rate = FrameRate::FPS_240;
    let mut tl = Timeline::new("test", VideoFormat { width: 1920, height: 1080, rate });
    let dur = Time::from_secs(10);
    tl.append(Clip::from_asset(&asset(dur))).unwrap();
    let id = tl.spine[0].id;
    let frame_ticks = rate.frame_ticks();

    for i in 1..1000 {
        let at = Time::from_frames(i, rate);
        let mut test_tl = tl.clone();
        let (_, r_id) = test_tl.blade_clip(id, at).unwrap();
        let left = test_tl.clip(id).unwrap();
        let right = test_tl.clip(r_id).unwrap();

        assert_eq!(left.duration + right.duration, dur);
        assert_eq!(right.source_in, left.source_in + left.duration);
        assert_eq!(left.duration.ticks() % frame_ticks, 0);
        assert_eq!(right.duration.ticks() % frame_ticks, 0);
        assert_eq!(left.source_in.ticks() % frame_ticks, 0);
    }
}

#[test]
fn ripple_trims() {
    let mut tl = Timeline::default();
    tl.append(Clip::from_asset(&asset(Time::from_secs(10)))).unwrap();
    let b = tl.append(Clip::from_asset(&asset(Time::from_secs(10)))).unwrap();
    let c = tl.append(Clip::from_asset(&asset(Time::from_secs(10)))).unwrap();

    let delta = Time::from_secs(2);
    let at = tl.clip_start(b).unwrap() + delta;
    tl.ripple_trim_start_to(b, at).unwrap();
    assert_eq!(tl.clip(b).unwrap().duration, Time::from_secs(8));
    assert_eq!(tl.clip(b).unwrap().source_in, delta);
    assert_eq!(tl.clip_start(c), Some(Time::from_secs(18)));
    assert_eq!(tl.duration(), Time::from_secs(28));

    let end_at = tl.clip_start(b).unwrap() + Time::from_secs(5);
    tl.ripple_trim_end_to(b, end_at).unwrap();
    assert_eq!(tl.clip(b).unwrap().duration, Time::from_secs(5));
    assert_eq!(tl.clip(b).unwrap().source_in, delta, "end trim must not move the in-point");
    assert_eq!(tl.clip_start(c), Some(Time::from_secs(15)));
    assert_eq!(tl.duration(), Time::from_secs(25));
}

#[test]
fn trim_edge_cases() {
    let mut tl = Timeline::default();
    let id = tl.append(Clip::from_asset(&asset(Time::from_secs(10)))).unwrap();
    let start = tl.clip_start(id).unwrap();
    let original = tl.clone();
    assert_eq!(tl.ripple_trim_start_to(id, start), Err(EditError::AtBoundary));
    assert_eq!(tl.ripple_trim_end_to(id, start + Time::from_secs(10)), Err(EditError::AtBoundary));
    assert_eq!(tl.ripple_trim_start_to(id, start + Time::from_secs(20)), Err(EditError::AtBoundary));
    assert_eq!(tl.ripple_trim_end_to(id, start), Err(EditError::AtBoundary));
    assert_eq!(tl.ripple_trim_end_to(id, start + Time::from_secs(20)), Err(EditError::AtBoundary));
    assert!(tl.ripple_trim_start_to(ClipId::new(), start + Time::from_secs(1)).is_err());
    // one tick inside either edge is a legal trim
    assert!(tl.clone().ripple_trim_start_to(id, start + Time(1)).is_ok());
    assert!(tl.clone().ripple_trim_end_to(id, start + Time::from_secs(10) - Time(1)).is_ok());
    assert_eq!(tl, original, "rejected trims must leave the timeline untouched");
}

#[test]
fn every_cut_command_undoes_and_redoes_exactly() {
    let rate = FrameRate::FPS_240;
    let mut tl = Timeline::new("t", VideoFormat { width: 1920, height: 1080, rate });
    let ids: Vec<_> = (0..3).map(|_| tl.append(Clip::from_asset(&asset(Time::from_secs(10)))).unwrap()).collect();
    let mid = tl.clip_start(ids[1]).unwrap() + Time::from_frames(777, rate);
    let cmds = [
        Command::Blade { at: mid },
        Command::BladeClip { id: ids[1], at: mid },
        Command::BladeAll { at: mid },
        Command::RippleTrimStartTo { id: ids[1], at: mid },
        Command::RippleTrimEndTo { id: ids[1], at: mid },
        Command::DeleteRipple { id: ids[1] },
        Command::DeleteWithGap { id: ids[1] },
        Command::DeleteRange { start: Time::from_frames(1201, rate), end: mid },
    ];
    for cmd in cmds {
        let label = cmd.label();
        let original = tl.clone();
        let mut h = History::default();
        h.execute(&mut tl, cmd).unwrap_or_else(|e| panic!("{label}: {e:?}"));
        let edited = tl.clone();
        assert_ne!(edited, original, "{label} changed nothing");
        assert!(h.undo(&mut tl), "{label}: undo");
        assert_eq!(tl, original, "{label}: undo must restore every tick");
        assert!(h.redo(&mut tl), "{label}: redo");
        assert_eq!(tl, edited, "{label}: redo must reproduce the edit");
        assert!(h.undo(&mut tl));
        assert_eq!(tl, original);
    }
}

#[test]
fn chained_cuts_keep_audio_and_video_on_the_same_tick() {
    // 1000 alternating Q/W/blade edits on a 240 fps timeline: the sum of
    // clip durations always equals the timeline duration and every source
    // in-point stays on both the 240 fps frame grid and the 48 kHz sample grid
    // (a 240 fps frame is exactly 200 samples).
    let rate = FrameRate::FPS_240;
    let mut tl = Timeline::new("t", VideoFormat { width: 2560, height: 1440, rate });
    let mut id = tl.append(Clip::from_asset(&asset(Time::from_secs(600)))).unwrap();
    let ft = rate.frame_ticks();
    for i in 1..=1000i64 {
        let start = tl.clip_start(id).unwrap();
        let at = start + Time::from_frames(3 + i % 7, rate);
        match i % 3 {
            0 => {
                tl.ripple_trim_start_to(id, at).unwrap();
            }
            1 => id = tl.blade_clip(id, at).unwrap().1,
            _ => {
                let (l, r) = tl.blade_clip(id, at).unwrap();
                tl.ripple_trim_end_to(l, start + Time::from_frames(2, rate)).unwrap();
                id = r;
            }
        }
        let total: i64 = tl.spine.iter().map(|c| c.duration.ticks()).sum();
        assert_eq!(total, tl.duration().ticks());
    }
    for c in &tl.spine {
        assert_eq!(c.source_in.ticks() % ft, 0);
        assert_eq!(c.duration.ticks() % ft, 0);
        assert_eq!(Time::from_samples(c.source_in.samples_round(48_000), 48_000), c.source_in);
    }
}

#[test]
fn project_round_trip_precision() {
    let rate = FrameRate::FPS_120;
    let mut project = Project::new("test");
    project.timeline.format.rate = rate;
    let asset_id = project.add_asset(asset(Time::from_secs(100)));
    let mut id = project.timeline.append(Clip::from_asset(project.asset(asset_id).unwrap())).unwrap();

    for i in 1..50 {
        let at = Time::from_frames(i, rate);
        let (_, r) = project.timeline.blade_clip(id, at).unwrap();
        id = r;
    }

    let json = project.to_json().unwrap();
    let loaded = Project::from_json(&json).unwrap();

    for (a, b) in project.timeline.spine.iter().zip(&loaded.timeline.spine) {
        assert_eq!(a.source_in.ticks(), b.source_in.ticks());
        assert_eq!(a.duration.ticks(), b.duration.ticks());
    }
}
