//! The mixer window. It holds no state of its own: it shows what the engine reports and
//! sends the engine commands, so it can be opened and closed without affecting audio.

use crate::config::{
    self, Action, BandKind, Binding, Bus, Config, EQ_MAX_BANDS, EQ_MAX_DB, EQ_MAX_HZ, EQ_MAX_Q,
    EQ_MIN_HZ, EQ_MIN_Q, Effects, EqBand, Kind, Layout as ChannelLayout, Midi, Mode, Rule, Strip,
    Upmix,
};
use crate::ipc::{self, AppInfo, Command, Device, Event, LearnTarget, RecordingInfo};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense,
    Stroke, StrokeKind, Ui, Vec2,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const STRIP_WIDTH: f32 = 112.0;
/// Width a strip/bus card takes in the row (content + frame margins + spacing).
const CARD_W: f32 = STRIP_WIDTH + 16.0 + 6.0;
/// Width of an "add" card plus spacing.
const ADD_W: f32 = 64.0 + 6.0;
const FADER_HEIGHT: f32 = 260.0;
/// How long a value the user just set wins over (possibly stale) engine updates.
const LOCAL_HOLD: Duration = Duration::from_millis(600);

// The Mixrail look: dark neutral surfaces, an indigo accent, teal for speaker/headphone
// outputs (A) and violet for virtual microphones (B).
const ACCENT: Color32 = Color32::from_rgb(0x6c, 0x8c, 0xff);
const MUTE_RED: Color32 = Color32::from_rgb(0xef, 0x4b, 0x5b);
const METER_GREEN: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
const METER_YELLOW: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
const METER_RED: Color32 = Color32::from_rgb(0xf4, 0x3f, 0x5e);
const OUT_A: Color32 = Color32::from_rgb(0x2d, 0xd4, 0xbf);
const OUT_B: Color32 = Color32::from_rgb(0xa7, 0x8b, 0xfa);

/// The colours that differ between the dark and the light theme.
struct Palette {
    dark: bool,
    bg: Color32,
    window: Color32,
    panel: Color32,
    card_border: Color32,
    surface: Color32,
    surface_border: Color32,
    surface_hover: Color32,
    surface_active: Color32,
    text: Color32,
    text_muted: Color32,
    /// Fader tracks, meter backgrounds, graph background.
    groove: Color32,
    grid: Color32,
    cap: Color32,
    cap_border: Color32,
}

static DARK_PALETTE: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(0x0e, 0x10, 0x14),
    window: Color32::from_rgb(0x14, 0x17, 0x1d),
    panel: Color32::from_rgb(0x17, 0x1a, 0x21),
    card_border: Color32::from_rgb(0x25, 0x2a, 0x34),
    surface: Color32::from_rgb(0x20, 0x24, 0x2d),
    surface_border: Color32::from_rgb(0x2c, 0x31, 0x3c),
    surface_hover: Color32::from_rgb(0x29, 0x2e, 0x3a),
    surface_active: Color32::from_rgb(0x31, 0x37, 0x46),
    text: Color32::from_rgb(0xe6, 0xe8, 0xee),
    text_muted: Color32::from_rgb(0x8a, 0x93, 0xa3),
    groove: Color32::from_rgb(0x09, 0x0a, 0x0e),
    grid: Color32::from_rgb(0x26, 0x2a, 0x33),
    cap: Color32::from_rgb(0xdc, 0xe0, 0xe8),
    cap_border: Color32::from_rgb(0xdc, 0xe0, 0xe8),
};

static LIGHT_PALETTE: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(0xec, 0xee, 0xf2),
    window: Color32::from_rgb(0xf8, 0xf9, 0xfb),
    panel: Color32::from_rgb(0xff, 0xff, 0xff),
    card_border: Color32::from_rgb(0xdc, 0xe0, 0xe7),
    surface: Color32::from_rgb(0xf1, 0xf3, 0xf6),
    surface_border: Color32::from_rgb(0xd2, 0xd7, 0xdf),
    surface_hover: Color32::from_rgb(0xe6, 0xe9, 0xef),
    surface_active: Color32::from_rgb(0xdb, 0xe0, 0xe9),
    text: Color32::from_rgb(0x1b, 0x1f, 0x27),
    text_muted: Color32::from_rgb(0x67, 0x6f, 0x7f),
    groove: Color32::from_rgb(0xdf, 0xe3, 0xea),
    grid: Color32::from_rgb(0xcc, 0xd1, 0xda),
    cap: Color32::from_rgb(0xff, 0xff, 0xff),
    cap_border: Color32::from_rgb(0xb9, 0xc0, 0xcc),
};

thread_local! {
    static DARK_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// The palette of the theme shown right now (set at the start of every frame).
fn pal() -> &'static Palette {
    if DARK_MODE.with(|d| d.get()) {
        &DARK_PALETTE
    } else {
        &LIGHT_PALETTE
    }
}

/// A colour used for text or lines on the current background: slightly deeper on light.
fn ink(c: Color32) -> Color32 {
    if pal().dark {
        c
    } else {
        c.lerp_to_gamma(Color32::BLACK, 0.45)
    }
}

/// Which theme the window uses; stored in ~/.config/mixrail/gui.toml.
#[derive(Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct GuiSettings {
    theme: ThemeChoice,
}

impl GuiSettings {
    fn path() -> std::path::PathBuf {
        Config::default_path().with_file_name("gui.toml")
    }
    fn load() -> Self {
        let mut s: Self = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        // Developer aid (screenshots): LAM_THEME=dark|light.
        match std::env::var("LAM_THEME").as_deref() {
            Ok("dark") => s.theme = ThemeChoice::Dark,
            Ok("light") => s.theme = ThemeChoice::Light,
            _ => {}
        }
        s
    }
    fn save(&self) {
        if let Ok(text) = toml::to_string(self) {
            let _ = std::fs::create_dir_all(Self::path().parent().unwrap_or(std::path::Path::new(".")));
            let _ = std::fs::write(Self::path(), text);
        }
    }
}

impl ThemeChoice {
    fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
        }
    }
    fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
        }
    }
}

/// The app icon (also drawn in the top bar).
const ICON_PNG: &[u8] = include_bytes!("../assets/icons/mixrail-256.png");

/// The semibold font, for names and buttons.
fn semibold() -> egui::FontFamily {
    egui::FontFamily::Name("semibold".into())
}

/// Inter (SIL Open Font License, see assets/fonts/OFL.txt) for all text.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "inter".into(),
        Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"))),
    );
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf"))),
    );
    let fallback = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut regular = vec!["inter".to_string()];
    regular.extend(fallback.iter().cloned());
    let mut bold = vec!["inter-semibold".to_string()];
    bold.extend(fallback);
    fonts.families.insert(egui::FontFamily::Proportional, regular);
    fonts.families.insert(semibold(), bold);
    ctx.set_fonts(fonts);
}

/// Colours, corners and shadows for every standard control and window.
fn theme(style: &mut egui::Style, p: &Palette) {
    use egui::style::WidgetVisuals;
    let v = &mut style.visuals;
    *v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.window;
    v.extreme_bg_color = if p.dark { p.groove } else { p.panel };
    v.faint_bg_color = p.panel;
    v.code_bg_color = p.surface;
    v.window_stroke = Stroke::new(1.0, p.card_border);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = egui::Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 140 } else { 40 }),
    };
    v.popup_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 120 } else { 35 }),
    };
    v.selection.bg_fill = ACCENT.gamma_multiply(if p.dark { 0.45 } else { 0.3 });
    v.selection.stroke = Stroke::new(
        1.0,
        if p.dark {
            Color32::from_rgb(0xb4, 0xc4, 0xff)
        } else {
            Color32::from_rgb(0x2f, 0x4f, 0xd8)
        },
    );
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = METER_YELLOW;
    v.error_fg_color = MUTE_RED;
    v.slider_trailing_fill = true;
    let w = |bg: Color32, stroke: Color32, fg: Color32| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: CornerRadius::same(7),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    };
    let strong = if p.dark { Color32::WHITE } else { Color32::BLACK };
    v.widgets.noninteractive = w(p.panel, p.card_border, p.text.gamma_multiply(0.85));
    v.widgets.inactive = w(p.surface, p.surface_border, p.text);
    v.widgets.hovered = w(p.surface_hover, p.surface_border.lerp_to_gamma(strong, 0.12), strong);
    v.widgets.active = w(p.surface_active, ACCENT, strong);
    v.widgets.open = v.widgets.hovered;
    style.spacing.button_padding = Vec2::new(10.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(6);
}

/// A rounded toggle button: tinted in `color` when on, quiet when off.
fn chip(text: &str, on: bool, color: Color32, size: Vec2) -> egui::Button<'static> {
    let (fill, stroke, fg) = if on {
        (
            color.gamma_multiply(if pal().dark { 0.2 } else { 0.16 }),
            color.gamma_multiply(0.85),
            ink(color),
        )
    } else {
        (pal().surface, pal().surface_border, pal().text_muted)
    };
    egui::Button::new(RichText::new(text).family(semibold()).size(11.0).color(fg))
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(7)
        .min_size(size)
}

/// A rounded button filled with `color` (an active alarm-like state: muted, recording, cue).
fn chip_solid(text: &str, color: Color32, text_color: Color32, size: Vec2) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).family(semibold()).size(11.0).color(text_color))
        .fill(color)
        .stroke(Stroke::new(1.0, color))
        .corner_radius(7)
        .min_size(size)
}

/// Teal for outputs to speakers/headphones, violet for virtual microphones.
fn output_color(b: &Bus) -> Color32 {
    match b.kind {
        Kind::Physical { .. } => OUT_A,
        Kind::Virtual => OUT_B,
    }
}

#[derive(Default)]
struct Shared {
    connected: bool,
    config: Option<Config>,
    devices: Vec<Device>,
    midi_connected: bool,
    midi_ports: Vec<String>,
    learning: Option<LearnTarget>,
    apps: Vec<AppInfo>,
    recordings: Vec<RecordingInfo>,
    /// What each channel's effects are doing right now.
    fx_status: HashMap<String, ipc::FxStatus>,
    /// Running speaker test: (device, channel sounding now).
    speaker_test: Option<(String, String)>,
    meters: HashMap<String, [f32; 2]>,
    error: Option<String>,
    writer: Option<UnixStream>,
}

/// Developer aid: shows the config at `path` with made-up devices, apps and moving meters,
/// without an engine (for documentation screenshots). Nothing is sent anywhere.
fn demo_thread(shared: Arc<Mutex<Shared>>, ctx: egui::Context, path: String) {
    let cfg = match Config::load(std::path::Path::new(&path)) {
        Ok(c) => c,
        Err(e) => {
            shared.lock().unwrap().error = Some(format!("demo config: {e:#}"));
            return;
        }
    };
    let profile = |index, description: &str, outputs, inputs| ipc::Profile {
        index,
        description: description.into(),
        available: true,
        outputs,
        inputs,
    };
    let ch = |c: &str| c.split(',').map(str::to_string).collect::<Vec<_>>();
    let devices = vec![
        Device {
            name: "alsa_output.pci-0000_00_1f.3.analog-surround-51".into(),
            description: "Built-in Audio Analog Surround 5.1".into(),
            output: true,
            channels: ch("FL,FR,FC,LFE,RL,RR"),
            card: Some(1),
            profiles: vec![
                profile(1, "Analog Stereo Duplex", true, true),
                profile(2, "Analog Stereo Output", true, false),
                profile(3, "Analog Surround 2.1 Output", true, false),
                profile(4, "Analog Surround 4.0 Output", true, false),
                profile(5, "Analog Surround 5.1 Output", true, false),
                profile(6, "Analog Surround 7.1 Output", true, false),
            ],
            profile: Some(5),
        },
        Device {
            name: "alsa_output.usb-Headset-00.analog-stereo".into(),
            description: "USB Headset Analog Stereo".into(),
            output: true,
            channels: ch("FL,FR"),
            card: Some(2),
            profiles: vec![],
            profile: None,
        },
        Device {
            name: "alsa_input.usb-Microphone-00.analog-stereo".into(),
            description: "USB Microphone Analog Stereo".into(),
            output: false,
            channels: ch("FL,FR"),
            card: Some(3),
            profiles: vec![],
            profile: None,
        },
    ];
    {
        let mut s = shared.lock().unwrap();
        s.connected = true;
        s.midi_connected = true;
        s.midi_ports = vec!["Example MIDI Controller".into()];
        s.devices = devices;
        s.apps = vec![
            AppInfo {
                name: "Firefox".into(),
                strips: vec!["browser".into()],
                other: vec![],
                streams: 3,
            },
            AppInfo {
                name: "Music Player".into(),
                strips: vec!["music".into()],
                other: vec![],
                streams: 1,
            },
            AppInfo {
                name: "Voice Chat".into(),
                strips: vec!["chat".into()],
                other: vec![],
                streams: 1,
            },
            AppInfo {
                name: "Game".into(),
                strips: vec!["game".into()],
                other: vec![],
                streams: 1,
            },
        ];
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        s.recordings = cfg
            .buses
            .iter()
            .filter(|b| b.id == "b2")
            .map(|b| RecordingInfo {
                bus: b.id.clone(),
                path: String::new(),
                started: now - 754,
            })
            .collect();
        s.config = Some(cfg.clone());
    }
    let start = Instant::now();
    loop {
        let t = start.elapsed().as_secs_f32();
        let mut meters = HashMap::new();
        let mut fx = HashMap::new();
        let ids = cfg
            .strips
            .iter()
            .map(|s| (s.id.clone(), s.mute, s.gain_db, s.fx.duck.enabled))
            .chain(
                cfg.buses
                    .iter()
                    .map(|b| (b.id.clone(), b.mute, b.gain_db, b.ducks())),
            );
        for (i, (id, mute, gain, ducks)) in ids.enumerate() {
            let k = i as f32;
            let wave = 0.5 + 0.5 * (t * (1.3 + k * 0.37) + k).sin() * (t * 0.7 + k * 2.0).cos();
            let db = if mute { -120.0 } else { -30.0 + 22.0 * wave + gain * 0.3 };
            let lin = 10f32.powf(db / 20.0);
            meters.insert(id.clone(), [lin, lin * (0.85 + 0.1 * (t + k).sin())]);
            fx.insert(
                id,
                ipc::FxStatus {
                    level_db: db + 4.0,
                    gate_open: db > -34.0,
                    reduction_db: (wave * 6.0 - 1.0).max(0.0),
                    limit_db: (wave * 3.0 - 1.5).max(0.0),
                    duck_db: if ducks { 9.0 } else { 0.0 },
                    duck_source_db: -18.0,
                    deess_db: (wave * 8.0 - 3.0).max(0.0),
                },
            );
        }
        {
            let mut s = shared.lock().unwrap();
            for b in &cfg.buses {
                if let Some(st) = fx.get(&b.id).copied() {
                    fx.insert(crate::graph::bus_duck_id(&b.id), st);
                }
            }
            s.meters = meters;
            s.fx_status = fx;
        }
        ctx.request_repaint();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Start the engine and have it start at every login: through its systemd user service
/// when there is one (installed packages), otherwise by running it directly.
fn start_engine() {
    let enabled = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "mixrail"])
        .status()
        .is_ok_and(|s| s.success());
    if !enabled {
        let exe = std::env::current_exe().unwrap_or_else(|_| "mixrail".into());
        if let Ok(mut child) = std::process::Command::new(exe).arg("run").spawn() {
            let _ = child.wait();
        }
    }
}

/// Keeps a connection to the engine, reconnecting whenever it drops.
fn connection_thread(shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    loop {
        if let Ok(stream) = UnixStream::connect(ipc::socket_path()) {
            {
                let mut s = shared.lock().unwrap();
                s.connected = true;
                s.writer = stream.try_clone().ok();
            }
            ctx.request_repaint();
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let Ok(ev) = serde_json::from_str::<Event>(&line) else {
                    continue;
                };
                let mut s = shared.lock().unwrap();
                match ev {
                    Event::State {
                        config,
                        devices,
                        midi_connected,
                        midi_ports,
                        learning,
                        apps,
                        recordings,
                        ..
                    } => {
                        s.config = Some(config);
                        s.devices = devices;
                        s.midi_connected = midi_connected;
                        s.midi_ports = midi_ports;
                        s.learning = learning;
                        s.apps = apps;
                        s.recordings = recordings;
                    }
                    Event::Meters { levels, fx } => {
                        s.meters = levels;
                        s.fx_status = fx;
                    }
                    Event::Error { message } => s.error = Some(message),
                    Event::SpeakerTest { device, channel } => {
                        s.speaker_test = channel.map(|c| (device, c));
                    }
                    Event::Raise => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                drop(s);
                ctx.request_repaint();
            }
            let mut s = shared.lock().unwrap();
            s.connected = false;
            s.writer = None;
            s.meters.clear();
            drop(s);
            ctx.request_repaint();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The add/edit dialog for a strip or bus.
struct Editor {
    is_bus: bool,
    /// None when adding a new one.
    id: Option<String>,
    name: String,
    physical: bool,
    device: String,
    /// Outputs only: how stereo is spread over surround speakers.
    upmix: Upmix,
    /// Inputs only: the channels a virtual input offers apps.
    layout: ChannelLayout,
    /// Sound card of the chosen device; when the card switches profile the device gets a
    /// new name, and the dialog follows it.
    card: Option<u32>,
    confirm_delete: bool,
}

struct App {
    shared: Arc<Mutex<Shared>>,
    /// Gains/mutes the user just changed here, by strip/bus id.
    local_gain: HashMap<String, (f32, Instant)>,
    local_mute: HashMap<String, (bool, Instant)>,
    local_pan: HashMap<String, (f32, Instant)>,
    /// Smoothed meter positions (0..1) per id, so meters fall back gracefully.
    meter_pos: HashMap<String, [f32; 2]>,
    last_frame: Instant,
    editor: Option<Editor>,
    /// Window preferences (theme).
    settings: GuiSettings,
    /// The app icon as a texture, for the top bar.
    logo: Option<egui::TextureHandle>,
    /// Whether the engine was already started automatically (only tried once).
    auto_started: bool,
    /// Developer aid: `LAM_SCREENSHOT=/path/out.ppm` saves an image of the window and exits.
    screenshot: Option<(String, Instant)>,
    /// (strips, buses) the window was last sized for; resize when that changes.
    sized_for: Option<(usize, usize)>,
    started: Instant,
    /// When the window was last made taller to fit its content.
    last_grow: Option<Instant>,
    /// The width fit_window last asked for (so growing taller keeps it).
    target_width: Option<f32>,
    /// MIDI learn in progress (copied from the engine each frame).
    learning: Option<LearnTarget>,
    midi_window: bool,
    /// Apps currently playing (copied from the engine each frame).
    apps: Vec<AppInfo>,
    apps_window: bool,
    /// Name being typed in the "save scene" dialog.
    scene_dialog: Option<String>,
    /// Strip whose gate/compressor settings panel is open.
    fx_window: Option<String>,
    /// Strip whose EQ window is open.
    eq_window: Option<String>,
    /// EQ point being dragged on the graph.
    eq_drag: Option<usize>,
    /// EQ point the right-click menu is for.
    eq_menu: Option<usize>,
    /// Name being typed to save the EQ as a preset.
    eq_preset_name: Option<String>,
    /// Effects the user just changed here, by strip id.
    local_fx: HashMap<String, (Effects, Instant)>,
}

/// Width of the outputs area for `buses` output cards.
fn outputs_width(buses: usize) -> f32 {
    buses as f32 * CARD_W + ADD_W + 24.0
}

/// Window width that shows every strip and bus without scrolling.
fn ideal_width(cfg: &Config) -> f32 {
    let inputs = cfg.strips.len() as f32 * CARD_W + ADD_W;
    inputs + outputs_width(cfg.buses.len()) + 40.0
}

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mixrail")
            .with_app_id("mixrail")
            .with_icon(
                eframe::icon_data::from_png_bytes(ICON_PNG)
                    .map(Arc::new)
                    .unwrap_or_default(),
            )
            .with_inner_size([1480.0, 620.0])
            .with_min_inner_size([420.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "mixrail",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            cc.egui_ctx
                .style_mut_of(egui::Theme::Dark, |style| theme(style, &DARK_PALETTE));
            cc.egui_ctx
                .style_mut_of(egui::Theme::Light, |style| theme(style, &LIGHT_PALETTE));
            let settings = GuiSettings::load();
            cc.egui_ctx.set_theme(settings.theme.preference());
            cc.egui_ctx.all_styles_mut(|style| {
                style.spacing.item_spacing = Vec2::new(6.0, 6.0);
                style.spacing.button_padding = Vec2::new(8.0, 4.0);
                // A real, always-visible scroll bar when channels don't fit (not one that
                // only appears on hover).
                style.spacing.scroll = egui::style::ScrollStyle::solid();
                style.spacing.scroll.bar_width = 10.0;
            });
            let shared = Arc::new(Mutex::new(Shared::default()));
            let (s, ctx) = (shared.clone(), cc.egui_ctx.clone());
            match std::env::var("LAM_DEMO") {
                // Developer aid (README screenshots): an example mixer, no engine needed.
                Ok(path) => {
                    std::thread::spawn(move || demo_thread(s, ctx, path));
                }
                Err(_) => {
                    std::thread::spawn(move || connection_thread(s, ctx));
                }
            }
            Ok(Box::new(App {
                shared,
                local_gain: HashMap::new(),
                local_mute: HashMap::new(),
                local_pan: HashMap::new(),
                meter_pos: HashMap::new(),
                last_frame: Instant::now(),
                editor: None,
                settings,
                logo: None,
                auto_started: std::env::var("LAM_DEMO").is_ok(),
                screenshot: std::env::var("LAM_SCREENSHOT")
                    .ok()
                    .map(|p| (p, Instant::now())),
                sized_for: None,
                started: Instant::now(),
                last_grow: None,
                target_width: None,
                learning: None,
                // Developer aid for screenshots of the MIDI window.
                midi_window: false,
                apps: Vec::new(),
                apps_window: false,
                scene_dialog: None,
                fx_window: None,
                eq_window: None,
                eq_drag: None,
                eq_menu: None,
                eq_preset_name: None,
                local_fx: HashMap::new(),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

impl App {
    fn send(&self, cmd: &Command) {
        let mut s = self.shared.lock().unwrap();
        if let (Some(w), Ok(line)) = (s.writer.as_mut(), serde_json::to_string(cmd)) {
            let _ = w.write_all(format!("{line}\n").as_bytes());
        }
    }

    fn gain(&self, id: &str, engine_value: f32) -> f32 {
        match self.local_gain.get(id) {
            Some((v, t)) if t.elapsed() < LOCAL_HOLD => *v,
            _ => engine_value,
        }
    }

    fn mute(&self, id: &str, engine_value: bool) -> bool {
        match self.local_mute.get(id) {
            Some((v, t)) if t.elapsed() < LOCAL_HOLD => *v,
            _ => engine_value,
        }
    }

    fn set_gain(&mut self, id: &str, db: f32) {
        self.local_gain.insert(id.to_string(), (db, Instant::now()));
        self.send(&Command::SetGain {
            id: id.to_string(),
            db,
        });
    }

    fn toggle_mute(&mut self, id: &str, current: bool) {
        self.local_mute
            .insert(id.to_string(), (!current, Instant::now()));
        self.send(&Command::SetMute {
            id: id.to_string(),
            mute: !current,
        });
    }

    /// Update smoothed meter positions from the latest peaks.
    fn update_meters(&mut self, peaks: &HashMap<String, [f32; 2]>) {
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.2);
        self.last_frame = Instant::now();
        let fall = 0.9 * dt; // full scale in ~1.1 s
        for (id, p) in peaks {
            let target = p.map(|v| config::db_to_pos(config::linear_to_db(v)));
            let cur = self.meter_pos.entry(id.clone()).or_insert([0.0; 2]);
            for i in 0..2 {
                cur[i] = target[i].max(cur[i] - fall);
            }
        }
    }

    fn meter(&self, id: &str) -> [f32; 2] {
        self.meter_pos.get(id).copied().unwrap_or([0.0; 2])
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        DARK_MODE.with(|d| d.set(ui.ctx().theme() == egui::Theme::Dark));
        let (connected, config, devices, midi_connected, midi_ports, peaks, error) = {
            let s = self.shared.lock().unwrap();
            (
                s.connected,
                s.config.clone(),
                s.devices.clone(),
                s.midi_connected,
                s.midi_ports.clone(),
                s.meters.clone(),
                s.error.clone(),
            )
        };
        {
            let s = self.shared.lock().unwrap();
            self.learning = s.learning.clone();
            self.apps = s.apps.clone();
        }
        self.update_meters(&peaks);
        // Opened while the engine isn't running (e.g. right after installing): start it,
        // and have it start at login from now on.
        if !connected && !self.auto_started && self.started.elapsed() > Duration::from_secs(2) {
            self.auto_started = true;
            std::thread::spawn(start_engine);
        }
        if connected {
            self.auto_started = true;
        }
        if let Some(cfg) = &config {
            if panel_shot() {
                // Room for a whole panel without scrolling (asked until the window has it).
                // LAM_ZOOM < 1 fits a long panel on the screen.
                let zoom: f32 = std::env::var("LAM_ZOOM")
                    .ok()
                    .and_then(|z| z.parse().ok())
                    .unwrap_or(1.0);
                ui.ctx().set_zoom_factor(zoom);
                if ui.ctx().content_rect().height() < 1300.0 / zoom {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(
                        Vec2::new(900.0, 1350.0) / zoom,
                    ));
                }
            } else {
                self.fit_window(ui.ctx(), cfg);
            }
            // Developer aid for screenshots: LAM_OPEN=midi|apps|bus:<id>|strip:<id>
            if let Some(what) = std::env::var("LAM_OPEN")
                .ok()
                .filter(|_| self.screenshot.is_some())
                && self.started.elapsed() < Duration::from_millis(500)
            {
                match what.split_once(':') {
                    None if what == "midi" => self.midi_window = true,
                    None if what == "apps" => self.apps_window = true,
                    Some(("fx", id)) => self.fx_window = Some(id.to_string()),
                    Some(("eq", id)) => self.eq_window = Some(id.to_string()),
                    Some(("bus", id)) => {
                        self.editor = cfg.buses.iter().find(|b| b.id == id).map(Editor::edit_bus)
                    }
                    Some(("strip", id)) => {
                        self.editor = cfg
                            .strips
                            .iter()
                            .find(|s| s.id == id)
                            .map(Editor::edit_strip)
                    }
                    _ => {}
                }
            }
        }
        if let Some((path, _)) = self
            .screenshot
            .take_if(|(_, t)| t.elapsed() > Duration::from_secs(3))
        {
            let ppp = ui.ctx().pixels_per_point();
            let crop: Option<Rect> = ui
                .ctx()
                .data(|d| d.get_temp(egui::Id::unique("lam_shot_rect")));
            ui.ctx().request_screenshot(move |img| {
                let [iw, ih] = img.size;
                // The whole window, or just the dialog/panel (with a small margin).
                let (x0, y0, x1, y1) = match crop {
                    Some(r) => {
                        let r = r.expand(6.0);
                        let px = |v: f32, max: usize| ((v * ppp).round().max(0.0) as usize).min(max);
                        (px(r.left(), iw), px(r.top(), ih), px(r.right(), iw), px(r.bottom(), ih))
                    }
                    None => (0, 0, iw, ih),
                };
                let (w, h) = (x1 - x0, y1 - y0);
                let mut out = format!("P6 {w} {h} 255\n").into_bytes();
                for y in y0..y1 {
                    for p in &img.pixels[y * iw + x0..y * iw + x1] {
                        out.extend_from_slice(&[p.r(), p.g(), p.b()]);
                    }
                }
                let _ = std::fs::write(&path, out);
                std::process::exit(0);
            });
        }
        if self.screenshot.is_some() {
            ui.ctx().request_repaint();
        }
        if connected {
            // Keep meters animating smoothly between engine updates.
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let logo = self.logo.get_or_insert_with(|| {
                    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).unwrap_or_default();
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [icon.width as usize, icon.height as usize],
                        &icon.rgba,
                    );
                    ui.ctx().load_texture("logo", image, egui::TextureOptions::LINEAR)
                });
                ui.add(egui::Image::new((logo.id(), Vec2::splat(24.0))));
                ui.label(RichText::new("Mixrail").family(semibold()).size(18.0).color(pal().text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Theme: follow the system, or always dark / light.
                    let before = self.settings.theme;
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(format!("Theme: {}", before.label()))
                        .show_ui(ui, |ui| {
                            for t in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
                                ui.selectable_value(&mut self.settings.theme, t, t.label());
                            }
                        })
                        .response
                        .on_hover_text("System follows your desktop's light/dark setting");
                    if self.settings.theme != before {
                        ui.ctx().set_theme(self.settings.theme.preference());
                        self.settings.save();
                    }
                    if ui
                        .add_enabled(connected, egui::Button::new("MIDI…"))
                        .clicked()
                    {
                        self.midi_window = true;
                    }
                    if ui
                        .add_enabled(connected, egui::Button::new("Apps…"))
                        .on_hover_text("Choose which channel each app plays into")
                        .clicked()
                    {
                        self.apps_window = true;
                    }
                    // Which output plays cued channels.
                    if let Some(cfg) = config.as_ref().filter(|_| connected) {
                        let current = cfg.cue_bus.clone();
                        let shown = current
                            .as_ref()
                            .and_then(|id| cfg.buses.iter().find(|b| &b.id == id))
                            .map(|b| b.name.clone())
                            .unwrap_or_else(|| "off".into());
                        let mut choice = current.clone();
                        egui::ComboBox::from_id_salt("cue-bus")
                            .selected_text(format!("Cue: {shown}"))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut choice, None, "Off");
                                for b in &cfg.buses {
                                    ui.selectable_value(&mut choice, Some(b.id.clone()), &b.name);
                                }
                            })
                            .response
                            .on_hover_text(
                                "Cue lets you listen to chosen channels on this output (e.g. headphones)\n\
                                 while every other output keeps the normal mix.",
                            );
                        if choice != current {
                            self.send(&Command::SetCueBus { bus: choice });
                        }
                    }
                    if config.as_ref().is_some_and(|c| c.midi.is_some()) {
                        status_dot(ui, midi_connected);
                        ui.label(if midi_connected {
                            "MIDI controller"
                        } else {
                            "MIDI: not connected"
                        });
                        ui.separator();
                    }
                    if connected {
                        status_dot(ui, true);
                        ui.label("Engine running");
                    } else {
                        if ui.button("Start engine").clicked() {
                            std::thread::spawn(start_engine);
                        }
                        status_dot(ui, false);
                        ui.label("Engine not running");
                    }
                });
            });
            ui.add_space(4.0);
        });

        if let Some(msg) = &error {
            egui::Panel::top("error").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(MUTE_RED, format!("⚠ {msg}"));
                    if ui.small_button("Dismiss").clicked() {
                        self.shared.lock().unwrap().error = None;
                    }
                });
            });
        }

        if let (true, Some(cfg)) = (connected, &config) {
            egui::Panel::top("scenes").show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("SCENES")
                            .small()
                            .strong()
                            .color(pal().text_muted),
                    );
                    for scene in &cfg.scenes {
                        let active = cfg.scene_active(&scene.name);
                        let btn = chip(&scene.name, active, ACCENT, Vec2::new(70.0, 24.0));
                        let resp = ui.add(btn).on_hover_text(if active {
                            "This scene is active. Right-click for options."
                        } else {
                            "Click to switch to this mix. Right-click for options."
                        });
                        if resp.clicked() {
                            self.send(&Command::RecallScene {
                                name: scene.name.clone(),
                            });
                        }
                        let extra = vec![
                            (
                                "Update with the current mix",
                                MenuItem::Send(Command::SaveScene {
                                    name: scene.name.clone(),
                                }),
                            ),
                            (
                                "Delete scene",
                                MenuItem::Send(Command::DeleteScene {
                                    name: scene.name.clone(),
                                }),
                            ),
                        ];
                        self.midi_control(ui, &resp, cfg, &scene.name, Action::Scene, extra);
                    }
                    if ui
                        .button("+ Save current mix")
                        .on_hover_text(
                            "Save every fader, mute and send as a scene you can switch back to",
                        )
                        .clicked()
                    {
                        self.scene_dialog = Some(format!("Scene {}", cfg.scenes.len() + 1));
                    }
                });
                ui.add_space(2.0);
            });
        }

        if let Some(cfg) = config
            .as_ref()
            .filter(|c| connected && c.cueing().is_some())
        {
            let cue_name = cfg
                .cueing()
                .and_then(|id| cfg.buses.iter().find(|b| b.id == id))
                .map(|b| b.name.clone())
                .unwrap_or_default();
            let cued: Vec<&str> = cfg
                .strips
                .iter()
                .filter(|s| s.cue)
                .map(|s| s.name.as_str())
                .collect();
            let mut stop = false;
            egui::Panel::top("cueing")
                .frame(
                    egui::Frame::new()
                        .fill(pal().bg.lerp_to_gamma(METER_YELLOW, 0.14))
                        .stroke(Stroke::new(1.0, pal().bg.lerp_to_gamma(METER_YELLOW, 0.45)))
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("CUE").family(semibold()).color(ink(METER_YELLOW)));
                        ui.label(
                            RichText::new(format!(
                                "{cue_name} plays only {} — other outputs keep the normal mix",
                                cued.join(", ")
                            ))
                            .color(pal().text),
                        );
                        stop = ui.add(chip("Stop cueing", false, METER_YELLOW, Vec2::new(0.0, 22.0))).clicked();
                    });
                });
            if stop {
                for s in cfg.strips.iter().filter(|s| s.cue) {
                    self.send(&Command::SetCue {
                        id: s.id.clone(),
                        on: false,
                    });
                }
            }
        }

        if let (Some(lt), Some(cfg)) = (&self.learning, &config) {
            let what = describe(cfg, &lt.target, &lt.action);
            let mut cancel = false;
            egui::Panel::top("learning")
                .frame(
                    egui::Frame::new()
                        .fill(pal().bg.lerp_to_gamma(ACCENT, 0.3))
                        .inner_margin(8),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "MIDI learn: move or press the control you want to use for “{what}”"
                            ))
                            .strong()
                            .color(pal().text),
                        );
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if cancel {
                self.send(&Command::CancelLearn);
            }
        }

        let central = egui::CentralPanel::default_margins().show(ui, |ui| {
            let Some(cfg) = config.clone() else {
                ui.centered_and_justified(|ui| {
                    ui.label(if connected {
                        "Loading…"
                    } else {
                        "Waiting for the mixer engine…\n\nIt normally starts automatically at login.\nUse \"Start engine\" above if it isn't running."
                    });
                });
                return None;
            };
            ui.add_enabled_ui(connected, |ui| {
                // Outputs sit on the right; inputs scroll beside them.
                // Wide enough for every output card plus the add button, but never so wide
                // that the inputs disappear.
                let outputs_w = outputs_width(cfg.buses.len()).min(ui.available_width() * 0.6);
                egui::Panel::right("outputs")
                    .resizable(false)
                    .exact_size(outputs_w)
                    .frame(egui::Frame::new().inner_margin(egui::Margin { left: 10, ..Default::default() }))
                    .show(ui, |ui| {
                        egui::ScrollArea::horizontal().id_salt("outputs-scroll").show(ui, |ui| {
                            self.section(ui, "OUTPUTS", |app, ui| {
                                for b in &cfg.buses {
                                    app.bus_panel(ui, &cfg, b, &devices);
                                }
                                if add_card(ui, "Add output").clicked() {
                                    app.editor = Some(Editor::new(true));
                                }
                            });
                        });
                    });
                egui::ScrollArea::horizontal().id_salt("inputs-scroll").show(ui, |ui| {
                    self.section(ui, "INPUTS", |app, ui| {
                        for s in &cfg.strips {
                            app.strip_panel(ui, &cfg, s, &devices);
                        }
                        if add_card(ui, "Add input").clicked() {
                            app.editor = Some(Editor::new(false));
                        }
                    });
                });
            });
            // How tall the content is, so the window can grow to fit it.
            Some(ui.min_rect().height())
        });
        // Grow the window if the content needs more height (an extra row of buttons, a
        // banner…), up to the screen's height. Width is handled by fit_window.
        let needed = central.response.rect.top() + central.inner.unwrap_or(0.0) + 16.0;
        let (have, monitor) = ui
            .ctx()
            .input(|i| (i.content_rect().height(), i.viewport().monitor_size));
        if needed > have + 1.0
            && self
                .last_grow
                .is_none_or(|t| t.elapsed() > Duration::from_millis(300))
        {
            self.last_grow = Some(Instant::now());
            let max_h = monitor.map(|m| m.y - 60.0).unwrap_or(1000.0);
            let width = self
                .target_width
                .unwrap_or_else(|| ui.ctx().content_rect().width());
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
                    width,
                    needed.min(max_h),
                )));
        }

        if let Some(cfg) = &config {
            self.editor_window(ui.ctx(), cfg, &devices);
            self.midi_settings_window(ui.ctx(), cfg, &midi_ports, midi_connected);
            self.apps_window(ui.ctx(), cfg);
            self.scene_save_dialog(ui.ctx(), cfg);
            self.fx_panel(ui.ctx(), cfg);
            self.eq_window(ui.ctx(), cfg);
        }
    }
}

impl App {
    /// Grow or shrink the window to fit all channels whenever channels are added or
    /// removed, up to the monitor width (beyond that the inputs scroll). Between those
    /// moments the user is free to resize the window.
    fn fit_window(&mut self, ctx: &egui::Context, cfg: &Config) {
        let counts = (cfg.strips.len(), cfg.buses.len());
        if self.sized_for == Some(counts) {
            return;
        }
        let (monitor, height) =
            ctx.input(|i| (i.viewport().monitor_size, i.content_rect().height()));
        // The monitor size arrives a frame or two after the window opens (on Wayland the
        // window position never does). Wait briefly for it, then fall back to a guess.
        if monitor.is_none() && self.started.elapsed() < Duration::from_secs(1) {
            ctx.request_repaint();
            return;
        }
        self.sized_for = Some(counts);
        let max_w = monitor.map(|m| m.x).unwrap_or(1920.0) - 40.0;
        let width = ideal_width(cfg).clamp(420.0, max_w);
        self.target_width = Some(width);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
            width,
            height.max(560.0),
        )));
    }

    fn section(&mut self, ui: &mut Ui, title: &str, body: impl FnOnce(&mut Self, &mut Ui)) {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).small().strong().color(pal().text_muted));
            ui.horizontal_top(|ui| body(self, ui));
        });
    }

    fn strip_panel(&mut self, ui: &mut Ui, cfg: &Config, s: &Strip, devices: &[Device]) {
        let gain = self.gain(&s.id, s.gain_db);
        let muted = self.mute(&s.id, s.mute);
        let meter = self.meter(&s.id);
        card(ui, |ui| {
            if title(ui, &s.name, &kind_label(&s.kind, devices, false)).clicked() {
                self.editor = Some(Editor::edit_strip(s));
            }
            // Which apps are playing into this strip right now.
            let playing: Vec<&str> = self
                .apps
                .iter()
                .filter(|a| a.strips.contains(&s.id))
                .map(|a| a.name.as_str())
                .collect();
            let text = if playing.is_empty() {
                " ".to_string()
            } else {
                playing.join(", ")
            };
            let resp = ui.add(
                egui::Label::new(RichText::new(text).small().color(ACCENT))
                    .truncate()
                    .sense(Sense::click()),
            );
            if !playing.is_empty() {
                resp.clone()
                    .on_hover_text(format!("Playing here: {}", playing.join(", ")));
            }
            if resp.clicked() {
                self.apps_window = true;
            }
            let pan = match self.local_pan.get(&s.id) {
                Some((p, t)) if t.elapsed() < LOCAL_HOLD => *p,
                _ => s.pan,
            };
            let (resp, new_pan) = pan_slider(ui, pan);
            let resp = resp.on_hover_text(format!(
                "{}\nDrag or scroll; double-click for centre; right-click for MIDI",
                pan_text(pan)
            ));
            if let Some(p) = new_pan {
                self.local_pan.insert(s.id.clone(), (p, Instant::now()));
                self.send(&Command::SetPan {
                    id: s.id.clone(),
                    pan: p,
                });
            }
            self.midi_control(ui, &resp, cfg, &s.id, Action::Pan, Vec::new());
            let (resp, new_db) = fader(ui, gain, meter, muted);
            if let Some(db) = new_db {
                self.set_gain(&s.id, db);
            }
            self.midi_control(ui, &resp, cfg, &s.id, Action::Gain, Vec::new());
            let ducking = self
                .shared
                .lock()
                .unwrap()
                .fx_status
                .get(&s.id)
                .map(|st| st.duck_db)
                .filter(|d| *d > 0.5);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(3.0, 0.0);
                ui.add_space(if ducking.is_some() { 4.0 } else { 18.0 });
                ui.label(RichText::new(db_text(gain)).monospace());
                if let Some(d) = ducking {
                    ui.label(
                        RichText::new(format!("▼{d:.0}"))
                            .small()
                            .color(ink(METER_YELLOW)),
                    )
                    .on_hover_text(format!("Ducked {d:.0} dB: another channel is active"));
                }
            });
            // Send buttons, one per bus.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                for b in &cfg.buses {
                    let on = s.buses.contains(&b.id);
                    let btn = chip(&short_name(b), on, output_color(b), Vec2::new(30.0, 22.0));
                    let resp = ui.add(btn).on_hover_text(format!("Send to {}", b.name));
                    if resp.clicked() {
                        self.send(&Command::SetRoute {
                            strip: s.id.clone(),
                            bus: b.id.clone(),
                            on: !on,
                        });
                    }
                    self.midi_control(
                        ui,
                        &resp,
                        cfg,
                        &s.id,
                        Action::Route { bus: b.id.clone() },
                        Vec::new(),
                    );
                }
                // Cue: listen to this channel on the cue output.
                if let Some(cue_bus) = cfg
                    .cue_bus
                    .as_ref()
                    .and_then(|id| cfg.buses.iter().find(|b| &b.id == id))
                {
                    let btn = if s.cue {
                        chip_solid("CUE", METER_YELLOW, Color32::from_rgb(0x1a, 0x14, 0x00), Vec2::new(30.0, 22.0))
                    } else {
                        chip("CUE", false, METER_YELLOW, Vec2::new(30.0, 22.0))
                    };
                    let resp = ui.add(btn).on_hover_text(format!(
                        "Listen to {} on {} (other outputs keep the normal mix)",
                        s.name, cue_bus.name
                    ));
                    if resp.clicked() {
                        self.send(&Command::SetCue {
                            id: s.id.clone(),
                            on: !s.cue,
                        });
                    }
                    self.midi_control(ui, &resp, cfg, &s.id, Action::Cue, Vec::new());
                }
            });
            // Effects: an EQ button and the gate and compressor knobs.
            let mut fx = self.effects(s);
            let before = fx.clone();
            let status = self.shared.lock().unwrap().fx_status.get(&s.id).copied();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(2.0, 0.0);
                let eq_on = fx.eq.enabled || fx.low_cut;
                // EQ and noise suppression, stacked.
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                    let eq_btn = chip("EQ", eq_on, ACCENT, Vec2::new(30.0, 18.0));
                    if ui.add(eq_btn).on_hover_text("Equalizer and low cut").clicked() {
                        self.eq_window = Some(s.id.clone());
                    }
                    let ns_btn = chip("NS", fx.noise.enabled, ACCENT, Vec2::new(30.0, 18.0));
                    let hint = if fx.noise.enabled {
                        "Noise suppression on: removes fans, keyboards, hum…\nClick to switch off, right-click for settings"
                    } else {
                        "Noise suppression: removes fans, keyboards, hum while keeping voices.\nClick to switch on, right-click for settings"
                    };
                    let resp = ui.add(ns_btn).on_hover_text(hint);
                    if resp.clicked() {
                        fx.noise.enabled = !fx.noise.enabled;
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Settings…").clicked() {
                            self.fx_window = Some(s.id.clone());
                            ui.close();
                        }
                    });
                });
                let gate_closed = status.is_some_and(|st| !st.gate_open) && fx.gate.enabled;
                let (resp, new) = knob(ui, fx.gate.knob(), "GATE", gate_closed);
                let resp = resp.on_hover_text(if fx.gate.enabled {
                    format!(
                        "Noise gate: cuts sound below {:.0} dB{}\nDrag to turn, double-click for off, right-click for more",
                        fx.gate.threshold_db,
                        if gate_closed { " (closed now)" } else { "" }
                    )
                } else {
                    "Noise gate: off. Turn up to silence background noise.\nRight-click for more".to_string()
                });
                if let Some(pos) = new {
                    fx.gate.set_knob(pos);
                }
                let settings = || vec![("Settings…", MenuItem::OpenFx(s.id.clone()))];
                self.midi_control(ui, &resp, cfg, &s.id, Action::Gate, settings());
                let reducing = status.filter(|st| st.reduction_db > 1.0 && fx.comp.enabled);
                let (resp, new) = knob(ui, fx.comp.knob(), "COMP", reducing.is_some());
                let resp = resp.on_hover_text(match (fx.comp.enabled, reducing) {
                    (true, r) => format!(
                        "Compressor: evens out loud moments above {:.0} dB{}\nDrag to turn, double-click for off, right-click for more",
                        fx.comp.threshold_db,
                        r.map(|st| format!(" (turning down {:.0} dB now)", st.reduction_db)).unwrap_or_default()
                    ),
                    (false, _) => "Compressor: off. Turn up to even out the level.\nRight-click for more".to_string(),
                });
                if let Some(pos) = new {
                    fx.comp.set_knob(pos);
                }
                self.midi_control(ui, &resp, cfg, &s.id, Action::Comp, settings());
            });
            if fx != before {
                self.set_effects(&s.id, fx);
            }
            let resp = mute_button(ui, muted);
            if resp.clicked() {
                self.toggle_mute(&s.id, muted);
            }
            self.midi_control(ui, &resp, cfg, &s.id, Action::Mute, Vec::new());
        });
    }

    /// A strip's effects, preferring what the user just changed here.
    fn effects(&self, s: &Strip) -> Effects {
        self.effects_of(&s.id, &s.fx)
    }

    /// A strip's or output's effects, preferring what the user just changed here.
    fn effects_of(&self, id: &str, saved: &Effects) -> Effects {
        match self.local_fx.get(id) {
            Some((fx, t)) if t.elapsed() < LOCAL_HOLD => fx.clone(),
            _ => saved.clone(),
        }
    }

    fn set_effects(&mut self, id: &str, fx: Effects) {
        self.local_fx
            .insert(id.to_string(), (fx.clone(), Instant::now()));
        self.send(&Command::SetEffects {
            id: id.to_string(),
            fx,
        });
    }

    /// The detailed gate and compressor settings of a channel.
    fn fx_panel(&mut self, ctx: &egui::Context, cfg: &Config) {
        let Some(id) = self.fx_window.clone() else {
            return;
        };
        let Some((name, saved, is_bus)) = channel_effects(cfg, &id) else {
            self.fx_window = None;
            return;
        };
        let mut fx = self.effects_of(&id, saved);
        let before = fx.clone();
        let status = self.shared.lock().unwrap().fx_status.get(&id).copied();
        let window_title = format!("Effects — {name}");
        let open = separate_window(ctx, "fx", &window_title, [600.0, 720.0], |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().slider_width = 220.0;
                let hint = |ui: &mut Ui, text: &str| {
                    ui.label(RichText::new(text).small().color(pal().text_muted));
                };
                let section = |ui: &mut Ui, title: &str, on: &mut bool| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(title).strong().size(15.0));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.checkbox(on, "On");
                        });
                    });
                };

                if !is_bus {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut fx.mono, RichText::new("Mono").strong().size(15.0));
                        hint(ui, "mixes left and right together (e.g. a mic on one side, one-ear listening)");
                    });
                    ui.separator();
                    section(ui, "Noise suppression", &mut fx.noise.enabled);
                    hint(
                        ui,
                        "Removes background noise (fans, keyboards, hum) while keeping voices. Adds 10 ms of delay.",
                    );
                    ui.add_enabled_ui(fx.noise.enabled, |ui| {
                        let mut pct = fx.noise.amount * 100.0;
                        if ui
                            .add(egui::Slider::new(&mut pct, 0.0..=100.0).text("amount").suffix(" %"))
                            .on_hover_text("Lower mixes some of the original back in, if it sounds too processed")
                            .changed()
                        {
                            fx.noise.amount = pct / 100.0;
                        }
                    });
                    ui.separator();
                    section(ui, "Noise gate", &mut fx.gate.enabled);
                    hint(
                        ui,
                        "Silences the channel while it's quiet, e.g. a mic between sentences.",
                    );
                    ui.add_enabled_ui(fx.gate.enabled, |ui| {
                    ui.add(
                        egui::Slider::new(&mut fx.gate.threshold_db, -80.0..=0.0)
                            .text("threshold")
                            .suffix(" dB"),
                    );
                    let level = status.map(|s| s.level_db).unwrap_or(-120.0);
                    ui.horizontal(|ui| {
                        level_bar(ui, level, Some(fx.gate.threshold_db), -80.0, 0.0);
                        let (text, color) = match status {
                            Some(s) if !s.gate_open => ("closed", ink(METER_YELLOW)),
                            Some(_) => ("open", ink(METER_GREEN)),
                            None => ("", pal().text_muted),
                        };
                        ui.label(RichText::new(text).strong().color(color));
                    });
                    hint(
                        ui,
                        "Set the threshold just above the level of the background noise (the bar).",
                    );
                    ui.add(ms_slider(
                        &mut fx.gate.attack_ms,
                        0.1..=50.0,
                        "attack — how fast it opens",
                    ));
                    ui.add(ms_slider(
                        &mut fx.gate.hold_ms,
                        0.0..=2000.0,
                        "hold — stays open after the sound stops",
                    ));
                    ui.add(ms_slider(
                        &mut fx.gate.release_ms,
                        5.0..=2000.0,
                        "release — how fast it fades out",
                    ));
                    ui.add(
                        egui::Slider::new(&mut fx.gate.range_db, 6.0..=80.0)
                            .text("range — how far it turns down")
                            .suffix(" dB"),
                    );
                });

                    ui.separator();
                    section(ui, "De-esser", &mut fx.deess.enabled);
                    hint(
                        ui,
                        "Softens sharp \"s\" and \"sh\" sounds on a voice, leaving the rest alone.",
                    );
                    ui.add_enabled_ui(fx.deess.enabled, |ui| {
                        ui.add(
                            egui::Slider::new(&mut fx.deess.threshold_db, -60.0..=0.0)
                                .text("threshold")
                                .suffix(" dB"),
                        )
                        .on_hover_text("Lower = it reacts to softer \"s\" sounds too");
                        let cut = status.map(|s| s.deess_db).unwrap_or(0.0);
                        ui.horizontal(|ui| {
                            let most = fx.deess.amount_db.max(1.0);
                            level_bar(ui, cut - most, None, -most, 0.0);
                            ui.label(
                                RichText::new(if cut > 0.5 {
                                    format!("softening {cut:.0} dB")
                                } else {
                                    "listening".into()
                                })
                                .strong()
                                .color(if cut > 0.5 { ink(METER_YELLOW) } else { pal().text_muted }),
                            );
                        });
                        ui.add(
                            egui::Slider::new(&mut fx.deess.amount_db, 2.0..=20.0)
                                .text("amount — the most it softens")
                                .suffix(" dB"),
                        );
                        ui.add(
                            egui::Slider::new(&mut fx.deess.freq, 3000.0..=10000.0)
                                .logarithmic(true)
                                .text("from — sounds above this count as sharp")
                                .suffix(" Hz")
                                .fixed_decimals(0),
                        )
                        .on_hover_text("Lower voices: around 4–5 kHz. Higher voices: 6–8 kHz.");
                    });
                    ui.separator();
                }
                if is_bus {
                    limiter_section(ui, &mut fx, status);
                    ui.separator();
                }
                section(ui, "Compressor", &mut fx.comp.enabled);
                hint(ui, "Turns down loud moments so the level is more even.");
                ui.add_enabled_ui(fx.comp.enabled, |ui| {
                    ui.add(
                        egui::Slider::new(&mut fx.comp.threshold_db, -60.0..=0.0)
                            .text("threshold")
                            .suffix(" dB"),
                    );
                    ui.add(
                        egui::Slider::new(&mut fx.comp.ratio, 1.0..=20.0)
                            .logarithmic(true)
                            .text("ratio")
                            .custom_formatter(|v, _| format!("{v:.1}:1")),
                    );
                    ui.add(
                        egui::Slider::new(&mut fx.comp.makeup_db, 0.0..=24.0)
                            .text("makeup gain")
                            .suffix(" dB"),
                    );
                    ui.add(ms_slider(
                        &mut fx.comp.attack_ms,
                        0.1..=200.0,
                        "attack — how fast it reacts",
                    ));
                    ui.add(ms_slider(
                        &mut fx.comp.release_ms,
                        10.0..=2000.0,
                        "release — how fast it lets go",
                    ));
                    ui.add(
                        egui::Slider::new(&mut fx.comp.knee_db, 0.0..=24.0)
                            .text("knee — how gently it eases in")
                            .suffix(" dB"),
                    );
                    let reduction = status.map(|s| s.reduction_db).unwrap_or(0.0);
                    ui.horizontal(|ui| {
                        reduction_bar(ui, reduction);
                        ui.label(format!("turning down {reduction:.1} dB"));
                    });
                });
                if !is_bus {
                    ui.separator();
                    ducking_section(ui, &mut fx, status, cfg, &id, false);
                    ui.separator();
                    limiter_section(ui, &mut fx, status);
                } else {
                    // An output's ducking runs in its own stage, which reports separately.
                    let duck_status = self
                        .shared
                        .lock()
                        .unwrap()
                        .fx_status
                        .get(&crate::graph::bus_duck_id(&id))
                        .copied();
                    ui.separator();
                    ducking_section(ui, &mut fx, duck_status, cfg, &id, true);
                }
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Delay").strong().size(15.0));
                    ui.add(
                        egui::Slider::new(&mut fx.delay_ms, 0.0..=crate::dsp::MAX_DELAY_MS)
                            .suffix(" ms")
                            .step_by(1.0),
                    );
                });
                hint(
                    ui,
                    "Holds the sound back, e.g. to line it up with video that runs behind (capture cards, \
                     webcams). 0 = off.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Defaults")
                        .on_hover_text("Reset the timing settings")
                        .clicked()
                    {
                        let (g, c) = (config::Gate::default(), config::Comp::default());
                        fx.gate = config::Gate {
                            enabled: fx.gate.enabled,
                            threshold_db: fx.gate.threshold_db,
                            ..g
                        };
                        fx.comp = config::Comp {
                            enabled: fx.comp.enabled,
                            threshold_db: fx.comp.threshold_db,
                            ratio: fx.comp.ratio,
                            makeup_db: fx.comp.makeup_db,
                            ..c
                        };
                    }
                    hint(
                        ui,
                        "Gate and compressor react to the level before the fader.",
                    );
                });
              });
        });
        if !open {
            self.fx_window = None;
        }
        if fx != before {
            self.set_effects(&id, fx);
        }
    }

    /// The EQ graph: click to add points, drag them, scroll to change their width.
    fn eq_window(&mut self, ctx: &egui::Context, cfg: &Config) {
        let Some(id) = self.eq_window.clone() else {
            return;
        };
        let Some((name, saved, is_bus)) = channel_effects(cfg, &id) else {
            self.eq_window = None;
            return;
        };
        let mut fx = self.effects_of(&id, saved);
        let before = fx.clone();
        let mut cmds: Vec<Command> = Vec::new();
        let _ = is_bus;
        let window_title = format!("Equalizer — {name}");
        let open = separate_window(ctx, "eq", &window_title, [740.0, 640.0], |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut fx.eq.enabled, RichText::new("Equalizer on").strong());
                ui.separator();
                // Tall enough for every preset; it only scrolls once the screen runs out.
                let screen = ui.ctx().content_rect().height();
                egui::ComboBox::from_id_salt(("eq-preset", &id))
                    .selected_text("Presets…")
                    .height(screen - 40.0)
                    .show_ui(ui, |ui| {
                        for name in config::Eq::PRESET_NAMES {
                            if ui.selectable_label(false, name).clicked() {
                                let (eq, low_cut) = config::Eq::preset(name);
                                fx.eq = eq;
                                if let Some(hz) = low_cut {
                                    fx.low_cut = true;
                                    fx.low_cut_hz = hz;
                                }
                            }
                        }
                        if !cfg.eq_presets.is_empty() {
                            ui.separator();
                            ui.label(
                                RichText::new("Your presets")
                                    .small()
                                    .strong()
                                    .color(pal().text_muted),
                            );
                            for preset in &cfg.eq_presets {
                                ui.horizontal(|ui| {
                                    if ui.selectable_label(false, &preset.name).clicked() {
                                        preset.apply(&mut fx);
                                        ui.close();
                                    }
                                    if ui
                                        .small_button("✖")
                                        .on_hover_text("Delete this preset")
                                        .clicked()
                                    {
                                        cmds.push(Command::DeleteEqPreset {
                                            name: preset.name.clone(),
                                        });
                                    }
                                });
                            }
                        }
                    });
                if ui
                    .button("Save as preset…")
                    .on_hover_text("Save these points and the low cut to reuse on any channel")
                    .clicked()
                {
                    self.eq_preset_name = Some(format!("My preset {}", cfg.eq_presets.len() + 1));
                }
                ui.separator();
                ui.checkbox(&mut fx.low_cut, "Low cut");
                ui.add_enabled(
                    fx.low_cut,
                    egui::DragValue::new(&mut fx.low_cut_hz)
                        .range(20.0..=300.0)
                        .speed(1.0)
                        .suffix(" Hz"),
                )
                .on_hover_text(
                    "Removes rumble below this frequency (desk bumps, fans, breath pops)",
                );
            });
            if let Some(name) = &mut self.eq_preset_name {
                let trimmed = name.trim().to_string();
                let builtin = config::Eq::PRESET_NAMES
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&trimmed));
                let exists = cfg.eq_presets.iter().any(|p| p.name == trimmed);
                let mut done = false;
                ui.horizontal(|ui| {
                    ui.label("Preset name");
                    let resp = ui.text_edit_singleline(name);
                    resp.request_focus();
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ok = !trimmed.is_empty() && !builtin;
                    if (ui
                        .add_enabled(
                            ok,
                            egui::Button::new(if exists { "Replace" } else { "Save" }),
                        )
                        .clicked()
                        || enter)
                        && ok
                    {
                        cmds.push(Command::SaveEqPreset {
                            preset: config::EqPreset::from_effects(&trimmed, &fx),
                        });
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                });
                if builtin {
                    ui.colored_label(
                        METER_YELLOW,
                        "That's the name of a built-in preset; pick another name.",
                    );
                } else if exists {
                    ui.colored_label(
                        METER_YELLOW,
                        "You already have a preset with this name; it will be replaced.",
                    );
                }
                if done {
                    self.eq_preset_name = None;
                }
            }
            ui.add_space(4.0);
            self.eq_graph(ui, &mut fx);
            ui.label(
                    RichText::new(
                        "Click to add a point · drag to move it · scroll over it to make it narrower/wider · right-click for type or delete",
                    )
                    .small()
                    .color(pal().text_muted),
                );
            ui.add_space(4.0);
            // The points as numbers, for exact values.
            let mut remove = None;
            egui::Grid::new(("eq-points", &id))
                .striped(true)
                .num_columns(6)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    if !fx.eq.bands.is_empty() {
                        for h in ["", "Type", "Frequency", "Gain", "Width (Q)", ""] {
                            ui.label(RichText::new(h).small().strong());
                        }
                        ui.end_row();
                    }
                    for (i, b) in fx.eq.bands.iter_mut().enumerate() {
                        ui.label(
                            RichText::new(format!("{}", i + 1))
                                .strong()
                                .color(band_color(i)),
                        );
                        egui::ComboBox::from_id_salt(("eq-kind", &id, i))
                            .selected_text(kind_name(b.kind))
                            .width(100.0)
                            .show_ui(ui, |ui| {
                                for k in [BandKind::LowShelf, BandKind::Bell, BandKind::HighShelf] {
                                    ui.selectable_value(&mut b.kind, k, kind_name(k));
                                }
                            });
                        let speed = b.freq * 0.01;
                        ui.add(
                            egui::DragValue::new(&mut b.freq)
                                .range(EQ_MIN_HZ..=EQ_MAX_HZ)
                                .speed(speed)
                                .suffix(" Hz")
                                .max_decimals(0),
                        );
                        ui.add(
                            egui::DragValue::new(&mut b.gain_db)
                                .range(-EQ_MAX_DB..=EQ_MAX_DB)
                                .speed(0.1)
                                .suffix(" dB")
                                .max_decimals(1),
                        );
                        ui.add(
                            egui::DragValue::new(&mut b.q)
                                .range(EQ_MIN_Q..=EQ_MAX_Q)
                                .speed(0.01)
                                .max_decimals(2),
                        );
                        if ui
                            .small_button("✖")
                            .on_hover_text("Delete this point")
                            .clicked()
                        {
                            remove = Some(i);
                        }
                        ui.end_row();
                    }
                });
            if let Some(i) = remove {
                fx.eq.bands.remove(i);
            }
        });
        for cmd in cmds {
            self.send(&cmd);
        }
        if !open {
            self.eq_window = None;
            self.eq_preset_name = None;
        }
        if fx != before {
            self.set_effects(&id, fx);
        }
    }

    fn eq_graph(&mut self, ui: &mut Ui, fx: &mut Effects) {
        const DB_RANGE: f32 = 18.0;
        // Fills the window's width (drag the window's edge to make it bigger).
        let width = ui.available_width().max(400.0);
        let height = (width * 0.44).clamp(200.0, 700.0);
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
        let p = ui.painter_at(rect);
        let (lmin, lmax) = (EQ_MIN_HZ.log10(), EQ_MAX_HZ.log10());
        let x_of = |hz: f32| {
            rect.left()
                + (hz.clamp(EQ_MIN_HZ, EQ_MAX_HZ).log10() - lmin) / (lmax - lmin) * rect.width()
        };
        let hz_of = |x: f32| {
            10f32.powf(lmin + ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) * (lmax - lmin))
        };
        let y_of = |db: f32| {
            rect.center().y - db.clamp(-DB_RANGE, DB_RANGE) / DB_RANGE * (rect.height() / 2.0 - 8.0)
        };
        let db_of = |y: f32| {
            ((rect.center().y - y) / (rect.height() / 2.0 - 8.0) * DB_RANGE)
                .clamp(-EQ_MAX_DB, EQ_MAX_DB)
        };

        // Background and grid.
        p.rect_filled(rect, 6, pal().groove);
        let grid = pal().grid;
        for hz in [
            20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0,
        ] {
            let x = x_of(hz);
            p.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(1.0, grid),
            );
            let label = if hz >= 1000.0 {
                format!("{}k", hz / 1000.0)
            } else {
                format!("{hz}")
            };
            p.text(
                Pos2::new(x + 3.0, rect.bottom() - 3.0),
                Align2::LEFT_BOTTOM,
                label,
                FontId::proportional(10.0),
                pal().text_muted,
            );
        }
        for db in [-15.0, -10.0, -5.0, 0.0, 5.0, 10.0, 15.0] {
            let y = y_of(db);
            let c = if db == 0.0 { pal().text_muted } else { grid };
            p.line_segment(
                [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                Stroke::new(1.0, c),
            );
            p.text(
                Pos2::new(rect.left() + 3.0, y - 1.0),
                Align2::LEFT_BOTTOM,
                format!("{db:+.0}"),
                FontId::proportional(10.0),
                pal().text_muted,
            );
        }

        // The resulting curve, exactly as applied to the audio.
        let active = fx.eq.enabled || fx.low_cut;
        let curve: Vec<Pos2> = (0..=240)
            .map(|i| {
                let x = rect.left() + i as f32 / 240.0 * rect.width();
                Pos2::new(x, y_of(crate::dsp::response_db(fx, hz_of(x) as f64) as f32))
            })
            .collect();
        let color = if active { ACCENT } else { pal().text_muted };
        p.add(egui::Shape::line(curve, Stroke::new(2.5, color)));

        // Points.
        let pos_of = |b: &EqBand| Pos2::new(x_of(b.freq), y_of(b.gain_db));
        let pointer = resp.hover_pos().or(resp.interact_pointer_pos());
        let near = |bands: &[EqBand], pt: Pos2| {
            bands
                .iter()
                .enumerate()
                .map(|(i, b)| (i, pos_of(b).distance(pt)))
                .filter(|(_, d)| *d < 12.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        };
        let hovered = pointer.and_then(|pt| near(&fx.eq.bands, pt));
        for (i, b) in fx.eq.bands.iter().enumerate() {
            let c = band_color(i);
            let pt = pos_of(b);
            let r = if hovered == Some(i) || self.eq_drag == Some(i) {
                9.0
            } else {
                7.0
            };
            p.circle_filled(
                pt,
                r,
                if fx.eq.enabled {
                    c
                } else {
                    c.gamma_multiply(0.4)
                },
            );
            p.text(
                pt,
                Align2::CENTER_CENTER,
                format!("{}", i + 1),
                FontId::proportional(10.0),
                Color32::BLACK,
            );
        }
        if fx.low_cut {
            let pt = Pos2::new(x_of(fx.low_cut_hz), y_of(-3.0));
            p.circle_stroke(pt, 6.0, Stroke::new(2.0, pal().text));
        }

        // Interaction.
        if resp.drag_started() {
            self.eq_drag = resp
                .interact_pointer_pos()
                .and_then(|pt| near(&fx.eq.bands, pt));
        }
        if let (Some(i), Some(pt)) = (self.eq_drag, resp.interact_pointer_pos())
            && resp.dragged()
            && let Some(b) = fx.eq.bands.get_mut(i)
        {
            b.freq = (hz_of(pt.x) * 10.0).round() / 10.0;
            b.gain_db = (db_of(pt.y) * 10.0).round() / 10.0;
            fx.eq.enabled = true;
        }
        if resp.drag_stopped() {
            self.eq_drag = None;
        }
        if resp.clicked()
            && let Some(pt) = resp.interact_pointer_pos()
            && near(&fx.eq.bands, pt).is_none()
            && fx.eq.bands.len() < EQ_MAX_BANDS
        {
            fx.eq.bands.push(EqBand::at(
                hz_of(pt.x).round(),
                (db_of(pt.y) * 10.0).round() / 10.0,
            ));
            fx.eq.enabled = true;
        }
        if let Some(i) = hovered {
            let scroll = ui.input(|inp| inp.smooth_scroll_delta.y);
            if scroll != 0.0
                && let Some(b) = fx.eq.bands.get_mut(i)
            {
                b.q =
                    (b.q * if scroll > 0.0 { 1.08 } else { 1.0 / 1.08 }).clamp(EQ_MIN_Q, EQ_MAX_Q);
            }
            if let Some(b) = fx.eq.bands.get(i) {
                resp.clone().on_hover_text(format!(
                    "{} · {} · {:+.1} dB · width {:.2}",
                    i + 1,
                    fmt_hz(b.freq),
                    b.gain_db,
                    b.q
                ));
            }
        }
        if resp.secondary_clicked() {
            self.eq_menu = resp
                .interact_pointer_pos()
                .and_then(|pt| near(&fx.eq.bands, pt));
        }
        let menu_band = self.eq_menu;
        resp.context_menu(|ui| match menu_band.filter(|i| *i < fx.eq.bands.len()) {
            Some(i) => {
                ui.label(RichText::new(format!("Point {}", i + 1)).strong());
                for k in [BandKind::LowShelf, BandKind::Bell, BandKind::HighShelf] {
                    if ui.radio(fx.eq.bands[i].kind == k, kind_name(k)).clicked() {
                        fx.eq.bands[i].kind = k;
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("Reset to 0 dB").clicked() {
                    fx.eq.bands[i].gain_db = 0.0;
                    ui.close();
                }
                if ui.button("Delete point").clicked() {
                    fx.eq.bands.remove(i);
                    ui.close();
                }
            }
            None => {
                ui.label("Right-click a point to change or delete it.");
                if ui.button("Remove all points").clicked() {
                    fx.eq.bands.clear();
                    ui.close();
                }
            }
        });
    }

    fn bus_panel(&mut self, ui: &mut Ui, cfg: &Config, b: &Bus, devices: &[Device]) {
        let gain = self.gain(&b.id, b.gain_db);
        let muted = self.mute(&b.id, b.mute);
        let meter = self.meter(&b.id);
        card(ui, |ui| {
            let layout = match &b.kind {
                Kind::Physical { device } => devices
                    .iter()
                    .find(|d| &d.name == device)
                    .filter(|d| d.channels.len() > 2)
                    .map(|d| format!("{} · ", config::speaker_layout(&d.channels)))
                    .unwrap_or_default(),
                Kind::Virtual => String::new(),
            };
            let label = format!(
                "{} · {layout}{}",
                short_name(b),
                kind_label(&b.kind, devices, true)
            );
            if title(ui, &b.name, &label).clicked() {
                self.editor = Some(Editor::edit_bus(b));
            }
            let (resp, new_db) = fader(ui, gain, meter, muted);
            if let Some(db) = new_db {
                self.set_gain(&b.id, db);
            }
            self.midi_control(ui, &resp, cfg, &b.id, Action::Gain, Vec::new());
            ui.label(RichText::new(db_text(gain)).monospace());
            // Output effects: EQ and the limiter.
            let recording = self
                .shared
                .lock()
                .unwrap()
                .recordings
                .iter()
                .find(|r| r.bus == b.id)
                .cloned();
            let mut fx = self.effects_of(&b.id, &b.fx);
            let before = fx.clone();
            let limiting = self
                .shared
                .lock()
                .unwrap()
                .fx_status
                .get(&b.id)
                .is_some_and(|st| st.limit_db > 0.5 && fx.limiter.enabled);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(4.0, 0.0);
                let eq_btn = chip("EQ", fx.eq.enabled || fx.low_cut, ACCENT, Vec2::new(30.0, 24.0));
                if ui.add(eq_btn).on_hover_text("Equalizer for this output").clicked() {
                    self.eq_window = Some(b.id.clone());
                }
                let btn = chip(
                    "LIMIT",
                    fx.limiter.enabled,
                    if limiting { METER_YELLOW } else { ACCENT },
                    Vec2::new(42.0, 24.0),
                );
                let hint = if fx.limiter.enabled {
                    format!(
                        "Limiter on: never louder than {:.1} dB{}\nClick to switch off, right-click for settings",
                        fx.limiter.ceiling_db,
                        if limiting { " (limiting now)" } else { "" }
                    )
                } else {
                    "Limiter: protects against sudden loud peaks.\nClick to switch on, right-click for settings".to_string()
                };
                let resp = ui.add(btn).on_hover_text(hint);
                if resp.clicked() {
                    fx.limiter.enabled = !fx.limiter.enabled;
                }
                resp.context_menu(|ui| {
                    if ui.button("Settings…").clicked() {
                        self.fx_window = Some(b.id.clone());
                        ui.close();
                    }
                });
                // Record what this output plays.
                let rec = if recording.is_some() {
                    chip_solid("REC", MUTE_RED, Color32::WHITE, Vec2::new(30.0, 24.0))
                } else {
                    chip("REC", false, MUTE_RED, Vec2::new(30.0, 24.0))
                };
                let hint = match &recording {
                    Some(r) => format!("Recording to {}\nClick to stop", r.path),
                    None => "Record what this output plays (FLAC, in your Music folder)\nRight-click to open the folder".to_string(),
                };
                let resp = ui.add(rec).on_hover_text(hint);
                if resp.clicked() {
                    let bus = b.id.clone();
                    self.send(&if recording.is_some() {
                        Command::StopRecording { bus }
                    } else {
                        Command::StartRecording { bus }
                    });
                }
                resp.context_menu(|ui| {
                    if ui.button("Open recordings folder").clicked() {
                        let dir = dirs::audio_dir()
                            .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
                            .unwrap_or_default()
                            .join("Mixrail");
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                        ui.close();
                    }
                });
            });
            if let Some(r) = &recording {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_sub(r.started);
                ui.label(
                    RichText::new(format!("● REC {}:{:02}", secs / 60, secs % 60))
                        .small()
                        .strong()
                        .color(MUTE_RED),
                );
            }
            if fx != before {
                self.set_effects(&b.id, fx);
            }
            let resp = mute_button(ui, muted);
            if resp.clicked() {
                self.toggle_mute(&b.id, muted);
            }
            self.midi_control(ui, &resp, cfg, &b.id, Action::Mute, Vec::new());
        });
    }

    /// MIDI extras for a control: its mapping on hover, a highlight while it is being
    /// learned, and a right-click menu to learn or remove a mapping.
    fn midi_control(
        &mut self,
        ui: &Ui,
        resp: &egui::Response,
        cfg: &Config,
        target: &str,
        action: Action,
        // Extra right-click menu entries shown above the MIDI ones.
        extra: Vec<(&str, MenuItem)>,
    ) {
        let lt = LearnTarget {
            target: target.to_string(),
            action,
        };
        let binding = find_binding(cfg, &lt);
        if self.learning.as_ref() == Some(&lt) {
            ui.painter().rect_stroke(
                resp.rect.expand(3.0),
                5,
                Stroke::new(2.0, ACCENT),
                StrokeKind::Outside,
            );
        }
        if let Some(b) = binding {
            resp.clone()
                .on_hover_text(format!("MIDI: {}", control_name(b)));
        }
        let mut cmd = None;
        let mut open_fx = None;
        resp.context_menu(|ui| {
            for (label, item) in extra {
                if ui.button(label).clicked() {
                    match item {
                        MenuItem::Send(c) => cmd = Some(c),
                        MenuItem::OpenFx(id) => open_fx = Some(id),
                    }
                    ui.close();
                }
            }
            if cfg.midi.is_none() {
                ui.label("Choose a MIDI controller first\n(the MIDI… button at the top).");
                return;
            }
            if ui.button("MIDI learn").clicked() {
                cmd = Some(Command::MidiLearn { learn: lt.clone() });
                ui.close();
            }
            if let Some(b) = binding
                && ui
                    .button(format!("Remove MIDI mapping ({})", control_name(b)))
                    .clicked()
            {
                cmd = Some(Command::RemoveBinding { learn: lt.clone() });
                ui.close();
            }
        });
        if let Some(cmd) = cmd {
            self.send(&cmd);
        }
        if open_fx.is_some() {
            self.fx_window = open_fx;
        }
    }

    fn scene_save_dialog(&mut self, ctx: &egui::Context, cfg: &Config) {
        let Some(name) = &mut self.scene_dialog else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Save scene")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("Saves every fader, mute and send. Channels themselves aren't changed.");
                ui.horizontal(|ui| {
                    ui.label("Name");
                    let resp = ui.text_edit_singleline(name);
                    resp.request_focus();
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        save = true;
                    }
                });
                let exists = cfg.scenes.iter().any(|s| s.name == name.trim());
                if exists {
                    ui.colored_label(
                        METER_YELLOW,
                        "A scene with this name exists and will be replaced.",
                    );
                }
                ui.horizontal(|ui| {
                    let label = if exists { "Replace" } else { "Save" };
                    if ui
                        .add_enabled(!name.trim().is_empty(), egui::Button::new(label))
                        .clicked()
                    {
                        save = true;
                    }
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if save && !name.trim().is_empty() {
            let name = name.trim().to_string();
            self.send(&Command::SaveScene { name });
            self.scene_dialog = None;
        } else if cancel || !open {
            self.scene_dialog = None;
        }
    }

    fn apps_window(&mut self, ctx: &egui::Context, cfg: &Config) {
        if !self.apps_window {
            return;
        }
        let mut cmds: Vec<Command> = Vec::new();
        let strip_name = |id: &str| {
            cfg.strips
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| id.to_string())
        };
        // Apps can only play into virtual strips.
        let targets: Vec<&Strip> = cfg
            .strips
            .iter()
            .filter(|s| s.kind == Kind::Virtual)
            .collect();
        let window_title = "Apps";
        let open = separate_window(ctx, "apps", window_title, [560.0, 420.0], |ui| {
            ui.label(
                RichText::new(
                    "Pick the channel each app plays into. Tick “Always” to keep doing that \
                         automatically, also for apps that open new streams like browser tabs.",
                )
                .small()
                .color(pal().text_muted),
            );
            ui.add_space(6.0);
            if self.apps.is_empty() {
                ui.label("No apps are playing audio right now.");
            }
            egui::Grid::new("apps")
                .striped(true)
                .num_columns(3)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    if !self.apps.is_empty() {
                        ui.label(RichText::new("App").strong());
                        ui.label(RichText::new("Plays into").strong());
                        ui.label(RichText::new("Always").strong());
                        ui.end_row();
                    }
                    for app in &self.apps {
                        let rule = cfg.rules.iter().find(|r| r.app == app.name);
                        let label = if app.streams > 1 {
                            format!("{} ({} streams)", app.name, app.streams)
                        } else {
                            app.name.clone()
                        };
                        ui.label(label);

                        let current = app.strips.first().cloned();
                        let shown = match (&current, app.other.first()) {
                            (Some(id), _) if app.strips.len() == 1 => strip_name(id),
                            (Some(_), _) => "several channels".to_string(),
                            (None, Some(dev)) => format!("{dev} (directly)"),
                            (None, None) => "—".to_string(),
                        };
                        let mut choice = current.clone();
                        egui::ComboBox::from_id_salt(("app-target", &app.name))
                            .selected_text(shown)
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                for s in &targets {
                                    ui.selectable_value(&mut choice, Some(s.id.clone()), &s.name);
                                }
                            });
                        if choice != current
                            && let Some(strip) = choice
                        {
                            cmds.push(Command::RouteApp {
                                app: app.name.clone(),
                                strip,
                                remember: rule.is_some(),
                            });
                        }

                        let mut always = rule.is_some();
                        let can_remember = rule.is_some() || app.strips.len() == 1;
                        let resp =
                            ui.add_enabled(can_remember, egui::Checkbox::without_text(&mut always));
                        if resp.changed() {
                            if always {
                                cmds.push(Command::RouteApp {
                                    app: app.name.clone(),
                                    strip: app.strips[0].clone(),
                                    remember: true,
                                });
                            } else {
                                cmds.push(Command::ForgetApp {
                                    app: app.name.clone(),
                                });
                            }
                        }
                        ui.end_row();
                    }
                });

            // Rules for apps that aren't playing right now.
            let idle: Vec<&Rule> = cfg
                .rules
                .iter()
                .filter(|r| !self.apps.iter().any(|a| a.name == r.app))
                .collect();
            if !idle.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("Saved for apps that aren't playing now").strong());
                egui::Grid::new("idle-rules")
                    .num_columns(3)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for r in idle {
                            ui.label(&r.app);
                            ui.label(format!("→ {}", strip_name(&r.strip)));
                            if ui.small_button("Forget").clicked() {
                                cmds.push(Command::ForgetApp { app: r.app.clone() });
                            }
                            ui.end_row();
                        }
                    });
            }
        });
        self.apps_window = open;
        for cmd in cmds {
            self.send(&cmd);
        }
    }

    fn midi_settings_window(
        &mut self,
        ctx: &egui::Context,
        cfg: &Config,
        ports: &[String],
        connected: bool,
    ) {
        if !self.midi_window {
            return;
        }
        let mut new_cfg: Option<Config> = None;
        let mut remove: Option<LearnTarget> = None;
        let window_title = "MIDI controller";
        let open = separate_window(ctx, "midi", window_title, [680.0, 540.0], |ui| {
            let current = cfg.midi.as_ref().map(|m| m.device.clone());
            ui.horizontal(|ui| {
                ui.label("Controller");
                let shown = match &current {
                    None => "None".to_string(),
                    Some(d) if ports.contains(d) || connected => d.clone(),
                    Some(d) => format!("{d} (not connected)"),
                };
                let mut choice = current.clone();
                egui::ComboBox::from_id_salt("midi-device")
                    .selected_text(shown)
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut choice, None, "None");
                        for p in ports {
                            ui.selectable_value(&mut choice, Some(p.clone()), p);
                        }
                    });
                if choice != current {
                    let mut c = cfg.clone();
                    c.midi = match (choice, c.midi.take()) {
                        (None, _) => None,
                        (Some(device), Some(m)) => Some(Midi { device, ..m }),
                        (Some(device), None) => Some(Midi {
                            device,
                            feedback: true,
                            bindings: vec![],
                        }),
                    };
                    new_cfg = Some(c);
                }
            });
            if current.is_some() && ports.is_empty() && !connected {
                ui.colored_label(
                    pal().text_muted,
                    "No MIDI controllers found. Is it plugged in?",
                );
            }
            let Some(midi) = &cfg.midi else {
                ui.add_space(6.0);
                ui.label("Choosing \"None\" removes all mappings.");
                return;
            };
            let mut feedback = midi.feedback;
            if ui
                .checkbox(&mut feedback, "Light up button LEDs (mute / send state)")
                .changed()
            {
                let mut c = cfg.clone();
                if let Some(m) = &mut c.midi {
                    m.feedback = feedback;
                }
                new_cfg = Some(c);
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Mappings").strong());
            if midi.bindings.is_empty() {
                ui.label("None yet.");
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("bindings")
                        .striped(true)
                        .num_columns(4)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            let mut sorted: Vec<(usize, &Binding)> =
                                midi.bindings.iter().enumerate().collect();
                            sorted.sort_by_key(|(_, b)| (b.channel, b.control));
                            for (index, b) in sorted {
                                ui.label(RichText::new(control_name(b)).monospace());
                                ui.label(describe(cfg, &b.target, &b.action));
                                // How the control behaves (different controllers differ here).
                                let choices = mode_choices(&b.action);
                                if choices.is_empty() {
                                    ui.label("");
                                } else {
                                    let current = b.mode.unwrap_or(choices[0].0);
                                    let mut chosen = current;
                                    let label = choices
                                        .iter()
                                        .find(|(m, _)| *m == current)
                                        .map(|(_, l)| *l)
                                        .unwrap_or("?");
                                    egui::ComboBox::from_id_salt(("mode", index))
                                        .selected_text(label)
                                        .width(170.0)
                                        .show_ui(ui, |ui| {
                                            for (m, l) in choices {
                                                ui.selectable_value(&mut chosen, *m, *l);
                                            }
                                        });
                                    if chosen != current {
                                        let mut c = cfg.clone();
                                        if let Some(m) = &mut c.midi {
                                            m.bindings[index].mode = Some(chosen);
                                        }
                                        new_cfg = Some(c);
                                    }
                                }
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(LearnTarget {
                                        target: b.target.clone(),
                                        action: b.action.clone(),
                                    });
                                }
                                ui.end_row();
                            }
                        });
                });
            ui.add_space(8.0);
            ui.label(
                    RichText::new(
                        "To add a mapping, right-click a fader or button in the mixer and choose MIDI learn. \
                         Works with any controller: knobs and faders sending CC or pitch bend, buttons \
                         sending CC or notes.",
                    )
                        .small()
                        .color(pal().text_muted),
                );
        });
        self.midi_window = open;
        if let Some(c) = new_cfg {
            self.send(&Command::SetConfig { config: c });
        }
        if let Some(learn) = remove {
            self.send(&Command::RemoveBinding { learn });
        }
    }

    fn editor_window(&mut self, ctx: &egui::Context, cfg: &Config, devices: &[Device]) {
        let speaker_test = self.shared.lock().unwrap().speaker_test.clone();
        let mut test_device: Option<String> = None;
        let mut set_profile: Option<(String, u32)> = None;
        let Some(ed) = &mut self.editor else { return };
        match devices.iter().find(|d| d.name == ed.device) {
            Some(d) => ed.card = d.card,
            None => {
                if let Some(d) = devices
                    .iter()
                    .find(|d| d.card.is_some() && d.card == ed.card && d.output == ed.is_bus)
                {
                    ed.device = d.name.clone();
                }
            }
        }
        let what = if ed.is_bus { "output" } else { "input" };
        let title = match &ed.id {
            Some(_) => format!("Edit {what}"),
            None => format!("Add {what}"),
        };
        let mut open = true;
        let mut action: Option<EditorAction> = None;
        let shown = egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .min_width(500.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("editor")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut ed.name);
                        ui.end_row();

                        ui.label("Type");
                        ui.vertical(|ui| {
                            let (virt, phys) = if ed.is_bus {
                                (
                                    "Virtual microphone (apps can record it)",
                                    "Speakers / headphones",
                                )
                            } else {
                                (
                                    "Virtual device (apps can play into it)",
                                    "Microphone / line-in",
                                )
                            };
                            ui.radio_value(&mut ed.physical, false, virt);
                            ui.radio_value(&mut ed.physical, true, phys);
                        });
                        ui.end_row();

                        if !ed.is_bus && !ed.physical {
                            ui.label("Channels");
                            ui.vertical(|ui| {
                                ui.radio_value(&mut ed.layout, ChannelLayout::Surround, "Surround 7.1 (recommended)");
                                ui.label(
                                    RichText::new(
                                        "    Games and films that output surround keep it;\n    stereo apps play on front left/right.",
                                    )
                                    .small()
                                    .color(pal().text_muted),
                                );
                                ui.radio_value(&mut ed.layout, ChannelLayout::Stereo, "Stereo");
                            });
                            ui.end_row();
                        }

                        if ed.physical {
                            ui.label("Device");
                            let choices: Vec<&Device> =
                                devices.iter().filter(|d| d.output == ed.is_bus).collect();
                            let selected = devices
                                .iter()
                                .find(|d| d.name == ed.device)
                                .map(|d| d.description.clone())
                                .unwrap_or_else(|| {
                                    if ed.device.is_empty() {
                                        "Choose…".into()
                                    } else {
                                        format!("{} (not connected)", ed.device)
                                    }
                                });
                            egui::ComboBox::from_id_salt("device")
                                .selected_text(selected)
                                .width(320.0)
                                .show_ui(ui, |ui| {
                                    for d in choices {
                                        ui.selectable_value(
                                            &mut ed.device,
                                            d.name.clone(),
                                            &d.description,
                                        );
                                    }
                                });
                            ui.end_row();

                            // The sound card's profiles that have an output (stereo, 5.1, ...).
                            let device = devices.iter().find(|d| d.name == ed.device);
                            let setups: Vec<&ipc::Profile> = device
                                .map(|d| {
                                    d.profiles
                                        .iter()
                                        .filter(|p| {
                                            p.outputs && (p.available || Some(p.index) == d.profile)
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if ed.is_bus && setups.len() > 1 {
                                let d = device.expect("setups come from the device");
                                let current = setups
                                    .iter()
                                    .find(|p| Some(p.index) == d.profile)
                                    .map(|p| p.description.clone())
                                    .unwrap_or_else(|| "Switching…".into());
                                ui.label("Speaker setup");
                                egui::ComboBox::from_id_salt("profile")
                                    .selected_text(current)
                                    .width(320.0)
                                    .show_ui(ui, |ui| {
                                        for p in &setups {
                                            if ui
                                                .selectable_label(Some(p.index) == d.profile, &p.description)
                                                .clicked()
                                                && Some(p.index) != d.profile
                                            {
                                                set_profile = Some((d.name.clone(), p.index));
                                            }
                                        }
                                    })
                                    .response
                                    .on_hover_text(
                                        "How the sound card is set up. Applies right away, to the\n\
                                         whole card, and is remembered by the system.",
                                    );
                                ui.end_row();
                            }

                            // Speaker layout of an output, and how stereo is spread over it.
                            let channels = device.map(|d| d.channels.clone()).unwrap_or_default();
                            if ed.is_bus && !channels.is_empty() {
                                ui.label("Speakers");
                                ui.label(format!(
                                    "{} — as the device is set up now",
                                    config::speaker_layout(&channels)
                                ));
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    let running = speaker_test.as_ref().filter(|(d, _)| *d == ed.device);
                                    if ui
                                        .add_enabled(running.is_none(), egui::Button::new("Test speakers"))
                                        .on_hover_text("Plays a short sound on each speaker in turn")
                                        .clicked()
                                    {
                                        test_device = Some(ed.device.clone());
                                    }
                                    if let Some((_, ch)) = running {
                                        ui.label(
                                            RichText::new(format!("Now playing: {}", speaker_name(ch)))
                                                .strong()
                                                .color(ACCENT),
                                        );
                                    }
                                });
                                ui.end_row();
                                let surround = channels.len() > 2;
                                if surround {
                                    ui.label("Stereo sound\nplays as");
                                    ui.vertical(|ui| {
                                        ui.radio_value(&mut ed.upmix, Upmix::Off, "Front left/right only");
                                        ui.label(
                                            RichText::new("    For stereo speakers, or speaker systems that spread stereo by themselves.\n    Real surround from games and films always plays on the matching speakers.")
                                                .small()
                                                .color(pal().text_muted),
                                        );
                                        ui.radio_value(&mut ed.upmix, Upmix::Surround, "Surround (recommended)");
                                        ui.label(
                                            RichText::new(
                                                "    Centre gets what left and right share, rears get the room\n\
                                                 and ambience, subwoofer gets only the bass.",
                                            )
                                            .small()
                                            .color(pal().text_muted),
                                        );
                                        ui.radio_value(&mut ed.upmix, Upmix::All, "Same sound on all speakers");
                                        ui.label(
                                            RichText::new(
                                                "    For parties and open spaces; subwoofer gets only the bass.")
                                            .small()
                                            .color(pal().text_muted),
                                        );
                                    });
                                    ui.end_row();
                                } else {
                                    let hint = if setups.iter().any(|p| is_surround(&p.description)) {
                                        "Have more speakers? Choose a surround speaker setup\n\
                                         above, then pick how stereo is spread here."
                                    } else {
                                        "Have more speakers? Switch this device to a surround\n\
                                         profile in your system's sound settings, then pick\n\
                                         how stereo is spread here."
                                    };
                                    ui.label("");
                                    ui.label(RichText::new(hint).small().color(pal().text_muted));
                                    ui.end_row();
                                }
                            }
                        }
                    });
                ui.add_space(8.0);
                let valid = !ed.name.trim().is_empty() && (!ed.physical || !ed.device.is_empty());
                ui.horizontal(|ui| {
                    if ui.add_enabled(valid, egui::Button::new("Save")).clicked() {
                        action = Some(EditorAction::Save);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(EditorAction::Cancel);
                    }
                    if ed.id.is_some() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let label = if ed.confirm_delete {
                                "Really delete?"
                            } else {
                                "Delete"
                            };
                            let btn = if ed.confirm_delete {
                                chip_solid(label, MUTE_RED, Color32::WHITE, Vec2::ZERO)
                            } else {
                                chip(label, false, MUTE_RED, Vec2::ZERO)
                            };
                            if ui.add(btn).clicked() {
                                if ed.confirm_delete {
                                    action = Some(EditorAction::Delete);
                                } else {
                                    ed.confirm_delete = true;
                                }
                            }
                        });
                    }
                });
            });
        if let Some(shown) = shown {
            shot_rect(ctx, shown.response.rect);
        }
        if let Some(device) = test_device {
            self.send(&Command::TestSpeakers { device });
        }
        if let Some((device, index)) = set_profile {
            self.send(&Command::SetProfile { device, index });
        }
        if !open {
            action = Some(EditorAction::Cancel);
        }
        let Some(action) = action else { return };
        let ed = self.editor.take().expect("editor is open");
        let mut new_cfg = cfg.clone();
        match action {
            EditorAction::Cancel => return,
            EditorAction::Delete => {
                let id = ed.id.as_deref().expect("only existing ones can be deleted");
                if ed.is_bus {
                    new_cfg.remove_bus(id);
                } else {
                    new_cfg.remove_strip(id);
                }
            }
            EditorAction::Save => ed.apply(&mut new_cfg),
        }
        self.send(&Command::SetConfig { config: new_cfg });
    }
}

/// An extra entry in a control's right-click menu.
enum MenuItem {
    Send(Command),
    /// Open the gate/compressor settings of this strip.
    OpenFx(String),
}

enum EditorAction {
    Save,
    Cancel,
    Delete,
}

/// A sound card profile with more than two speakers.
fn is_surround(description: &str) -> bool {
    let d = description.to_lowercase();
    ["surround", "2.1", "4.0", "4.1", "5.0", "5.1", "7.1"]
        .iter()
        .any(|k| d.contains(k))
}

impl Editor {
    fn new(is_bus: bool) -> Self {
        Editor {
            is_bus,
            id: None,
            name: String::new(),
            physical: false,
            device: String::new(),
            upmix: Upmix::Off,
            layout: ChannelLayout::default(),
            card: None,
            confirm_delete: false,
        }
    }

    fn from_kind(is_bus: bool, id: &str, name: &str, kind: &Kind) -> Self {
        let (physical, device) = match kind {
            Kind::Virtual => (false, String::new()),
            Kind::Physical { device } => (true, device.clone()),
        };
        Editor {
            is_bus,
            id: Some(id.to_string()),
            name: name.to_string(),
            physical,
            device,
            upmix: Upmix::Off,
            layout: ChannelLayout::default(),
            card: None,
            confirm_delete: false,
        }
    }

    fn edit_strip(s: &Strip) -> Self {
        Editor {
            layout: s.layout,
            ..Self::from_kind(false, &s.id, &s.name, &s.kind)
        }
    }

    fn edit_bus(b: &Bus) -> Self {
        Editor {
            upmix: b.upmix,
            ..Self::from_kind(true, &b.id, &b.name, &b.kind)
        }
    }

    fn apply(&self, cfg: &mut Config) {
        let name = self.name.trim().to_string();
        let upmix = if self.physical {
            self.upmix
        } else {
            Upmix::Off
        };
        let kind = if self.physical {
            Kind::Physical {
                device: self.device.clone(),
            }
        } else {
            Kind::Virtual
        };
        match (&self.id, self.is_bus) {
            (Some(id), false) => {
                if let Some(s) = cfg.strips.iter_mut().find(|s| &s.id == id) {
                    s.name = name;
                    s.kind = kind;
                    s.layout = self.layout;
                }
            }
            (Some(id), true) => {
                if let Some(b) = cfg.buses.iter_mut().find(|b| &b.id == id) {
                    b.name = name;
                    b.kind = kind;
                    b.upmix = upmix;
                }
            }
            (None, is_bus) => {
                let taken: Vec<&str> = cfg
                    .strips
                    .iter()
                    .map(|s| s.id.as_str())
                    .chain(cfg.buses.iter().map(|b| b.id.as_str()))
                    .collect();
                let id = config::make_id(&name, &taken);
                if is_bus {
                    cfg.buses.push(Bus {
                        upmix,
                        ..Bus::new(&id, &name, kind)
                    });
                } else {
                    // New inputs go to the first output by default.
                    let first: Vec<&str> = cfg
                        .buses
                        .first()
                        .map(|b| b.id.as_str())
                        .into_iter()
                        .collect();
                    let strip = Strip {
                        layout: self.layout,
                        ..Strip::new(&id, &name, kind, &first)
                    };
                    cfg.strips.push(strip);
                }
            }
        }
    }
}

// ---------- widgets ----------

fn card(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(pal().panel)
        .stroke(Stroke::new(1.0, pal().card_border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(STRIP_WIDTH);
            // Compact buttons, so a row of them fits the card.
            ui.spacing_mut().button_padding = Vec2::new(4.0, 3.0);
            ui.vertical_centered(body);
        });
}

fn add_card(ui: &mut Ui, label: &str) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(64.0, FADER_HEIGHT + 120.0), Sense::click());
    let stroke = if resp.hovered() { ACCENT } else { pal().surface_border };
    let text_color = if resp.hovered() { ACCENT } else { pal().text_muted };
    ui.painter().rect_stroke(
        rect.shrink(2.0),
        12,
        Stroke::new(1.5, stroke),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center() - Vec2::new(0.0, 12.0),
        Align2::CENTER_CENTER,
        "+",
        FontId::proportional(28.0),
        text_color,
    );
    ui.painter().text(
        rect.center() + Vec2::new(0.0, 16.0),
        Align2::CENTER_CENTER,
        label.replace(' ', "\n"),
        FontId::proportional(12.0),
        text_color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn title(ui: &mut Ui, name: &str, subtitle: &str) -> egui::Response {
    let resp = ui
        .add(
            egui::Label::new(RichText::new(name).family(semibold()).size(15.0).color(pal().text))
                .truncate()
                .sense(Sense::click()),
        )
        .on_hover_text("Click to edit");
    ui.add(egui::Label::new(RichText::new(subtitle).size(10.5).color(pal().text_muted)).truncate())
        .on_hover_text(subtitle);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn mute_button(ui: &mut Ui, muted: bool) -> egui::Response {
    let size = Vec2::new(STRIP_WIDTH - 8.0, 26.0);
    ui.add(if muted {
        chip_solid("MUTED", MUTE_RED, Color32::WHITE, size)
    } else {
        chip("MUTE", false, MUTE_RED, size)
    })
}

fn status_dot(ui: &mut Ui, ok: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.5, if ok { METER_GREEN } else { MUTE_RED });
}

/// Vertical fader with a stereo level meter beside it.
/// Drag to move, scroll for fine steps, double-click for 0 dB.
fn fader(ui: &mut Ui, db: f32, meter: [f32; 2], muted: bool) -> (egui::Response, Option<f32>) {
    let size = Vec2::new(84.0, FADER_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let painter = ui.painter_at(rect.expand(2.0));
    let top = rect.top() + 10.0;
    let bottom = rect.bottom() - 10.0;
    let y_of = |pos: f32| bottom - pos.clamp(0.0, 1.0) * (bottom - top);

    // Level meter: two slim bars on the left, green → yellow → red.
    let stops = [
        (0.0, METER_GREEN),
        (config::db_to_pos(-18.0), METER_GREEN),
        (config::db_to_pos(-6.0), METER_YELLOW),
        (config::db_to_pos(0.0), Color32::from_rgb(0xfb, 0x92, 0x3c)),
        (1.0, METER_RED),
    ];
    for (i, level) in meter.iter().enumerate() {
        let x = rect.left() + 5.0 + i as f32 * 8.0;
        let bar = Rect::from_x_y_ranges(x..=x + 5.0, top..=bottom);
        painter.rect_filled(bar, 3, pal().groove);
        if *level > 0.0 {
            let lit = Rect::from_x_y_ranges(x..=x + 5.0, y_of(*level)..=bottom);
            let mut mesh = egui::Mesh::default();
            for pair in stops.windows(2) {
                let ((from, c0), (to, c1)) = (pair[0], pair[1]);
                let (c0, c1) = if muted {
                    (pal().surface_border, pal().surface_border)
                } else {
                    (c0, c1)
                };
                let i0 = mesh.vertices.len() as u32;
                mesh.colored_vertex(Pos2::new(x, y_of(from)), c0);
                mesh.colored_vertex(Pos2::new(x + 5.0, y_of(from)), c0);
                mesh.colored_vertex(Pos2::new(x + 5.0, y_of(to)), c1);
                mesh.colored_vertex(Pos2::new(x, y_of(to)), c1);
                mesh.add_triangle(i0, i0 + 1, i0 + 2);
                mesh.add_triangle(i0, i0 + 2, i0 + 3);
            }
            painter.with_clip_rect(lit).add(mesh);
        }
    }

    // Scale.
    let track_x = rect.left() + 60.0;
    for mark in [12.0, 6.0, 0.0, -6.0, -12.0, -24.0, -40.0, -60.0] {
        let y = y_of(config::db_to_pos(mark));
        let color = if mark == 0.0 {
            pal().text
        } else {
            pal().text_muted.gamma_multiply(0.8)
        };
        painter.text(
            Pos2::new(track_x - 16.0, y),
            Align2::RIGHT_CENTER,
            if mark == -60.0 {
                "-∞".to_string()
            } else {
                format!("{mark:+.0}").replace("+0", "0")
            },
            FontId::monospace(9.0),
            color,
        );
        painter.line_segment(
            [Pos2::new(track_x - 13.0, y), Pos2::new(track_x - 9.0, y)],
            Stroke::new(1.0, color),
        );
    }

    // Track and handle.
    let groove = Rect::from_x_y_ranges(track_x - 2.5..=track_x + 2.5, top..=bottom);
    painter.rect_filled(groove, 3, pal().groove);
    let pos = config::db_to_pos(db);
    let fill = Rect::from_x_y_ranges(track_x - 2.5..=track_x + 2.5, y_of(pos)..=bottom);
    painter.rect_filled(
        fill,
        3,
        if muted { pal().surface_border } else { ACCENT },
    );
    let handle = Rect::from_center_size(Pos2::new(track_x, y_of(pos)), Vec2::new(30.0, 16.0));
    let active = resp.dragged() || resp.hovered();
    // A soft shadow under the cap.
    painter.rect_filled(
        handle.translate(Vec2::new(0.0, 2.0)).expand(1.0),
        6,
        Color32::from_black_alpha(if pal().dark { 90 } else { 30 }),
    );
    let cap = if active {
        pal().cap.lerp_to_gamma(ACCENT, 0.12)
    } else {
        pal().cap
    };
    painter.rect_filled(handle, 6, cap);
    painter.rect_stroke(handle, 6, Stroke::new(1.0, pal().cap_border), StrokeKind::Inside);
    painter.line_segment(
        [
            Pos2::new(handle.left() + 7.0, handle.center().y),
            Pos2::new(handle.right() - 7.0, handle.center().y),
        ],
        Stroke::new(2.0, if muted { pal().text_muted } else { ACCENT }),
    );

    let resp = resp.on_hover_cursor(egui::CursorIcon::ResizeVertical);
    if resp.double_clicked() {
        return (resp, Some(0.0));
    }
    if resp.dragged() {
        let dy = resp.drag_delta().y;
        if dy != 0.0 {
            let new_pos = (pos - dy / (bottom - top)).clamp(0.0, 1.0);
            return (resp, Some(round_db(config::pos_to_db(new_pos))));
        }
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let step = if scroll > 0.0 { 0.5 } else { -0.5 };
            return (
                resp,
                Some(round_db(
                    (db + step).clamp(config::MIN_GAIN_DB, config::MAX_GAIN_DB),
                )),
            );
        }
    }
    (resp, None)
}

fn find_binding<'a>(cfg: &'a Config, lt: &LearnTarget) -> Option<&'a Binding> {
    cfg.midi
        .as_ref()?
        .bindings
        .iter()
        .find(|b| b.target == lt.target && b.action == lt.action)
}

/// Plain name of a speaker channel.
fn speaker_name(ch: &str) -> &str {
    match ch {
        "FL" => "front left",
        "FR" => "front right",
        "FC" => "centre",
        "LFE" => "subwoofer",
        "RL" => "rear left",
        "RR" => "rear right",
        "SL" => "side left",
        "SR" => "side right",
        "MONO" => "mono speaker",
        other => other,
    }
}

fn control_name(b: &Binding) -> String {
    b.control_label()
}

/// Behaviours a mapping can have, with plain-language names. The first is the default.
fn mode_choices(action: &Action) -> &'static [(Mode, &'static str)] {
    match action {
        Action::Gain | Action::Gate | Action::Comp | Action::Pan => &[
            (Mode::Absolute, "Fader / knob"),
            (Mode::Relative, "Endless knob"),
            (Mode::RelativeOffset, "Endless knob (64 = still)"),
        ],
        Action::Scene => &[],
        _ => &[
            (Mode::Momentary, "Toggle on press"),
            (Mode::Latching, "Follows the button"),
        ],
    }
}

/// Human description of a mappable function, e.g. "Spotify fader".
fn describe(cfg: &Config, target: &str, action: &Action) -> String {
    let name = |id: &str| {
        cfg.strips
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .or_else(|| {
                cfg.buses
                    .iter()
                    .find(|b| b.id == id)
                    .map(|b| b.name.clone())
            })
            .unwrap_or_else(|| id.to_string())
    };
    match action {
        Action::Gain => format!("{} fader", name(target)),
        Action::Mute => format!("{} mute", name(target)),
        Action::Route { bus } => format!("{} → {}", name(target), name(bus)),
        Action::Scene => format!("Scene “{target}”"),
        Action::Gate => format!("{} gate knob", name(target)),
        Action::Comp => format!("{} compressor knob", name(target)),
        Action::Pan => format!("{} balance", name(target)),
        Action::Cue => format!("{} cue", name(target)),
    }
}

/// Name, saved effects and whether it's an output, for a strip or output id.
fn channel_effects<'a>(cfg: &'a Config, id: &str) -> Option<(String, &'a Effects, bool)> {
    if let Some(s) = cfg.strips.iter().find(|s| s.id == id) {
        return Some((s.name.clone(), &s.fx, false));
    }
    cfg.buses
        .iter()
        .find(|b| b.id == id)
        .map(|b| (b.name.clone(), &b.fx, true))
}

fn ducking_section(
    ui: &mut Ui,
    fx: &mut Effects,
    status: Option<ipc::FxStatus>,
    cfg: &Config,
    own: &str,
    is_bus: bool,
) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Ducking").strong().size(15.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.checkbox(&mut fx.duck.enabled, "On");
        });
    });
    let (about, ask) = if is_bus {
        (
            "Turns the other channels in this output down while the ticked channels are active,\n\
             e.g. music on a stream while you or a call speak. Only this output is affected.",
            "Turn the others down while these are active:",
        )
    } else {
        (
            "Turns this channel down while other channels are active, e.g. music while you talk.\n\
             This happens on every output it plays on.",
            "Turn down while these are active:",
        )
    };
    ui.label(RichText::new(about).small().color(pal().text_muted));
    ui.add_enabled_ui(fx.duck.enabled, |ui| {
        ui.label(ask);
        ui.horizontal_wrapped(|ui| {
            for other in cfg.strips.iter().filter(|o| o.id != own) {
                let mut on = fx.duck.sources.contains(&other.id);
                if ui.checkbox(&mut on, &other.name).changed() {
                    fx.duck.sources.retain(|x| x != &other.id);
                    if on {
                        fx.duck.sources.push(other.id.clone());
                    }
                }
            }
        });
        ui.add(egui::Slider::new(&mut fx.duck.amount_db, 0.0..=40.0).text("turn down by").suffix(" dB"));
        ui.add(egui::Slider::new(&mut fx.duck.threshold_db, -80.0..=0.0).text("active above").suffix(" dB"));
        let source = status.map(|s| s.duck_source_db).unwrap_or(-120.0);
        let ducked = status.map(|s| s.duck_db).unwrap_or(0.0);
        ui.horizontal(|ui| {
            level_bar(ui, source, Some(fx.duck.threshold_db), -80.0, 0.0);
            if ducked > 0.5 {
                ui.label(RichText::new(format!("down {ducked:.0} dB")).strong().color(ink(METER_YELLOW)));
            }
        });
        ui.label(
            RichText::new("The bar is how loud those channels are; set the mark just under their normal level.")
                .small()
                .color(pal().text_muted),
        );
        ui.add(ms_slider(&mut fx.duck.attack_ms, 1.0..=500.0, "fade down"));
        ui.add(ms_slider(&mut fx.duck.hold_ms, 0.0..=3000.0, "hold — stays down after they stop"));
        ui.add(ms_slider(&mut fx.duck.release_ms, 10.0..=5000.0, "come back"));
        if fx.duck.enabled && fx.duck.sources.is_empty() {
            ui.colored_label(ink(METER_YELLOW), "Tick at least one channel above.");
        }
    });
}

fn limiter_section(ui: &mut Ui, fx: &mut Effects, status: Option<ipc::FxStatus>) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Limiter").strong().size(15.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.checkbox(&mut fx.limiter.enabled, "On");
        });
    });
    ui.label(
        RichText::new("Never lets the level above the ceiling: protects ears and streams from sudden loud peaks.")
            .small()
            .color(pal().text_muted),
    );
    ui.add_enabled_ui(fx.limiter.enabled, |ui| {
        ui.add(
            egui::Slider::new(&mut fx.limiter.ceiling_db, -24.0..=0.0)
                .text("ceiling")
                .suffix(" dB"),
        );
        ui.add(ms_slider(
            &mut fx.limiter.release_ms,
            10.0..=1000.0,
            "release — how fast it lets go",
        ));
        let limit = status.map(|s| s.limit_db).unwrap_or(0.0);
        ui.horizontal(|ui| {
            reduction_bar(ui, limit);
            ui.label(format!("turning down {limit:.1} dB"));
        });
    });
}

/// Show a panel as its own window (movable anywhere, any size, even on another screen).
/// Returns false once the user closes it.
fn separate_window(
    ctx: &egui::Context,
    key: &str,
    title: &str,
    size: [f32; 2],
    mut body: impl FnMut(&mut Ui),
) -> bool {
    if panel_shot() {
        // Screenshot of this window alone: drawn inside the main window, then cropped.
        if let Some(shown) = egui::Window::new(title)
            .id(egui::Id::unique(("panel", key)))
            .fixed_pos([8.0, 8.0])
            .fixed_size([
                size[0],
                std::env::var("LAM_SHOT_H")
                    .ok()
                    .and_then(|h| h.parse().ok())
                    .unwrap_or(size[1]),
            ])
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| body(ui))
        {
            shot_rect(ctx, shown.response.rect);
        }
        return true;
    }
    let builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_app_id("mixrail")
        .with_inner_size(size)
        .with_min_inner_size([360.0, 200.0]);
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of(("panel", key)),
        builder,
        |ui, _| {
            egui::CentralPanel::default_margins().show(ui, |ui| body(ui));
            !ui.ctx().input(|i| i.viewport().close_requested())
        },
    )
}

/// Developer aid: true when taking a screenshot of one of the separate windows
/// (`LAM_SCREENSHOT` with `LAM_OPEN=fx:…|eq:…|midi|apps`).
fn panel_shot() -> bool {
    static SHOT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SHOT.get_or_init(|| {
        std::env::var("LAM_SCREENSHOT").is_ok()
            && std::env::var("LAM_OPEN").is_ok_and(|o| {
                o == "midi" || o == "apps" || o.starts_with("fx:") || o.starts_with("eq:")
            })
    })
}

/// Remember the area a screenshot should be cropped to (a dialog or panel).
fn shot_rect(ctx: &egui::Context, rect: Rect) {
    if std::env::var("LAM_SCREENSHOT").is_ok() && std::env::var("LAM_OPEN").is_ok() {
        ctx.data_mut(|d| d.insert_temp(egui::Id::unique("lam_shot_rect"), rect));
    }
}

/// A slider for a time in milliseconds.
fn ms_slider<'a>(
    v: &'a mut f32,
    range: std::ops::RangeInclusive<f32>,
    label: &str,
) -> egui::Slider<'a> {
    egui::Slider::new(v, range)
        .logarithmic(true)
        .text(label.to_string())
        .suffix(" ms")
}

/// A turning knob, 0..1 (0 = off). Drag up/down or scroll to turn, double-click for off.
/// `busy` lights it yellow (gate closed / compressor working).
fn knob(ui: &mut Ui, pos: f32, label: &str, busy: bool) -> (egui::Response, Option<f32>) {
    let size = Vec2::new(38.0, 46.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let p = ui.painter();
    let center = Pos2::new(rect.center().x, rect.top() + 17.0);
    let radius = 13.0;
    // 270° of travel, starting bottom-left.
    let angle = |t: f32| std::f32::consts::PI * (0.75 + 1.5 * t);
    let point = |t: f32, r: f32| center + Vec2::angled(angle(t)) * r;
    let arc = |from: f32, to: f32| -> Vec<Pos2> {
        let n = 24;
        (0..=n)
            .map(|i| point(from + (to - from) * i as f32 / n as f32, radius))
            .collect()
    };
    p.circle_filled(
        center,
        radius - 3.5,
        if resp.hovered() {
            Color32::from_rgb(0x2e, 0x33, 0x40)
        } else {
            pal().surface
        },
    );
    p.circle_stroke(center, radius - 3.5, Stroke::new(1.0, pal().surface_border));
    p.add(egui::Shape::line(arc(0.0, 1.0), Stroke::new(3.0, pal().groove)));
    let color = if busy { METER_YELLOW } else { ACCENT };
    if pos > 0.0 {
        p.add(egui::Shape::line(arc(0.0, pos), Stroke::new(3.0, color)));
    }
    p.line_segment(
        [point(pos, 3.0), point(pos, radius - 5.0)],
        Stroke::new(2.0, if pos > 0.0 { pal().text } else { pal().text_muted }),
    );
    let text_color = if pos > 0.0 { pal().text } else { pal().text_muted };
    p.text(
        Pos2::new(rect.center().x, rect.bottom()),
        Align2::CENTER_BOTTOM,
        label,
        FontId::new(9.0, semibold()),
        text_color,
    );

    let resp = resp.on_hover_cursor(egui::CursorIcon::ResizeVertical);
    if resp.double_clicked() {
        return (resp, Some(0.0));
    }
    if resp.dragged() {
        let dy = resp.drag_delta().y;
        if dy != 0.0 {
            return (resp, Some((pos - dy / 150.0).clamp(0.0, 1.0)));
        }
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let step = if scroll > 0.0 { 0.02 } else { -0.02 };
            return (resp, Some((pos + step).clamp(0.0, 1.0)));
        }
    }
    (resp, None)
}

/// A small horizontal balance control. Drag or scroll; double-click for centre.
fn pan_slider(ui: &mut Ui, pan: f32) -> (egui::Response, Option<f32>) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(84.0, 12.0), Sense::click_and_drag());
    let p = ui.painter();
    let mid = rect.center();
    p.rect_filled(
        Rect::from_center_size(mid, Vec2::new(rect.width(), 4.0)),
        2,
        pal().groove,
    );
    p.line_segment(
        [
            Pos2::new(mid.x, rect.top() + 2.0),
            Pos2::new(mid.x, rect.bottom() - 2.0),
        ],
        Stroke::new(1.0, pal().text_muted),
    );
    let x = mid.x + pan.clamp(-1.0, 1.0) * (rect.width() / 2.0 - 4.0);
    if pan != 0.0 {
        p.line_segment(
            [Pos2::new(mid.x, mid.y), Pos2::new(x, mid.y)],
            Stroke::new(3.0, ACCENT),
        );
    }
    let color = if resp.hovered() || resp.dragged() {
        pal().cap.lerp_to_gamma(ACCENT, 0.15)
    } else {
        pal().cap
    };
    p.circle_filled(Pos2::new(x, mid.y), 5.0, color);
    p.circle_stroke(Pos2::new(x, mid.y), 5.0, Stroke::new(1.0, pal().cap_border));
    let resp = resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    if resp.double_clicked() {
        return (resp, Some(0.0));
    }
    let snap = |v: f32| {
        if v.abs() < 0.03 {
            0.0
        } else {
            (v.clamp(-1.0, 1.0) * 100.0).round() / 100.0
        }
    };
    if resp.dragged() && resp.drag_delta().x != 0.0 {
        return (
            resp.clone(),
            Some(snap(pan + resp.drag_delta().x / (rect.width() / 2.0))),
        );
    }
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            return (
                resp,
                Some(snap(pan + if scroll > 0.0 { 0.05 } else { -0.05 })),
            );
        }
    }
    (resp, None)
}

fn pan_text(pan: f32) -> String {
    if pan == 0.0 {
        "Balance: centre".into()
    } else if pan < 0.0 {
        format!("Balance: {:.0}% left", -pan * 100.0)
    } else {
        format!("Balance: {:.0}% right", pan * 100.0)
    }
}

fn kind_name(k: BandKind) -> &'static str {
    match k {
        BandKind::LowShelf => "Bass shelf",
        BandKind::Bell => "Bell",
        BandKind::HighShelf => "Treble shelf",
    }
}

fn fmt_hz(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{:.1} kHz", hz / 1000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// A distinct colour per EQ point.
fn band_color(i: usize) -> Color32 {
    const COLORS: [Color32; 8] = [
        Color32::from_rgb(0xff, 0x9f, 0x43),
        Color32::from_rgb(0xfe, 0xca, 0x57),
        Color32::from_rgb(0x1d, 0xd1, 0xa1),
        Color32::from_rgb(0x54, 0xa0, 0xff),
        Color32::from_rgb(0xc8, 0x6d, 0xd7),
        Color32::from_rgb(0xff, 0x6b, 0x6b),
        Color32::from_rgb(0x48, 0xdb, 0xfb),
        Color32::from_rgb(0xa3, 0xcb, 0x38),
    ];
    COLORS[i % COLORS.len()]
}

/// Horizontal level bar (dB) with an optional threshold marker.
fn level_bar(ui: &mut Ui, level_db: f32, marker_db: Option<f32>, min: f32, max: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(220.0, 10.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2, pal().groove);
    let x = |db: f32| rect.left() + ((db - min) / (max - min)).clamp(0.0, 1.0) * rect.width();
    if level_db > min {
        p.rect_filled(
            Rect::from_x_y_ranges(rect.left()..=x(level_db), rect.y_range()),
            2,
            METER_GREEN,
        );
    }
    if let Some(m) = marker_db {
        let mx = x(m);
        p.line_segment(
            [
                Pos2::new(mx, rect.top() - 3.0),
                Pos2::new(mx, rect.bottom() + 3.0),
            ],
            Stroke::new(2.0, pal().text),
        );
    }
}

/// How much the compressor is turning down (0–20 dB), growing from the left.
fn reduction_bar(ui: &mut Ui, reduction_db: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(220.0, 10.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2, pal().groove);
    let w = (reduction_db / 20.0).clamp(0.0, 1.0) * rect.width();
    if w > 0.0 {
        p.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(w, rect.height())),
            2,
            METER_YELLOW,
        );
    }
}

fn round_db(db: f32) -> f32 {
    (db * 10.0).round() / 10.0
}

fn db_text(db: f32) -> String {
    if db <= config::MIN_GAIN_DB {
        "-∞ dB".into()
    } else {
        format!("{db:+.1} dB")
    }
}

/// Short label for a bus: its id if short ("A1"), else its initials.
fn short_name(b: &Bus) -> String {
    if b.id.chars().count() <= 3 {
        b.id.to_uppercase()
    } else {
        b.name.chars().take(3).collect::<String>().to_uppercase()
    }
}

fn kind_label(kind: &Kind, devices: &[Device], is_bus: bool) -> String {
    match kind {
        Kind::Virtual if is_bus => "virtual mic".into(),
        Kind::Virtual => "virtual".into(),
        Kind::Physical { device } => devices
            .iter()
            .find(|d| &d.name == device)
            .map(|d| d.description.clone())
            .unwrap_or_else(|| "not connected".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_and_editing_from_the_dialog() {
        let mut cfg = Config::starter(Some("hw_out".into()), Some("hw_mic".into()));

        let mut ed = Editor::new(false);
        ed.name = "Game Chat".into();
        ed.apply(&mut cfg);
        let s = cfg.strips.last().unwrap();
        assert_eq!((s.id.as_str(), s.name.as_str()), ("game_chat", "Game Chat"));
        assert_eq!(
            s.buses,
            vec!["a1".to_string()],
            "new inputs go to the first output"
        );

        let mut ed = Editor::new(true);
        ed.name = "Stream Mix".into();
        ed.apply(&mut cfg);
        assert_eq!(cfg.buses.last().unwrap().kind, Kind::Virtual);

        let mut ed = Editor::edit_strip(cfg.strips.last().unwrap());
        ed.name = "Voice".into();
        ed.physical = true;
        ed.device = "hw_mic".into();
        ed.apply(&mut cfg);
        let s = cfg.strips.last().unwrap();
        assert_eq!(
            s.id, "game_chat",
            "editing keeps the id (and MIDI mappings)"
        );
        assert_eq!(
            s.kind,
            Kind::Physical {
                device: "hw_mic".into()
            }
        );
        cfg.validate().unwrap();
    }
}
