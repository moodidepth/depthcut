//! CLAP plug-in hosting for the mixer bus inserts.
//!
//! Audio effects only (a stereo or mono main input and output), parameters
//! through the `clap.params` extension. No plug-in editor windows, no state
//! chunks beyond the parameter values, no latency compensation: the insert
//! runs in place on the bus block.
//!
//! Threading follows the CLAP rules where the mixer allows it: plug-ins are
//! created, activated and destroyed on the thread that calls
//! [`TimelineMixer::set_mixer`](super::TimelineMixer::set_mixer) (the UI thread
//! in playback, the export thread in export) and processed on the mixer
//! thread. Libraries stay loaded for the life of the process.

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_header, clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::version::{clap_version_is_compatible, CLAP_VERSION};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Largest block handed to a plug-in in one `process` call.
pub const MAX_BLOCK: usize = 4096;

/// A plug-in found in a `.clap` file.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInfo {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub vendor: String,
}

/// A parameter the editor can show.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub stepped: bool,
}

// ---------------------------------------------------------------------------
// Libraries
// ---------------------------------------------------------------------------

struct Library {
    _lib: libloading::Library,
    entry: *const clap_plugin_entry,
}

// The entry table is immutable static data in the plug-in.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

/// The loadable file: a `.clap` file, or the binary inside a macOS bundle.
fn binary_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        let macos = path.join("Contents").join("MacOS");
        if let Some(bin) = std::fs::read_dir(&macos).ok().and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()))) {
            return bin;
        }
    }
    path.to_path_buf()
}

/// Load and initialise a `.clap` once per process; it is never unloaded
/// (plug-ins commonly leave threads or atexit hooks behind).
fn library(path: &Path) -> Result<Arc<Library>, String> {
    static LIBS: OnceLock<Mutex<HashMap<PathBuf, Arc<Library>>>> = OnceLock::new();
    let mut libs = LIBS.get_or_init(Default::default).lock();
    if let Some(l) = libs.get(path) {
        return Ok(l.clone());
    }
    let lib = unsafe { libloading::Library::new(binary_path(path)) }.map_err(|e| format!("cannot load {}: {e}", path.display()))?;
    let entry: *const clap_plugin_entry =
        unsafe { *lib.get::<*const clap_plugin_entry>(b"clap_entry\0").map_err(|_| format!("{} is not a CLAP plug-in (no clap_entry)", path.display()))? };
    if entry.is_null() {
        return Err(format!("{}: clap_entry is null", path.display()));
    }
    let e = unsafe { &*entry };
    if !clap_version_is_compatible(e.clap_version) {
        return Err(format!("{}: unsupported CLAP version {}.{}", path.display(), e.clap_version.major, e.clap_version.minor));
    }
    let cpath = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let ok = e.init.is_some_and(|init| unsafe { init(cpath.as_ptr()) });
    if !ok {
        return Err(format!("{}: plug-in entry init failed", path.display()));
    }
    let l = Arc::new(Library { _lib: lib, entry });
    libs.insert(path.to_path_buf(), l.clone());
    Ok(l)
}

impl Library {
    fn factory(&self) -> Result<&clap_plugin_factory, String> {
        let get = unsafe { (*self.entry).get_factory }.ok_or("no get_factory")?;
        let f = unsafe { get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
        if f.is_null() {
            return Err("no plug-in factory".into());
        }
        Ok(unsafe { &*f })
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn cbuf(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The plug-ins inside one `.clap` file.
pub fn describe(path: &Path) -> Result<Vec<PluginInfo>, String> {
    let lib = library(path)?;
    let f = lib.factory()?;
    let count = f.get_plugin_count.map_or(0, |c| unsafe { c(f) });
    let mut out = Vec::new();
    for i in 0..count {
        let d = f.get_plugin_descriptor.map_or(std::ptr::null(), |g| unsafe { g(f, i) });
        if d.is_null() {
            continue;
        }
        let d = unsafe { &*d };
        out.push(PluginInfo { path: path.to_path_buf(), id: cstr(d.id), name: cstr(d.name), vendor: cstr(d.vendor) });
    }
    Ok(out)
}

/// The standard CLAP folders plus `CLAP_PATH`.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("CLAP_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    #[cfg(windows)]
    {
        if let Some(c) = std::env::var_os("COMMONPROGRAMFILES") {
            dirs.push(PathBuf::from(c).join("CLAP"));
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(l).join("Programs").join("Common").join("CLAP"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join("Library/Audio/Plug-Ins/CLAP"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join(".clap"));
        }
        dirs.push(PathBuf::from("/usr/lib/clap"));
    }
    dirs
}

/// `.clap` files under `dirs` (recursive; bundles are not descended into).
pub fn find_clap_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("clap")) {
                out.push(p);
            } else if p.is_dir() && depth < 4 {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(d, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Every plug-in in the standard folders. Loads each library, so call it
/// on demand (a "Scan" button), not at start-up.
pub fn scan() -> (Vec<PluginInfo>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for f in find_clap_files(&search_dirs()) {
        match describe(&f) {
            Ok(v) => found.extend(v),
            Err(e) => errors.push(e),
        }
    }
    (found, errors)
}

// ---------------------------------------------------------------------------
// Host callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn host_get_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    std::ptr::null()
}
unsafe extern "C" fn host_request(_: *const clap_host) {}

static HOST: clap_host = clap_host {
    clap_version: CLAP_VERSION,
    host_data: std::ptr::null_mut(),
    name: c"Depthcut".as_ptr(),
    vendor: c"Moodi Depth".as_ptr(),
    url: c"".as_ptr(),
    version: c"0.1".as_ptr(),
    get_extension: Some(host_get_extension),
    request_restart: Some(host_request),
    request_process: Some(host_request),
    request_callback: Some(host_request),
};

unsafe extern "C" fn in_size(list: *const clap_input_events) -> u32 {
    let v = unsafe { &*((*list).ctx as *const Vec<clap_event_param_value>) };
    v.len() as u32
}
unsafe extern "C" fn in_get(list: *const clap_input_events, i: u32) -> *const clap_event_header {
    let v = unsafe { &*((*list).ctx as *const Vec<clap_event_param_value>) };
    v.get(i as usize).map_or(std::ptr::null(), |e| &e.header as *const clap_event_header)
}
unsafe extern "C" fn out_push(_: *const clap_output_events, _: *const clap_event_header) -> bool {
    true
}

fn param_event(id: u32, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: id,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

// ---------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------

/// One activated plug-in instance.
pub struct ClapInstance {
    _lib: Arc<Library>,
    plugin: *const clap_plugin,
    params: *const clap_plugin_params,
    in_ch: usize,
    out_ch: usize,
    processing: bool,
    failed: bool,
    steady: i64,
    /// Parameter changes not yet sent (id, value).
    pending: Vec<clap_event_param_value>,
    /// Last value sent per parameter.
    sent: Vec<(u32, f64)>,
    in_buf: [Vec<f32>; 2],
    out_buf: [Vec<f32>; 2],
}

// A CLAP plug-in may be moved between threads; the mixer never calls it
// from two threads at once (it lives behind the mixer's lock).
unsafe impl Send for ClapInstance {}

impl ClapInstance {
    /// Create, initialise and activate plug-in `id` from `path`.
    pub fn load(path: &Path, id: &str, sample_rate: u32) -> Result<ClapInstance, String> {
        let lib = library(path)?;
        let f = lib.factory()?;
        let cid = CString::new(id).map_err(|e| e.to_string())?;
        let create = f.create_plugin.ok_or("factory has no create_plugin")?;
        let plugin = unsafe { create(f, &HOST, cid.as_ptr()) };
        if plugin.is_null() {
            return Err(format!("{}: no plug-in with id {id}", path.display()));
        }
        let p = unsafe { &*plugin };
        let destroy = |why: String| {
            if let Some(d) = p.destroy {
                unsafe { d(plugin) };
            }
            Err(why)
        };
        if !p.init.is_some_and(|i| unsafe { i(plugin) }) {
            return destroy(format!("{id}: init failed"));
        }
        let ext = |name: &CStr| p.get_extension.map_or(std::ptr::null(), |g| unsafe { g(plugin, name.as_ptr()) });
        let params = ext(CLAP_EXT_PARAMS) as *const clap_plugin_params;
        let ports = ext(CLAP_EXT_AUDIO_PORTS) as *const clap_plugin_audio_ports;
        let (in_ch, out_ch) = if ports.is_null() { (2, 2) } else { (main_port_channels(plugin, ports, true), main_port_channels(plugin, ports, false)) };
        if in_ch == 0 || out_ch == 0 {
            return destroy(format!("{id}: not an audio effect (needs an audio input and output)"));
        }
        if in_ch > 2 || out_ch > 2 {
            return destroy(format!("{id}: only mono and stereo effects are supported ({in_ch} in / {out_ch} out)"));
        }
        if !p.activate.is_some_and(|a| unsafe { a(plugin, sample_rate as f64, 1, MAX_BLOCK as u32) }) {
            return destroy(format!("{id}: activate failed"));
        }
        let ch = |n: usize| [vec![0.0; MAX_BLOCK * (n > 0) as usize], vec![0.0; MAX_BLOCK * (n > 1) as usize]];
        Ok(ClapInstance {
            _lib: lib,
            plugin,
            params,
            in_ch,
            out_ch,
            processing: false,
            failed: false,
            steady: 0,
            pending: Vec::new(),
            sent: Vec::new(),
            in_buf: ch(in_ch),
            out_buf: ch(out_ch),
        })
    }

    /// The visible, writable parameters.
    pub fn param_infos(&self) -> Vec<ParamInfo> {
        if self.params.is_null() {
            return Vec::new();
        }
        let ext = unsafe { &*self.params };
        let n = ext.count.map_or(0, |c| unsafe { c(self.plugin) });
        let mut out = Vec::new();
        for i in 0..n {
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            if !ext.get_info.is_some_and(|g| unsafe { g(self.plugin, i, &mut info) }) {
                continue;
            }
            if info.flags & (CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_READONLY) != 0 {
                continue;
            }
            out.push(ParamInfo {
                id: info.id,
                name: cbuf(&info.name),
                min: info.min_value,
                max: info.max_value,
                default: info.default_value,
                stepped: info.flags & CLAP_PARAM_IS_STEPPED != 0,
            });
        }
        out
    }

    /// The plug-in's current value of a parameter.
    pub fn param_value(&self, id: u32) -> Option<f64> {
        if self.params.is_null() {
            return None;
        }
        let mut v = 0.0;
        unsafe { &*self.params }.get_value.is_some_and(|g| unsafe { g(self.plugin, id, &mut v) }).then_some(v)
    }

    /// Queue parameter values; only changes are sent, with the next block.
    pub fn set_params(&mut self, values: &[(u32, f64)]) {
        for &(id, v) in values {
            match self.sent.iter_mut().find(|(p, _)| *p == id) {
                Some(s) if s.1 == v => continue,
                Some(s) => s.1 = v,
                None => self.sent.push((id, v)),
            }
            self.pending.retain(|e| e.param_id != id);
            self.pending.push(param_event(id, v));
        }
    }

    /// Process interleaved stereo in place. A plug-in that fails to start
    /// or returns an error is passed through from then on.
    pub fn process(&mut self, buf: &mut [f32]) {
        if self.failed {
            return;
        }
        let p = unsafe { &*self.plugin };
        if !self.processing {
            if !p.start_processing.is_none_or(|s| unsafe { s(self.plugin) }) {
                log::warn!("CLAP plug-in failed to start processing; bypassed");
                self.failed = true;
                return;
            }
            self.processing = true;
        }
        let Some(process) = p.process else {
            self.failed = true;
            return;
        };
        for block in buf.chunks_mut(2 * MAX_BLOCK) {
            let n = block.len() / 2;
            for (i, f) in block.as_chunks::<2>().0.iter().enumerate() {
                if self.in_ch == 1 {
                    self.in_buf[0][i] = 0.5 * (f[0] + f[1]);
                } else {
                    self.in_buf[0][i] = f[0];
                    self.in_buf[1][i] = f[1];
                }
            }
            let mut in_ptrs: [*mut f32; 2] = [self.in_buf[0].as_mut_ptr(), self.in_buf[1].as_mut_ptr()];
            let mut out_ptrs: [*mut f32; 2] = [self.out_buf[0].as_mut_ptr(), self.out_buf[1].as_mut_ptr()];
            let input = clap_audio_buffer { data32: in_ptrs.as_mut_ptr(), data64: std::ptr::null_mut(), channel_count: self.in_ch as u32, latency: 0, constant_mask: 0 };
            let mut output = clap_audio_buffer { data32: out_ptrs.as_mut_ptr(), data64: std::ptr::null_mut(), channel_count: self.out_ch as u32, latency: 0, constant_mask: 0 };
            let events = std::mem::take(&mut self.pending);
            let in_events = clap_input_events { ctx: &events as *const Vec<clap_event_param_value> as *mut c_void, size: Some(in_size), get: Some(in_get) };
            let out_events = clap_output_events { ctx: std::ptr::null_mut(), try_push: Some(out_push) };
            let pr = clap_process {
                steady_time: self.steady,
                frames_count: n as u32,
                transport: std::ptr::null(),
                audio_inputs: &input,
                audio_outputs: &mut output,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: &in_events,
                out_events: &out_events,
            };
            let status = unsafe { process(self.plugin, &pr) };
            self.steady += n as i64;
            // keep the allocation for the next block
            let mut events = events;
            events.clear();
            self.pending = events;
            if status == CLAP_PROCESS_ERROR {
                log::warn!("CLAP plug-in returned a processing error; bypassed");
                self.failed = true;
                return;
            }
            for (i, f) in block.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let l = self.out_buf[0][i];
                let r = if self.out_ch == 1 { l } else { self.out_buf[1][i] };
                f[0] = if l.is_finite() { l } else { 0.0 };
                f[1] = if r.is_finite() { r } else { 0.0 };
            }
        }
    }

    /// Clear delay lines / tails (after a seek).
    pub fn reset(&mut self) {
        if let Some(r) = unsafe { &*self.plugin }.reset {
            unsafe { r(self.plugin) };
        }
    }
}

fn main_port_channels(plugin: *const clap_plugin, ports: *const clap_plugin_audio_ports, input: bool) -> usize {
    let ports = unsafe { &*ports };
    let n = ports.count.map_or(0, |c| unsafe { c(plugin, input) });
    for i in 0..n {
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if ports.get.is_some_and(|g| unsafe { g(plugin, i, input, &mut info) }) {
            // the first port is the main one by convention
            return info.channel_count as usize;
        }
    }
    0
}

impl Drop for ClapInstance {
    fn drop(&mut self) {
        let p = unsafe { &*self.plugin };
        unsafe {
            if self.processing {
                if let Some(s) = p.stop_processing {
                    s(self.plugin);
                }
            }
            if let Some(d) = p.deactivate {
                d(self.plugin);
            }
            if let Some(d) = p.destroy {
                d(self.plugin);
            }
        }
    }
}

/// Parameters of a plug-in, for the editor (cached per plug-in: creating an
/// instance can be slow).
pub fn param_infos(path: &Path, id: &str) -> Result<Vec<ParamInfo>, String> {
    type Cache = HashMap<(PathBuf, String), Result<Vec<ParamInfo>, String>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let key = (path.to_path_buf(), id.to_string());
    if let Some(r) = CACHE.get_or_init(Default::default).lock().get(&key) {
        return r.clone();
    }
    let r = ClapInstance::load(path, id, 48_000).map(|i| i.param_infos());
    CACHE.get_or_init(Default::default).lock().insert(key, r.clone());
    r
}

/// Build the `clap-test-gain` fixture (its own target dir: the outer cargo
/// holds the workspace one) and return the `.clap` path.
#[cfg(test)]
pub(crate) fn test_plugin() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target = root.join("target").join("clap-test-plugin");
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let st =
                std::process::Command::new(cargo).args(["build", "--quiet", "-p", "clap-test-gain", "--target-dir"]).arg(&target).current_dir(&root).status().expect("run cargo");
            assert!(st.success(), "building clap-test-gain failed");
            let lib = if cfg!(windows) {
                "clap_test_gain.dll"
            } else if cfg!(target_os = "macos") {
                "libclap_test_gain.dylib"
            } else {
                "libclap_test_gain.so"
            };
            let dst = target.join("Test Gain.clap");
            std::fs::copy(target.join("debug").join(lib), &dst).unwrap();
            dst
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "org.rustcutpro.test-gain";

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [(i as f32 * 0.05).sin() * 0.5, (i as f32 * 0.03).cos() * 0.25]).collect()
    }

    #[test]
    fn finds_and_describes_the_plugin() {
        let path = test_plugin();
        let found = find_clap_files(&[path.parent().unwrap().to_path_buf()]);
        assert!(found.contains(&path), "{found:?}");
        let d = describe(&path).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].id.as_str(), d[0].name.as_str(), d[0].vendor.as_str()), (ID, "Test Gain", "Depthcut"));
        assert!(describe(Path::new("C:/nope/missing.clap")).is_err());
    }

    #[test]
    fn processes_with_parameters() {
        let path = test_plugin();
        let mut p = ClapInstance::load(&path, ID, 48_000).unwrap();
        let infos = p.param_infos();
        assert_eq!(infos.len(), 1, "hidden parameter not listed: {infos:?}");
        assert_eq!((infos[0].id, infos[0].name.as_str(), infos[0].min, infos[0].max, infos[0].default), (0, "Gain", -60.0, 12.0, 0.0));
        assert_eq!(param_infos(&path, ID).unwrap(), infos);

        // default 0 dB: unchanged; bigger than one plug-in block
        let x = tone(MAX_BLOCK * 2 + 100);
        let mut y = x.clone();
        p.process(&mut y);
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-6));

        // −6.02 dB halves it, sent with the next block
        p.set_params(&[(0, -6.0206)]);
        let mut y = x.clone();
        p.process(&mut y);
        assert!(x.iter().zip(&y).all(|(a, b)| (a * 0.5 - b).abs() < 1e-4));
        assert!((p.param_value(0).unwrap() + 6.0206).abs() < 1e-9);
        // an unchanged value is not re-sent; a new one is
        p.set_params(&[(0, -6.0206)]);
        assert!(p.pending.is_empty());
        p.set_params(&[(0, 0.0)]);
        assert_eq!(p.pending.len(), 1);
        assert!(ClapInstance::load(&path, "no.such.id", 48_000).is_err());
    }
}
