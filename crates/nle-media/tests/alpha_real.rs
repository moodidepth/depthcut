//! Alpha media: a red picture whose left half is opaque, next quarter half
//! transparent and last quarter fully transparent, written as QuickTime
//! Animation, ProRes 4444 and WebM VP9 (alpha side channel), is probed as
//! alpha and decodes to straight RGBA with the alpha intact — from the
//! source and from its ProRes 4444 proxy, never from a legacy 4:2:0 proxy.
//! Skips itself without FFmpeg.

use nle_core::Time;
use nle_media::decode::{self, DecodeOptions};
use nle_media::{ffmpeg_cli, probe, proxy, PixelFormat, VideoFrame};
use std::path::{Path, PathBuf};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const W: u32 = 64;
const H: u32 = 32;

const ALPHA_RAMP: &str = "format=rgba,geq=r=255:g=0:b=0:a='if(lt(X,32),255,if(lt(X,48),128,0))'";

fn make(path: &Path, codec: &[&str]) {
    make_with(path, ALPHA_RAMP, codec)
}

fn make_with(path: &Path, vf: &str, codec: &[&str]) {
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", &format!("nullsrc=s={W}x{H}:r=25:d=1")])
        .args(["-vf", vf])
        .args(codec)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "encode {}", path.display());
}

fn pixel(f: &VideoFrame, x: u32) -> [u8; 4] {
    let i = ((H / 2 * W + x) * 4) as usize;
    [f.data[i], f.data[i + 1], f.data[i + 2], f.data[i + 3]]
}

fn check(f: &VideoFrame, what: &str, tol: i32) {
    assert_eq!(f.format, PixelFormat::Rgba8, "{what}: decoded as RGBA");
    assert_eq!((f.width, f.height), (W, H), "{what}: source size (proxies are scaled back up)");
    let near = |v: u8, want: i32| (v as i32 - want).abs() <= tol;
    let [r, g, b, a] = pixel(f, 12);
    assert!(near(a, 255) && near(r, 255) && g < 40 && b < 40, "{what}: opaque red, got {:?}", [r, g, b, a]);
    let a = pixel(f, 40)[3];
    assert!(near(a, 128), "{what}: half transparent, alpha {a}");
    let a = pixel(f, 58)[3];
    assert!(a as i32 <= tol, "{what}: transparent, alpha {a}");
}

#[test]
fn alpha_media_decode_to_straight_rgba_and_keep_it_in_proxies() {
    if !ffmpeg_cli::available() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let dir = Dir(std::env::temp_dir().join(format!("rcp-alpha-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let cases: [(&str, &[&str], i32); 3] = [
        ("anim.mov", &["-c:v", "qtrle"], 2),
        ("prores.mov", &["-c:v", "prores_ks", "-profile:v", "4", "-pix_fmt", "yuva444p10le"], 6),
        ("vp9.webm", &["-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-crf", "10", "-b:v", "0"], 14),
    ];
    for (name, codec, tol) in cases {
        let path = dir.0.join(name);
        make(&path, codec);
        let asset = probe::probe(&path).unwrap();
        let v = asset.video.as_ref().unwrap();
        assert!(v.has_alpha && v.alpha(), "{name}: probed alpha ({} {})", v.codec, v.pix_fmt);
        let mut d = decode::open(&asset, DecodeOptions::default()).unwrap();
        check(&d.frame_at(Time::from_secs_f64(0.2)).unwrap(), name, tol);
    }

    // the WebM's proxy is ProRes 4444 and decodes with its alpha
    let mut asset = probe::probe(&dir.0.join("vp9.webm")).unwrap();
    let out = proxy::transcode(&asset, &dir.0.join("proxies"), |_, _| {}).unwrap();
    assert_eq!(out.extension().unwrap(), "mov");
    asset.proxy_path = Some(out);
    let opts = DecodeOptions { use_proxy: true, max_height: None };
    assert!(decode::proxy_usable(&asset, opts));
    let mut d = decode::open(&asset, opts).unwrap();
    check(&d.frame_at(Time::from_secs_f64(0.2)).unwrap(), "ProRes 4444 proxy", 16);

    // a proxy made before alpha support (4:2:0 .mp4) is passed over for the source
    let legacy = dir.0.join("vp9_proxy.mp4");
    make(&legacy, &["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
    asset.proxy_path = Some(legacy);
    assert!(!decode::proxy_usable(&asset, opts));
    let mut d = decode::open(&asset, opts).unwrap();
    check(&d.frame_at(Time::from_secs_f64(0.2)).unwrap(), "source instead of a legacy proxy", 14);

    // an opaque source's 4:2:0 proxy is half size; frames still come back at
    // the source size (export decodes proxies with no max height), so the
    // red left half / blue right half split stays in the middle
    let opaque = dir.0.join("opaque.mp4");
    make_with(&opaque, "format=rgb24,geq=r='if(lt(X,32),255,0)':g=0:b='if(lt(X,32),0,255)'", &["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
    let mut asset = probe::probe(&opaque).unwrap();
    assert!(!asset.video.as_ref().unwrap().alpha());
    asset.proxy_path = Some(proxy::transcode(&asset, &dir.0.join("proxies"), |_, _| {}).unwrap());
    let mut d = decode::open(&asset, opts).unwrap();
    let f = d.frame_at(Time::from_secs_f64(0.2)).unwrap();
    assert_eq!((f.width, f.height), (W, H));
    let (left, right) = (f.rgb_at(26, H / 2), f.rgb_at(38, H / 2));
    assert!(left[0] > 200 && left[2] < 60, "left of the split is red: {left:?}");
    assert!(right[2] > 200 && right[0] < 60, "right of the split is blue: {right:?}");
}
