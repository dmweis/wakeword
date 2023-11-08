use config::Config;
use serde::Deserialize;
use std::{path::PathBuf, str};
use tracing::*;
use zenoh::config::Config as ZenohConfig;

use crate::WakewordError;

/// Use default config if no path is provided
pub fn get_configuration(config: &Option<PathBuf>) -> anyhow::Result<WakewordConfig> {
    let settings = if let Some(config) = config {
        info!("Using configuration from {:?}", config);
        Config::builder()
            .add_source(config::Environment::with_prefix("APP"))
            .add_source(config::File::with_name(
                config
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("Failed to convert path"))?,
            ))
            .build()?
    } else {
        info!("Using dev configuration");
        Config::builder()
            .add_source(config::Environment::with_prefix("APP"))
            .add_source(config::File::with_name("config/settings"))
            .add_source(config::File::with_name("config/dev_settings"))
            .build()?
    };

    Ok(settings.try_deserialize()?)
}

#[derive(Deserialize, Debug, Clone)]
pub struct WakewordConfig {
    pub app: AppConfig,
    pub picovoice: PicovoiceConfig,
    pub openai: WakeWordOpenaiConfig,
    pub zenoh: WakewordZenohConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AppConfig {
    pub zenoh_prefix: String,
}

// zenoh topic
const VOICE_PROBABILITY_TOPIC: &str = "telemetry/voice_probability";
const VOICE_PROBABILITY_PRETTY_PRINT_TOPIC: &str = "telemetry/voice_probability_pretty_print";
const WAKE_WORD_DETECTION_TOPIC: &str = "event/wake_word_detection";
const WAKE_WORD_DETECTION_END_TOPIC: &str = "event/wake_word_detection_end";
const TRANSCRIPT_TOPIC: &str = "event/transcript";

impl AppConfig {
    pub fn get_voice_probability_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, VOICE_PROBABILITY_TOPIC)
    }

    pub fn get_voice_probability_pretty_print_topic(&self) -> String {
        format!(
            "{}/{}",
            self.zenoh_prefix, VOICE_PROBABILITY_PRETTY_PRINT_TOPIC
        )
    }

    pub fn get_wake_word_detection_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, WAKE_WORD_DETECTION_TOPIC)
    }

    pub fn get_wake_word_detection_end_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, WAKE_WORD_DETECTION_END_TOPIC)
    }

    pub fn get_transcript_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, TRANSCRIPT_TOPIC)
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct PicovoiceConfig {
    pub access_key: String,
    pub keywords: Option<Vec<String>>,
    pub keyword_paths: Option<Vec<std::path::PathBuf>>,
    pub model_path: Option<std::path::PathBuf>,
    pub sensitivities: Option<Vec<f32>>,
    pub audio_device_index: Option<i32>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct WakeWordOpenaiConfig {
    pub api_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct WakewordZenohConfig {
    pub connect: Vec<zenoh_config::EndPoint>,
    pub listen: Vec<zenoh_config::EndPoint>,
    pub config_path: Option<String>,
}

impl WakewordZenohConfig {
    pub fn get_zenoh_config(&self) -> anyhow::Result<ZenohConfig> {
        let mut config = if let Some(conf_file) = &self.config_path {
            ZenohConfig::from_file(conf_file).map_err(WakewordError::ZenohError)?
        } else {
            ZenohConfig::default()
        };
        if !self.connect.is_empty() {
            config.connect.endpoints = self.connect.clone();
        }
        if !self.listen.is_empty() {
            config.listen.endpoints = self.listen.clone();
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DEFAULT_CONFIG: &str = include_str!("../config/settings.yaml");

    #[test]
    fn test_config() {
        let builder = Config::builder()
            .add_source(config::File::from_str(
                DEFAULT_CONFIG,
                config::FileFormat::Yaml,
            ))
            .build()
            .unwrap();
        builder.try_deserialize::<WakewordConfig>().unwrap();
    }
}
