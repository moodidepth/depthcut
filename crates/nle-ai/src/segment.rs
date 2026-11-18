//! Local AI mattes: person / subject segmentation and monocular depth with
//! ONNX models, run fully offline.
//!
//! The model files are user-supplied (licences vary) and live in
//! `%LOCALAPPDATA%/RustCutPro/models/`:
//!
//! * `person.onnx`: a portrait-matting / segmentation model with one RGB
//!   input (`1×3×H×W`, values normalised with `(x − 0.5) / 0.5`, e.g. MODNet)
//!   and one `1×1×H×W` output in 0..1 (probabilities or logits, see
//!   [`OutputMap`]).
//! * `depth.onnx`: a relative depth model (`1×3×H×W` input, ImageNet
//!   mean / std, e.g. Depth Anything V2 Small) whose output grows towards the
//!   camera (inverse depth / disparity).
//!
//! Inference needs the `onnx` cargo feature (the `ort` crate with
//! `load-dynamic`): ONNX Runtime is loaded at run time from
//! `onnxruntime.dll` next to the executable, the models folder, or
//! `ORT_DYLIB_PATH`, so builds without it still start. Pre- and
//! post-processing are plain functions so they are tested without a model.

use nle_core::MatteKind;
use nle_media::matte::Matte;
use std::path::PathBuf;

/// How raw model output becomes a 0..1 matte.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputMap {
    /// Already 0..1 (clamped); values outside 0..1 are treated as logits.
    Probability,
    /// Normalised per frame by its 1st / 99th percentiles (relative depth).
    Normalize,
}

/// Input / output conventions of a matte model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelSpec {
    pub kind: MatteKind,
    /// Input size used when the model's own input is dynamic.
    pub size: (u32, u32),
    pub mean: [f32; 3],
    pub std: [f32; 3],
    pub output: OutputMap,
}

impl ModelSpec {
    pub fn for_kind(kind: MatteKind) -> Self {
        match kind {
            MatteKind::Person => ModelSpec { kind, size: (512, 512), mean: [0.5; 3], std: [0.5; 3], output: OutputMap::Probability },
            MatteKind::Depth => ModelSpec { kind, size: (518, 518), mean: [0.485, 0.456, 0.406], std: [0.229, 0.224, 0.225], output: OutputMap::Normalize },
        }
    }
}

/// Expected model file for `kind`.
pub fn model_path(kind: MatteKind) -> PathBuf {
    nle_media::paths::models_dir().join(format!("{}.onnx", kind.slug()))
}

/// True when this build can run ONNX models at all.
pub const fn onnx_supported() -> bool {
    cfg!(feature = "onnx")
}

/// Bilinear RGBA8 sample at pixel-centre coordinates.
fn sample_rgba(rgba: &[u8], w: u32, h: u32, x: f32, y: f32, c: usize) -> f32 {
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let g = |x: u32, y: u32| rgba[((y * w + x) * 4) as usize + c] as f32;
    let top = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * fx;
    let bot = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * fx;
    top + (bot - top) * fy
}

/// RGBA8 `w × h` → normalised planar `1×3×ih×iw` f32 (stretched to the
/// model input; the output is stretched back, so aspect is preserved).
pub fn preprocess(rgba: &[u8], w: u32, h: u32, spec: &ModelSpec, input: (u32, u32)) -> Vec<f32> {
    let (iw, ih) = (input.0.max(1), input.1.max(1));
    let plane = (iw * ih) as usize;
    let mut out = vec![0.0f32; plane * 3];
    if w == 0 || h == 0 || rgba.len() < (w * h * 4) as usize {
        return out;
    }
    let (sx, sy) = (w as f32 / iw as f32, h as f32 / ih as f32);
    for y in 0..ih {
        let fy = (y as f32 + 0.5) * sy - 0.5;
        for x in 0..iw {
            let fx = (x as f32 + 0.5) * sx - 0.5;
            let i = (y * iw + x) as usize;
            for c in 0..3 {
                let v = sample_rgba(rgba, w, h, fx, fy, c) / 255.0;
                out[c * plane + i] = (v - spec.mean[c]) / spec.std[c];
            }
        }
    }
    out
}

/// Raw `ow × oh` model output → 8-bit matte of `target` size.
pub fn postprocess(raw: &[f32], ow: u32, oh: u32, spec: &ModelSpec, target: (u32, u32)) -> Option<Matte> {
    if ow == 0 || oh == 0 || raw.len() < (ow * oh) as usize {
        return None;
    }
    let raw = &raw[..(ow * oh) as usize];
    let map: Box<dyn Fn(f32) -> f32> = match spec.output {
        OutputMap::Probability => {
            let logits = raw.iter().any(|v| *v < -0.01 || *v > 1.01);
            Box::new(move |v: f32| if logits { 1.0 / (1.0 + (-v).exp()) } else { v.clamp(0.0, 1.0) })
        }
        OutputMap::Normalize => {
            let mut s: Vec<f32> = raw.iter().copied().filter(|v| v.is_finite()).collect();
            if s.is_empty() {
                return None;
            }
            s.sort_by(f32::total_cmp);
            let lo = s[s.len() / 100];
            let hi = s[(s.len() * 99 / 100).min(s.len() - 1)];
            let span = (hi - lo).max(1e-6);
            Box::new(move |v: f32| ((v - lo) / span).clamp(0.0, 1.0))
        }
    };
    let (tw, th) = (target.0.max(1), target.1.max(1));
    let (sx, sy) = (ow as f32 / tw as f32, oh as f32 / th as f32);
    let g = |x: u32, y: u32| map(raw[(y * ow + x) as usize]);
    let mut data = Vec::with_capacity((tw * th) as usize);
    for y in 0..th {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (oh - 1) as f32);
        let (y0, ty) = (fy.floor() as u32, fy.fract());
        let y1 = (y0 + 1).min(oh - 1);
        for x in 0..tw {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (ow - 1) as f32);
            let (x0, tx) = (fx.floor() as u32, fx.fract());
            let x1 = (x0 + 1).min(ow - 1);
            let top = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * tx;
            let bot = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * tx;
            data.push(((top + (bot - top) * ty) * 255.0).round().clamp(0.0, 255.0) as u8);
        }
    }
    Matte::new(tw, th, data)
}

/// Matte-weighted centroid (normalised) and coverage of a person matte;
/// `None` when nobody is in frame (coverage under 0.5 %).
pub fn subject_centroid(m: &Matte) -> Option<([f32; 2], f32)> {
    let (mut sx, mut sy, mut sw) = (0.0f64, 0.0f64, 0.0f64);
    for y in 0..m.height {
        for x in 0..m.width {
            let w = m.data[(y * m.width + x) as usize] as f64 / 255.0;
            if w > 0.5 {
                sx += w * (x as f64 + 0.5);
                sy += w * (y as f64 + 0.5);
                sw += w;
            }
        }
    }
    let cover = sw / (m.width * m.height) as f64;
    (cover >= 0.005).then(|| ([(sx / sw / m.width as f64) as f32, (sy / sw / m.height as f64) as f32], cover as f32))
}

/// A loaded matte model.
pub struct MatteModel {
    pub spec: ModelSpec,
    #[cfg(feature = "onnx")]
    session: ort::session::Session,
    #[cfg(feature = "onnx")]
    input: (u32, u32),
}

impl MatteModel {
    /// Load `models/<kind>.onnx` (CPU, or DirectML when ONNX Runtime ships it).
    #[cfg(not(feature = "onnx"))]
    pub fn load(kind: MatteKind) -> Result<Self, String> {
        let _ = ModelSpec::for_kind(kind);
        Err("this build has no ONNX Runtime support (rebuild with `--features onnx`)".into())
    }

    #[cfg(feature = "onnx")]
    pub fn load(kind: MatteKind) -> Result<Self, String> {
        let path = model_path(kind);
        if !path.is_file() {
            return Err(format!("{} model not found: put it at {}", kind.label(), path.display()));
        }
        onnx::init_runtime()?;
        // a wrong / incompatible runtime DLL panics inside ort: report it
        let session = std::panic::catch_unwind(|| ort::session::Session::builder().and_then(|b| b.with_intra_threads(onnx::threads())).and_then(|b| b.commit_from_file(&path)))
            .map_err(|_| "ONNX Runtime failed to initialise (incompatible onnxruntime library?)".to_string())?
            .map_err(|e| format!("could not load {}: {e}", path.display()))?;
        let spec = ModelSpec::for_kind(kind);
        let input = onnx::input_size(&session).unwrap_or(spec.size);
        Ok(MatteModel { spec, session, input })
    }

    /// Run the model on an RGBA8 frame; the matte has the frame's size.
    #[cfg(not(feature = "onnx"))]
    pub fn infer(&mut self, _rgba: &[u8], _w: u32, _h: u32) -> Result<Matte, String> {
        Err("ONNX Runtime support not built".into())
    }

    #[cfg(feature = "onnx")]
    pub fn infer(&mut self, rgba: &[u8], w: u32, h: u32) -> Result<Matte, String> {
        let input = preprocess(rgba, w, h, &self.spec, self.input);
        let (raw, ow, oh) = onnx::run(&mut self.session, input, self.input)?;
        postprocess(&raw, ow, oh, &self.spec, (w, h)).ok_or_else(|| "model returned an empty output".to_string())
    }
}

#[cfg(feature = "onnx")]
mod onnx {
    use ort::session::Session;
    use ort::value::Tensor;

    pub fn threads() -> usize {
        std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8)
    }

    /// Load ONNX Runtime (once): `ORT_DYLIB_PATH`, next to the executable,
    /// then the models folder.
    pub fn init_runtime() -> Result<(), String> {
        static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        INIT.get_or_init(|| {
            let name = if cfg!(windows) {
                "onnxruntime.dll"
            } else if cfg!(target_os = "macos") {
                "libonnxruntime.dylib"
            } else {
                "libonnxruntime.so"
            };
            let mut candidates: Vec<std::path::PathBuf> = std::env::var_os("ORT_DYLIB_PATH").map(Into::into).into_iter().collect();
            if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
                candidates.push(dir.join(name));
            }
            candidates.push(nle_media::paths::models_dir().join(name));
            let path =
                candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| format!("ONNX Runtime ({name}) not found next to the app, in the models folder or ORT_DYLIB_PATH"))?;
            ort::init_from(path.to_string_lossy().to_string()).commit().map_err(|e| format!("ONNX Runtime: {e}"))?;
            Ok(())
        })
        .clone()
    }

    /// Fixed `(width, height)` of the first input, if the model declares one.
    pub fn input_size(s: &Session) -> Option<(u32, u32)> {
        let ort::value::ValueType::Tensor { shape, .. } = &s.inputs.first()?.input_type else { return None };
        let (h, w) = (*shape.get(2)?, *shape.get(3)?);
        (h > 0 && w > 0).then_some((w as u32, h as u32))
    }

    /// Run on a `1×3×h×w` input; returns the first output and its size.
    pub fn run(s: &mut Session, input: Vec<f32>, size: (u32, u32)) -> Result<(Vec<f32>, u32, u32), String> {
        let t = Tensor::from_array(([1usize, 3, size.1 as usize, size.0 as usize], input.into_boxed_slice())).map_err(|e| e.to_string())?;
        let outputs = s.run(ort::inputs![t]).map_err(|e| e.to_string())?;
        let (shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        let dims: Vec<i64> = shape.iter().copied().collect();
        // [1, 1, h, w], [1, h, w] or [h, w]
        let (h, w) = match dims.as_slice() {
            [.., h, w] => (*h as u32, *w as u32),
            _ => return Err(format!("unexpected output shape {dims:?}")),
        };
        Ok((data.to_vec(), w, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_normalises_planar_rgb() {
        // 2×1: pure red, pure blue
        let rgba = [255, 0, 0, 255, 0, 0, 255, 255];
        let spec = ModelSpec::for_kind(MatteKind::Person);
        let t = preprocess(&rgba, 2, 1, &spec, (2, 1));
        assert_eq!(t.len(), 6);
        // planes R, G, B; (x − 0.5) / 0.5
        assert_eq!(&t[0..2], &[1.0, -1.0]);
        assert_eq!(&t[2..4], &[-1.0, -1.0]);
        assert_eq!(&t[4..6], &[-1.0, 1.0]);
        let d = preprocess(&rgba, 2, 1, &ModelSpec::for_kind(MatteKind::Depth), (4, 2));
        assert_eq!(d.len(), 4 * 2 * 3);
        assert!((d[0] - (1.0 - 0.485) / 0.229).abs() < 1e-4, "ImageNet normalisation: {}", d[0]);
    }

    #[test]
    fn postprocess_maps_and_resizes() {
        let p = ModelSpec::for_kind(MatteKind::Person);
        let m = postprocess(&[0.0, 1.0, 0.0, 1.0], 2, 2, &p, (4, 2)).unwrap();
        assert_eq!((m.width, m.height), (4, 2));
        assert_eq!(m.data[0], 0);
        assert_eq!(m.data[3], 255);
        // logits go through a sigmoid
        let l = postprocess(&[-8.0, 8.0], 2, 1, &p, (2, 1)).unwrap();
        assert!(l.data[0] < 2 && l.data[1] > 253);
        // depth: normalised to its own range, bigger = nearer
        let d = postprocess(&[10.0, 20.0, 30.0, 40.0], 4, 1, &ModelSpec::for_kind(MatteKind::Depth), (4, 1)).unwrap();
        assert!(d.data[0] < 10 && d.data[3] > 245 && d.data[1] < d.data[2]);
        assert!(postprocess(&[0.5], 2, 2, &p, (2, 2)).is_none(), "short output");
    }

    #[test]
    fn centroid_of_a_subject_on_the_right() {
        let mut data = vec![0u8; 10 * 4];
        for y in 0..4 {
            data[y * 10 + 7] = 255;
            data[y * 10 + 8] = 255;
        }
        let (c, cover) = subject_centroid(&Matte::new(10, 4, data).unwrap()).unwrap();
        assert!((c[0] - 0.8).abs() < 1e-5 && (c[1] - 0.5).abs() < 1e-5 && (cover - 0.2).abs() < 1e-5);
        assert!(subject_centroid(&Matte::new(10, 4, vec![0; 40]).unwrap()).is_none());
    }

    #[test]
    fn builds_without_onnx_report_an_error_not_a_panic() {
        if !onnx_supported() {
            assert!(MatteModel::load(MatteKind::Person).err().unwrap().contains("--features onnx"));
        }
    }
}
