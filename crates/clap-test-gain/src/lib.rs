//! A minimal CLAP audio effect for testing the host: stereo in / out, one
//! "Gain" parameter in dB (id 0, −60..+12, default 0) and one hidden
//! parameter (id 1) the host must not show.

#![allow(clippy::missing_safety_doc)]

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_header, clap_event_param_value, clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_STEREO};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_HIDDEN};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::CLAP_INVALID_ID;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR};
use clap_sys::version::CLAP_VERSION;
use std::ffi::{c_char, c_void, CStr};
use std::ptr::null;

const ID: &CStr = c"org.rustcutpro.test-gain";

struct Features([*const c_char; 3]);
unsafe impl Sync for Features {}
static FEATURES: Features = Features([c"audio-effect".as_ptr(), c"stereo".as_ptr(), null()]);

struct Desc(clap_plugin_descriptor);
unsafe impl Sync for Desc {}
static DESC: Desc = Desc(clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: ID.as_ptr(),
    name: c"Test Gain".as_ptr(),
    vendor: c"Depthcut".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1.0.0".as_ptr(),
    description: c"Gain in dB".as_ptr(),
    features: &FEATURES.0 as *const [*const c_char; 3] as *const *const c_char,
});

struct Gain {
    plugin: clap_plugin,
    gain_db: f64,
    hidden: f64,
    active: bool,
}

unsafe fn state<'a>(p: *const clap_plugin) -> &'a mut Gain {
    unsafe { &mut *((*p).plugin_data as *mut Gain) }
}

fn apply_events(g: &mut Gain, events: *const clap_input_events) {
    if events.is_null() {
        return;
    }
    let ev = unsafe { &*events };
    let n = ev.size.map_or(0, |s| unsafe { s(events) });
    for i in 0..n {
        let h = ev.get.map_or(null(), |get| unsafe { get(events, i) });
        if h.is_null() {
            continue;
        }
        let hdr: &clap_event_header = unsafe { &*h };
        if hdr.space_id == CLAP_CORE_EVENT_SPACE_ID && hdr.type_ == CLAP_EVENT_PARAM_VALUE {
            let pv = unsafe { &*(h as *const clap_event_param_value) };
            match pv.param_id {
                0 => g.gain_db = pv.value.clamp(-60.0, 12.0),
                1 => g.hidden = pv.value,
                _ => {}
            }
        }
    }
}

unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn destroy(p: *const clap_plugin) {
    drop(unsafe { Box::from_raw((*p).plugin_data as *mut Gain) });
}
unsafe extern "C" fn activate(p: *const clap_plugin, _sr: f64, _min: u32, _max: u32) -> bool {
    unsafe { state(p) }.active = true;
    true
}
unsafe extern "C" fn deactivate(p: *const clap_plugin) {
    unsafe { state(p) }.active = false;
}
unsafe extern "C" fn start_processing(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn stop_processing(_: *const clap_plugin) {}
unsafe extern "C" fn reset(_: *const clap_plugin) {}

unsafe extern "C" fn process(p: *const clap_plugin, pr: *const clap_process) -> clap_process_status {
    let g = unsafe { state(p) };
    let pr = unsafe { &*pr };
    if !g.active || pr.audio_inputs_count < 1 || pr.audio_outputs_count < 1 {
        return CLAP_PROCESS_ERROR;
    }
    apply_events(g, pr.in_events);
    let gain = 10f64.powf(g.gain_db / 20.0) as f32;
    let (inp, out): (&clap_audio_buffer, &clap_audio_buffer) = unsafe { (&*pr.audio_inputs, &*pr.audio_outputs) };
    for c in 0..out.channel_count.min(inp.channel_count) as usize {
        let (i, o) = unsafe { (*inp.data32.add(c), *out.data32.add(c)) };
        for k in 0..pr.frames_count as usize {
            unsafe { *o.add(k) = *i.add(k) * gain };
        }
    }
    CLAP_PROCESS_CONTINUE
}

// --- params -------------------------------------------------------------------

unsafe extern "C" fn params_count(_: *const clap_plugin) -> u32 {
    2
}
unsafe extern "C" fn params_info(_: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    let info = unsafe { &mut *info };
    let (id, name, flags, min, max, def): (u32, &[u8], u32, f64, f64, f64) = match index {
        0 => (0, b"Gain", CLAP_PARAM_IS_AUTOMATABLE, -60.0, 12.0, 0.0),
        1 => (1, b"Hidden", CLAP_PARAM_IS_HIDDEN, 0.0, 1.0, 0.0),
        _ => return false,
    };
    *info = unsafe { std::mem::zeroed() };
    info.id = id;
    info.flags = flags;
    for (d, s) in info.name.iter_mut().zip(name) {
        *d = *s as c_char;
    }
    info.min_value = min;
    info.max_value = max;
    info.default_value = def;
    true
}
unsafe extern "C" fn params_value(p: *const clap_plugin, id: u32, out: *mut f64) -> bool {
    let g = unsafe { state(p) };
    let v = match id {
        0 => g.gain_db,
        1 => g.hidden,
        _ => return false,
    };
    unsafe { *out = v };
    true
}
unsafe extern "C" fn params_flush(p: *const clap_plugin, inp: *const clap_input_events, _: *const clap_output_events) {
    apply_events(unsafe { state(p) }, inp);
}

struct Params(clap_plugin_params);
unsafe impl Sync for Params {}
static PARAMS: Params = Params(clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_info),
    get_value: Some(params_value),
    value_to_text: None,
    text_to_value: None,
    flush: Some(params_flush),
});

// --- audio ports ----------------------------------------------------------------

unsafe extern "C" fn ports_count(_: *const clap_plugin, _is_input: bool) -> u32 {
    1
}
unsafe extern "C" fn ports_get(_: *const clap_plugin, index: u32, _is_input: bool, info: *mut clap_audio_port_info) -> bool {
    if index != 0 {
        return false;
    }
    let info = unsafe { &mut *info };
    *info = unsafe { std::mem::zeroed() };
    info.id = 0;
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
    info.in_place_pair = CLAP_INVALID_ID;
    true
}

struct Ports(clap_plugin_audio_ports);
unsafe impl Sync for Ports {}
static PORTS: Ports = Ports(clap_plugin_audio_ports { count: Some(ports_count), get: Some(ports_get) });

unsafe extern "C" fn get_extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_PARAMS {
        &PARAMS.0 as *const clap_plugin_params as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &PORTS.0 as *const clap_plugin_audio_ports as *const c_void
    } else {
        null()
    }
}
unsafe extern "C" fn on_main_thread(_: *const clap_plugin) {}

// --- factory / entry ------------------------------------------------------------

unsafe extern "C" fn factory_count(_: *const clap_plugin_factory) -> u32 {
    1
}
unsafe extern "C" fn factory_desc(_: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
    if index == 0 {
        &DESC.0
    } else {
        null()
    }
}
unsafe extern "C" fn factory_create(_: *const clap_plugin_factory, _host: *const clap_host, id: *const c_char) -> *const clap_plugin {
    if unsafe { CStr::from_ptr(id) } != ID {
        return null();
    }
    let g = Box::into_raw(Box::new(Gain {
        plugin: clap_plugin {
            desc: &DESC.0,
            plugin_data: std::ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        gain_db: 0.0,
        hidden: 0.0,
        active: false,
    }));
    unsafe {
        (*g).plugin.plugin_data = g as *mut c_void;
        &(*g).plugin
    }
}

struct Factory(clap_plugin_factory);
unsafe impl Sync for Factory {}
static FACTORY: Factory = Factory(clap_plugin_factory { get_plugin_count: Some(factory_count), get_plugin_descriptor: Some(factory_desc), create_plugin: Some(factory_create) });

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}
unsafe extern "C" fn entry_deinit() {}
unsafe extern "C" fn entry_factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY.0 as *const clap_plugin_factory as *const c_void
    } else {
        null()
    }
}

#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(entry_init), deinit: Some(entry_deinit), get_factory: Some(entry_factory) };
