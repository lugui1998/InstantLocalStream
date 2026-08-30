use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};

use crate::cli::StartArgs;

pub const TOKEN_LENGTH: usize = 12;
pub const QUALITY_PRESETS: &[&str] = &[
    "source", "144p", "240p", "360p", "480p", "720p", "1080p", "1440p", "2160p", "4320p",
];
pub const FPS_PRESETS: &[&str] = &["source", "5", "10", "24", "30", "60", "75", "120"];
pub const AUDIO_MODES: &[&str] = &["off", "system", "window"];
pub const QUALITY_MODES: &[&str] = &["manual", "adaptive"];
pub const BITRATE_MODES: &[&str] = &["fixed", "automatic"];
pub const LATENCY_PREFERENCES: &[&str] = &["low", "balanced", "quality"];
pub const DEFAULT_AUDIO_EXCLUSIONS: &[&str] = &[
    "Discord",
    "WhatsApp",
    "Telegram",
    "Microsoft Teams",
    "Zoom",
    "Skype",
    "Slack",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub bind: String,
    pub http_port: u16,
    pub media_ports: PortRange,
    pub advertise_host: Option<String>,
    pub token: String,
    pub max_viewers: usize,
    pub source: SourceSpec,
    pub draw_mouse: bool,
    pub codec: String,
    pub quality: String,
    pub fps_preset: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub quality_mode: String,
    pub bitrate_mode: String,
    pub adaptive_quality_ceiling: String,
    pub adaptive_fps_ceiling: String,
    pub max_quality_groups: String,
    pub latency_preference: String,
    pub audio_mode: String,
    pub excluded_audio_processes: Vec<String>,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortRange {
    pub first: u16,
    pub last: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSpec {
    pub kind: String,
    pub index: usize,
    /// Stable, session-scoped native source identity selected explicitly by
    /// the UI. CLI `kind:index` sources retain positional-index compatibility.
    #[serde(default)]
    pub native_id: Option<u64>,
}

#[derive(Debug, Clone, Args)]
pub struct ConfigArgs {
    #[arg(
        long,
        env = "ILS_PORT",
        help = "Use the same numeric port for HTTP/TCP and WebRTC/UDP"
    )]
    pub port: Option<u16>,
    #[arg(long, default_value = "lan", env = "ILS_BIND")]
    pub bind: String,
    #[arg(long, default_value_t = 8080, env = "ILS_HTTP_PORT")]
    pub http_port: u16,
    #[arg(long, default_value = "40000", env = "ILS_MEDIA_PORTS")]
    pub media_ports: String,
    #[arg(
        long,
        env = "ILS_ADVERTISE_HOST",
        help = "Host or IPv4 address used in the copied viewer URL"
    )]
    pub advertise_host: Option<String>,
    #[arg(long, env = "ILS_TOKEN")]
    pub token: Option<String>,
    #[arg(long, default_value_t = 8, env = "ILS_MAX_VIEWERS")]
    pub max_viewers: usize,
    #[arg(
        long,
        default_value = "monitor:0",
        env = "ILS_SOURCE",
        help = "Capture source: monitor:0, window:2, or test:0"
    )]
    pub source: String,
    #[arg(
        long,
        env = "ILS_SOURCE_NATIVE_ID",
        hide = true,
        help = "Stable native window identity used with a window source"
    )]
    pub source_native_id: Option<u64>,
    #[arg(long, default_value_t = true, env = "ILS_DRAW_MOUSE")]
    pub draw_mouse: bool,
    #[arg(long, default_value = "auto", env = "ILS_CODEC")]
    pub codec: String,
    #[arg(
        long,
        env = "ILS_QUALITY",
        help = "Quality preset such as source, 720p, or 1080p"
    )]
    pub quality: Option<String>,
    #[arg(
        long,
        env = "ILS_FPS_PRESET",
        help = "Frame-rate preset such as source, 30, or 60"
    )]
    pub fps_preset: Option<String>,
    #[arg(long, env = "ILS_WIDTH", hide = true)]
    pub width: Option<u32>,
    #[arg(long, env = "ILS_HEIGHT", hide = true)]
    pub height: Option<u32>,
    #[arg(long, env = "ILS_FPS", hide = true)]
    pub fps: Option<u32>,
    #[arg(long, default_value_t = 14_000_000, env = "ILS_BITRATE")]
    pub bitrate: u32,
    #[arg(long, default_value = "adaptive", env = "ILS_QUALITY_MODE")]
    pub quality_mode: String,
    #[arg(long, default_value = "automatic", env = "ILS_BITRATE_MODE")]
    pub bitrate_mode: String,
    #[arg(long, default_value = "source", env = "ILS_ADAPTIVE_QUALITY_CEILING")]
    pub adaptive_quality_ceiling: String,
    #[arg(long, default_value = "source", env = "ILS_ADAPTIVE_FPS_CEILING")]
    pub adaptive_fps_ceiling: String,
    #[arg(long, default_value = "2", env = "ILS_MAX_QUALITY_GROUPS")]
    pub max_quality_groups: String,
    #[arg(long, default_value = "low", env = "ILS_LATENCY_PREFERENCE")]
    pub latency_preference: String,
    #[arg(long, env = "ILS_AUDIO")]
    pub audio: Option<String>,
    #[arg(
        long = "exclude-audio-process",
        action = ArgAction::Append,
        env = "ILS_EXCLUDE_AUDIO_PROCESS",
        help = "Process name to exclude from system audio; may be repeated"
    )]
    pub exclude_audio_process: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind: "lan".to_owned(),
            http_port: 8080,
            media_ports: PortRange {
                first: 40_000,
                last: 40_000,
            },
            advertise_host: None,
            token: generate_token(),
            max_viewers: 8,
            source: SourceSpec {
                kind: "monitor".to_owned(),
                index: 0,
                native_id: None,
            },
            draw_mouse: true,
            codec: "auto".to_owned(),
            quality: "source".to_owned(),
            fps_preset: "source".to_owned(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 14_000_000,
            quality_mode: "adaptive".to_owned(),
            bitrate_mode: "automatic".to_owned(),
            adaptive_quality_ceiling: "source".to_owned(),
            adaptive_fps_ceiling: "source".to_owned(),
            max_quality_groups: "2".to_owned(),
            latency_preference: "low".to_owned(),
            audio_mode: "system".to_owned(),
            excluded_audio_processes: DEFAULT_AUDIO_EXCLUSIONS
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            json: false,
        }
    }
}

impl AppConfig {
    pub fn from_cli(args: Option<StartArgs>) -> Result<Self> {
        let mut config = Self::default();
        if let Some(args) = args {
            config.apply_args(args.config)?;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn apply_args(&mut self, args: ConfigArgs) -> Result<()> {
        let shared_port = args.port;
        self.bind = args.bind;
        self.http_port = args.http_port;
        self.media_ports = parse_port_range(&args.media_ports)?;
        self.advertise_host = args.advertise_host;
        if let Some(token) = args.token {
            validate_token(&token)?;
            self.token = token;
        }
        self.max_viewers = args.max_viewers;
        self.source = parse_source(&args.source)?;
        if args.source_native_id.is_some() && self.source.kind != "window" {
            bail!("--source-native-id can only be used with a window source");
        }
        self.source.native_id = args.source_native_id;
        self.draw_mouse = args.draw_mouse;
        self.codec = args.codec;
        let explicit_dimensions = args.width.is_some() || args.height.is_some();
        let explicit_fps = args.fps.is_some();
        self.quality = args.quality.unwrap_or_else(|| {
            if explicit_dimensions {
                "custom".to_owned()
            } else {
                "source".to_owned()
            }
        });
        self.fps_preset = args.fps_preset.unwrap_or_else(|| {
            if explicit_fps {
                "custom".to_owned()
            } else {
                "source".to_owned()
            }
        });
        if let Some(width) = args.width {
            self.width = width;
        }
        if let Some(height) = args.height {
            self.height = height;
        }
        if let Some(fps) = args.fps {
            self.fps = fps;
        }
        self.bitrate = args.bitrate;
        self.quality_mode = args.quality_mode;
        self.bitrate_mode = args.bitrate_mode;
        self.adaptive_quality_ceiling = args.adaptive_quality_ceiling;
        self.adaptive_fps_ceiling = args.adaptive_fps_ceiling;
        self.max_quality_groups = args.max_quality_groups;
        self.latency_preference = args.latency_preference;
        self.audio_mode = args.audio.unwrap_or_else(|| {
            if self.source.kind == "test" {
                "off".to_owned()
            } else {
                "system".to_owned()
            }
        });
        if !args.exclude_audio_process.is_empty() {
            self.excluded_audio_processes = args.exclude_audio_process;
        }
        if let Some(port) = shared_port {
            if port == 0 {
                bail!("--port must be between 1 and 65535");
            }
            self.http_port = port;
            self.media_ports = PortRange {
                first: port,
                last: port,
            };
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.http_port == 0 {
            bail!("HTTP port must be between 1 and 65535");
        }
        if let Some(host) = &self.advertise_host
            && (host.is_empty() || host.chars().any(char::is_whitespace) || host.contains('/'))
        {
            bail!("advertised host must be a hostname or IP address");
        }
        if self.media_ports.first == 0 || self.media_ports.last < self.media_ports.first {
            bail!("media port range is invalid");
        }
        if self.max_viewers == 0 {
            bail!("max viewers must be greater than zero");
        }
        if self.width == 0 || self.height == 0 || self.fps == 0 || self.bitrate == 0 {
            bail!("width, height, fps, and bitrate must be greater than zero");
        }
        if self.fps > 240 {
            bail!("capture FPS must not exceed 240");
        }
        match self.codec.to_ascii_lowercase().as_str() {
            "auto" | "vp8" | "vp9" | "h264" => {}
            other => bail!("unsupported initial codec '{other}', choose auto, vp8, vp9, or h264"),
        }
        if self.quality != "custom" && !QUALITY_PRESETS.contains(&self.quality.as_str()) {
            bail!(
                "unsupported quality preset '{}', choose source or a standard resolution",
                self.quality
            );
        }
        if self.fps_preset != "custom" && !FPS_PRESETS.contains(&self.fps_preset.as_str()) {
            bail!(
                "unsupported FPS preset '{}', choose source or a standard frame rate",
                self.fps_preset
            );
        }
        if !QUALITY_MODES.contains(&self.quality_mode.as_str()) {
            bail!(
                "unsupported quality mode '{}', choose manual or adaptive",
                self.quality_mode
            );
        }
        if !BITRATE_MODES.contains(&self.bitrate_mode.as_str()) {
            bail!(
                "unsupported bitrate mode '{}', choose fixed or automatic",
                self.bitrate_mode
            );
        }
        if !QUALITY_PRESETS.contains(&self.adaptive_quality_ceiling.as_str()) {
            bail!(
                "unsupported adaptive quality ceiling '{}', choose a standard preset or source",
                self.adaptive_quality_ceiling
            );
        }
        if !FPS_PRESETS.contains(&self.adaptive_fps_ceiling.as_str()) {
            bail!(
                "unsupported adaptive FPS ceiling '{}', choose a standard FPS or source",
                self.adaptive_fps_ceiling
            );
        }
        if self.max_quality_groups != "auto"
            && self
                .max_quality_groups
                .parse::<usize>()
                .ok()
                .is_none_or(|groups| !(1..=4).contains(&groups))
        {
            bail!("the current adaptive implementation supports one to four quality groups");
        }
        if !LATENCY_PREFERENCES.contains(&self.latency_preference.as_str()) {
            bail!(
                "unsupported latency preference '{}', choose low, balanced, or quality",
                self.latency_preference
            );
        }
        if !AUDIO_MODES.contains(&self.audio_mode.as_str()) {
            bail!(
                "unsupported audio mode '{}', choose off, system, or window",
                self.audio_mode
            );
        }
        if self.audio_mode == "window" && self.source.kind != "window" {
            bail!("window audio requires a selected application window");
        }
        if self.audio_mode == "system" && self.source.kind == "test" {
            bail!("system audio is unavailable for the test pattern");
        }
        Ok(())
    }

    pub fn output_height(&self) -> Option<u32> {
        let quality = if self.quality_mode == "adaptive" {
            &self.adaptive_quality_ceiling
        } else {
            &self.quality
        };
        quality_height(quality).or_else(|| (quality == "custom").then_some(self.height))
    }

    pub fn output_fps(&self) -> Option<u32> {
        let fps = if self.quality_mode == "adaptive" {
            &self.adaptive_fps_ceiling
        } else {
            &self.fps_preset
        };
        fps_value(fps).or_else(|| (fps == "custom").then_some(self.fps))
    }

    pub fn effective_bitrate(&self) -> u32 {
        if self.bitrate_mode == "fixed" {
            return self.bitrate;
        }
        let height = self.output_height().unwrap_or(self.height).max(144);
        let source_width = self.width.max(2);
        let source_height = self.height.max(2);
        let width = ((source_width as u64 * height as u64) / source_height as u64).max(2) as u32;
        let fps = self.output_fps().unwrap_or(self.fps).max(5);
        let bits_per_pixel_frame = match self.codec.to_ascii_lowercase().as_str() {
            "vp8" => 0.080,
            "vp9" => 0.055,
            "h264" => 0.065,
            "av1" => 0.040,
            _ => 0.080,
        };
        let multiplier = match self.latency_preference.as_str() {
            "low" => 0.90,
            "quality" => 1.20,
            _ => 1.0,
        };
        // The test pattern contains continuous motion and fine detail, so it is a
        // useful conservative stand-in for screen content with frequent changes.
        let content_multiplier = if self.source.kind == "test" {
            1.10
        } else {
            1.0
        };
        let recommendation = (width as f64
            * height as f64
            * fps as f64
            * bits_per_pixel_frame
            * multiplier
            * content_multiplier) as u32;
        recommendation
            .max(automatic_bitrate_floor(height, fps))
            .clamp(250_000, 25_000_000)
    }

    pub fn http_addr(&self) -> Result<SocketAddr> {
        let host = match self.bind.as_str() {
            "localhost" | "loopback" => "127.0.0.1",
            "lan" | "public" | "all" => "0.0.0.0",
            custom => custom,
        };
        let ip: IpAddr = host
            .parse()
            .with_context(|| format!("invalid bind address '{host}'"))?;
        Ok(SocketAddr::new(ip, self.http_port))
    }

    pub fn advertised_host(&self) -> String {
        if let Some(host) = &self.advertise_host {
            return host.clone();
        }
        match self.bind.as_str() {
            "localhost" | "loopback" => "127.0.0.1".to_owned(),
            "lan" | "all" => local_ipv4()
                .map(|address| address.to_string())
                .unwrap_or_else(|| "<lan-ip>".to_owned()),
            "public" => "<public-ip>".to_owned(),
            custom => custom.to_owned(),
        }
    }

    pub fn media_bind_host(&self) -> &'static str {
        match self.bind.as_str() {
            "localhost" | "loopback" => "127.0.0.1",
            _ => "0.0.0.0",
        }
    }

    pub fn advertised_host_for_media(&self) -> String {
        if let Some(host) = &self.advertise_host
            && host.parse::<IpAddr>().is_ok()
        {
            return host.clone();
        }
        match self.bind.as_str() {
            "localhost" | "loopback" => "127.0.0.1".to_owned(),
            "lan" | "public" | "all" => local_ipv4()
                .map(|address| address.to_string())
                .unwrap_or_else(|| "127.0.0.1".to_owned()),
            custom => custom.to_owned(),
        }
    }

    pub fn viewer_url(&self) -> String {
        self.viewer_url_for_host(&self.advertised_host())
    }

    pub fn viewer_url_for_host(&self, host: &str) -> String {
        format!("http://{}:{}/{}", host, self.http_port, self.token)
    }
}

/// Conservative automatic starting floors for readable screen content.
///
/// These are starting targets, not a promise that every network can sustain
/// them: the adaptive controller still lowers an individual group after a
/// credible rolling congestion signal.  The 1080p/30 floor intentionally
/// starts at 14 Mbps because text, UI edges, and scrolling expose compression
/// artifacts well before a camera-video heuristic would.
fn automatic_bitrate_floor(height: u32, fps: u32) -> u32 {
    match height {
        value if value >= 2_160 => 25_000_000,
        value if value >= 1_440 && fps >= 60 => 20_000_000,
        value if value >= 1_440 => 15_000_000,
        value if value >= 1_080 => 14_000_000,
        value if value >= 720 && fps >= 60 => 7_000_000,
        value if value >= 720 => 4_000_000,
        value if value >= 480 => 2_000_000,
        value if value >= 360 => 1_000_000,
        _ => 500_000,
    }
}

pub fn quality_height(value: &str) -> Option<u32> {
    match value {
        "144p" => Some(144),
        "240p" => Some(240),
        "360p" => Some(360),
        "480p" => Some(480),
        "720p" => Some(720),
        "1080p" => Some(1080),
        "1440p" => Some(1440),
        "2160p" => Some(2160),
        "4320p" => Some(4320),
        _ => None,
    }
}

pub fn fps_value(value: &str) -> Option<u32> {
    (!matches!(value, "source" | "custom"))
        .then(|| value.parse().ok())
        .flatten()
}

pub fn local_ipv4() -> Option<std::net::Ipv4Addr> {
    static LOCAL_IPV4: OnceLock<Option<std::net::Ipv4Addr>> = OnceLock::new();
    *LOCAL_IPV4.get_or_init(|| {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("8.8.8.8:80").ok()?;
        match socket.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(address) => Some(address),
            std::net::IpAddr::V6(_) => None,
        }
    })
}

pub fn generate_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(TOKEN_LENGTH)
        .map(char::from)
        .collect()
}

pub fn validate_token(token: &str) -> Result<()> {
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        bail!("token must contain exactly {TOKEN_LENGTH} ASCII letters or digits");
    }
    Ok(())
}

pub fn parse_port_range(value: &str) -> Result<PortRange> {
    let mut parts = value.split('-');
    let first: u16 = parts
        .next()
        .context("media port range is empty")?
        .parse()
        .context("invalid first media port")?;
    let last: u16 = parts
        .next()
        .unwrap_or(value)
        .parse()
        .context("invalid last media port")?;
    if parts.next().is_some() {
        bail!("media port range must look like 40000-40010");
    }
    Ok(PortRange { first, last })
}

pub fn parse_source(value: &str) -> Result<SourceSpec> {
    let (kind, identity) = value
        .split_once(':')
        .context("source must look like monitor:0")?;
    let index = identity.parse().context("source index must be a number")?;
    match kind {
        "monitor" | "window" | "test" => Ok(SourceSpec {
            kind: kind.to_owned(),
            index,
            native_id: None,
        }),
        other => bail!("unsupported source kind '{other}', use monitor, window, or test"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_twelve_ascii_alphanumeric_characters() {
        let token = generate_token();
        assert_eq!(token.len(), TOKEN_LENGTH);
        assert!(token.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    fn default_uses_one_shared_media_port() {
        let config = AppConfig::default();
        assert_eq!(config.media_ports.first, config.media_ports.last);
        assert_eq!(config.codec, "auto");
        assert_eq!(config.bitrate, 14_000_000);
    }

    #[test]
    fn viewer_url_for_host_always_uses_http_and_the_supplied_host() {
        let mut config = AppConfig::default();
        config.http_port = 9000;
        config.token = "Ab12Cd34Ef56".to_owned();

        assert_eq!(
            config.viewer_url_for_host("stream.example.com"),
            "http://stream.example.com:9000/Ab12Cd34Ef56"
        );
    }

    #[test]
    fn h264_is_an_available_initial_codec() {
        let mut config = AppConfig::default();
        config.codec = "h264".to_owned();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn capture_fps_is_bounded_for_timing_retention() {
        let mut config = AppConfig::default();
        config.fps = 240;
        assert!(config.validate().is_ok());
        config.fps = 241;
        assert!(config.validate().is_err());
    }

    #[test]
    fn automatic_vp8_1080p60_uses_a_quality_preserving_start_bitrate() {
        let mut config = AppConfig::default();
        config.source.kind = "test".to_owned();
        config.codec = "vp8".to_owned();
        config.width = 1920;
        config.height = 1080;
        config.fps = 60;
        config.quality_mode = "manual".to_owned();
        config.quality = "1080p".to_owned();
        config.fps_preset = "60".to_owned();
        config.bitrate_mode = "automatic".to_owned();
        config.latency_preference = "low".to_owned();
        assert!(config.effective_bitrate() >= 14_000_000);
    }

    #[test]
    fn automatic_1080p30_starts_at_fourteen_megabits_or_more() {
        let mut config = AppConfig::default();
        config.source.kind = "test".to_owned();
        config.codec = "vp8".to_owned();
        config.width = 1920;
        config.height = 1080;
        config.fps = 30;
        config.quality_mode = "manual".to_owned();
        config.quality = "1080p".to_owned();
        config.fps_preset = "30".to_owned();
        config.bitrate_mode = "automatic".to_owned();

        assert!(config.effective_bitrate() >= 14_000_000);
    }

    #[test]
    fn shared_port_configures_tcp_and_udp() {
        let mut config = AppConfig::default();
        config
            .apply_args(ConfigArgs {
                port: Some(18080),
                bind: "localhost".to_owned(),
                http_port: 8080,
                media_ports: "40000-40010".to_owned(),
                advertise_host: None,
                token: None,
                max_viewers: 8,
                source: "monitor:0".to_owned(),
                source_native_id: None,
                draw_mouse: true,
                codec: "vp8".to_owned(),
                quality: None,
                fps_preset: None,
                width: Some(1920),
                height: Some(1080),
                fps: Some(60),
                bitrate: 6_000_000,
                quality_mode: "manual".to_owned(),
                bitrate_mode: "fixed".to_owned(),
                adaptive_quality_ceiling: "source".to_owned(),
                adaptive_fps_ceiling: "source".to_owned(),
                max_quality_groups: "1".to_owned(),
                latency_preference: "balanced".to_owned(),
                audio: Some("off".to_owned()),
                exclude_audio_process: Vec::new(),
            })
            .unwrap();
        assert_eq!(config.http_port, 18080);
        assert_eq!(config.media_ports.first, 18080);
        assert_eq!(config.media_ports.last, 18080);
    }
}
