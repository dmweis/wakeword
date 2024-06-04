//! This application listens to wakewords and sends recordings of audio with wakewords in them
//!
//! Based on examples for [porcupine](https://github.com/Picovoice/porcupine/blob/master/demo/rust/micdemo/src/main.rs)
//! and [cobra](https://github.com/Picovoice/cobra/blob/main/demo/rust/micdemo/src/main.rs)
//! By the excellent folks at https://picovoice.ai/

mod configuration;
mod listener;
mod logging;
mod messages;

use anyhow::Context;
use async_openai::{
    config::OpenAIConfig, types::CreateTranscriptionRequestArgs, Client as OpenAiClient,
};
use clap::Parser;
use listener::{AudioDetectorData, Listener};
use logging::{set_global_tracing_zenoh_subscriber, setup_tracing};
use pv_recorder::PvRecorderBuilder;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tempdir::TempDir;
use thiserror::Error;
use tracing::info;
use zenoh::{prelude::r#async::*, publication::Publisher};

use configuration::{get_configuration, AppConfig, PicovoiceConfig};
use messages::{AudioSample, AudioTranscript, PrivacyModeCommand, VoiceProbability};

const VOICE_TO_TEXT_TRANSCRIBE_MODEL: &str = "whisper-1";
const VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE: &str = "en";
const HUMAN_SPEECH_DETECTION_TIMEOUT: Duration = Duration::from_millis(1500);
const RECORDING_INITIAL_TIMEOUT: chrono::TimeDelta = chrono::TimeDelta::milliseconds(4000);
const HUMAN_SPEECH_DETECTION_PROBABILITY_THRESHOLD: f32 = 0.5;

/// Wake Word detection application using picovoice and zenoh
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
    setup_tracing(args.verbose, "wakeword");

    let app_config = get_configuration(&args.config)?;

    if args.show_audio_devices {
        show_audio_devices(&app_config.picovoice);
        return Ok(());
    }

    let zenoh_config = app_config.zenoh.get_zenoh_config()?;
    let zenoh_session = zenoh::open(zenoh_config)
        .res()
        .await
        .map_err(WakewordError::ZenohError)?
        .into_arc();

    set_global_tracing_zenoh_subscriber(zenoh_session.clone());

    let (audio_sample_sender, mut audio_sample_receiver) = tokio::sync::mpsc::channel(100);
    let (audio_detector_event_sender, audio_detector_event_receiver) =
        tokio::sync::mpsc::channel(100);

    let privacy_mode_flag = Arc::new(AtomicBool::new(false));

    // start listener
    let _listener_loop_join_handle = std::thread::spawn({
        let app_config = app_config.clone();
        let privacy_mode_flag = privacy_mode_flag.clone();

        move || loop {
            let mut listener = match Listener::new(
                app_config.picovoice.clone(),
                audio_sample_sender.clone(),
                audio_detector_event_sender.clone(),
                privacy_mode_flag.clone(),
            ) {
                Ok(listener) => listener,
                Err(err) => {
                    tracing::error!("Error while creating listener {:?}", err);
                    continue;
                }
            };
            match listener.listener_loop() {
                Ok(()) => (),
                Err(err) => {
                    tracing::error!("Error in listener loop: {:?}", err);
                }
            }
        }
    });

    let privacy_mode_subscriber = zenoh_session
        .declare_subscriber(app_config.app.get_privacy_mode_topic())
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    tokio::spawn(async move {
        loop {
            let res: anyhow::Result<()> = async {
                let msg = privacy_mode_subscriber.recv_async().await?;
                let msg: String = msg.value.try_into()?;
                let privacy_mode: PrivacyModeCommand = serde_json::from_str(&msg)?;
                privacy_mode_flag.store(privacy_mode.privacy_mode, Ordering::Relaxed);
                Ok(())
            }
            .await;
            if let Err(err) = res {
                tracing::error!("Error in privacy mode subscriber: {:?}", err);
            }
        }
    });

    tokio::spawn({
        let app_config = app_config.clone();
        let zenoh_session = zenoh_session.clone();
        async move {
            if let Err(err) = start_event_publisher(
                zenoh_session.clone(),
                app_config.app.clone(),
                audio_detector_event_receiver,
            )
            .await
            {
                tracing::error!("Error in event publisher: {:?}", err);
            }
        }
    });

    // start transcriber in current task
    let config = OpenAIConfig::new().with_api_key(&app_config.openai.api_key);
    let open_ai_client = OpenAiClient::with_config(config);

    let transcript_publisher = zenoh_session
        .declare_publisher(app_config.app.get_transcript_topic())
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    let wake_word_audio_recording_wav_publisher = zenoh_session
        .declare_publisher(app_config.app.get_wake_word_audio_recording_wav_topic())
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    while let Some(audio_sample) = audio_sample_receiver.recv().await {
        let system_prompt = app_config.app.system_prompts.get(&audio_sample.wake_word);

        let system_prompt = match system_prompt {
            Some(sys) => sys.as_str(),
            None => {
                tracing::warn!(
                    "No system prompt for wake word {:?}",
                    audio_sample.wake_word
                );
                // return empty string
                ""
            }
        };

        match transcribe(
            &audio_sample,
            system_prompt,
            &open_ai_client,
            &wake_word_audio_recording_wav_publisher,
        )
        .await
        {
            Ok(transcript) => {
                tracing::info!("Transcript {:?}", transcript);

                let transcript = AudioTranscript {
                    wake_word: audio_sample.wake_word,
                    timestamp: audio_sample.timestamp,
                    transcript,
                };
                let transcript_json = serde_json::to_string(&transcript)?;
                transcript_publisher
                    .put(transcript_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
            Err(err) => {
                tracing::error!("Error transcribing audio: {:?}", err);
            }
        }
    }

    Ok(())
}

async fn transcribe(
    audio_sample: &AudioSample,
    system_prompt: &str,
    open_ai_client: &OpenAiClient<OpenAIConfig>,
    audio_publisher: &Publisher<'_>,
) -> anyhow::Result<String> {
    let temp_dir = TempDir::new("audio_message_temp_dir")?;
    let temp_audio_file = temp_dir.path().join("recorded.wav");

    audio_sample
        .write_to_wav_file(&temp_audio_file)
        .context("Failed to write audio sample to wav file")?;

    let wav_file = tokio::fs::read(&temp_audio_file).await?;
    audio_publisher
        .put(wav_file)
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    tracing::info!("Wrote audio sample to {:?}", temp_audio_file);

    let request = CreateTranscriptionRequestArgs::default()
        .file(temp_audio_file)
        .model(VOICE_TO_TEXT_TRANSCRIBE_MODEL)
        .language(VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE)
        .prompt(system_prompt)
        .build()?;
    let response = open_ai_client.audio().transcribe(request).await?;
    Ok(response.text)
}

async fn start_event_publisher(
    zenoh_session: Arc<Session>,
    app_config: AppConfig,
    mut audio_detector_event_receiver: tokio::sync::mpsc::Receiver<AudioDetectorData>,
) -> anyhow::Result<()> {
    let voice_probability_publisher = zenoh_session
        .declare_publisher(app_config.get_voice_probability_topic())
        .priority(Priority::InteractiveLow)
        .congestion_control(CongestionControl::Drop)
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    let voice_probability_pretty_print_publisher = zenoh_session
        .declare_publisher(app_config.get_voice_probability_pretty_print_topic())
        .priority(Priority::InteractiveLow)
        .congestion_control(CongestionControl::Drop)
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    let recording_started_publisher = zenoh_session
        .declare_publisher(app_config.get_wake_word_recording_started_topic())
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    let wake_word_detection_publisher = zenoh_session
        .declare_publisher(app_config.get_wake_word_detected_topic())
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    let wake_word_detection_end_publisher = zenoh_session
        .declare_publisher(app_config.get_wake_word_recording_end_topic())
        .res()
        .await
        .map_err(WakewordError::ZenohError)?;

    while let Some(event) = audio_detector_event_receiver.recv().await {
        match event {
            AudioDetectorData::VoiceProbability(voice_probability) => {
                let voice_probability_json = serde_json::to_string(&voice_probability)?;
                voice_probability_publisher
                    .put(voice_probability_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;

                let pretty_print = voice_activity_to_text(&voice_probability);
                voice_probability_pretty_print_publisher
                    .put(pretty_print)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
            AudioDetectorData::WakeWordDetected(wake_word_detection) => {
                let wake_word_detection_json = serde_json::to_string(&wake_word_detection)?;
                wake_word_detection_publisher
                    .put(wake_word_detection_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
            AudioDetectorData::RecordingStarted(wake_word_detection) => {
                let wake_word_detection_json = serde_json::to_string(&wake_word_detection)?;
                recording_started_publisher
                    .put(wake_word_detection_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
            AudioDetectorData::RecordingEnd(wake_word_detection_end) => {
                let wake_word_detection_end_json = serde_json::to_string(&wake_word_detection_end)?;
                wake_word_detection_end_publisher
                    .put(wake_word_detection_end_json)
                    .res()
                    .await
                    .map_err(WakewordError::ZenohError)?;
            }
        }
    }

    Ok(())
}

fn show_audio_devices(config: &PicovoiceConfig) {
    info!("Listing audio devices");
    let mut recorder_builder = PvRecorderBuilder::default();
    if let Some(lib_path) = &config.recorder_lib_path {
        info!("Loading audio library from {:?}", lib_path);
        recorder_builder.library_path(lib_path);
    } else {
        info!("Using default audio library path");
    }

    let audio_devices = recorder_builder.get_available_devices();
    match audio_devices {
        Ok(audio_devices) => {
            for (idx, device) in audio_devices.iter().enumerate() {
                tracing::info!("index: {idx}, device name: {device:?}");
            }
        }
        Err(err) => panic!("Failed to get audio devices: {:?}", err),
    };
}

#[derive(Error, Debug)]
pub enum WakewordError {
    #[error("Zenoh error {0:?}")]
    ZenohError(#[from] zenoh::Error),
    #[error("Cobra error {0:?}")]
    CobraError(cobra::CobraError),
}

fn voice_activity_to_text(voice_probability: &VoiceProbability) -> String {
    let voice_percentage = voice_probability.probability * 100.0;
    let bar_length = ((voice_percentage / 10.0) * 3.0).ceil() as usize;
    let empty_length = 30 - bar_length;

    let detection_timed_out = voice_probability.time_since_last_human_ms
        > HUMAN_SPEECH_DETECTION_TIMEOUT.as_millis() as u64;
    let timeout_flare = if detection_timed_out { " " } else { "D" };
    let recording_flare = if voice_probability.currently_recording {
        "R"
    } else {
        " "
    };

    format!(
        "[{:3.0}]|{}{}| {} {}",
        voice_percentage,
        "█".repeat(bar_length),
        " ".repeat(empty_length),
        timeout_flare,
        recording_flare
    )
}
