mod config;
mod dsp;
mod engine;
mod fx;
mod graph;
mod gui;
mod ipc;
mod meter;
mod midi;
mod tray;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;
use std::time::Duration;

/// Mixrail: a virtual audio mixer for Linux, built on PipeWire.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file (default: ~/.config/mixrail/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the mixer engine. Edits to the config file are applied live.
    Run,
    /// Open the mixer window (the engine must be running).
    Gui,
    /// Show a tray icon (for desktops with a system tray, e.g. KDE Plasma, XFCE).
    Tray,
    /// List hardware audio devices (names to use for `device = "..."`).
    Devices,
    /// Write a starter config based on this machine's default devices.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Check the config file for mistakes and print what it sets up.
    Check,
    /// Remove all virtual devices and routing the mixer created.
    Teardown,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The engine logs its activity; the other commands only report problems.
    let level = if matches!(cli.cmd, Cmd::Run) {
        "info"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
    let explicit = cli.config.is_some();
    if !explicit {
        Config::move_from_old_location();
    }
    let path = cli.config.unwrap_or_else(Config::default_path);

    match cli.cmd {
        Cmd::Run => {
            if !path.exists() {
                // First start: make a starter layout, but only in the normal place. A
                // config named explicitly must exist: running a whole different layout
                // because of a typo would tear down the user's channels.
                if explicit {
                    bail!("{} doesn't exist (use `init` to create it)", path.display());
                }
                init(&path)?;
            }
            let cfg = Config::load(&path)?;
            engine::run(cfg, Some(path), None, true)
        }
        Cmd::Gui => gui::run(),
        Cmd::Tray => tray::run(),
        Cmd::Devices => {
            for (class, name, desc) in engine::list_devices()? {
                let kind = if class == "Audio/Sink" {
                    "output"
                } else {
                    "input "
                };
                println!("{kind}  {name}\n        {desc}");
            }
            Ok(())
        }
        Cmd::Init { force } => {
            if path.exists() && !force {
                bail!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                );
            }
            init(&path)
        }
        Cmd::Check => {
            let cfg = Config::load(&path)?;
            println!("{} is valid:", path.display());
            for b in &cfg.buses {
                println!("  bus   {:<10} {:<14} {:?}", b.id, b.name, b.kind);
            }
            for s in &cfg.strips {
                println!(
                    "  strip {:<10} {:<14} -> [{}]",
                    s.id,
                    s.name,
                    s.buses.join(", ")
                );
            }
            Ok(())
        }
        Cmd::Teardown => {
            let empty = Config::default();
            if std::os::unix::net::UnixStream::connect(ipc::socket_path()).is_ok() {
                bail!(
                    "the mixer engine is running; stop it first: systemctl --user stop mixrail"
                );
            }
            engine::run(empty, None, Some(Duration::from_millis(1500)), false)
        }
    }
}

fn init(path: &std::path::Path) -> Result<()> {
    let devices = engine::list_devices()?;
    let pick = |class: &str, pactl_arg: &str| -> Option<String> {
        let names: Vec<&String> = devices
            .iter()
            .filter(|(c, ..)| c == class)
            .map(|(_, n, _)| n)
            .collect();
        let default = std::process::Command::new("pactl")
            .arg(pactl_arg)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        // Use the desktop default if it's a real device, otherwise the first one found.
        default
            .filter(|d| names.contains(&d))
            .or_else(|| names.first().map(|n| n.to_string()))
    };
    let cfg = Config::starter(
        pick("Audio/Sink", "get-default-sink"),
        pick("Audio/Source", "get-default-source"),
    );
    cfg.save(path)?;
    println!("wrote {}", path.display());
    Ok(())
}
