//! This application listens to wakewords and sends recordings of audio with wakewords in them
//!
//! Based on examples for [porcupine](https://github.com/Picovoice/porcupine/blob/master/demo/rust/micdemo/src/main.rs)
//! and [cobra](https://github.com/Picovoice/cobra/blob/main/demo/rust/micdemo/src/main.rs)
//! By the excellent folks at https://picovoice.ai/

mod configuration;
mod messages;

use anyhow::Context;
use async_openai::{
    config::OpenAIConfig, types::CreateTranscriptionRequestArgs, Client as OpenAiClient,
};
use clap::Parser;
use cobra::Cobra;
use porcupine::Porcupine;
use pv_recorder::{PvRecorder, PvRecorderBuilder};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tempdir::TempDir;
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;
use zenoh::prelude::r#async::*;

use configuration::{get_configuration, AppConfig, PicovoiceConfig};
use messages::{
    AudioSample, AudioTranscript, PrivacyModeCommand, VoiceProbability, WakeWordDetection,
    WakeWordDetectionEnd,
};

const VOICE_TO_TEXT_TRANSCRIBE_MODEL: &str = "whisper-1";
const VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE: &str = "en";
const HUMAN_SPEECH_DETECTION_TIMEOUT: Duration = Duration::from_secs(3);
const HUMAN_SPEECH_DETECTION_PROBABILITY_THRESHOLD: f32 = 0.5;

static PRIVACY_MODE: AtomicBool = AtomicBool::new(false);

enum AudioDetectorData {
    VoiceProbability(VoiceProbability),
    RecordingStarted(WakeWordDetection),
    WakeWordDetected(WakeWordDetection),
    RecordingEnd(WakeWordDetectionEnd),
}

struct Listener {
    recorder: PvRecorder,
    porcupine: Porcupine,
    cobra: Cobra,
    selected_keywords: Vec<(String, PathBuf)>,
    audio_sample_sender: tokio::sync::mpsc::Sender<AudioSample>,
    audio_detector_data: tokio::sync::mpsc::Sender<AudioDetectorData>,
}

impl Listener {
    fn new(
        config: PicovoiceConfig,
        audio_sample_sender: tokio::sync::mpsc::Sender<AudioSample>,
        audio_detector_data: tokio::sync::mpsc::Sender<AudioDetectorData>,
    ) -> anyhow::Result<Self> {
        let selected_keywords = config.keyword_pairs()?;

        let porcupine = config.build_porcupine()?;

        let cobra = if let Some(cobra_lib_path) = config.cobra_lib_path {
            Cobra::new_with_library(config.access_key, cobra_lib_path)
                .map_err(WakewordError::CobraError)
                .context("Failed to create Cobra")?
        } else {
            Cobra::new(config.access_key)
                .map_err(WakewordError::CobraError)
                .context("Failed to create Cobra")?
        };

        let mut recorder_builder = PvRecorderBuilder::new(porcupine.frame_length() as i32);
        recorder_builder.device_index(config.audio_device_index.unwrap_or(-1));

        if let Some(lib_path) = config.recorder_lib_path {
            recorder_builder.library_path(&lib_path);
        }

        let recorder = recorder_builder
            .init()
            .context("Failed to initialize pvrecorder")?;

        recorder
            .start()
            .context("Failed to start audio recording")?;

        let listener = Self {
            recorder,
            porcupine,
            cobra,
            selected_keywords,
            audio_sample_sender,
            audio_detector_data,
        };

        Ok(listener)
    }

    /// detect if wake word is present in sample
    fn detect_wake_word(&self, audio_frame: &[i16]) -> anyhow::Result<Option<String>> {
        let keyword_index = self
            .porcupine
            .process(audio_frame)
            .context("Failed to process audio frame")?;

        let wake_word_detected = keyword_index >= 0;
        if wake_word_detected {
            let wake_word = self
                .selected_keywords
                .get(keyword_index as usize)
                .context("Keyword index unknown")?
                .0
                .clone();
            Ok(Some(wake_word))
        } else {
            Ok(None)
        }
    }

    fn send_event(&self, event: AudioDetectorData) -> anyhow::Result<()> {
        if let Err(TrySendError::Closed(_)) = self.audio_detector_data.try_send(event) {
            anyhow::bail!("Audio detector channel closed");
        }
        Ok(())
    }

    fn listener_loop(&self) -> anyhow::Result<()> {
        tracing::info!("Listening for wake words...");

        let mut audio_buffer = Vec::new();

        let mut recording_triggering_timestamp = chrono::Utc::now();
        let mut recording_triggering_wake_word = String::new();
        let mut currently_recording = false;

        let mut last_human_speech_detected = Instant::now();
        loop {
            let ts_now = chrono::Utc::now();
            let audio_frame = self.recorder.read().context("Failed to read audio frame")?;

            // skip in privacy mode
            if PRIVACY_MODE.load(Ordering::Relaxed) {
                // stop any recording
                currently_recording = false;
                audio_buffer.clear();
                continue;
            }

            // wake word detection
            let detected_wake_word = self.detect_wake_word(&audio_frame)?;
            if let Some(detected_wake_word) = detected_wake_word {
                // don't update wake word if we're already recording
                if !currently_recording {
                    recording_triggering_timestamp = ts_now;
                    recording_triggering_wake_word = detected_wake_word.clone();
                    // only send event when we start recording
                    let event = AudioDetectorData::RecordingStarted(WakeWordDetection {
                        wake_word: recording_triggering_wake_word.clone(),
                        timestamp: ts_now,
                    });
                    self.send_event(event)?;
                }
                // flip to true if we detect a wake word
                currently_recording = true;

                // also bump this to prevent going to sleep if human detection is slow
                last_human_speech_detected = Instant::now();

                tracing::info!("Detected {:?}", detected_wake_word);

                let event = AudioDetectorData::WakeWordDetected(WakeWordDetection {
                    wake_word: detected_wake_word.clone(),
                    timestamp: ts_now,
                });
                self.send_event(event)?;
            }

            // voice probability
            let voice_probability = self
                .cobra
                .process(&audio_frame)
                .map_err(WakewordError::CobraError)
                .context("Cobra processing failed")?;

            // send event
            let event = AudioDetectorData::VoiceProbability(VoiceProbability {
                probability: voice_probability,
                timestamp: ts_now,
            });
            self.send_event(event)?;

            // Add sample to buffer
            if currently_recording {
                audio_buffer.extend_from_slice(&audio_frame);
            }

            // Check human speech presence
            let human_speech_detected =
                voice_probability > HUMAN_SPEECH_DETECTION_PROBABILITY_THRESHOLD;
            if human_speech_detected {
                last_human_speech_detected = Instant::now();
            }

            let should_be_recording =
                last_human_speech_detected.elapsed() < HUMAN_SPEECH_DETECTION_TIMEOUT;

            if currently_recording && !should_be_recording {
                // stop recording
                currently_recording = false;
                let audio_sample = AudioSample {
                    data: audio_buffer.clone(),
                    wake_word: recording_triggering_wake_word.clone(),
                    sample_rate: self.porcupine.sample_rate(),
                    timestamp: recording_triggering_timestamp,
                };
                // erase audio buffer after sending
                audio_buffer.clear();

                tracing::info!("Sending audio sample");
                if let Err(TrySendError::Closed(_)) =
                    self.audio_sample_sender.try_send(audio_sample)
                {
                    anyhow::bail!("Audio sample channel closed");
                }

                let event = AudioDetectorData::RecordingEnd(WakeWordDetectionEnd {
                    wake_word: recording_triggering_wake_word.clone(),
                    timestamp: recording_triggering_timestamp,
                });
                self.send_event(event)?;
            }
        }

        // TODO(David): Is this object RAII?
        // Maybe we should have some nicer termination detection
        //recorder.stop().context("Failed to stop audio recording")?;
    }
}

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
    setup_tracing(args.verbose);
    let app_config = get_configuration(&args.config)?;

    if args.show_audio_devices {
        show_audio_devices();
        return Ok(());
    }

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
    let _listener_loop_join_handle = std::thread::spawn({
        let app_config = app_config.clone();

        let listener = Listener::new(
            app_config.picovoice.clone(),
            audio_sample_sender.clone(),
            audio_detector_event_sender.clone(),
        )?;

        move || loop {
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
                PRIVACY_MODE.store(privacy_mode.privacy_mode, Ordering::Relaxed);
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

        match transcribe(&audio_sample, system_prompt, &open_ai_client).await {
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
) -> anyhow::Result<String> {
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

                let pretty_print = voice_activity_to_text(voice_probability.probability);
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

fn show_audio_devices() {
    let audio_devices = PvRecorderBuilder::default().get_available_devices();
    match audio_devices {
        Ok(audio_devices) => {
            for (idx, device) in audio_devices.iter().enumerate() {
                tracing::info!("index: {idx}, device name: {device:?}");
            }
        }
        Err(err) => panic!("Failed to get audio devices: {:?}", err),
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

fn voice_activity_to_text(voice_probability: f32) -> String {
    let voice_percentage = voice_probability * 100.0;
    let bar_length = ((voice_percentage / 10.0) * 3.0).ceil() as usize;
    let empty_length = 30 - bar_length;
    format!(
        "[{:3.0}]|{}{}|",
        voice_percentage,
        "█".repeat(bar_length),
        " ".repeat(empty_length)
    )
}
