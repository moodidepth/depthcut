//! Built-in demo project so the editor is usable with no media or FFmpeg.

use nle_core::{AudioRole, Clip, DuckingSettings, Project, Time, TitleParams, Transition, TransitionKind};
use nle_media::probe::{synthetic_asset, synthetic_music};

pub fn demo_project() -> Project {
    let mut p = Project::new("Demo Project");
    let assets =
        [synthetic_asset("Golden Hour", 14, 0.08), synthetic_asset("Ocean Drive", 12, 0.55), synthetic_asset("City Lights", 16, 0.78), synthetic_asset("Forest Walk", 10, 0.33)];
    let music = synthetic_music("Score - Ambient Bed", 40);
    let ids: Vec<_> = assets.iter().map(|a| p.add_asset(a.clone())).collect();
    p.add_asset(music.clone());
    let a: Vec<_> = ids.iter().map(|id| p.asset(*id).cloned().expect("asset")).collect();

    let tl = &mut p.timeline;
    let mut c0 = Clip::from_asset_range(&a[0], Time::from_secs(1), Time::from_secs(6));
    c0.transition_in = Some(Transition::new(TransitionKind::DipToBlack, Time::from_millis(800)));
    c0.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, Time::from_secs(1)));
    let mut c1 = Clip::from_asset_range(&a[1], Time::from_secs(2), Time::from_secs(5));
    c1.transition_out = Some(Transition::new(TransitionKind::Push, Time::from_millis(700)));
    let c2 = Clip::from_asset_range(&a[2], Time::ZERO, Time::from_secs(6));
    let mut c3 = Clip::from_asset_range(&a[3], Time::from_secs(1), Time::from_secs(5));
    c3.transition_out = Some(Transition::new(TransitionKind::DipToBlack, Time::from_secs(1)));
    for c in [c0, c1, c2, c3] {
        let _ = tl.append(c);
    }
    let title = Clip::title(TitleParams::lower_third("Depthcut", "Magnetic timeline demo"), Time::from_secs(4));
    let _ = tl.connect(title, Time::from_millis(1500), 1);
    let mut bed = Clip::from_asset_range(&music, Time::ZERO, Time::from_secs(22));
    bed.audio.role = AudioRole::Music;
    bed.audio.volume_db.value = -8.0;
    bed.audio.fade_in = Time::from_secs(1);
    bed.audio.fade_out = Time::from_secs(2);
    bed.audio.ducking = DuckingSettings { enabled: true, ..Default::default() };
    let _ = tl.connect(bed, Time::ZERO, -1);
    p
}
