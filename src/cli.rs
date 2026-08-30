use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use tracing_subscriber::{EnvFilter, fmt};

use crate::capture;
use crate::config::{AppConfig, ConfigArgs};
use crate::encoder;
use crate::server;

#[derive(Debug, Parser)]
#[command(
    name = "instant-local-stream",
    version,
    about = "Portable low-latency local screen streaming host"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        default_value_t = false,
        help = "Open the native control UI instead of running headlessly"
    )]
    pub ui: bool,
    #[arg(
        long,
        global = true,
        default_value_t = false,
        help = "Emit JSON logs where supported"
    )]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Gui,
    Start(StartArgs),
    ListSources,
    ListWindows,
    Validate(ValidateArgs),
    PublicIp,
    Status {
        #[arg(long, default_value_t = 8475)]
        http_port: u16,
    },
    Version,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
}

pub fn init_logging(json_output: bool, gui_mode: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if cfg!(windows) && gui_mode && !json_output {
        // A Windows GUI executable may have no console at all, or it may
        // outlive the terminal pipe inherited from `cargo run`. The default
        // tracing stderr writer panics when that pipe is closed. Recovery
        // warnings must never be able to terminate the host.
        let _ = fmt()
            .with_env_filter(filter)
            .with_writer(std::io::sink)
            .try_init();
    } else if json_output {
        let _ = fmt().with_env_filter(filter).json().try_init();
    } else {
        let _ = fmt().with_env_filter(filter).try_init();
    }
}

pub fn run_headless(config: AppConfig) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create Tokio runtime")?;
    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let mut server_task = tokio::spawn(server::run(config.clone(), shutdown_rx));
        tokio::select! {
            result = &mut server_task => result.context("server task panicked")??,
            _ = tokio::signal::ctrl_c() => {
                let _ = shutdown_tx.send(());
                server_task.await.context("server task panicked")??;
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

pub fn validate(args: ValidateArgs, json_output: bool) -> Result<()> {
    let mut config = AppConfig::default();
    config.apply_args(args.config)?;
    config.validate()?;
    let ffmpeg = encoder::find_ffmpeg();
    let (sources, source_error) = if config.source.kind == "test" {
        (Vec::new(), None)
    } else {
        let source_result = capture::list_sources();
        let source_error = source_result.as_ref().err().map(ToString::to_string);
        (source_result.unwrap_or_default(), source_error)
    };
    let source_available = config.source.kind == "test"
        || (config.source.kind == "window"
            && config
                .source
                .native_id
                .is_some_and(capture::native_window_exists))
        || sources.iter().any(|source| {
            source.kind == config.source.kind
                && match config.source.native_id {
                    Some(native_id) => source.native_id == Some(native_id),
                    None => source.index == config.source.index,
                }
        });
    let ok = ffmpeg.available && source_available && source_error.is_none();
    let result = json!({
        "ok": ok,
        "http_url": config.viewer_url(),
        "codec": config.codec,
        "quality": config.quality,
        "fps_preset": config.fps_preset,
        "output_height": config.output_height(),
        "output_fps": config.output_fps(),
        "audio_mode": config.audio_mode,
        "excluded_audio_processes": config.excluded_audio_processes,
        "ffmpeg": ffmpeg,
        "sources": sources,
        "source_available": source_available,
        "source_error": source_error,
        "draw_mouse": config.draw_mouse,
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("configuration: {}", if ok { "valid" } else { "invalid" });
        println!("viewer URL: {}", config.viewer_url());
        println!("ffmpeg: {}", result["ffmpeg"]);
        println!("capture sources: {}", sources.len());
    }
    if !ok {
        bail!("validation failed; inspect the JSON or messages above");
    }
    Ok(())
}

pub fn print_status(http_port: u16, json_output: bool) -> Result<()> {
    let status = match crate::runtime::read(http_port)? {
        Some(status) => json!({ "running": true, "record": status }),
        None => json!({ "running": false, "message": "no active runtime record was found" }),
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("running: {}", status["running"]);
        if let Some(record) = status.get("record") {
            println!("viewer URL: {}", record["viewer_url"]);
            println!("pid: {}", record["pid"]);
        } else {
            println!("{}", status["message"]);
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn print_json_event(event: serde_json::Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", serde_json::to_string_pretty(&event)?)?;
    stdout.flush()?;
    Ok(())
}

#[allow(dead_code)]
pub fn wait_for_viewer_timeout() -> Duration {
    Duration::from_secs(30)
}
