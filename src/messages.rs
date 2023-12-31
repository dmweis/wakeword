use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub struct AudioSample {
    pub data: Vec<i16>,
    pub wake_word: String,
    pub sample_rate: u32,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl AudioSample {
    pub fn write_to_wav_file(&self, output_path: &Path) -> anyhow::Result<()> {
        let wavspec = hound::WavSpec {
            channels: 1,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(output_path, wavspec)
            .context("Failed to open output audio file")?;
        for sample in &self.data {
            writer
                .write_sample(*sample)
                .context("Failed to write sample")?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PrivacyModeCommand {
    pub privacy_mode: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VoiceProbability {
    /// 0.0 to 1.0
    pub probability: f32,
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl VoiceProbability {
    pub fn new(probability: f32, timestamp: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            probability,
            timestamp,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WakeWordDetection {
    wake_word: String,
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl WakeWordDetection {
    pub fn new(wake_word: String, timestamp: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            wake_word,
            timestamp,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AudioTranscript {
    pub wake_word: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub transcript: String,
}
