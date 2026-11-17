//! `.cube` 3D LUT parsing, CPU trilinear reference sampling and GPU upload.

use thiserror::Error;
use wgpu::util::DeviceExt;

#[derive(Debug, Error, PartialEq)]
pub enum LutError {
    #[error("missing LUT_3D_SIZE")]
    MissingSize,
    #[error("LUT size {0} out of range (2..=256)")]
    BadSize(usize),
    #[error("expected {expected} entries, found {found}")]
    WrongCount { expected: usize, found: usize },
    #[error("line {0}: cannot parse")]
    Parse(usize),
    #[error("I/O: {0}")]
    Io(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CubeLut {
    pub title: String,
    pub size: usize,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
    /// `size³` RGB entries, red varying fastest (Adobe/Resolve order).
    pub data: Vec<[f32; 3]>,
}

impl CubeLut {
    pub fn identity(size: usize) -> Self {
        let mut data = Vec::with_capacity(size * size * size);
        let s = (size - 1) as f32;
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    data.push([r as f32 / s, g as f32 / s, b as f32 / s]);
                }
            }
        }
        CubeLut { title: "Identity".into(), size, domain_min: [0.0; 3], domain_max: [1.0; 3], data }
    }

    pub fn load(path: &std::path::Path) -> Result<Self, LutError> {
        let s = std::fs::read_to_string(path).map_err(|e| LutError::Io(e.to_string()))?;
        Self::parse(&s)
    }

    pub fn parse(src: &str) -> Result<Self, LutError> {
        let mut size = None;
        let mut title = String::new();
        let mut dmin = [0.0f32; 3];
        let mut dmax = [1.0f32; 3];
        let mut data = Vec::new();
        for (ln, raw) in src.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let head = it.next().unwrap_or("");
            let three = |it: std::str::SplitWhitespace| -> Result<[f32; 3], LutError> {
                let v: Vec<f32> = it.map(|x| x.parse::<f32>()).collect::<Result<_, _>>().map_err(|_| LutError::Parse(ln + 1))?;
                if v.len() != 3 {
                    return Err(LutError::Parse(ln + 1));
                }
                Ok([v[0], v[1], v[2]])
            };
            match head {
                "TITLE" => title = line[5..].trim().trim_matches('"').to_string(),
                "LUT_3D_SIZE" => {
                    let n: usize = it.next().and_then(|x| x.parse().ok()).ok_or(LutError::Parse(ln + 1))?;
                    if !(2..=256).contains(&n) {
                        return Err(LutError::BadSize(n));
                    }
                    size = Some(n);
                    data.reserve(n * n * n);
                }
                "DOMAIN_MIN" => dmin = three(it)?,
                "DOMAIN_MAX" => dmax = three(it)?,
                "LUT_1D_SIZE" | "LUT_3D_INPUT_RANGE" | "LUT_1D_INPUT_RANGE" => {}
                _ if head.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => {
                    let first: f32 = head.parse().map_err(|_| LutError::Parse(ln + 1))?;
                    let rest: Vec<f32> = it.map(|x| x.parse::<f32>()).collect::<Result<_, _>>().map_err(|_| LutError::Parse(ln + 1))?;
                    if rest.len() != 2 {
                        return Err(LutError::Parse(ln + 1));
                    }
                    data.push([first, rest[0], rest[1]]);
                }
                _ => {} // unknown keyword: ignore
            }
        }
        let size = size.ok_or(LutError::MissingSize)?;
        let expected = size * size * size;
        if data.len() != expected {
            return Err(LutError::WrongCount { expected, found: data.len() });
        }
        Ok(CubeLut { title, size, domain_min: dmin, domain_max: dmax, data })
    }

    fn at(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        self.data[(b * self.size + g) * self.size + r]
    }

    /// Reference trilinear lookup (mirrors the GPU sampler).
    pub fn sample(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        let s = (n - 1) as f32;
        let mut idx = [0usize; 3];
        let mut frac = [0f32; 3];
        for c in 0..3 {
            let range = (self.domain_max[c] - self.domain_min[c]).max(1e-6);
            let v = ((rgb[c] - self.domain_min[c]) / range).clamp(0.0, 1.0) * s;
            let i = (v.floor() as usize).min(n - 2);
            idx[c] = i;
            frac[c] = v - i as f32;
        }
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
        let [r, g, b] = idx;
        let c00 = lerp(self.at(r, g, b), self.at(r + 1, g, b), frac[0]);
        let c10 = lerp(self.at(r, g + 1, b), self.at(r + 1, g + 1, b), frac[0]);
        let c01 = lerp(self.at(r, g, b + 1), self.at(r + 1, g, b + 1), frac[0]);
        let c11 = lerp(self.at(r, g + 1, b + 1), self.at(r + 1, g + 1, b + 1), frac[0]);
        lerp(lerp(c00, c10, frac[1]), lerp(c01, c11, frac[1]), frac[2])
    }

    /// Upload as an `rgba16float` 3D texture (filterable → hardware trilinear).
    pub fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::Texture {
        let mut bytes: Vec<u8> = Vec::with_capacity(self.data.len() * 8);
        for c in &self.data {
            for v in [c[0], c[1], c[2], 1.0] {
                bytes.extend_from_slice(&half::f16::from_f32(v).to_le_bytes());
            }
        }
        let n = self.size as u32;
        device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("cube-lut"),
                size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment\nTITLE \"Warm\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n\
        0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";

    #[test]
    fn parses_cube_and_samples_identity() {
        let l = CubeLut::parse(SAMPLE).unwrap();
        assert_eq!(l.title, "Warm");
        assert_eq!(l.size, 2);
        let s = l.sample([0.25, 0.5, 0.75]);
        assert!((s[0] - 0.25).abs() < 1e-6 && (s[1] - 0.5).abs() < 1e-6 && (s[2] - 0.75).abs() < 1e-6);
        let id = CubeLut::identity(33);
        let s = id.sample([0.123, 0.456, 0.789]);
        assert!((s[0] - 0.123).abs() < 1e-5 && (s[1] - 0.456).abs() < 1e-5 && (s[2] - 0.789).abs() < 1e-5);
    }

    #[test]
    fn trilinear_interpolates_non_identity() {
        // invert LUT
        let mut l = CubeLut::identity(2);
        for c in &mut l.data {
            *c = [1.0 - c[0], 1.0 - c[1], 1.0 - c[2]];
        }
        let s = l.sample([0.2, 0.4, 0.9]);
        assert!((s[0] - 0.8).abs() < 1e-6 && (s[1] - 0.6).abs() < 1e-6 && (s[2] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_files() {
        assert_eq!(CubeLut::parse("0 0 0\n"), Err(LutError::MissingSize));
        assert!(matches!(CubeLut::parse("LUT_3D_SIZE 2\n0 0 0\n"), Err(LutError::WrongCount { .. })));
        assert_eq!(CubeLut::parse("LUT_3D_SIZE 1\n"), Err(LutError::BadSize(1)));
    }
}
