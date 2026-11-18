//! Hardware-acceleration planning for the Intel Arc A750.
//!
//! Decode priority: Intel Quick Sync (`qsv`) with a child device derived
//! from VA-API (`iHD` driver, Linux) or D3D11VA (Windows) → plain VA-API /
//! D3D11VA hwaccel → multi-threaded software decode. The Arc A750 (DG2,
//! Alchemist) decodes H.264, HEVC Main/Main10 (8/10-bit), VP9 and AV1 in
//! hardware and has two media engines for AV1/HEVC/H.264 encode.

use crate::ffmpeg_cli;
use std::collections::BTreeSet;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    Windows,
    Linux,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HwBackend {
    Qsv,
    Vaapi,
    D3d11va,
    Software,
}

impl HwBackend {
    pub fn label(self) -> &'static str {
        match self {
            HwBackend::Qsv => "Intel Quick Sync",
            HwBackend::Vaapi => "VA-API",
            HwBackend::D3d11va => "D3D11VA",
            HwBackend::Software => "Software",
        }
    }
}

/// Chroma subsampling of a stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Chroma {
    #[default]
    Yuv420,
    Yuv422,
    Yuv444,
}

impl Chroma {
    /// From an FFmpeg pixel format name (`yuv422p10le`, `yuvj444p`, `y210le`, ...).
    pub fn from_pix_fmt(pix_fmt: &str) -> Chroma {
        let f = pix_fmt.to_ascii_lowercase();
        if f.contains("444") || f.starts_with("gbr") || f.starts_with("rgb") || f.starts_with("bgr") || f.starts_with("argb") || f.starts_with("xv3") || f.starts_with("vuy") {
            Chroma::Yuv444
        } else if f.contains("422") || f.starts_with("yuyv") || f.starts_with("uyvy") || f.starts_with("y210") || f.starts_with("y212") || f.starts_with("p21") {
            Chroma::Yuv422
        } else {
            Chroma::Yuv420
        }
    }

    /// Software format to `hwdownload` a QSV / VA-API surface of this layout to.
    /// 4:2:2 surfaces are packed (Y210 / YUY2), verified on an Arc A750 with
    /// HEVC 4:2:2 8- and 10-bit: P010 / NV12 downloads fail there.
    pub fn hw_download_format(self, bit_depth: u8) -> &'static str {
        match (self, bit_depth > 8) {
            (Chroma::Yuv422, true) => "y210le",
            (Chroma::Yuv422, false) => "yuyv422",
            (_, true) => "p010le",
            (_, false) => "nv12",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    Vp9,
    Mpeg2,
    ProRes,
    Other,
}

impl VideoCodec {
    pub fn from_ffmpeg_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" => VideoCodec::H264,
            "hevc" | "h265" | "hvc1" => VideoCodec::Hevc,
            "av1" | "av01" => VideoCodec::Av1,
            "vp9" => VideoCodec::Vp9,
            "mpeg2video" | "mpeg2" => VideoCodec::Mpeg2,
            "prores" => VideoCodec::ProRes,
            _ => VideoCodec::Other,
        }
    }

    /// Name of FFmpeg's QSV decoder for this codec.
    pub fn qsv_decoder(self) -> Option<&'static str> {
        match self {
            VideoCodec::H264 => Some("h264_qsv"),
            VideoCodec::Hevc => Some("hevc_qsv"),
            VideoCodec::Av1 => Some("av1_qsv"),
            VideoCodec::Vp9 => Some("vp9_qsv"),
            VideoCodec::Mpeg2 => Some("mpeg2_qsv"),
            _ => None,
        }
    }
}

/// Fixed-function media capabilities of a GPU.
#[derive(Clone, Debug)]
pub struct GpuProfile {
    pub name: &'static str,
    pub vendor_id: u32,
    pub vram_mb: u32,
    /// (codec, max bit depth) decodable in hardware.
    pub decode: &'static [(VideoCodec, u8)],
    pub media_engines: u8,
}

pub const INTEL_ARC_A750: GpuProfile = GpuProfile {
    name: "Intel Arc A750 (DG2)",
    vendor_id: 0x8086,
    vram_mb: 8192,
    decode: &[(VideoCodec::H264, 8), (VideoCodec::Hevc, 12), (VideoCodec::Av1, 12), (VideoCodec::Vp9, 12), (VideoCodec::Mpeg2, 8)],
    media_engines: 2,
};

impl GpuProfile {
    pub fn can_decode(&self, codec: VideoCodec, bit_depth: u8) -> bool {
        self.decode.iter().any(|(c, d)| *c == codec && bit_depth <= *d)
    }
}

/// What the installed FFmpeg build + driver can actually do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HwCaps {
    pub hwaccels: BTreeSet<String>,
    pub encoders: BTreeSet<String>,
    pub decoders: BTreeSet<String>,
}

impl HwCaps {
    pub fn has_hwaccel(&self, name: &str) -> bool {
        self.hwaccels.contains(name)
    }
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.contains(name)
    }
    pub fn has_decoder(&self, name: &str) -> bool {
        self.decoders.contains(name)
    }

    /// Assume a full-featured FFmpeg build (used for planning tests and when
    /// probing isn't possible).
    pub fn assume_full(platform: Platform) -> Self {
        let mut c = HwCaps::default();
        c.hwaccels.insert("qsv".into());
        match platform {
            Platform::Windows => {
                c.hwaccels.insert("d3d11va".into());
            }
            _ => {
                c.hwaccels.insert("vaapi".into());
            }
        }
        for e in ["av1_qsv", "hevc_qsv", "h264_qsv", "h264_vaapi", "hevc_vaapi", "libx264", "libx265", "aac"] {
            c.encoders.insert(e.into());
        }
        for d in ["h264_qsv", "hevc_qsv", "av1_qsv", "vp9_qsv", "mpeg2_qsv"] {
            c.decoders.insert(d.into());
        }
        c
    }

    /// Parse `ffmpeg -hwaccels`, `-encoders` and `-decoders` output.
    pub fn from_cli_output(hwaccels: &str, encoders: &str, decoders: &str) -> Self {
        let mut c = HwCaps::default();
        let mut in_list = false;
        for line in hwaccels.lines() {
            let l = line.trim();
            if l.starts_with("Hardware acceleration methods") {
                in_list = true;
                continue;
            }
            if in_list && !l.is_empty() {
                c.hwaccels.insert(l.to_string());
            }
        }
        let codec_list = |text: &str, out: &mut BTreeSet<String>| {
            let mut started = false;
            for line in text.lines() {
                let l = line.trim();
                if l.starts_with("------") {
                    started = true;
                    continue;
                }
                if started {
                    let mut it = l.split_whitespace();
                    if let (Some(_flags), Some(name)) = (it.next(), it.next()) {
                        out.insert(name.to_string());
                    }
                }
            }
        };
        codec_list(encoders, &mut c.encoders);
        codec_list(decoders, &mut c.decoders);
        c
    }
}

static CAPS: OnceLock<HwCaps> = OnceLock::new();

/// Probe the local FFmpeg once (cached). Returns empty caps if FFmpeg is
/// not installed.
pub fn probe_caps() -> &'static HwCaps {
    CAPS.get_or_init(|| {
        if !ffmpeg_cli::available() {
            return HwCaps::default();
        }
        let run = |arg: &str| ffmpeg_cli::run_capture(&ffmpeg_cli::ffmpeg_path(), &["-hide_banner", arg]).unwrap_or_default();
        let caps = HwCaps::from_cli_output(&run("-hwaccels"), &run("-encoders"), &run("-decoders"));
        log::info!("ffmpeg hwaccels: {:?}", caps.hwaccels);
        caps
    })
}

/// Filter chain that uploads system-memory NV12 frames to QSV surfaces.
///
/// On Windows (D3D11VA child device) an explicit `extra_hw_frames` pool makes
/// the D3D11 texture-array allocation fail with `E_INVALIDARG` (verified on
/// an Arc A750 with FFmpeg 9.0), so the default dynamic pool is used. The
/// VA-API child device on Linux needs the fixed, enlarged pool.
pub fn qsv_upload_filter(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => "hwupload,format=qsv",
        _ => "hwupload=extra_hw_frames=64,format=qsv",
    }
}

/// Arguments that create the QSV device (with derived child device) for
/// this platform. The device is named `hw`.
pub fn qsv_device_args(platform: Platform) -> Vec<String> {
    match platform {
        Platform::Windows => vec!["-init_hw_device".into(), "d3d11va=dx:,vendor_id=0x8086".into(), "-init_hw_device".into(), "qsv=hw@dx".into()],
        _ => vec!["-init_hw_device".into(), "vaapi=va:/dev/dri/renderD128,driver=iHD".into(), "-init_hw_device".into(), "qsv=hw@va".into()],
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderPlan {
    pub backend: HwBackend,
    /// Explicit decoder (`-c:v`), if any.
    pub decoder: Option<String>,
    /// Input-side FFmpeg arguments (placed before `-i`).
    pub input_args: Vec<String>,
    pub reason: String,
}

/// Choose the decode path for a stream, in priority order. Returns every
/// viable plan so callers can fall back if one fails at runtime.
///
/// Chroma: 4:2:2 goes to QSV only for HEVC (Arc decodes HEVC Rext 4:2:2; its
/// H.264 4:2:2 fails with "Error querying IO surface"), and 4:4:4 never does
/// (untested). D3D11VA / VA-API stay in the list: FFmpeg itself falls back to
/// software there when the GPU can't decode the profile.
pub fn plan_decode(codec: VideoCodec, bit_depth: u8, chroma: Chroma, platform: Platform, caps: &HwCaps, gpu: &GpuProfile) -> Vec<DecoderPlan> {
    let mut plans = Vec::new();
    let hw_ok = gpu.can_decode(codec, bit_depth);
    let qsv_chroma_ok = match chroma {
        Chroma::Yuv420 => true,
        Chroma::Yuv422 => codec == VideoCodec::Hevc,
        Chroma::Yuv444 => false,
    };
    if hw_ok {
        if let Some(dec) = codec.qsv_decoder().filter(|_| qsv_chroma_ok) {
            if caps.has_hwaccel("qsv") && (caps.decoders.is_empty() || caps.has_decoder(dec)) {
                let mut args = qsv_device_args(platform);
                args.extend(["-hwaccel", "qsv", "-hwaccel_device", "hw", "-c:v", dec].map(String::from));
                plans.push(DecoderPlan {
                    backend: HwBackend::Qsv,
                    decoder: Some(dec.into()),
                    input_args: args,
                    reason: format!("{} {}-bit {:?} on {} via QSV", dec, bit_depth, chroma, gpu.name),
                });
            }
        }
        match platform {
            Platform::Linux if caps.has_hwaccel("vaapi") => plans.push(DecoderPlan {
                backend: HwBackend::Vaapi,
                decoder: None,
                input_args: ["-hwaccel", "vaapi", "-hwaccel_device", "/dev/dri/renderD128"].map(String::from).to_vec(),
                reason: "VA-API (iHD) hwaccel".into(),
            }),
            Platform::Windows if caps.has_hwaccel("d3d11va") => plans.push(DecoderPlan {
                backend: HwBackend::D3d11va,
                decoder: None,
                input_args: ["-hwaccel", "d3d11va"].map(String::from).to_vec(),
                reason: "D3D11VA hwaccel".into(),
            }),
            _ => {}
        }
    }
    let threads = crate::threads::MAX_MEDIA_WORKERS.to_string();
    plans.push(DecoderPlan {
        backend: HwBackend::Software,
        decoder: None,
        input_args: vec!["-threads".into(), threads],
        reason: if hw_ok { "software fallback".into() } else { format!("{codec:?} {bit_depth}-bit not supported by {} — software decode", gpu.name) },
    });
    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_prefers_qsv_for_modern_codecs() {
        for platform in [Platform::Windows, Platform::Linux] {
            let caps = HwCaps::assume_full(platform);
            for (codec, depth) in [(VideoCodec::H264, 8), (VideoCodec::Hevc, 10), (VideoCodec::Av1, 10)] {
                let plans = plan_decode(codec, depth, Chroma::Yuv420, platform, &caps, &INTEL_ARC_A750);
                assert_eq!(plans[0].backend, HwBackend::Qsv, "{codec:?}");
                assert_eq!(plans.last().unwrap().backend, HwBackend::Software);
                let joined = plans[0].input_args.join(" ");
                match platform {
                    Platform::Windows => {
                        assert!(joined.contains("d3d11va=dx"));
                        assert_eq!(plans[1].backend, HwBackend::D3d11va);
                    }
                    _ => {
                        assert!(joined.contains("driver=iHD"));
                        assert_eq!(plans[1].backend, HwBackend::Vaapi);
                    }
                }
            }
        }
    }

    #[test]
    fn legacy_codecs_fall_back_to_software() {
        let caps = HwCaps::assume_full(Platform::Linux);
        let plans = plan_decode(VideoCodec::ProRes, 10, Chroma::Yuv422, Platform::Linux, &caps, &INTEL_ARC_A750);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].backend, HwBackend::Software);
        // 10-bit H.264 (Hi10P) is not decodable on Arc
        let plans = plan_decode(VideoCodec::H264, 10, Chroma::Yuv420, Platform::Linux, &caps, &INTEL_ARC_A750);
        assert_eq!(plans[0].backend, HwBackend::Software);
        // no hw support in ffmpeg build → software
        let plans = plan_decode(VideoCodec::Hevc, 8, Chroma::Yuv420, Platform::Linux, &HwCaps::default(), &INTEL_ARC_A750);
        assert_eq!(plans[0].backend, HwBackend::Software);
    }

    #[test]
    fn four_two_two_uses_qsv_only_for_hevc() {
        let caps = HwCaps::assume_full(Platform::Windows);
        let plans = plan_decode(VideoCodec::Hevc, 10, Chroma::Yuv422, Platform::Windows, &caps, &INTEL_ARC_A750);
        assert_eq!(plans[0].backend, HwBackend::Qsv);
        let plans = plan_decode(VideoCodec::H264, 8, Chroma::Yuv422, Platform::Windows, &caps, &INTEL_ARC_A750);
        assert!(plans.iter().all(|p| p.backend != HwBackend::Qsv), "{plans:?}");
        let plans = plan_decode(VideoCodec::Hevc, 10, Chroma::Yuv444, Platform::Windows, &caps, &INTEL_ARC_A750);
        assert!(plans.iter().all(|p| p.backend != HwBackend::Qsv), "{plans:?}");
        assert_eq!(Chroma::from_pix_fmt("yuv422p10le"), Chroma::Yuv422);
        assert_eq!(Chroma::from_pix_fmt("yuvj444p"), Chroma::Yuv444);
        assert_eq!(Chroma::from_pix_fmt("yuv420p"), Chroma::Yuv420);
        assert_eq!(Chroma::Yuv422.hw_download_format(10), "y210le");
        assert_eq!(Chroma::Yuv422.hw_download_format(8), "yuyv422");
        assert_eq!(Chroma::Yuv420.hw_download_format(10), "p010le");
    }

    #[test]
    fn parses_ffmpeg_listing() {
        let hw = "Hardware acceleration methods:\nvdpau\nqsv\nvaapi\n";
        let enc =
            "Encoders:\n V..... = Video\n ------\n V....D av1_qsv              AV1 (Intel Quick Sync Video acceleration) (codec av1)\n V....D h264_qsv  H264\n A....D aac   AAC\n";
        let caps = HwCaps::from_cli_output(hw, enc, "");
        assert!(caps.has_hwaccel("qsv") && caps.has_hwaccel("vaapi"));
        assert!(caps.has_encoder("av1_qsv") && caps.has_encoder("aac"));
        assert!(!caps.has_encoder("hevc_qsv"));
    }
}
