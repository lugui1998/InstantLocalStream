use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{
    AppConfig, DEFAULT_AUDIO_EXCLUSIONS, DEFAULT_TOKEN_LENGTH, MAX_QUALITY_GROUPS,
    MAX_TOKEN_LENGTH, MIN_TOKEN_LENGTH, SourceSpec, TOKEN_MODES, generate_token_with_options,
    token_for_display, validate_token, validate_token_with_length,
};

const CURRENT_PREFERENCES_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostNetworkTestResult {
    pub upload_bps: u64,
    pub tested_at_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserPreferences {
    #[serde(default)]
    pub schema_version: u32,
    pub bind: String,
    pub http_port: u16,
    #[serde(default = "default_max_viewers")]
    pub max_viewers: usize,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_token_mode")]
    pub token_mode: String,
    #[serde(default)]
    pub token_locked: bool,
    #[serde(default = "default_token_length")]
    pub token_length: usize,
    #[serde(default = "default_token_case_sensitive")]
    pub token_case_sensitive: bool,
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
    #[serde(default = "default_encoder_device")]
    pub encoder_device: String,
    pub latency_preference: String,
    pub audio_mode: String,
    pub excluded_audio_processes: Vec<String>,
    #[serde(default = "default_diagnostics_enabled")]
    pub diagnostics_enabled: bool,
    #[serde(default)]
    pub host_network_test: Option<HostNetworkTestResult>,
}

fn default_max_viewers() -> usize {
    8
}

fn default_token_length() -> usize {
    DEFAULT_TOKEN_LENGTH
}

fn default_token_mode() -> String {
    "automatic".to_owned()
}

fn default_token_case_sensitive() -> bool {
    true
}

fn default_encoder_device() -> String {
    "auto".to_owned()
}

fn default_diagnostics_enabled() -> bool {
    true
}

impl UserPreferences {
    pub fn from_config(
        config: &AppConfig,
        host_network_test: Option<HostNetworkTestResult>,
    ) -> Self {
        let mut source = config.source.clone();
        // HWND values are reusable OS resources, not durable identities. Keep
        // them only in the running UI session and require a fresh card click
        // after relaunch.
        source.native_id = None;
        Self {
            schema_version: CURRENT_PREFERENCES_VERSION,
            bind: config.bind.clone(),
            http_port: config.http_port,
            max_viewers: config.max_viewers,
            token: (config.token_mode == "custom" || config.token_locked)
                .then(|| token_for_display(&config.token, config.token_case_sensitive)),
            token_mode: config.token_mode.clone(),
            token_locked: config.token_locked,
            token_length: config.token_length,
            token_case_sensitive: config.token_case_sensitive,
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
            encoder_device: config.encoder_device.clone(),
            latency_preference: config.latency_preference.clone(),
            audio_mode: config.audio_mode.clone(),
            excluded_audio_processes: config.excluded_audio_processes.clone(),
            diagnostics_enabled: config.diagnostics_enabled,
            host_network_test,
        }
    }

    pub fn apply_to(&self, config: &mut AppConfig) {
        // Version 0 predates the current persisted preference schema. Keep
        // its legacy system-audio selection when restoring old preferences.
        let legacy_defaults = self.schema_version == 0;
        config.bind = self.bind.clone();
        config.http_port = self.http_port;
        config.max_viewers = self.max_viewers.max(1);
        config.token_mode = if TOKEN_MODES.contains(&self.token_mode.as_str()) {
            self.token_mode.clone()
        } else {
            default_token_mode()
        };
        config.token_length = self.token_length.clamp(MIN_TOKEN_LENGTH, MAX_TOKEN_LENGTH);
        config.token_case_sensitive = if config.token_mode == "custom" {
            true
        } else {
            self.token_case_sensitive
        };
        config.token_locked = config.token_mode == "automatic" && self.token_locked;
        config.token = match config.token_mode.as_str() {
            "custom" => match self.token.as_deref() {
                Some(token) if validate_token(token).is_ok() => token.to_owned(),
                _ => generate_token_with_options(config.token_length, config.token_case_sensitive),
            },
            _ if config.token_locked => match self.token.as_deref() {
                Some(token) if validate_token_with_length(token, config.token_length).is_ok() => {
                    token_for_display(token, config.token_case_sensitive)
                }
                _ => generate_token_with_options(config.token_length, config.token_case_sensitive),
            },
            _ => generate_token_with_options(config.token_length, config.token_case_sensitive),
        };
        if config.token_mode == "custom" {
            config.token_length = config.token.len();
        }
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
            Ok(groups) if groups > MAX_QUALITY_GROUPS => MAX_QUALITY_GROUPS.to_string(),
            _ => self.max_quality_groups.clone(),
        };
        config.encoder_device = if self.encoder_device == "software"
            || self.encoder_device == "auto"
            || self.encoder_device.starts_with("gpu:")
        {
            self.encoder_device.clone()
        } else {
            "auto".to_owned()
        };
        config.latency_preference = self.latency_preference.clone();
        config.audio_mode = self.audio_mode.clone();
        config.diagnostics_enabled = self.diagnostics_enabled;
        let legacy_default_exclusions = self.excluded_audio_processes.len()
            == DEFAULT_AUDIO_EXCLUSIONS.len()
            && DEFAULT_AUDIO_EXCLUSIONS.iter().all(|default| {
                self.excluded_audio_processes
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(default))
            });
        config.excluded_audio_processes = if legacy_defaults && legacy_default_exclusions {
            Vec::new()
        } else {
            self.excluded_audio_processes.clone()
        };
    }
}

pub fn load() -> Option<UserPreferences> {
    let bytes = fs::read(path())
        // Preserve settings written under either historical directory name
        // until the next save migrates them to Instant-Local-Stream.
        .or_else(|_| fs::read(legacy_preferences_path()))
        .or_else(|_| fs::read(legacy_temp_path()))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save(config: &AppConfig, host_network_test: Option<HostNetworkTestResult>) -> Result<()> {
    let directory = preferences_directory();
    fs::create_dir_all(&directory).context("create preferences directory")?;
    let bytes =
        serde_json::to_vec_pretty(&UserPreferences::from_config(config, host_network_test))?;
    fs::write(path(), bytes).context("write user preferences")?;
    Ok(())
}

fn preferences_directory() -> PathBuf {
    preferences_base_directory().join("Instant-Local-Stream")
}

fn preferences_base_directory() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".config"))
            })
    };
    base.unwrap_or_else(std::env::temp_dir)
}

fn path() -> PathBuf {
    preferences_directory().join("preferences.json")
}

fn legacy_preferences_path() -> PathBuf {
    preferences_base_directory()
        .join("InstantLocalStream")
        .join("preferences.json")
}

fn legacy_temp_path() -> PathBuf {
    std::env::temp_dir()
        .join("InstantLocalStream")
        .join("preferences.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_handles_are_not_persisted_across_sessions() {
        let mut config = AppConfig::default();
        config.source.kind = "window".to_owned();
        config.source.native_id = Some(42);

        assert_eq!(
            UserPreferences::from_config(&config, None).source.native_id,
            None
        );
    }

    #[test]
    fn unlocked_tokens_are_not_persisted() {
        let config = AppConfig::default();

        assert!(!config.token_locked);
        assert_eq!(UserPreferences::from_config(&config, None).token, None);
    }

    #[test]
    fn locked_token_and_generation_settings_round_trip() {
        let mut config = AppConfig::default();
        config.token = "Abcdefgh123456".to_owned();
        config.token_mode = "automatic".to_owned();
        config.token_locked = true;
        config.token_length = config.token.len();
        config.token_case_sensitive = false;

        let preferences = UserPreferences::from_config(&config, None);
        let mut restored = AppConfig::default();
        preferences.apply_to(&mut restored);

        assert_eq!(restored.token, "ABCDEFGH123456");
        assert!(restored.token_locked);
        assert_eq!(restored.token_length, 14);
        assert!(!restored.token_case_sensitive);
    }

    #[test]
    fn custom_token_is_persisted_without_the_automatic_lock() {
        let mut config = AppConfig::default();
        config.token_mode = "custom".to_owned();
        config.token = "MyCustomToken123".to_owned();
        config.token_length = config.token.len();

        let preferences = UserPreferences::from_config(&config, None);
        assert_eq!(preferences.token.as_deref(), Some("MyCustomToken123"));
        assert!(!preferences.token_locked);
    }

    #[test]
    fn diagnostics_visibility_round_trips() {
        let mut config = AppConfig::default();
        config.diagnostics_enabled = false;

        let preferences = UserPreferences::from_config(&config, None);
        let mut restored = AppConfig::default();
        preferences.apply_to(&mut restored);

        assert!(!restored.diagnostics_enabled);
    }

    #[test]
    fn older_preferences_get_defaults_for_new_persisted_fields() {
        let config = AppConfig::default();
        let mut value = serde_json::to_value(UserPreferences::from_config(&config, None)).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("max_viewers");
        object.remove("host_network_test");
        object.remove("diagnostics_enabled");

        let restored: UserPreferences = serde_json::from_value(value).unwrap();

        assert_eq!(restored.max_viewers, 8);
        assert_eq!(restored.host_network_test, None);
        assert!(restored.diagnostics_enabled);
    }

    #[test]
    fn legacy_network_selection_is_preserved_with_system_audio() {
        let config = AppConfig {
            bind: "lan".to_owned(),
            audio_mode: "system".to_owned(),
            excluded_audio_processes: DEFAULT_AUDIO_EXCLUSIONS
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            ..Default::default()
        };
        let mut value = serde_json::to_value(UserPreferences::from_config(&config, None)).unwrap();
        value.as_object_mut().unwrap().remove("schema_version");
        let legacy: UserPreferences = serde_json::from_value(value).unwrap();
        let mut migrated = config;

        legacy.apply_to(&mut migrated);

        assert_eq!(migrated.bind, "lan");
        assert_eq!(migrated.audio_mode, "system");
        assert!(migrated.excluded_audio_processes.is_empty());
    }
}
