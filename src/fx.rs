//! A PipeWire filter node that runs a channel's effects (see `dsp`). It has one input
//! and one output port per audio channel; the engine links it between the channel and
//! its outputs. Processing happens in PipeWire's real-time thread.

use crate::config::Effects;
use crate::dsp::{Chain, MAX_CHANNELS, Stats};
use anyhow::{Result, bail};
use pipewire as pw;
use std::ffi::{CString, c_void};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Largest PipeWire cycle we keep spare buffers for (PipeWire's maximum quantum).
const SPARE_SAMPLES: usize = 8192;

/// Settings in, stats out, shared between the engine and the audio thread.
pub struct Shared {
    settings: Mutex<(Effects, f32)>,
    version: AtomicU32,
    level_db: AtomicU32,
    reduction_db: AtomicU32,
    limit_db: AtomicU32,
    duck_db: AtomicU32,
    duck_source_db: AtomicU32,
    deess_db: AtomicU32,
    gate_open: AtomicBool,
}

impl Shared {
    /// What the effects are doing right now (for the GUI).
    pub fn stats(&self) -> Stats {
        Stats {
            level_db: f32::from_bits(self.level_db.load(Ordering::Relaxed)),
            reduction_db: f32::from_bits(self.reduction_db.load(Ordering::Relaxed)),
            limit_db: f32::from_bits(self.limit_db.load(Ordering::Relaxed)),
            duck_db: f32::from_bits(self.duck_db.load(Ordering::Relaxed)),
            duck_source_db: f32::from_bits(self.duck_source_db.load(Ordering::Relaxed)),
            deess_db: f32::from_bits(self.deess_db.load(Ordering::Relaxed)),
            gate_open: self.gate_open.load(Ordering::Relaxed),
        }
    }
}

/// Everything the audio thread touches; boxed so its address is stable.
struct State {
    inputs: Vec<*mut c_void>,
    outputs: Vec<*mut c_void>,
    /// Ducking source inputs (FL, FR).
    sidechain: Vec<*mut c_void>,
    /// Stand-in buffers for outputs that aren't connected (PipeWire gives those none),
    /// so the effects always process every channel. Reserved up front.
    spare: Vec<Vec<f32>>,
    chain: Chain,
    fader_db: f32,
    seen_version: u32,
    shared: Arc<Shared>,
}

pub struct FxNode {
    filter: NonNull<pw::sys::pw_filter>,
    _hook: Box<pw::spa::sys::spa_hook>,
    _state: Box<State>,
    shared: Arc<Shared>,
    pub channels: Vec<String>,
}

static EVENTS: pw::sys::pw_filter_events = pw::sys::pw_filter_events {
    version: pw::sys::PW_VERSION_FILTER_EVENTS,
    destroy: None,
    state_changed: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
};

/// The real-time callback: copy each input to its output and run the effects on it.
unsafe extern "C" fn on_process(data: *mut c_void, position: *mut pw::spa::sys::spa_io_position) {
    // SAFETY: `data` is the boxed State registered with this filter; PipeWire calls
    // process from one thread at a time, and the State outlives the filter.
    let st = unsafe { &mut *(data as *mut State) };
    let Some(pos) = (unsafe { position.as_ref() }) else {
        return;
    };
    let n = pos.clock.duration as u32;
    if n == 0 {
        return;
    }
    if pos.clock.rate.denom > 0 {
        st.chain.set_rate(pos.clock.rate.denom as f64);
    }
    // New settings? Never block the audio thread: if the engine holds the lock right
    // now, pick them up next cycle.
    let version = st.shared.version.load(Ordering::Acquire);
    if version != st.seen_version
        && let Ok(guard) = st.shared.settings.try_lock()
    {
        // Copies the settings into the chain's reserved room: no allocation here.
        st.chain.set(&guard.0);
        let fader = guard.1;
        st.fader_db = fader;
        st.seen_version = version;
    }

    // Split the borrows: the buffers below point into `spare` while `chain` runs.
    let State {
        inputs,
        outputs,
        sidechain,
        spare,
        chain,
        fader_db,
        ..
    } = st;
    // The ducking sources (unconnected = silent).
    let mut side: [&[f32]; 2] = [&[], &[]];
    for (s, port) in side.iter_mut().zip(sidechain.iter()) {
        // SAFETY: as for the other ports below.
        let p = unsafe { pw::sys::pw_filter_get_dsp_buffer(*port, n) } as *const f32;
        if !p.is_null() {
            *s = unsafe { std::slice::from_raw_parts(p, n as usize) };
        }
    }
    let count = inputs.len().min(MAX_CHANNELS);
    let mut bufs: [&mut [f32]; MAX_CHANNELS] = Default::default();
    for (c, spare) in spare.iter_mut().enumerate().take(count) {
        // SAFETY: port handles come from pw_filter_add_port on this filter; buffers
        // are valid for `n` samples during this callback.
        let input = unsafe { pw::sys::pw_filter_get_dsp_buffer(inputs[c], n) } as *const f32;
        let output = unsafe { pw::sys::pw_filter_get_dsp_buffer(outputs[c], n) } as *mut f32;
        let out: &mut [f32] = if output.is_null() {
            // Not connected: still process it (the effects look at all channels
            // together), just into a buffer nobody listens to.
            let len = (n as usize).min(spare.len());
            &mut spare[..len]
        } else {
            unsafe { std::slice::from_raw_parts_mut(output, n as usize) }
        };
        if input.is_null() {
            out.fill(0.0);
        } else {
            let len = out.len();
            out.copy_from_slice(unsafe { &std::slice::from_raw_parts(input, n as usize)[..len] });
        }
        bufs[c] = out;
    }
    let stats = chain.process(&mut bufs[..count], &side, *fader_db);
    st.shared
        .level_db
        .store(stats.level_db.to_bits(), Ordering::Relaxed);
    st.shared
        .reduction_db
        .store(stats.reduction_db.to_bits(), Ordering::Relaxed);
    st.shared
        .limit_db
        .store(stats.limit_db.to_bits(), Ordering::Relaxed);
    st.shared
        .duck_db
        .store(stats.duck_db.to_bits(), Ordering::Relaxed);
    st.shared
        .duck_source_db
        .store(stats.duck_source_db.to_bits(), Ordering::Relaxed);
    st.shared
        .deess_db
        .store(stats.deess_db.to_bits(), Ordering::Relaxed);
    st.shared
        .gate_open
        .store(stats.gate_open, Ordering::Relaxed);
}

fn props(text: &str) -> Result<*mut pw::sys::pw_properties> {
    let c = CString::new(text)?;
    // SAFETY: parses the string into a new properties object (ownership passes on).
    let p = unsafe { pw::sys::pw_properties_new_string(c.as_ptr()) };
    if p.is_null() {
        bail!("bad properties: {text}");
    }
    Ok(p)
}

impl FxNode {
    /// Create the node `name` with ports for `channels` (e.g. FL, FR).
    pub fn new(
        core: &pw::core::CoreRc,
        name: &str,
        description: &str,
        channels: &[String],
        fx: Effects,
        fader_db: f32,
    ) -> Result<Self> {
        let clean = |s: &str| s.replace(['"', '\\'], "");
        let node_props = props(&format!(
            r#"{{ node.name = "{}" node.description = "{}" media.type = Audio
                  media.category = Filter media.role = DSP node.autoconnect = false }}"#,
            clean(name),
            clean(description)
        ))?;
        let cname = CString::new(name)?;
        // SAFETY: core is connected; props ownership passes to the filter.
        let filter =
            unsafe { pw::sys::pw_filter_new(core.as_raw_ptr(), cname.as_ptr(), node_props) };
        let Some(filter) = NonNull::new(filter) else {
            bail!("could not create effects node")
        };

        let shared = Arc::new(Shared {
            settings: Mutex::new((fx.clone(), fader_db)),
            version: AtomicU32::new(1),
            level_db: AtomicU32::new((-120f32).to_bits()),
            reduction_db: AtomicU32::new(0f32.to_bits()),
            limit_db: AtomicU32::new(0f32.to_bits()),
            duck_db: AtomicU32::new(0f32.to_bits()),
            duck_source_db: AtomicU32::new((-120f32).to_bits()),
            deess_db: AtomicU32::new(0f32.to_bits()),
            gate_open: AtomicBool::new(true),
        });
        let mut state = Box::new(State {
            inputs: Vec::new(),
            outputs: Vec::new(),
            sidechain: Vec::new(),
            spare: (0..channels.len())
                .map(|_| vec![0.0; SPARE_SAMPLES])
                .collect(),
            chain: Chain::new(channels.len(), 48000.0, &fx),
            fader_db,
            seen_version: 1,
            shared: shared.clone(),
        });
        // Two inputs that listen to the ducking sources (see graph::SIDECHAIN_PREFIX).
        for ch in ["FL", "FR"] {
            let p = props(&format!(
                r#"{{ format.dsp = "32 bit float mono audio" port.name = "sidechain_{ch}" audio.channel = "{ch}" }}"#
            ))?;
            // SAFETY: filter is live; props ownership passes on; no extra params.
            let port = unsafe {
                pw::sys::pw_filter_add_port(
                    filter.as_ptr(),
                    pw::spa::sys::SPA_DIRECTION_INPUT,
                    pw::sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                    0,
                    p,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if port.is_null() {
                bail!("could not add ducking input sidechain_{ch}");
            }
            state.sidechain.push(port);
        }
        for ch in channels {
            for (dir, prefix) in [
                (pw::spa::sys::SPA_DIRECTION_INPUT, "input"),
                (pw::spa::sys::SPA_DIRECTION_OUTPUT, "output"),
            ] {
                let p = props(&format!(
                    r#"{{ format.dsp = "32 bit float mono audio" port.name = "{prefix}_{ch}" audio.channel = "{ch}" }}"#
                ))?;
                // SAFETY: filter is live; props ownership passes on; no extra params.
                let port = unsafe {
                    pw::sys::pw_filter_add_port(
                        filter.as_ptr(),
                        dir,
                        pw::sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                        0,
                        p,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if port.is_null() {
                    bail!("could not add effects port {prefix}_{ch}");
                }
                if dir == pw::spa::sys::SPA_DIRECTION_INPUT {
                    state.inputs.push(port);
                } else {
                    state.outputs.push(port);
                }
            }
        }
        // SAFETY: a zeroed spa_hook is its valid initial state.
        let mut hook: Box<pw::spa::sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: the hook and state are boxed and owned by the FxNode, which destroys
        // the filter (stopping callbacks) before they are freed.
        unsafe {
            pw::sys::pw_filter_add_listener(
                filter.as_ptr(),
                &mut *hook,
                &EVENTS,
                &mut *state as *mut State as *mut c_void,
            );
        }
        let node = FxNode {
            filter,
            _hook: hook,
            _state: state,
            shared,
            channels: channels.to_vec(),
        };
        // SAFETY: filter is set up; process in PipeWire's real-time thread.
        let res = unsafe {
            pw::sys::pw_filter_connect(
                filter.as_ptr(),
                pw::sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
                std::ptr::null_mut(),
                0,
            )
        };
        if res < 0 {
            bail!("could not start effects node ({res})");
        }
        Ok(node)
    }

    /// Update settings; the audio thread picks them up on its next cycle.
    pub fn set(&self, fx: &Effects, fader_db: f32) {
        let mut guard = self
            .shared
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.0 != *fx || guard.1 != fader_db {
            *guard = (fx.clone(), fader_db);
            self.shared.version.fetch_add(1, Ordering::Release);
        }
    }

    pub fn stats(&self) -> Stats {
        self.shared.stats()
    }
}

impl Drop for FxNode {
    fn drop(&mut self) {
        // SAFETY: destroying the filter stops its callbacks before State is freed
        // (fields are dropped after this).
        unsafe { pw::sys::pw_filter_destroy(self.filter.as_ptr()) };
    }
}
