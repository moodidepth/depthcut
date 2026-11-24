//! VST3 plug-in hosting for the mixer bus inserts, next to [`clap_host`].
//!
//! Audio effects only: a mono or stereo main input and output bus (side-chain
//! and other auxiliary busses get silence and are switched off), 32-bit float
//! processing, parameters through the edit controller. The component and a
//! separate edit controller are connected through their connection points
//! and the controller is given the component state. No plug-in editor
//! windows, no presets / state chunks beyond the parameter values, no latency
//! compensation, no host messages (`IHostApplication::createInstance` is
//! refused): the insert runs in place on the bus block.
//!
//! Threading follows the VST3 rules where the mixer allows it: instances are
//! created, set up, given parameter values and destroyed on the thread that
//! calls [`TimelineMixer::set_mixer`](super::TimelineMixer::set_mixer), and
//! processed (and reset through `setProcessing`) on the mixer thread.
//! Libraries stay loaded for the life of the process.
//!
//! [`clap_host`]: super::clap_host

use super::clap_host::{ParamInfo, PluginInfo, MAX_BLOCK};
use parking_lot::Mutex;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper, Interface};

// ---------------------------------------------------------------------------
// Libraries
// ---------------------------------------------------------------------------

struct Library {
    _lib: libloading::Library,
    factory: ComPtr<IPluginFactory>,
}

// The factory is a process-wide object meant to be used from any thread.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

/// The loadable file: the binary inside a `.vst3` bundle folder, or a
/// single-file `.vst3` (the older Windows layout).
fn binary_path(path: &Path) -> PathBuf {
    if !path.is_dir() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    let contents = path.join("Contents");
    let arch: &[&str] = if cfg!(target_os = "macos") {
        &["MacOS"]
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        &["arm64-win", "arm64x-win"]
    } else if cfg!(windows) {
        &["x86_64-win"]
    } else {
        &["x86_64-linux"]
    };
    for a in arch {
        let dir = contents.join(a);
        let mut name = stem.clone();
        if cfg!(windows) {
            name.push(".vst3");
        } else if !cfg!(target_os = "macos") {
            name.push(".so");
        }
        let p = dir.join(&name);
        if p.is_file() {
            return p;
        }
        // a binary named differently from the bundle
        if let Some(bin) = std::fs::read_dir(&dir).ok().and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path()).filter(|p| p.is_file()))) {
            return bin;
        }
    }
    path.to_path_buf()
}

/// Load a `.vst3` once per process; it is never unloaded.
fn library(path: &Path) -> Result<Arc<Library>, String> {
    static LIBS: OnceLock<Mutex<HashMap<PathBuf, Arc<Library>>>> = OnceLock::new();
    let mut libs = LIBS.get_or_init(Default::default).lock();
    if let Some(l) = libs.get(path) {
        return Ok(l.clone());
    }
    let lib = unsafe { libloading::Library::new(binary_path(path)) }.map_err(|e| format!("cannot load {}: {e}", path.display()))?;
    #[cfg(windows)]
    unsafe {
        if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0") {
            if !init() {
                return Err(format!("{}: InitDll failed", path.display()));
            }
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    unsafe {
        if let Ok(entry) = lib.get::<unsafe extern "system" fn(*mut c_void) -> bool>(b"ModuleEntry\0") {
            if !entry(std::ptr::null_mut()) {
                return Err(format!("{}: ModuleEntry failed", path.display()));
            }
        }
    }
    let get = unsafe { lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0") }
        .map_err(|_| format!("{} is not a VST3 plug-in (no GetPluginFactory)", path.display()))?;
    let factory = unsafe { ComPtr::from_raw(get()) }.ok_or_else(|| format!("{}: no plug-in factory", path.display()))?;
    let l = Arc::new(Library { _lib: lib, factory });
    libs.insert(path.to_path_buf(), l.clone());
    Ok(l)
}

/// A class id as 32 hex digits (the plug-in id stored in the project).
fn cid_hex(cid: &TUID) -> String {
    cid.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn parse_cid(id: &str) -> Option<TUID> {
    if id.len() != 32 || !id.is_ascii() {
        return None;
    }
    let mut out: TUID = [0; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&id[2 * i..2 * i + 2], 16).ok()? as c_char;
    }
    Some(out)
}

fn cbuf(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[allow(clippy::unnecessary_cast)] // TChar is not u16 on every target
fn wbuf(b: &[TChar]) -> String {
    let units: Vec<u16> = b.iter().take_while(|c| **c != 0).map(|c| *c as u16).collect();
    String::from_utf16_lossy(&units)
}

const AUDIO_MODULE: &str = "Audio Module Class";

/// The audio effects inside one `.vst3` (instruments are left out).
pub fn describe(path: &Path) -> Result<Vec<PluginInfo>, String> {
    let lib = library(path)?;
    let f = &lib.factory;
    let mut finfo: PFactoryInfo = unsafe { std::mem::zeroed() };
    let factory_vendor = if unsafe { f.getFactoryInfo(&mut finfo) } == kResultOk { cbuf(&finfo.vendor) } else { String::new() };
    let f2 = f.cast::<IPluginFactory2>();
    let mut out = Vec::new();
    for i in 0..unsafe { f.countClasses() } {
        let mut info: PClassInfo = unsafe { std::mem::zeroed() };
        if unsafe { f.getClassInfo(i, &mut info) } != kResultOk || cbuf(&info.category) != AUDIO_MODULE {
            continue;
        }
        let mut vendor = factory_vendor.clone();
        if let Some(f2) = &f2 {
            let mut i2: PClassInfo2 = unsafe { std::mem::zeroed() };
            if unsafe { f2.getClassInfo2(i, &mut i2) } == kResultOk {
                let subs = cbuf(&i2.subCategories);
                let subs: Vec<&str> = subs.split('|').collect();
                if subs.contains(&"Instrument") && !subs.contains(&"Fx") {
                    continue;
                }
                let v = cbuf(&i2.vendor);
                if !v.is_empty() {
                    vendor = v;
                }
            }
        }
        out.push(PluginInfo { path: path.to_path_buf(), id: cid_hex(&info.cid), name: cbuf(&info.name), vendor });
    }
    Ok(out)
}

/// The standard VST3 folders plus `VST3_PATH`.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("VST3_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    #[cfg(windows)]
    {
        if let Some(c) = std::env::var_os("COMMONPROGRAMFILES") {
            dirs.push(PathBuf::from(c).join("VST3"));
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(l).join("Programs").join("Common").join("VST3"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join("Library/Audio/Plug-Ins/VST3"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join(".vst3"));
        }
        dirs.push(PathBuf::from("/usr/lib/vst3"));
        dirs.push(PathBuf::from("/usr/local/lib/vst3"));
    }
    dirs
}

/// `.vst3` bundles and files under `dirs` (bundles are not descended into).
pub fn find_vst3_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3")) {
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

/// Every effect in the standard folders. Loads each library, so call it on
/// demand, not at start-up.
pub fn scan() -> (Vec<PluginInfo>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for f in find_vst3_files(&search_dirs()) {
        match describe(&f) {
            Ok(v) => found.extend(v),
            Err(e) => errors.push(e),
        }
    }
    (found, errors)
}

// ---------------------------------------------------------------------------
// Host-side COM objects
// ---------------------------------------------------------------------------

struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let name = unsafe { &mut *name };
        for (d, s) in name.iter_mut().zip("Rust Cut Pro".encode_utf16().chain(std::iter::repeat(0))) {
            *d = s as TChar;
        }
        kResultOk
    }
    unsafe fn createInstance(&self, _cid: *mut TUID, _iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if !obj.is_null() {
            unsafe { *obj = std::ptr::null_mut() };
        }
        kResultFalse
    }
}

/// In-memory `IBStream` for handing the component state to the controller.
#[derive(Default)]
struct MemStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (n.max(0) as usize).min(data.len() - pos);
        if n > 0 {
            unsafe { std::ptr::copy_nonoverlapping(data[pos..].as_ptr(), buffer as *mut u8, n) };
        }
        self.pos.set(pos + n);
        if !read.is_null() {
            unsafe { *read = n as int32 };
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        let n = n.max(0) as usize;
        let src = if n == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(buffer as *const u8, n) } };
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        data[pos..pos + n].copy_from_slice(src);
        self.pos.set(pos + n);
        if !written.is_null() {
            unsafe { *written = n as int32 };
        }
        kResultOk
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.data.borrow().len() as i64;
        let base = match mode as IBStream_::IStreamSeekMode {
            IBStream_::IStreamSeekMode_::kIBSeekSet => 0,
            IBStream_::IStreamSeekMode_::kIBSeekCur => self.pos.get() as i64,
            IBStream_::IStreamSeekMode_::kIBSeekEnd => len,
            _ => return kInvalidArgument,
        };
        let p = base + pos;
        if p < 0 {
            return kInvalidArgument;
        }
        self.pos.set(p as usize);
        if !result.is_null() {
            unsafe { *result = p };
        }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = self.pos.get() as int64 };
        kResultOk
    }
}

/// One parameter change at the start of the block.
struct ParamQueue {
    id: ParamID,
    value: Cell<ParamValue>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        1
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 || offset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        unsafe {
            *offset = 0;
            *value = self.value.get();
        }
        kResultOk
    }
    /// Output changes (meters etc.) are accepted and dropped.
    unsafe fn addPoint(&self, _offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        self.value.set(value);
        if !index.is_null() {
            unsafe { *index = 0 };
        }
        kResultOk
    }
}

/// Input changes for one block; as the output list it hands out a sink.
#[derive(Default)]
struct ParamChanges {
    queues: RefCell<Vec<ComWrapper<ParamQueue>>>,
    sink: RefCell<Option<ComWrapper<ParamQueue>>>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

fn queue_ptr(q: &ComWrapper<ParamQueue>) -> *mut IParamValueQueue {
    q.as_com_ref::<IParamValueQueue>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.borrow().len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        self.queues.borrow().get(index.max(0) as usize).map_or(std::ptr::null_mut(), queue_ptr)
    }
    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if !index.is_null() {
            unsafe { *index = 0 };
        }
        let mut sink = self.sink.borrow_mut();
        let q = sink.get_or_insert_with(|| ComWrapper::new(ParamQueue { id: 0, value: Cell::new(0.0) }));
        let _ = id;
        queue_ptr(q)
    }
}

// ---------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------

fn channels(arr: SpeakerArrangement) -> usize {
    arr.count_ones() as usize
}

/// Buffers of one bus: owned channel memory and the pointer table the
/// plug-in reads.
struct Bus {
    ptrs: Vec<*mut f32>,
}

/// One activated plug-in instance.
pub struct Vst3Instance {
    _lib: Arc<Library>,
    _host: ComWrapper<HostApp>,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// The controller is its own object (initialised / terminated by us).
    separate: bool,
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    in_ch: usize,
    out_ch: usize,
    active: bool,
    processing: bool,
    failed: bool,
    steady: i64,
    sample_rate: f64,
    input_changes: ComWrapper<ParamChanges>,
    output_changes: ComWrapper<ParamChanges>,
    /// Normalized values not yet sent (id, value).
    pending: Vec<(ParamID, f64)>,
    /// Last plain value set per parameter.
    sent: Vec<(u32, f64)>,
    in_buf: [Vec<f32>; 2],
    out_buf: [Vec<f32>; 2],
    /// Silence for auxiliary inputs, scratch for auxiliary outputs.
    _aux: [Vec<f32>; 2],
    in_busses: Vec<Bus>,
    out_busses: Vec<Bus>,
}

// A VST3 component may be moved between threads; the mixer never calls it
// from two threads at once (it lives behind the mixer's lock).
unsafe impl Send for Vst3Instance {}

fn create<I: Interface>(factory: &ComPtr<IPluginFactory>, cid: &TUID) -> Option<ComPtr<I>> {
    let mut obj: *mut c_void = std::ptr::null_mut();
    let r = unsafe { factory.createInstance(cid.as_ptr(), I::IID.as_ptr() as FIDString, &mut obj) };
    if r != kResultOk {
        return None;
    }
    unsafe { ComPtr::from_raw(obj as *mut I) }
}

impl Vst3Instance {
    /// Create, initialise, set up and activate the effect with class id `id`
    /// (32 hex digits) from `path`.
    pub fn load(path: &Path, id: &str, sample_rate: u32) -> Result<Vst3Instance, String> {
        let lib = library(path)?;
        let cid = parse_cid(id).ok_or_else(|| format!("{id}: not a VST3 class id"))?;
        let host = ComWrapper::new(HostApp);
        let host_unknown = host.as_com_ref::<FUnknown>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
        let component = create::<IComponent>(&lib.factory, &cid).ok_or_else(|| format!("{}: cannot create plug-in {id}", path.display()))?;
        if unsafe { component.initialize(host_unknown) } != kResultOk {
            return Err(format!("{id}: initialize failed"));
        }
        // from here on `inst` owns the objects and its Drop cleans up
        let processor = match component.cast::<IAudioProcessor>() {
            Some(p) => p,
            None => {
                unsafe { component.terminate() };
                return Err(format!("{id}: not an audio processor"));
            }
        };
        let mut inst = Vst3Instance {
            _lib: lib.clone(),
            _host: host,
            component,
            processor,
            controller: None,
            separate: false,
            connection: None,
            in_ch: 0,
            out_ch: 0,
            active: false,
            processing: false,
            failed: false,
            steady: 0,
            sample_rate: sample_rate as f64,
            input_changes: ComWrapper::new(ParamChanges::default()),
            output_changes: ComWrapper::new(ParamChanges::default()),
            pending: Vec::new(),
            sent: Vec::new(),
            in_buf: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            out_buf: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            _aux: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
            in_busses: Vec::new(),
            out_busses: Vec::new(),
        };
        inst.attach_controller(&lib, host_unknown);
        inst.setup_busses().map_err(|e| format!("{id}: {e}"))?;
        let p = &inst.processor;
        if unsafe { p.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as int32) } != kResultTrue {
            return Err(format!("{id}: no 32-bit float processing"));
        }
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: MAX_BLOCK as int32,
            sampleRate: sample_rate as f64,
        };
        if unsafe { p.setupProcessing(&mut setup) } != kResultOk {
            return Err(format!("{id}: setupProcessing failed"));
        }
        if unsafe { inst.component.setActive(1) } != kResultOk {
            return Err(format!("{id}: activate failed"));
        }
        inst.active = true;
        inst.sync_controller();
        Ok(inst)
    }

    /// The edit controller: the component itself, or a separate object
    /// created from the component's controller class and connected to it.
    fn attach_controller(&mut self, lib: &Library, host: *mut FUnknown) {
        if let Some(c) = self.component.cast::<IEditController>() {
            self.controller = Some(c);
            return;
        }
        let mut cid: TUID = [0; 16];
        if unsafe { self.component.getControllerClassId(&mut cid) } != kResultOk || cid == [0; 16] {
            return;
        }
        let Some(c) = create::<IEditController>(&lib.factory, &cid) else { return };
        if unsafe { c.initialize(host) } != kResultOk {
            return;
        }
        self.separate = true;
        if let (Some(a), Some(b)) = (self.component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>()) {
            unsafe {
                a.connect(b.as_ptr());
                b.connect(a.as_ptr());
            }
            self.connection = Some((a, b));
        }
        self.controller = Some(c);
    }

    /// Hand the component state to a separate controller.
    fn sync_controller(&self) {
        let (true, Some(c)) = (self.separate, &self.controller) else { return };
        let stream = ComWrapper::new(MemStream::default());
        let Some(s) = stream.as_com_ref::<IBStream>() else { return };
        if unsafe { self.component.getState(s.as_ptr()) } == kResultOk {
            stream.pos.set(0);
            unsafe { c.setComponentState(s.as_ptr()) };
        }
    }

    /// Main busses mono / stereo, auxiliary busses off (silence in, scratch
    /// out), and the pointer tables for `process`.
    fn setup_busses(&mut self) -> Result<(), String> {
        let audio = MediaTypes_::kAudio as MediaType;
        let (input, output) = (BusDirections_::kInput as BusDirection, BusDirections_::kOutput as BusDirection);
        let c = &self.component;
        let n_in = unsafe { c.getBusCount(audio, input) }.max(0) as usize;
        let n_out = unsafe { c.getBusCount(audio, output) }.max(0) as usize;
        if n_in == 0 || n_out == 0 {
            return Err("not an audio effect (needs an audio input and output)".into());
        }
        let main_ch = |dir| {
            let mut info: BusInfo = unsafe { std::mem::zeroed() };
            if unsafe { c.getBusInfo(audio, dir, 0, &mut info) } == kResultOk {
                info.channelCount.max(0) as usize
            } else {
                2
            }
        };
        let want = |ch: usize| if ch == 1 { SpeakerArr::kMono } else { SpeakerArr::kStereo };
        let current = |dir, i: usize| {
            let mut a: SpeakerArrangement = 0;
            if unsafe { self.processor.getBusArrangement(dir, i as int32, &mut a) } == kResultOk {
                a
            } else {
                0
            }
        };
        let mut ins: Vec<SpeakerArrangement> = (0..n_in).map(|i| if i == 0 { want(main_ch(input)) } else { current(input, i) }).collect();
        let mut outs: Vec<SpeakerArrangement> = (0..n_out).map(|i| if i == 0 { want(main_ch(output)) } else { current(output, i) }).collect();
        let p = &self.processor;
        if unsafe { p.setBusArrangements(ins.as_mut_ptr(), n_in as int32, outs.as_mut_ptr(), n_out as int32) } != kResultTrue {
            // keep what the plug-in proposes if it is mono / stereo
            ins = (0..n_in).map(|i| current(input, i)).collect();
            outs = (0..n_out).map(|i| current(output, i)).collect();
        }
        self.in_ch = channels(ins[0]);
        self.out_ch = channels(outs[0]);
        if self.in_ch == 0 || self.out_ch == 0 || self.in_ch > 2 || self.out_ch > 2 {
            return Err(format!("only mono and stereo effects are supported ({} in / {} out)", self.in_ch, self.out_ch));
        }
        for (dir, n) in [(input, n_in), (output, n_out)] {
            for i in 0..n {
                unsafe { c.activateBus(audio, dir, i as int32, (i == 0) as TBool) };
            }
        }
        let (silence, scratch) = (self._aux[0].as_ptr() as *mut f32, self._aux[1].as_mut_ptr());
        let main_in = [self.in_buf[0].as_mut_ptr(), self.in_buf[1].as_mut_ptr()];
        let main_out = [self.out_buf[0].as_mut_ptr(), self.out_buf[1].as_mut_ptr()];
        self.in_busses = ins.iter().enumerate().map(|(i, a)| Bus { ptrs: if i == 0 { main_in[..channels(*a)].to_vec() } else { vec![silence; channels(*a)] } }).collect();
        self.out_busses = outs.iter().enumerate().map(|(i, a)| Bus { ptrs: if i == 0 { main_out[..channels(*a)].to_vec() } else { vec![scratch; channels(*a)] } }).collect();
        Ok(())
    }

    /// The visible, writable parameters, in plain (display) units.
    pub fn param_infos(&self) -> Vec<ParamInfo> {
        use ParameterInfo_::ParameterFlags_::*;
        let Some(c) = &self.controller else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..unsafe { c.getParameterCount() } {
            let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
            if unsafe { c.getParameterInfo(i, &mut info) } != kResultOk {
                continue;
            }
            if info.flags as u32 & (kIsHidden | kIsReadOnly | kIsProgramChange | kIsBypass) as u32 != 0 {
                continue;
            }
            let plain = |n: f64| unsafe { c.normalizedParamToPlain(info.id, n) };
            let (a, b) = (plain(0.0), plain(1.0));
            out.push(ParamInfo { id: info.id, name: wbuf(&info.title), min: a.min(b), max: a.max(b), default: plain(info.defaultNormalizedValue), stepped: info.stepCount > 0 });
        }
        out
    }

    /// The controller's current value of a parameter (plain units).
    pub fn param_value(&self, id: u32) -> Option<f64> {
        let c = self.controller.as_ref()?;
        Some(unsafe { c.normalizedParamToPlain(id, c.getParamNormalized(id)) })
    }

    /// Set plain values; only changes are sent, with the next block. Also
    /// updates the controller (call from the thread that loaded the plug-in).
    pub fn set_params(&mut self, values: &[(u32, f64)]) {
        for &(id, v) in values {
            match self.sent.iter_mut().find(|(p, _)| *p == id) {
                Some(s) if s.1 == v => continue,
                Some(s) => s.1 = v,
                None => self.sent.push((id, v)),
            }
            let n = match &self.controller {
                Some(c) => unsafe {
                    let n = c.plainParamToNormalized(id, v).clamp(0.0, 1.0);
                    c.setParamNormalized(id, n);
                    n
                },
                None => v.clamp(0.0, 1.0),
            };
            self.pending.retain(|(p, _)| *p != id);
            self.pending.push((id, n));
        }
    }

    /// Process interleaved stereo in place. A plug-in that fails to start
    /// or returns an error is passed through from then on.
    pub fn process(&mut self, buf: &mut [f32]) {
        if self.failed || !self.active {
            return;
        }
        if !self.processing {
            // some plug-ins answer kNotImplemented; only a refusal counts
            if unsafe { self.processor.setProcessing(1) } == kResultFalse {
                log::warn!("VST3 plug-in refused to start processing; bypassed");
                self.failed = true;
                return;
            }
            self.processing = true;
        }
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
            {
                let mut q = self.input_changes.queues.borrow_mut();
                q.clear();
                q.extend(self.pending.drain(..).map(|(id, v)| ComWrapper::new(ParamQueue { id, value: Cell::new(v) })));
            }
            let mut ins: Vec<AudioBusBuffers> = self.in_busses.iter_mut().map(bus_buffers).collect();
            let mut outs: Vec<AudioBusBuffers> = self.out_busses.iter_mut().map(bus_buffers).collect();
            let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
            ctx.state = (ProcessContext_::StatesAndFlags_::kTempoValid | ProcessContext_::StatesAndFlags_::kTimeSigValid) as u32;
            ctx.sampleRate = self.sample_rate;
            ctx.projectTimeSamples = self.steady;
            ctx.continousTimeSamples = self.steady;
            ctx.tempo = 120.0;
            ctx.timeSigNumerator = 4;
            ctx.timeSigDenominator = 4;
            let changes_in = self.input_changes.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
            let changes_out = self.output_changes.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
            let mut data = ProcessData {
                processMode: ProcessModes_::kRealtime as int32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                numSamples: n as int32,
                numInputs: ins.len() as int32,
                numOutputs: outs.len() as int32,
                inputs: ins.as_mut_ptr(),
                outputs: outs.as_mut_ptr(),
                inputParameterChanges: changes_in,
                outputParameterChanges: changes_out,
                inputEvents: std::ptr::null_mut(),
                outputEvents: std::ptr::null_mut(),
                processContext: &mut ctx,
            };
            let status = unsafe { self.processor.process(&mut data) };
            self.steady += n as i64;
            if status != kResultOk {
                log::warn!("VST3 plug-in returned a processing error ({status}); bypassed");
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

    /// Clear delay lines / tails (after a seek): processing off, and back on
    /// with the next block.
    pub fn reset(&mut self) {
        if self.processing {
            unsafe { self.processor.setProcessing(0) };
            self.processing = false;
        }
    }
}

fn bus_buffers(b: &mut Bus) -> AudioBusBuffers {
    AudioBusBuffers { numChannels: b.ptrs.len() as int32, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: b.ptrs.as_mut_ptr() } }
}

impl Drop for Vst3Instance {
    fn drop(&mut self) {
        unsafe {
            if self.processing {
                self.processor.setProcessing(0);
            }
            if self.active {
                self.component.setActive(0);
            }
            if let Some((a, b)) = &self.connection {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let (true, Some(c)) = (self.separate, &self.controller) {
                c.terminate();
            }
            self.component.terminate();
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
    let r = Vst3Instance::load(path, id, 48_000).map(|i| i.param_infos());
    CACHE.get_or_init(Default::default).lock().insert(key, r.clone());
    r
}

/// Build the `vst3-test-gain` fixture and lay it out as a `.vst3` bundle;
/// returns the bundle folder.
#[cfg(test)]
pub(crate) fn test_plugin() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target = root.join("target").join("vst3-test-plugin");
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let st =
                std::process::Command::new(cargo).args(["build", "--quiet", "-p", "vst3-test-gain", "--target-dir"]).arg(&target).current_dir(&root).status().expect("run cargo");
            assert!(st.success(), "building vst3-test-gain failed");
            let (lib, arch, bin) = if cfg!(windows) {
                ("vst3_test_gain.dll", "x86_64-win", "Test Gain.vst3")
            } else if cfg!(target_os = "macos") {
                ("libvst3_test_gain.dylib", "MacOS", "Test Gain")
            } else {
                ("libvst3_test_gain.so", "x86_64-linux", "Test Gain.so")
            };
            let bundle = target.join("plugins").join("Test Gain.vst3");
            let dir = bundle.join("Contents").join(arch);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(target.join("debug").join(lib), dir.join(bin)).unwrap();
            bundle
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [(i as f32 * 0.05).sin() * 0.5, (i as f32 * 0.03).cos() * 0.25]).collect()
    }

    fn id() -> String {
        describe(&test_plugin()).unwrap()[0].id.clone()
    }

    #[test]
    fn finds_and_describes_the_plugin() {
        let path = test_plugin();
        let found = find_vst3_files(&[path.parent().unwrap().to_path_buf()]);
        assert_eq!(found, vec![path.clone()]);
        let d = describe(&path).unwrap();
        assert_eq!(d.len(), 1, "the controller class is not listed: {d:?}");
        assert_eq!((d[0].name.as_str(), d[0].vendor.as_str()), ("Test Gain", "Rust Cut Pro"));
        assert_eq!(d[0].id.len(), 32);
        assert_eq!(parse_cid(&d[0].id).map(|c| cid_hex(&c)), Some(d[0].id.clone()));
        assert!(describe(Path::new("C:/nope/missing.vst3")).is_err());
    }

    #[test]
    fn processes_with_parameters() {
        let path = test_plugin();
        let mut p = Vst3Instance::load(&path, &id(), 48_000).unwrap();
        assert!(p.separate && p.connection.is_some());
        let infos = p.param_infos();
        assert_eq!(infos.len(), 1, "hidden / read-only parameters not listed: {infos:?}");
        assert_eq!((infos[0].id, infos[0].name.as_str(), infos[0].min, infos[0].max), (0, "Gain", -60.0, 12.0));
        assert!(infos[0].default.abs() < 1e-9);
        // the controller got the component state (it starts at −60 dB) …
        assert!(p.param_value(0).unwrap().abs() < 1e-9);
        // … and that state carried the processor's connect count
        let connects = unsafe { p.controller.as_ref().unwrap().getParamNormalized(2) };
        assert!(connects >= 1.0, "component and controller were connected");
        assert_eq!(param_infos(&path, &id()).unwrap(), infos);

        // default 0 dB: unchanged; bigger than one plug-in block
        let x = tone(MAX_BLOCK * 2 + 100);
        let mut y = x.clone();
        p.process(&mut y);
        assert!(x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-6));

        // −6.02 dB halves it, sent with the next block
        p.set_params(&[(0, -6.0206)]);
        assert!((p.param_value(0).unwrap() + 6.0206).abs() < 1e-9);
        let mut y = x.clone();
        p.process(&mut y);
        assert!(x.iter().zip(&y).all(|(a, b)| (a * 0.5 - b).abs() < 1e-4));
        // an unchanged value is not re-sent; a new one is
        p.set_params(&[(0, -6.0206)]);
        assert!(p.pending.is_empty());
        p.set_params(&[(0, 0.0)]);
        assert_eq!(p.pending.len(), 1);
        // reset turns processing off until the next block
        p.reset();
        assert!(!p.processing);
        let mut y = x.clone();
        p.process(&mut y);
        assert!(p.processing && x.iter().zip(&y).all(|(a, b)| (a - b).abs() < 1e-6));

        assert!(Vst3Instance::load(&path, "00000000000000000000000000000000", 48_000).is_err());
        assert!(Vst3Instance::load(&path, "not-an-id", 48_000).is_err());
    }
}
