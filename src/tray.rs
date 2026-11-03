//! Tray icon: a small separate program (`mixrail tray`) started with the desktop
//! session. It uses the freedesktop StatusNotifierItem standard, shown by KDE Plasma, XFCE,
//! Cinnamon, MATE, Budgie, LXQt and others (GNOME only with the AppIndicator extension).
//! Where no tray exists it just waits quietly.
//!
//! It talks to the engine over the same socket as the window, announcing itself with
//! `Command::Tray` so the engine doesn't treat it as an open window.

use crate::ipc::{self, Command, Event};
use anyhow::Result;
use ksni::blocking::TrayMethods;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Tray {
    connected: bool,
    windows: usize,
    scenes: Vec<String>,
    writer: Arc<Mutex<Option<UnixStream>>>,
    /// When a window was last started (it takes a moment to connect).
    launched: Option<Instant>,
}

impl Tray {
    fn send(&self, cmd: &Command) {
        if let (Some(w), Ok(line)) = (
            self.writer.lock().unwrap().as_mut(),
            serde_json::to_string(cmd),
        ) {
            let _ = w.write_all(format!("{line}\n").as_bytes());
        }
    }

    /// Bring the open window to the front, or open one.
    fn show_window(&mut self) {
        if self.windows > 0 {
            self.send(&Command::RaiseWindow);
            return;
        }
        if self
            .launched
            .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| "mixrail".into());
        match std::process::Command::new(exe).arg("gui").spawn() {
            Ok(mut child) => {
                self.launched = Some(Instant::now());
                // Collect its exit status so it doesn't linger as a zombie.
                std::thread::spawn(move || child.wait());
            }
            Err(e) => log::warn!("cannot open the mixer window: {e}"),
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "mixrail".into()
    }
    fn title(&self) -> String {
        "Mixrail".into()
    }
    fn icon_name(&self) -> String {
        "mixrail".into()
    }
    /// The icon itself too, for when it isn't installed in the icon theme.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static PNG: &[u8] = include_bytes!("../assets/icons/mixrail-64.png");
        let Ok(icon) = eframe::icon_data::from_png_bytes(PNG) else {
            return vec![];
        };
        // RGBA → ARGB in network byte order, as the tray spec wants.
        let data = icon
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, a]| [a, r, g, b])
            .collect();
        vec![ksni::Icon {
            width: icon.width as i32,
            height: icon.height as i32,
            data,
        }]
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Mixrail".into(),
            description: if self.connected {
                String::new()
            } else {
                "The mixer engine is not running".into()
            },
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_window();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: "Open mixer".into(),
                icon_name: "mixrail".into(),
                enabled: self.connected,
                activate: Box::new(|t: &mut Self| t.show_window()),
                ..Default::default()
            }
            .into(),
        ];
        if !self.connected {
            items.push(
                StandardItem {
                    label: "Mixer engine is not running".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        if self.connected && !self.scenes.is_empty() {
            items.push(MenuItem::Separator);
            items.push(
                SubMenu {
                    label: "Scenes".into(),
                    submenu: self
                        .scenes
                        .iter()
                        .map(|name| {
                            let name = name.clone();
                            StandardItem {
                                label: name.replace('_', "__"),
                                activate: Box::new(move |t: &mut Self| {
                                    t.send(&Command::RecallScene { name: name.clone() })
                                }),
                                ..Default::default()
                            }
                            .into()
                        })
                        .collect(),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Hide this icon".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub fn run() -> Result<()> {
    let writer = Arc::new(Mutex::new(None));
    let tray = Tray {
        connected: false,
        windows: 0,
        scenes: Vec::new(),
        writer: writer.clone(),
        launched: None,
    };
    // At login the panel may appear after us: keep waiting for it instead of giving up.
    let handle = tray.assume_sni_available(true).spawn()?;
    loop {
        if let Ok(stream) = UnixStream::connect(ipc::socket_path()) {
            *writer.lock().unwrap() = stream.try_clone().ok();
            handle.update(|t| {
                t.connected = true;
                t.send(&Command::Tray);
            });
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                // Level meters stream while a window is open; nothing for us there.
                if line.starts_with(r#"{"type":"meters""#) {
                    continue;
                }
                if let Ok(Event::State {
                    config, windows, ..
                }) = serde_json::from_str::<Event>(&line)
                {
                    let scenes: Vec<String> = config.scenes.into_iter().map(|s| s.name).collect();
                    handle.update(|t| {
                        t.windows = windows;
                        t.scenes = scenes;
                    });
                }
            }
            *writer.lock().unwrap() = None;
            handle.update(|t| {
                t.connected = false;
                t.windows = 0;
            });
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
