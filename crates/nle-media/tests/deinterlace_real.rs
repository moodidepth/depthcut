//! Interlaced sources: a 25i MPEG-2 file made from fast motion is probed as
//! upper field first, and decoding it with deinterlacing removes the comb
//! (odd lines no longer disagree with their neighbours). Skips itself
//! without FFmpeg.

use nle_core::{Deinterlace, FieldOrder, Time};
use nle_media::decode::{self, DecodeOptions};
use nle_media::{ffmpeg_cli, probe};
use std::path::PathBuf;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Mean difference of each luma line from the average of the lines above and
/// below it: large when alternate lines come from different moments.
fn combing(f: &nle_media::VideoFrame) -> f64 {
    assert_eq!(f.format.bytes_per_sample(), 1, "8-bit source");
    let (w, h, s) = (f.width as usize, f.height as usize, f.stride());
    let y = &f.data[..s * h];
    let mut sum = 0.0;
    for row in 1..h - 1 {
        for x in 0..w {
            let up = y[(row - 1) * s + x] as f64;
            let down = y[(row + 1) * s + x] as f64;
            sum += (y[row * s + x] as f64 - 0.5 * (up + down)).abs();
        }
    }
    sum / ((h - 2) * w) as f64
}

#[test]
fn interlaced_file_is_detected_and_deinterlaced() {
    if !ffmpeg_cli::available() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let dir = Dir(std::env::temp_dir().join(format!("rcp-deint-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("interlaced.mpg");
    // a bar sweeping sideways at 50 pictures/s, woven into 25 interlaced frames
    let graph = "[0][1]overlay=x='mod(t*1200,680)':y=0,tinterlace=mode=interleave_top,setfield=tff";
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=black:s=720x576:r=50", "-f", "lavfi", "-i", "color=white:s=40x576:r=50"])
        .args(["-filter_complex", graph, "-frames:v", "25", "-c:v", "mpeg2video", "-q:v", "2", "-flags", "+ilme+ildct"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());

    let mut asset = probe::probe(&path).unwrap();
    assert_eq!(asset.field_order, FieldOrder::TopFirst, "probed scan type");
    assert_eq!(asset.deinterlace_fields(), Some(FieldOrder::TopFirst));

    let frame = |asset: &nle_core::MediaAsset| {
        let mut d = decode::open(asset, DecodeOptions::default()).unwrap();
        let f = d.frame_at(Time::from_secs_f64(0.4)).unwrap();
        assert_eq!((f.width, f.height), (720, 576), "same size and one frame per frame");
        let luma = &f.data[..f.stride() * f.height as usize];
        let mean = luma.iter().map(|&v| v as f64).sum::<f64>() / luma.len() as f64;
        assert!(mean > 20.0, "the white bar is in the picture (mean luma {mean:.1})");
        combing(&f)
    };
    let on = frame(&asset);
    asset.deinterlace = Deinterlace::Off;
    assert_eq!(asset.deinterlace_fields(), None);
    let off = frame(&asset);
    eprintln!("combing: woven {off:.2}, deinterlaced {on:.2}");
    assert!(off > 4.0 * on, "deinterlacing removes the comb ({off:.2} → {on:.2})");
}
