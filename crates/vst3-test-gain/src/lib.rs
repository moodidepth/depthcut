//! A minimal VST3 audio effect for testing the host: stereo in / out, a
//! separate edit controller, one "Gain" parameter in dB (id 0, −60..+12,
//! default 0, normalized linearly) and one hidden parameter (id 1).
//!
//! The controller starts at −60 dB and only learns the real gain from the
//! component state, so a host that skips `setComponentState` shows a wrong
//! value. The processor counts `connect` calls on its connection point and
//! reports them through a read-only parameter (id 2) of the controller,
//! which is told about it in the state blob.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::slice;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{uid, Class, ComRef, ComWrapper};

const DB_MIN: f64 = -60.0;
const DB_MAX: f64 = 12.0;

fn to_norm(db: f64) -> f64 {
    ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0)
}
fn to_db(n: f64) -> f64 {
    DB_MIN + n.clamp(0.0, 1.0) * (DB_MAX - DB_MIN)
}

fn copy_cstring(src: &str, dst: &mut [c_char]) {
    for (d, s) in dst.iter_mut().zip(src.bytes().chain(std::iter::repeat(0))) {
        *d = s as c_char;
    }
    if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    for (d, s) in dst.iter_mut().zip(src.encode_utf16().chain(std::iter::repeat(0))) {
        *d = s as TChar;
    }
    if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

/// Connection-point `connect` calls seen by the processor (any instance).
static CONNECTS: AtomicU32 = AtomicU32::new(0);

struct Processor {
    gain: AtomicU64,
    active: Cell<bool>,
}

impl Class for Processor {
    type Interfaces = (IComponent, IAudioProcessor, IConnectionPoint);
}

impl Processor {
    const CID: TUID = uid(0x52435020, 0x56535433, 0x5445_5354, 0x4741_494E);
}

impl IPluginBaseTrait for Processor {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IConnectionPointTrait for Processor {
    unsafe fn connect(&self, other: *mut IConnectionPoint) -> tresult {
        if other.is_null() {
            return kInvalidArgument;
        }
        CONNECTS.fetch_add(1, Ordering::Relaxed);
        kResultOk
    }
    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }
    unsafe fn notify(&self, _message: *mut IMessage) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for Processor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        *class_id = Controller::CID;
        kResultOk
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media_type: MediaType, _dir: BusDirection) -> i32 {
        (media_type as MediaTypes == MediaTypes_::kAudio) as i32
    }
    unsafe fn getBusInfo(&self, media_type: MediaType, dir: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
        if media_type as MediaTypes != MediaTypes_::kAudio || index != 0 {
            return kInvalidArgument;
        }
        let bus = &mut *bus;
        bus.mediaType = media_type;
        bus.direction = dir;
        bus.channelCount = 2;
        copy_wstring(if dir as BusDirections == BusDirections_::kInput { "Input" } else { "Output" }, &mut bus.name);
        bus.busType = BusTypes_::kMain as BusType;
        bus.flags = BusInfo_::BusFlags_::kDefaultActive as u32;
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _in_info: *mut RoutingInfo, _out_info: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(&self, _media_type: MediaType, _dir: BusDirection, _index: i32, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, state: TBool) -> tresult {
        self.active.set(state != 0);
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let Some(s) = ComRef::from_raw(state) else { return kInvalidArgument };
        let mut b = [0u8; 8];
        let mut n = 0;
        if s.read(b.as_mut_ptr() as *mut c_void, 8, &mut n) != kResultOk || n != 8 {
            return kResultFalse;
        }
        self.gain.store(f64::from_le_bytes(b).to_bits(), Ordering::Relaxed);
        kResultOk
    }
    /// Gain (normalized, f64) then the connect count (u32).
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let Some(s) = ComRef::from_raw(state) else { return kInvalidArgument };
        let mut b = f64::from_bits(self.gain.load(Ordering::Relaxed)).to_le_bytes().to_vec();
        b.extend(CONNECTS.load(Ordering::Relaxed).to_le_bytes());
        let mut n = 0;
        s.write(b.as_mut_ptr() as *mut c_void, b.len() as i32, &mut n);
        if n as usize == b.len() {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl IAudioProcessorTrait for Processor {
    unsafe fn setBusArrangements(&self, inputs: *mut SpeakerArrangement, num_ins: i32, outputs: *mut SpeakerArrangement, num_outs: i32) -> tresult {
        if num_ins == 1 && num_outs == 1 && *inputs == SpeakerArr::kStereo && *outputs == SpeakerArr::kStereo {
            kResultTrue
        } else {
            kResultFalse
        }
    }
    unsafe fn getBusArrangement(&self, _dir: BusDirection, index: i32, arr: *mut SpeakerArrangement) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        *arr = SpeakerArr::kStereo;
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size as SymbolicSampleSizes == SymbolicSampleSizes_::kSample32 {
            kResultTrue
        } else {
            kResultFalse
        }
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }
    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let d = &*data;
        if !self.active.get() {
            return kResultFalse;
        }
        if let Some(changes) = ComRef::from_raw(d.inputParameterChanges) {
            for i in 0..changes.getParameterCount() {
                let Some(q) = ComRef::from_raw(changes.getParameterData(i)) else { continue };
                let n = q.getPointCount();
                let (mut offset, mut value) = (0, 0.0);
                if q.getParameterId() == 0 && n > 0 && q.getPoint(n - 1, &mut offset, &mut value) == kResultOk {
                    self.gain.store(value.to_bits(), Ordering::Relaxed);
                }
            }
        }
        if d.numInputs != 1 || d.numOutputs != 1 || d.numSamples <= 0 {
            return kResultOk;
        }
        let g = 10f64.powf(to_db(f64::from_bits(self.gain.load(Ordering::Relaxed))) / 20.0) as f32;
        let n = d.numSamples as usize;
        let (inp, out) = (&*d.inputs, &*d.outputs);
        let ins = slice::from_raw_parts(inp.__field0.channelBuffers32, inp.numChannels as usize);
        let outs = slice::from_raw_parts(out.__field0.channelBuffers32, out.numChannels as usize);
        for (i, o) in ins.iter().zip(outs) {
            let (i, o) = (slice::from_raw_parts(*i, n), slice::from_raw_parts_mut(*o, n));
            for (a, b) in i.iter().zip(o.iter_mut()) {
                *b = a * g;
            }
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

struct Controller {
    gain: Cell<f64>,
    hidden: Cell<f64>,
    connects: Cell<u32>,
}

impl Class for Controller {
    type Interfaces = (IEditController, IConnectionPoint);
}

impl Controller {
    const CID: TUID = uid(0x52435020, 0x56535433, 0x5445_5354, 0x4354_524C);
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IConnectionPointTrait for Controller {
    unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }
    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultOk
    }
    unsafe fn notify(&self, _message: *mut IMessage) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        let Some(s) = ComRef::from_raw(state) else { return kInvalidArgument };
        let mut b = [0u8; 12];
        let mut n = 0;
        if s.read(b.as_mut_ptr() as *mut c_void, 12, &mut n) != kResultOk || n != 12 {
            return kResultFalse;
        }
        self.gain.set(f64::from_le_bytes(b[..8].try_into().unwrap()));
        self.connects.set(u32::from_le_bytes(b[8..].try_into().unwrap()));
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> i32 {
        3
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        use ParameterInfo_::ParameterFlags_::*;
        let info = &mut *info;
        let (id, title, default, flags) = match index {
            0 => (0, "Gain", to_norm(0.0), kCanAutomate),
            1 => (1, "Hidden", 0.0, kIsHidden),
            2 => (2, "Connections", 0.0, kIsReadOnly),
            _ => return kInvalidArgument,
        };
        info.id = id;
        copy_wstring(title, &mut info.title);
        copy_wstring(title, &mut info.shortTitle);
        copy_wstring(if id == 0 { "dB" } else { "" }, &mut info.units);
        info.stepCount = 0;
        info.defaultNormalizedValue = default;
        info.unitId = 0;
        info.flags = flags;
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, _id: u32, value: f64, string: *mut String128) -> tresult {
        copy_wstring(&format!("{:.1}", to_db(value)), &mut *string);
        kResultOk
    }
    unsafe fn getParamValueByString(&self, _id: u32, _string: *mut TChar, _value: *mut f64) -> tresult {
        kNotImplemented
    }
    unsafe fn normalizedParamToPlain(&self, id: u32, value: f64) -> f64 {
        match id {
            0 => to_db(value),
            _ => value,
        }
    }
    unsafe fn plainParamToNormalized(&self, id: u32, plain: f64) -> f64 {
        match id {
            0 => to_norm(plain),
            _ => plain,
        }
    }
    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        match id {
            0 => self.gain.get(),
            1 => self.hidden.get(),
            2 => self.connects.get() as f64,
            _ => 0.0,
        }
    }
    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        match id {
            0 => self.gain.set(value),
            1 => self.hidden.set(value),
            _ => return kInvalidArgument,
        }
        kResultOk
    }
    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }
    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        std::ptr::null_mut()
    }
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory2,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let info = &mut *info;
        copy_cstring("Rust Cut Pro", &mut info.vendor);
        copy_cstring("", &mut info.url);
        copy_cstring("", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }
    unsafe fn countClasses(&self) -> i32 {
        2
    }
    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let mut i2: PClassInfo2 = std::mem::zeroed();
        let r = self.getClassInfo2(index, &mut i2);
        if r == kResultOk {
            let info = &mut *info;
            info.cid = i2.cid;
            info.cardinality = i2.cardinality;
            info.category = i2.category;
            info.name = i2.name;
        }
        r
    }
    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        let unknown = match *(cid as *const TUID) {
            Processor::CID => ComWrapper::new(Processor { gain: AtomicU64::new(to_norm(0.0).to_bits()), active: Cell::new(false) }).to_com_ptr::<FUnknown>(),
            Controller::CID => ComWrapper::new(Controller { gain: Cell::new(0.0), hidden: Cell::new(0.0), connects: Cell::new(0) }).to_com_ptr::<FUnknown>(),
            _ => return kInvalidArgument,
        };
        let Some(unknown) = unknown else { return kNoInterface };
        let ptr = unknown.as_ptr();
        ((*(*ptr).vtbl).queryInterface)(ptr, iid as *mut TUID, obj)
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
        let info = &mut *info;
        let (cid, category, sub) = match index {
            0 => (Processor::CID, "Audio Module Class", "Fx|Dynamics"),
            1 => (Controller::CID, "Component Controller Class", ""),
            _ => return kInvalidArgument,
        };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring("Test Gain", &mut info.name);
        info.classFlags = 0;
        copy_cstring(sub, &mut info.subCategories);
        copy_cstring("Rust Cut Pro", &mut info.vendor);
        copy_cstring("1.0.0", &mut info.version);
        copy_cstring("VST 3.7", &mut info.sdkVersion);
        kResultOk
    }
}

#[cfg(target_os = "windows")]
#[no_mangle]
extern "system" fn InitDll() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[no_mangle]
extern "system" fn ExitDll() -> bool {
    true
}

#[cfg(target_os = "linux")]
#[no_mangle]
extern "system" fn ModuleEntry(_library_handle: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "linux")]
#[no_mangle]
extern "system" fn ModuleExit() -> bool {
    true
}

#[no_mangle]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>().map_or(std::ptr::null_mut(), |p| p.into_raw())
}
