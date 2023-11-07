//! This application listens to wakewords and sends recordings of audio with wakewords in them
//!
//! Based on examples for [porcupine](https://github.com/Picovoice/porcupine/blob/master/demo/rust/micdemo/src/main.rs)
//! and [cobra](https://github.com/Picovoice/cobra/blob/main/demo/rust/micdemo/src/main.rs)
//! By the excellent folks at https://picovoice.ai/

mod configuration;

use anyhow::Context;
use async_openai::{
    config::OpenAIConfig, types::CreateTranscriptionRequestArgs, Client as OpenAiClient,
};
use clap::Parser;
use cobra::Cobra;
use configuration::{get_configuration, PicovoiceConfig};
use porcupine::{BuiltinKeywords, PorcupineBuilder};
use pv_recorder::PvRecorderBuilder;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use tempdir::TempDir;
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;
use zenoh::prelude::r#async::*;

const VOICE_TO_TEXT_TRANSCRIBE_MODEL: &str = "whisper-1";
const VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE: &str = "en";

fn print_voice_activity(voice_probability: f32) {
    let voice_percentage = voice_probability * 100.0;
    let bar_length = ((voice_percentage / 10.0) * 3.0).ceil() as usize;
    let empty_length = 30 - bar_length;
    tracing::info!(
        "[{:3.0}]|{}{}|",
        voice_percentage,
        "█".repeat(bar_length),
        " ".repeat(empty_length)
    );
}

struct AudioSample {
    data: Vec<i16>,
    wake_word: String,
    sample_rate: u32,
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl AudioSample {
    fn write_to_wav_file(&self, output_path: &Path) -> anyhow::Result<()> {
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
struct VoiceProbability {
    probability: f32,
    timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Debug)]
struct WakeWordDetection {
    wake_word: String,
    timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, Debug)]
struct AudioTranscript {
    wake_word: String,
    timestamp: chrono::DateTime<chrono::Utc>,
    transcript: String,
}

enum AudioDetectorData {
    VoiceProbability {
        /// 0.0 to 1.0
        probability: f32,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    WakeWordDetection {
        wake_word: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
}

fn listener_loop(
    config: PicovoiceConfig,
    keywords_or_paths: KeywordsOrPaths,
    audio_sample_sender: tokio::sync::mpsc::Sender<AudioSample>,
    audio_detector_data: tokio::sync::mpsc::Sender<AudioDetectorData>,
) -> anyhow::Result<()> {
    let mut porcupine_builder = match keywords_or_paths {
        KeywordsOrPaths::Keywords(ref keywords) => {
            PorcupineBuilder::new_with_keywords(&config.access_key, keywords)
        }
        KeywordsOrPaths::KeywordPaths(ref keyword_paths) => {
            PorcupineBuilder::new_with_keyword_paths(&config.access_key, keyword_paths)
        }
    };

    let cobra = Cobra::new(config.access_key)
        .map_err(WakewordError::CobraError)
        .context("Failed to create Cobra")?;

    if let Some(sensitivities) = config.sensitivities {
        porcupine_builder.sensitivities(&sensitivities);
    }

    if let Some(model_path) = config.model_path {
        porcupine_builder.model_path(model_path);
    }

    let porcupine = porcupine_builder
        .init()
        .context("Failed to create Porcupine")?;

    let recorder = PvRecorderBuilder::new(porcupine.frame_length() as i32)
        .device_index(config.audio_device_index.unwrap_or(-1))
        .init()
        .context("Failed to initialize pvrecorder")?;

    recorder
        .start()
        .context("Failed to start audio recording")?;

    tracing::info!("Listening for wake words...");

    let mut audio_buffer = Vec::new();
    loop {
        let frame = recorder.read().context("Failed to read audio frame")?;

        let timestamp = chrono::Utc::now();

        let keyword_index = porcupine
            .process(&frame)
            .context("Failed to process audio frame")?;
        if keyword_index >= 0 {
            let wake_word = keywords_or_paths.get(keyword_index as usize);
            tracing::info!("Detected {}", wake_word);

            let event = AudioDetectorData::WakeWordDetection {
                wake_word,
                timestamp,
            };
            if let Err(TrySendError::Closed(_)) = audio_detector_data.try_send(event) {
                anyhow::bail!("Audio detector channel closed");
            }
        }

        let voice_probability = cobra
            .process(&frame)
            .map_err(WakewordError::CobraError)
            .context("Cobra processing failed")?;

        let event = AudioDetectorData::VoiceProbability {
            probability: voice_probability,
            timestamp,
        };
        if let Err(TrySendError::Closed(_)) = audio_detector_data.try_send(event) {
            anyhow::bail!("Audio detector channel closed");
        }

        print_voice_activity(voice_probability);

        let sample_rate = porcupine.sample_rate();

        if false {
            audio_buffer.extend_from_slice(&frame);
        }
    }

    recorder.stop().context("Failed to stop audio recording")?;
}

#[derive(Clone)]
enum KeywordsOrPaths {
    Keywords(Vec<BuiltinKeywords>),
    KeywordPaths(Vec<PathBuf>),
}

impl KeywordsOrPaths {
    fn get(&self, index: usize) -> String {
        match self {
            Self::Keywords(keywords) => keywords[index].to_str().to_string(),
            Self::KeywordPaths(keyword_paths) => keyword_paths[index]
                .clone()
                .into_os_string()
                .into_string()
                .unwrap(),
        }
    }
}

/// Picovoice Porcupine Rust Mic Demo
#[derive(Parser)]
#[command(author, version)]
struct Args {
    /// Display all audio devices
    #[arg(long)]
    show_audio_devices: bool,

    /// Path to config
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Sets the level of verbosity
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Args = Args::parse();
    setup_tracing(args.verbose);
    let app_config = get_configuration(&args.config)?;

    if args.show_audio_devices {
        show_audio_devices();
        return Ok(());
    }

    let keywords_or_paths: KeywordsOrPaths = {
        if let Some(keyword_paths) = &app_config.picovoice.keyword_paths {
            KeywordsOrPaths::KeywordPaths(keyword_paths.clone())
        } else if let Some(keywords) = &app_config.picovoice.keywords {
            KeywordsOrPaths::Keywords(
                keywords
                    .iter()
                    .flat_map(|keyword| match BuiltinKeywords::from_str(keyword) {
                        Ok(keyword) => vec![keyword],
                        Err(_) => vec![],
                    })
                    .collect(),
            )
        } else {
            anyhow::bail!("Keywords or keyword paths must be specified");
        }
    };

    let zenoh_config = app_config.zenoh.get_zenoh_config()?;
    let zenoh_session = zenoh::open(zenoh_config)
        .res()
        .await
        .map_err(WakewordError::ZenohError)?
        .into_arc();

    let (audio_sample_sender, mut audio_sample_receiver) = tokio::sync::mpsc::channel(100);
    let (audio_detector_event_sender, audio_detector_event_receiver) =
        tokio::sync::mpsc::channel(100);

    // start listener
    let _listener_loop_join_handle = std::thread::spawn(move || loop {
        match listener_loop(
            app_config.picovoice.clone(),
            keywords_or_paths.clone(),
            audio_sample_sender.clone(),
            audio_detector_event_sender.clone(),
        ) {
            Ok(_) => (),
            Err(err) => {
                tracing::error!("Error in listener loop: {:?}", err);
            }
        }
    });

    tokio::spawn({
        let zenoh_session = zenoh_session.clone();
        async move {
            if let Err(err) =
                start_event_publisher(zenoh_session.clone(), audio_detector_event_receiver).await
            {
                tracing::error!("Error in event publisher: {:?}", err);
            }
        }
    });

    // start transcriber in current task
    let config = OpenAIConfig::new().with_api_key(&app_config.openai.api_key);
    let open_ai_client = OpenAiClient::with_config(config);

    let transcript_publisher = zenoh_session
        .declare_publisher("wakeword/event/transcript")
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    while let Some(audio_sample) = audio_sample_receiver.recv().await {
        let temp_dir = TempDir::new("audio_message_temp_dir")?;
        let temp_audio_file = temp_dir.path().join("recorded.wav");

        audio_sample
            .write_to_wav_file(&temp_audio_file)
            .context("Failed to write audio sample to wav file")?;

        tracing::info!("Wrote audio sample to {:?}", temp_audio_file);

        let request = CreateTranscriptionRequestArgs::default()
            .file(temp_audio_file)
            .model(VOICE_TO_TEXT_TRANSCRIBE_MODEL)
            .language(VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE)
            .prompt("")
            .build()?;
        let response = open_ai_client.audio().transcribe(request).await?;
        tracing::info!("Transcript {}", response.text);

        let transcript = AudioTranscript {
            wake_word: audio_sample.wake_word,
            timestamp: audio_sample.timestamp,
            transcript: response.text,
        };
        let transcript_json = serde_json::to_string(&transcript)?;
        transcript_publisher
            .put(transcript_json)
            .res()
            .await
            .map_err(WakewordError::ZenohError)?;
    }

    Ok(())
}

async fn start_event_publisher(
    zenoh_session: Arc<Session>,
    mut audio_detector_event_receiver: tokio::sync::mpsc::Receiver<AudioDetectorData>,
) -> anyhow::Result<()> {
    let voice_probability_publisher = zenoh_session
        .declare_publisher("wakeword/event/voice_probability")
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    let wake_word_detection_publisher = zenoh_session
        .declare_publisher("wakeword/event/wake_word_detection")
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    while let Some(event) = audio_detector_event_receiver.recv().await {
        match event {
            AudioDetectorData::VoiceProbability {
                probability,
                timestamp,
            } => {
                let voice_probability = VoiceProbability {
                    probability,
                    timestamp,
                };
                let voice_probability_json = serde_json::to_string(&voice_probability)?;
                voice_probability_publisher
                    .put(voice_probability_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
            AudioDetectorData::WakeWordDetection {
                wake_word,
                timestamp,
            } => {
                let wake_word_detection = WakeWordDetection {
                    wake_word,
                    timestamp,
                };
                let wake_word_detection_json = serde_json::to_string(&wake_word_detection)?;
                wake_word_detection_publisher
                    .put(wake_word_detection_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
        }
    }

    Ok(())
}

fn show_audio_devices() {
    let audio_devices = PvRecorderBuilder::default().get_available_devices();
    match audio_devices {
        Ok(audio_devices) => {
            for (idx, device) in audio_devices.iter().enumerate() {
                tracing::info!("index: {idx}, device name: {device:?}");
            }
        }
        Err(err) => panic!("Failed to get audio devices: {}", err),
    };
}

pub fn setup_tracing(verbosity_level: u8) {
    let filter = match verbosity_level {
        0 => tracing::level_filters::LevelFilter::INFO,
        1 => tracing::level_filters::LevelFilter::INFO,
        2 => tracing::level_filters::LevelFilter::DEBUG,
        3 => tracing::level_filters::LevelFilter::TRACE,
        _ => tracing::level_filters::LevelFilter::TRACE,
    };

    tracing_subscriber::fmt()
        .with_thread_names(true)
        .with_max_level(filter)
        .init();
}

#[derive(Error, Debug)]
pub enum WakewordError {
    #[error("Zenoh error {0:?}")]
    ZenohError(#[from] zenoh::Error),
    #[error("Cobra error {0:?}")]
    CobraError(cobra::CobraError),
}
