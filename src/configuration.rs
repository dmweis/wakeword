use anyhow::Context;
use config::Config;
use porcupine::{util::pv_keyword_paths, BuiltinKeywords, Porcupine, PorcupineBuilder};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    str::{self, FromStr},
};
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
    #[serde(default)]
    pub zenoh: WakewordZenohConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AppConfig {
    pub zenoh_prefix: String,
    #[serde(default)]
    pub system_prompts: HashMap<String, String>,
}

// zenoh topic
const VOICE_PROBABILITY_TOPIC: &str = "telemetry/voice_probability";
const VOICE_PROBABILITY_PRETTY_PRINT_TOPIC: &str = "telemetry/voice_probability_pretty_print";
const WAKE_WORD_DETECTION_TOPIC: &str = "event/wake_word_detection";
const WAKE_WORD_RECORDING_STARTED_TOPIC: &str = "event/recording_started";
const WAKE_WORD_RECORDING_END_TOPIC: &str = "event/wake_word_detection_end";
const WAKE_WORD_RECORDING_AUDIO_WAV_FILE: &str = "event/wake_word_audio_wav";
const TRANSCRIPT_TOPIC: &str = "event/transcript";
const PRIVACY_MODE_TOPIC: &str = "control/privacy_mode";

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

    pub fn get_wake_word_detected_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, WAKE_WORD_DETECTION_TOPIC)
    }

    pub fn get_wake_word_recording_started_topic(&self) -> String {
        format!(
            "{}/{}",
            self.zenoh_prefix, WAKE_WORD_RECORDING_STARTED_TOPIC
        )
    }

    pub fn get_wake_word_recording_end_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, WAKE_WORD_RECORDING_END_TOPIC)
    }

    pub fn get_wake_word_audio_recording_wav_topic(&self) -> String {
        format!(
            "{}/{}",
            self.zenoh_prefix, WAKE_WORD_RECORDING_AUDIO_WAV_FILE
        )
    }

    pub fn get_transcript_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, TRANSCRIPT_TOPIC)
    }

    pub fn get_privacy_mode_topic(&self) -> String {
        format!("{}/{}", self.zenoh_prefix, PRIVACY_MODE_TOPIC)
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct PicovoiceConfig {
    pub access_key: String,
    pub keywords: Option<Vec<String>>,
    pub keyword_paths: Option<HashMap<String, std::path::PathBuf>>,
    pub model_path: Option<std::path::PathBuf>,
    pub sensitivities: Option<Vec<f32>>,
    pub audio_device_index: Option<i32>,
    /// Keyword used to dismiss active recording
    pub dismiss_keyword: Option<String>,
    // these are stupid. Why are they not included in a more sensible way?
    pub cobra_lib_path: Option<std::path::PathBuf>,
    pub porcupine_lib_path: Option<std::path::PathBuf>,
    pub recorder_lib_path: Option<std::path::PathBuf>,
}

impl PicovoiceConfig {
    #[allow(dead_code)]
    pub fn try_convert_keywords(&self) -> anyhow::Result<Vec<BuiltinKeywords>> {
        if let Some(keywords) = &self.keywords {
            let keywords = keywords
                .iter()
                .map(|keyword| BuiltinKeywords::from_str(keyword))
                .collect::<Result<Vec<_>, _>>();

            match keywords {
                Ok(keywords) => Ok(keywords),
                Err(()) => Err(anyhow::anyhow!(
                    "Failed to convert keywords to built in keywords"
                )),
            }
        } else {
            Ok(vec![])
        }
    }

    pub fn keyword_pairs(&self) -> anyhow::Result<Vec<(String, PathBuf)>> {
        let mut selected_keywords = vec![];

        for built_in_keyword in self.keywords.iter().flatten() {
            // only load this method if using built in keywords
            // this will load it multiple times if multiple built in keywords are used
            // but the issue is that this file might not be included with the binary
            // so we don't want to prevent users who don't have the default keywords form running
            let built_in_keyword_paths = pv_keyword_paths();

            if let Some(keyword_path) = built_in_keyword_paths.get(built_in_keyword) {
                info!(
                    "Loading built-in keyword {:?} from {:?}",
                    built_in_keyword, keyword_path
                );
                selected_keywords.push((built_in_keyword.clone(), PathBuf::from(keyword_path)));
            } else {
                return Err(anyhow::anyhow!(
                    "Keyword {} not found in built-in keywords",
                    built_in_keyword
                ));
            }
        }

        for (keyword, keyword_path) in self.keyword_paths.iter().flatten() {
            info!("Loading keyword {:?} from {:?}", keyword, keyword_path);
            selected_keywords.push((keyword.clone(), keyword_path.clone()));
        }

        Ok(selected_keywords)
    }

    pub fn build_porcupine(&self) -> anyhow::Result<Porcupine> {
        let selected_keywords = self.keyword_pairs()?;
        let keyword_paths = selected_keywords
            .iter()
            .map(|(_, path)| path)
            .collect::<Vec<_>>();

        let mut porcupine_builder =
            PorcupineBuilder::new_with_keyword_paths(&self.access_key, &keyword_paths);
        if let Some(sensitivities) = &self.sensitivities {
            info!("Applying sensitivities {:?}", sensitivities);
            porcupine_builder.sensitivities(sensitivities);
        }
        if let Some(model_path) = &self.model_path {
            info!("Loading porcupine model from {:?}", model_path);
            porcupine_builder.model_path(model_path);
        }
        if let Some(porcupine_lib_path) = &self.porcupine_lib_path {
            info!("Loading porcupine library from {:?}", porcupine_lib_path);
            porcupine_builder.library_path(porcupine_lib_path);
        }
        let porcupine = porcupine_builder
            .init()
            .context("Failed to create Porcupine")?;
        Ok(porcupine)
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct WakeWordOpenaiConfig {
    pub api_key: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct WakewordZenohConfig {
    #[serde(default)]
    pub connect: Vec<zenoh_config::EndPoint>,
    #[serde(default)]
    pub listen: Vec<zenoh_config::EndPoint>,
    #[serde(default)]
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
            config.connect.endpoints.clone_from(&self.connect);
        }
        if !self.listen.is_empty() {
            config.listen.endpoints.clone_from(&self.listen);
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
