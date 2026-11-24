//! Which picture a decoder hands back, up to and past the end of a file.
//! Frames are known by their pixels, against a straight FFmpeg decode: `pts`
//! is the decoder's own count and would prove nothing. Skips itself without
//! FFmpeg.

use nle_core::{MediaAsset, Time};
use nle_media::{ffmpeg_cli, FrameServer};
use std::path::PathBuf;

/// A generated file in a directory of its own, removed on drop.
struct Media {
    dir: PathBuf,
    path: PathBuf,
}

impl Media {
    fn new(name: &str, args: &[&str]) -> Option<Media> {
        if !ffmpeg_cli::available() {
            eprintln!("skipping: ffmpeg not found");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("rcp-decode-end-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let media = Media { path: dir.join(name), dir };
        let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).arg(&media.path).status().unwrap();
        assert!(status.success(), "could not create {name}");
        Some(media)
    }

    fn movie(name: &str, rate: &str, frames: &str) -> Option<Media> {
        let source = format!("testsrc2=size=320x240:rate={rate}");
        Media::new(name, &["-f", "lavfi", "-i", &source, "-frames:v", frames, "-c:v", "libx264", "-g", "15", "-bf", "2", "-pix_fmt", "yuv420p"])
    }

    fn open(&self) -> Reader {
        let out = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&self.path)
            .args(["-map", "0:v:0", "-fps_mode", "passthrough", "-pix_fmt", "nv12", "-f", "rawvideo", "pipe:1"])
            .output()
            .unwrap();
        assert!(out.status.success() && !out.stdout.is_empty(), "no reference decode");
        let pictures = out.stdout.chunks(320 * 240 * 3 / 2).map(<[u8]>::to_vec).collect();
        Reader { asset: nle_media::probe::probe(&self.path).unwrap(), server: FrameServer::new(2, None), pictures }
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Reader {
    asset: MediaAsset,
    server: FrameServer,
    pictures: Vec<Vec<u8>>,
}

impl Reader {
    /// Which picture of the file comes back for frame `n`, from a fresh decoder.
    fn fetched(&self, n: i64) -> Result<i64, String> {
        self.server.clear_sync();
        self.read(n)
    }

    /// The same, from the decoder already running. -1 is none of the file's pictures.
    fn read(&self, n: i64) -> Result<i64, String> {
        self.at(Time::from_frames(n, self.asset.frame_rate()))
    }

    /// The picture on screen at time `t`.
    fn at(&self, t: Time) -> Result<i64, String> {
        let frame = self.server.frame_sync(&self.asset, t, false).map_err(|e| e.to_string())?;
        Ok(self.pictures.iter().position(|p| *p == *frame.data).map_or(-1, |i| i as i64))
    }
}

#[test]
fn every_frame_up_to_the_last_is_the_one_asked_for() {
    for (i, rate) in ["30", "30000/1001", "24000/1001"].into_iter().enumerate() {
        let Some(media) = Media::movie(&format!("seek{i}.mp4"), rate, "40") else { return };
        let reader = media.open();
        let wrong: Vec<_> = (0..40).map(|n| (n, reader.fetched(n))).filter(|(n, got)| *got != Ok(*n)).collect();
        assert!(wrong.is_empty(), "at {rate}: {wrong:?}");
    }
}

/// Open GOPs lose their leading pictures on an input seek, transport and
/// program streams land on the next keyframe, FLV cannot seek before its start.
#[test]
fn seeks_are_exact_in_awkward_files() {
    let cases: [(&str, &[&str]); 11] = [
        ("open.mp4", &["-c:v", "libx264", "-g", "30", "-x264-params", "open-gop=1"]),
        ("open-hevc.mp4", &["-c:v", "libx265", "-g", "30", "-x265-params", "log-level=error"]),
        ("b0.mp4", &["-c:v", "libx264", "-g", "30", "-bf", "0"]),
        ("deep.mp4", &["-c:v", "libx264", "-g", "30", "-bf", "16", "-b_strategy", "0", "-x264-params", "open-gop=1"]),
        ("deep-hevc.mp4", &["-c:v", "libx265", "-g", "30", "-x265-params", "log-level=error:bframes=16:b-adapt=0"]),
        ("ms30.mkv", &["-c:v", "libx264", "-g", "30"]),
        ("ms23.mkv", &["-r", "24000/1001", "-c:v", "libx264", "-g", "30"]),
        ("ms120.mkv", &["-r", "120", "-c:v", "libx264", "-g", "30"]),
        ("ms240.mkv", &["-r", "240", "-c:v", "libx264", "-g", "30"]),
        ("long.ts", &["-c:v", "libx264", "-g", "120"]),
        ("start.flv", &["-c:v", "libx264", "-g", "30"]),
    ];
    for (name, codec) in cases {
        // long enough that a seek cannot simply decode from the top
        let frames = if name.ends_with(".ts") { "750" } else { "150" };
        let rate = codec.iter().position(|a| *a == "-r").map_or("30", |i| codec[i + 1]);
        let source = format!("testsrc2=size=320x240:rate={rate}");
        let input = ["-f", "lavfi", "-i", &source, "-frames:v", frames, "-pix_fmt", "yuv420p"];
        let Some(media) = Media::new(name, &[&input[..], codec].concat()) else { return };
        let reader = media.open();
        let mut points = vec![0, 5, 14, 29, 31, 39, 40, 41, 45, 46, 52, 59, 60, 61, 73, 89, 104, 119, 121, 149];
        if name.ends_with(".ts") {
            points.extend([300, 479, 600, 601, 719, 721, 749]);
        }
        let last: i64 = frames.parse::<i64>().unwrap() - 1;
        // the frame sought, and the one read on from it
        let wrong: Vec<_> =
            points.into_iter().map(|n| ([n, (n + 1).min(last)], [reader.fetched(n), reader.read((n + 1).min(last))])).filter(|(want, got)| *got != want.map(Ok)).collect();
        assert!(wrong.is_empty(), "{name}: {wrong:?}");
    }
}

/// A variable-rate file shows at each time the picture that is on screen
/// then, as its sound does. Pictures here come 30 a second, then 10.
#[test]
fn variable_rate_pictures_keep_to_their_times() {
    let args = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=4", "-vf", "select='lt(n,60)+not(mod(n,3))'", "-fps_mode", "vfr"];
    let Some(media) = Media::new("vfr.mp4", &[&args[..], &["-c:v", "libx264", "-pix_fmt", "yuv420p"]].concat()) else { return };
    let reader = media.open();
    assert!(reader.asset.video.as_ref().unwrap().is_vfr, "the file is not variable-rate");
    for (ms, want) in [(0, 0i64), (500, 15), (1500, 45), (3000, 70), (3500, 75)] {
        let got = reader.at(Time::from_secs_f64(ms as f64 / 1000.0)).unwrap();
        // the decoder samples at the average rate: one picture either way
        assert!((got - want).abs() <= 1, "at {ms} ms: picture {got}, not {want}");
    }
}

/// Program streams start at half a second, and flag their field rate.
#[test]
fn a_late_starting_stream_begins_with_its_first_picture() {
    let args = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-frames:v", "90", "-pix_fmt", "yuv420p", "-c:v", "mpeg1video", "-q:v", "3"];
    let Some(media) = Media::new("late.mpg", &args) else { return };
    let reader = media.open();
    assert_eq!([0, 1, 40].map(|n| reader.fetched(n)), [Ok(0), Ok(1), Ok(40)]);
}

#[test]
fn sequential_reads_deliver_every_frame_once() {
    let Some(media) = Media::movie("run.mp4", "30000/1001", "60") else { return };
    let reader = media.open();
    let got: Vec<_> = (0..60).map(|n| reader.read(n)).collect();
    assert_eq!(got, (0..60).map(Ok).collect::<Vec<_>>());
}

#[test]
fn past_the_end_holds_the_last_frame() {
    let Some(media) = Media::movie("hold.mp4", "30", "60") else { return };
    let reader = media.open();
    assert_eq!([60, 61, 120].map(|n| reader.fetched(n)), [Ok(59), Ok(59), Ok(59)]);
}

#[test]
fn a_jump_past_the_end_holds_the_last_frame_not_the_previous_one() {
    let Some(media) = Media::movie("jump.mp4", "30", "150") else { return };
    let reader = media.open();
    assert_eq!([3, 400, 149, 10].map(|n| reader.read(n)), [Ok(3), Ok(149), Ok(149), Ok(10)]);
}

#[test]
fn picture_shorter_than_its_sound_holds_its_last_frame() {
    let args = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=1", "-f", "lavfi", "-i", "sine=frequency=440:duration=20"];
    let Some(media) = Media::new("short.mp4", &[&args[..], &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"]].concat()) else { return };
    let reader = media.open();
    assert_eq!(reader.fetched(500), Ok(29));
}

#[test]
fn a_still_has_a_frame_at_any_time() {
    let Some(media) = Media::new("still.png", &["-f", "lavfi", "-i", "color=c=red:size=320x240", "-frames:v", "1"]) else { return };
    let reader = media.open();
    assert_eq!([0, 1, 45].map(|n| reader.fetched(n).map(|_| ())), [Ok(()), Ok(()), Ok(())]);
}

// The libav decoder keeps its file open, so it cannot vanish under it.
#[cfg(not(feature = "ffmpeg"))]
#[test]
fn a_file_that_vanishes_is_an_error_not_a_held_frame() {
    let Some(media) = Media::movie("gone.mp4", "30", "150") else { return };
    let reader = media.open();
    assert_eq!(reader.read(140), Ok(140));
    // reading on to the end lets go of the file
    let _ = reader.read(175);
    std::fs::remove_file(&media.path).unwrap();
    let got = reader.read(5);
    assert!(matches!(&got, Err(e) if e.contains("gone.mp4")), "{got:?}");
}
