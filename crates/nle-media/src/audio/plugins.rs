//! Bus-insert plug-ins of either format: CLAP ([`clap_host`]) or VST3
//! ([`vst3_host`]), told apart by the file extension.
//!
//! [`clap_host`]: super::clap_host
//! [`vst3_host`]: super::vst3_host

use super::clap_host::{self, ClapInstance};
pub use super::clap_host::{ParamInfo, PluginInfo};
use super::vst3_host::{self, Vst3Instance};
use std::path::Path;

/// Whether `path` is a VST3 plug-in (`.vst3` file or bundle).
pub fn is_vst3(path: &Path) -> bool {
    path.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3"))
}

/// "VST3" or "CLAP", for labels.
pub fn format_name(path: &Path) -> &'static str {
    if is_vst3(path) {
        "VST3"
    } else {
        "CLAP"
    }
}

/// The effects inside one `.clap` or `.vst3`.
pub fn describe(path: &Path) -> Result<Vec<PluginInfo>, String> {
    if is_vst3(path) {
        vst3_host::describe(path)
    } else {
        clap_host::describe(path)
    }
}

/// CLAP and VST3 effects in the standard folders (loads every library: call
/// it on demand).
pub fn scan() -> (Vec<PluginInfo>, Vec<String>) {
    let (mut found, mut errors) = clap_host::scan();
    let (v, e) = vst3_host::scan();
    found.extend(v);
    errors.extend(e);
    (found, errors)
}

/// Parameters of a plug-in for the editor (cached per plug-in).
pub fn param_infos(path: &Path, id: &str) -> Result<Vec<ParamInfo>, String> {
    if is_vst3(path) {
        vst3_host::param_infos(path, id)
    } else {
        clap_host::param_infos(path, id)
    }
}

/// A loaded insert of either format.
pub enum PluginInstance {
    Clap(ClapInstance),
    Vst3(Box<Vst3Instance>),
}

impl PluginInstance {
    pub fn load(path: &Path, id: &str, sample_rate: u32) -> Result<PluginInstance, String> {
        if is_vst3(path) {
            Vst3Instance::load(path, id, sample_rate).map(|i| PluginInstance::Vst3(Box::new(i)))
        } else {
            ClapInstance::load(path, id, sample_rate).map(PluginInstance::Clap)
        }
    }
    pub fn param_infos(&self) -> Vec<ParamInfo> {
        match self {
            PluginInstance::Clap(i) => i.param_infos(),
            PluginInstance::Vst3(i) => i.param_infos(),
        }
    }
    pub fn param_value(&self, id: u32) -> Option<f64> {
        match self {
            PluginInstance::Clap(i) => i.param_value(id),
            PluginInstance::Vst3(i) => i.param_value(id),
        }
    }
    pub fn set_params(&mut self, values: &[(u32, f64)]) {
        match self {
            PluginInstance::Clap(i) => i.set_params(values),
            PluginInstance::Vst3(i) => i.set_params(values),
        }
    }
    /// Interleaved stereo, in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        match self {
            PluginInstance::Clap(i) => i.process(buf),
            PluginInstance::Vst3(i) => i.process(buf),
        }
    }
    pub fn reset(&mut self) {
        match self {
            PluginInstance::Clap(i) => i.reset(),
            PluginInstance::Vst3(i) => i.reset(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_by_extension() {
        let clap = clap_host::test_plugin();
        let vst3 = vst3_host::test_plugin();
        assert_eq!((format_name(&clap), format_name(&vst3)), ("CLAP", "VST3"));
        for path in [clap, vst3] {
            let d = describe(&path).unwrap();
            let mut p = PluginInstance::load(&path, &d[0].id, 48_000).unwrap();
            assert_eq!(p.param_infos(), param_infos(&path, &d[0].id).unwrap());
            let mut buf = vec![0.5f32; 256];
            p.process(&mut buf);
            p.reset();
            p.process(&mut buf);
            assert!(buf.iter().all(|s| s.is_finite()));
        }
    }
}
