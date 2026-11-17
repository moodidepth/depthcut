use nle_core::time::FrameRate;
use nle_core::timeline::VideoFormat;
use nle_core::{Project, Time};
use nle_render::export::*;

#[test]
fn discord_presets_bitrate_logic() {
    let presets = [GamingDeliveryPreset::DiscordUnder25Mb, GamingDeliveryPreset::DiscordUnder50Mb];
    let durations = [5.0, 15.0, 30.0, 60.0, 120.0, 300.0];

    for preset in presets {
        let target = preset.target_bytes().unwrap();
        let audio = preset.audio_kbps();
        let mut last_video = u32::MAX;

        for d in durations {
            let video = size_budget_video_kbps(target, d, audio);
            let total_bits = (video as f64 + audio as f64) * 1000.0 * d;
            assert!(total_bits / 8.0 <= target as f64 || video == 150);
            assert!(fits_size_budget(target, d, video, audio));
            
            if video > 150 {
                assert!(video <= last_video);
                last_video = video;
            }
        }
    }
}

#[test]
fn size_budget_edge_cases() {
    let target = 25_000_000;
    let audio = 128;
    assert!(size_budget_video_kbps(target, 0.0, audio) > 0);
    assert!(size_budget_video_kbps(target, -1.0, audio) > 0);
    assert!(size_budget_video_kbps(target, f64::NAN, audio) > 0);
    assert!(size_budget_video_kbps(target, f64::INFINITY, audio) == 150);
}

#[test]
fn youtube_shorts_presets() {
    let yt1440 = GamingDeliveryPreset::YouTube1440p60;
    assert_eq!(yt1440.frame(), Some((2560, 1440, FrameRate::FPS_60)));
    assert_eq!(yt1440.video_kbps(10.0), Some(25_000));
    assert_eq!(yt1440.audio_kbps(), 320);

    let yt4k = GamingDeliveryPreset::YouTube4k60;
    assert_eq!(yt4k.frame(), Some((3840, 2160, FrameRate::FPS_60)));
    assert_eq!(yt4k.video_kbps(10.0), Some(40_000));

    let shorts = GamingDeliveryPreset::TikTokShorts1080x1920;
    assert_eq!(shorts.frame(), Some((1080, 1920, FrameRate::FPS_60)));
    assert_eq!(shorts.video_kbps(10.0), Some(18_000));
}

#[test]
fn apply_discord_settings() {
    let mut settings = ExportSettings::new("out.mp4", ExportPreset::H264Software, &Project::default());
    settings.start = Time::ZERO;
    settings.end = Time::from_secs(60);
    
    let preset = GamingDeliveryPreset::DiscordUnder25Mb;
    preset.apply(&mut settings, VideoFormat::default(), &ExportPreset::ALL);
    
    assert!(settings.size_capped);
    assert_eq!(settings.width, 1280);
    assert_eq!(settings.height, 720);
    
    let args = settings.ffmpeg_args(nle_media::hwaccel::Platform::Windows, None).join(" ");
    if let RateControl::Bitrate(k) = settings.rate {
        assert!(args.contains(&format!("-maxrate {k}k")));
    } else {
        panic!("Discord preset should use Bitrate rate control");
    }
}

#[test]
fn delivery_presets_set_platform_loudness_targets() {
    let p = Project::new("t");
    let fmt = VideoFormat::default();
    let mut s = ExportSettings::new("/out/a.mp4", ExportPreset::H264Software, &p);
    assert_eq!(s.loudness_target, None, "plain export leaves the mix untouched");
    GamingDeliveryPreset::YouTube1440p60.apply(&mut s, fmt, &[ExportPreset::H264Software]);
    assert_eq!(s.loudness_target, Some(-14.0));
    GamingDeliveryPreset::TikTokShorts1080x1920.apply(&mut s, fmt, &[ExportPreset::H264Software]);
    assert_eq!(s.loudness_target, Some(-16.0));
    assert_eq!(GamingDeliveryPreset::ProjectDefault.loudness_target(), None);
}