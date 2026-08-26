use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, SourceSpec};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserPreferences {
    pub bind: String,
    pub http_port: u16,
    pub source: SourceSpec,
    pub draw_mouse: bool,
    pub codec: String,
    pub quality: String,
    pub fps_preset: String,
    pub bitrate: u32,
    pub quality_mode: String,
    pub bitrate_mode: String,
    pub adaptive_quality_ceiling: String,
    pub adaptive_fps_ceiling: String,
    pub max_quality_groups: String,
    pub latency_preference: String,
    pub audio_mode: String,
    pub excluded_audio_processes: Vec<String>,
}

impl UserPreferences {
    pub fn from_config(config: &AppConfig) -> Self {
        let mut source = config.source.clone();
        // HWND values are reusable OS resources, not durable identities. Keep
        // them only in the running UI session and require a fresh card click
        // after relaunch.
        source.native_id = None;
        Self {
            bind: config.bind.clone(),
            http_port: config.http_port,
            source,
            draw_mouse: config.draw_mouse,
            codec: config.codec.clone(),
            quality: config.quality.clone(),
            fps_preset: config.fps_preset.clone(),
            bitrate: config.bitrate,
            quality_mode: config.quality_mode.clone(),
            bitrate_mode: config.bitrate_mode.clone(),
            adaptive_quality_ceiling: config.adaptive_quality_ceiling.clone(),
            adaptive_fps_ceiling: config.adaptive_fps_ceiling.clone(),
            max_quality_groups: config.max_quality_groups.clone(),
            latency_preference: config.latency_preference.clone(),
            audio_mode: config.audio_mode.clone(),
            excluded_audio_processes: config.excluded_audio_processes.clone(),
        }
    }

    pub fn apply_to(&self, config: &mut AppConfig) {
        config.bind = self.bind.clone();
        config.http_port = self.http_port;
        config.source = self.source.clone();
        config.draw_mouse = self.draw_mouse;
        config.codec = self.codec.clone();
        config.quality = self.quality.clone();
        config.fps_preset = self.fps_preset.clone();
        config.bitrate = self.bitrate;
        config.quality_mode = self.quality_mode.clone();
        config.bitrate_mode = self.bitrate_mode.clone();
        config.adaptive_quality_ceiling = self.adaptive_quality_ceiling.clone();
        config.adaptive_fps_ceiling = self.adaptive_fps_ceiling.clone();
        config.max_quality_groups = match self.max_quality_groups.parse::<usize>() {
            Ok(groups) if groups > 4 => "4".to_owned(),
            _ => self.max_quality_groups.clone(),
        };
        config.latency_preference = self.latency_preference.clone();
        config.audio_mode = self.audio_mode.clone();
        config.excluded_audio_processes = self.excluded_audio_processes.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_handles_are_not_persisted_across_sessions() {
        let mut config = AppConfig::default();
        config.source.kind = "window".to_owned();
        config.source.native_id = Some(42);

        assert_eq!(UserPreferences::from_config(&config).source.native_id, None);
    }
}

pub fn load() -> Option<UserPreferences> {
    let bytes = fs::read(path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save(config: &AppConfig) -> Result<()> {
    let directory = preferences_directory();
    fs::create_dir_all(&directory).context("create preferences directory")?;
    let bytes = serde_json::to_vec_pretty(&UserPreferences::from_config(config))?;
    fs::write(path(), bytes).context("write user preferences")?;
    Ok(())
}

fn preferences_directory() -> PathBuf {
    std::env::temp_dir().join("InstantLocalStream")
}

fn path() -> PathBuf {
    preferences_directory().join("preferences.json")
}
