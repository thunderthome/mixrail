//! Level meters: a small capture stream per strip/bus that records the peak level.
//!
//! The stream is created unconnected; the engine links it to the node's monitor ports
//! like any other managed link, so meters self-heal along with the routing.

use anyhow::Result;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Meter {
    _stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
    /// Peak of each channel since the last read, as f32 bits. For non-negative floats
    /// the bit pattern orders the same as the value, so `fetch_max` works on the bits.
    peaks: Arc<[AtomicU32; 2]>,
}

impl Meter {
    pub fn new(core: &pw::core::CoreRc, node_name: &str, description: &str) -> Result<Self> {
        let props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => node_name,
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::APP_NAME => "mixrail meters",
            // We link it ourselves, and it must never keep the audio graph awake.
            "node.autoconnect" => "false",
            "node.passive" => "true",
            "node.dont-reconnect" => "true",
            "state.restore-props" => "false",
            "state.restore-target" => "false",
        };
        let stream = pw::stream::StreamRc::new(core.clone(), node_name, props)?;
        let peaks: Arc<[AtomicU32; 2]> = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);

        let listener = stream
            .add_local_listener_with_user_data(())
            .process({
                let peaks = peaks.clone();
                move |stream, _| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let datas = buffer.datas_mut();
                    let Some(data) = datas.first_mut() else {
                        return;
                    };
                    let size = data.chunk().size() as usize;
                    let Some(bytes) = data.data() else { return };
                    let bytes = &bytes[..size.min(bytes.len())];
                    // Interleaved stereo f32.
                    let mut max = [0f32; 2];
                    for (i, s) in bytes.as_chunks::<4>().0.iter().enumerate() {
                        let v = f32::from_le_bytes(*s).abs();
                        let ch = i & 1;
                        if v > max[ch] {
                            max[ch] = v;
                        }
                    }
                    for (p, m) in peaks.iter().zip(max) {
                        if m.is_finite() {
                            p.fetch_max(m.to_bits(), Ordering::Relaxed);
                        }
                    }
                }
            })
            .register()?;

        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_channels(2);
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(position);
        let obj = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        };
        let bytes = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(obj),
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .0
        .into_inner();
        let mut params = [spa::pod::Pod::from_bytes(&bytes).expect("valid pod")];
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;

        Ok(Meter {
            _stream: stream,
            _listener: listener,
            peaks,
        })
    }

    /// Peak level per channel (linear, 1.0 = full scale) since the last call.
    pub fn take(&self) -> [f32; 2] {
        [0, 1].map(|i| f32::from_bits(self.peaks[i].swap(0, Ordering::Relaxed)))
    }
}
