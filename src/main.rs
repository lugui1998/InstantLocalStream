#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod audio;
mod capture;
mod cli;
mod config;
mod encoder;
mod media;
mod network;
mod packaging;
mod preferences;
mod runtime;
mod server;
mod shared_capture;
mod udp_mux;
mod ui;
mod window_capture;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::AppConfig;

fn main() {
    std::panic::set_hook(Box::new(|panic| {
        let directory = std::env::temp_dir().join("Instant-Local-Stream");
        let _ = std::fs::create_dir_all(&directory);
        let report = format!(
            "panic: {panic}\nbacktrace:\n{}\n",
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::write(directory.join("last-crash.txt"), report);
    }));
    if let Err(error) = run() {
        let directory = std::env::temp_dir().join("Instant-Local-Stream");
        let _ = std::fs::create_dir_all(&directory);
        let _ = std::fs::write(directory.join("last-error.txt"), format!("{error:?}\n"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    if std::env::args_os().nth(1).is_none() {
        cli::init_logging(false, true);
        return ui::run(AppConfig::from_cli(None)?);
    }
    let cli = Cli::parse();
    let gui_mode = cli.ui || matches!(&cli.command, None | Some(Command::Gui));
    cli::init_logging(cli.json, gui_mode);
    let json_output = cli.json;
    let open_ui = cli.ui;

    match cli.command.unwrap_or(Command::Gui) {
        Command::Gui => ui::run(AppConfig::from_cli(None)?)?,
        Command::Start(args) => {
            let mut config = AppConfig::from_cli(Some(args))?;
            if open_ui {
                ui::run_without_preferences(config)?;
            } else {
                config.json = json_output;
                cli::run_headless(config)?;
            }
        }
        Command::ListSources => capture::print_sources(json_output)?,
        Command::ListWindows => capture::print_windows(json_output)?,
        Command::Validate(args) => cli::validate(args, json_output)?,
        Command::PublicIp => {
            let address = network::lookup_public_ipv4()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "ipv4": address }))?
                );
            } else {
                println!("{address}");
            }
        }
        Command::Status { http_port } => cli::print_status(http_port, json_output)?,
        Command::Version => println!("instant-local-stream {}", env!("CARGO_PKG_VERSION")),
    }

    Ok(())
}
