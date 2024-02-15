use anyhow::Context;
use cobra::Cobra;
use porcupine::Porcupine;
use pv_recorder::{PvRecorder, PvRecorderBuilder};
use std::{path::PathBuf, sync::atomic::Ordering, time::Instant};
use tokio::sync::mpsc::error::TrySendError;
use tracing::info;

use crate::{
    configuration::PicovoiceConfig, WakewordError, HUMAN_SPEECH_DETECTION_PROBABILITY_THRESHOLD,
    HUMAN_SPEECH_DETECTION_TIMEOUT,
};
use crate::{
    messages::{AudioSample, VoiceProbability, WakeWordDetection},
    PRIVACY_MODE,
};

pub enum AudioDetectorData {
    VoiceProbability(VoiceProbability),
    RecordingStarted(WakeWordDetection),
    WakeWordDetected(WakeWordDetection),
    RecordingEnd(WakeWordDetection),
}

pub struct Listener {
    recorder: PvRecorder,
    porcupine: Porcupine,
    cobra: Cobra,
    selected_keywords: Vec<(String, PathBuf)>,
    dismiss_keyword: Option<String>,
    audio_sample_sender: tokio::sync::mpsc::Sender<AudioSample>,
    audio_detector_data: tokio::sync::mpsc::Sender<AudioDetectorData>,
}

impl Listener {
    pub fn new(
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
            dismiss_keyword: config.dismiss_keyword.clone(),
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

    pub fn listener_loop(&self) -> anyhow::Result<()> {
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
                // cancel recording if ongoing
                if currently_recording {
                    info!("Canceling recording because of privacy mode");
                    let event = AudioDetectorData::RecordingEnd(WakeWordDetection::new(
                        recording_triggering_wake_word.clone(),
                        recording_triggering_timestamp,
                    ));
                    self.send_event(event)?;
                }

                // stop any recording
                currently_recording = false;
                audio_buffer.clear();
                continue;
            }

            // wake word detection
            let detected_wake_word = self.detect_wake_word(&audio_frame)?;
            if let Some(detected_wake_word) = detected_wake_word {
                // detect dismiss keywords
                if Some(detected_wake_word.clone()) == self.dismiss_keyword {
                    info!("Dismiss keyword detected {:?}", self.dismiss_keyword);
                    // cancel recording if ongoing
                    if currently_recording {
                        info!("Canceling recording because of dismiss keyword");
                        let event = AudioDetectorData::RecordingEnd(WakeWordDetection::new(
                            recording_triggering_wake_word.clone(),
                            recording_triggering_timestamp,
                        ));
                        self.send_event(event)?;
                    }
                    // stop any recording
                    currently_recording = false;
                    audio_buffer.clear();
                    // send dismiss keyword detection
                    let event = AudioDetectorData::WakeWordDetected(WakeWordDetection::new(
                        detected_wake_word.clone(),
                        ts_now,
                    ));
                    self.send_event(event)?;
                    continue;
                }

                // don't update wake word if we're already recording
                if !currently_recording {
                    recording_triggering_timestamp = ts_now;
                    recording_triggering_wake_word = detected_wake_word.clone();
                    // only send event when we start recording
                    let event = AudioDetectorData::RecordingStarted(WakeWordDetection::new(
                        recording_triggering_wake_word.clone(),
                        ts_now,
                    ));
                    self.send_event(event)?;
                }
                // flip to true if we detect a wake word
                currently_recording = true;

                // also bump this to prevent going to sleep if human detection is slow
                last_human_speech_detected = Instant::now();

                tracing::info!("Detected {:?}", detected_wake_word);

                let event = AudioDetectorData::WakeWordDetected(WakeWordDetection::new(
                    detected_wake_word.clone(),
                    ts_now,
                ));
                self.send_event(event)?;
            }

            // voice probability
            let voice_probability = self
                .cobra
                .process(&audio_frame)
                .map_err(WakewordError::CobraError)
                .context("Cobra processing failed")?;

            // send event
            let event = AudioDetectorData::VoiceProbability(VoiceProbability::new(
                voice_probability,
                ts_now,
            ));
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

                let event = AudioDetectorData::RecordingEnd(WakeWordDetection::new(
                    recording_triggering_wake_word.clone(),
                    recording_triggering_timestamp,
                ));
                self.send_event(event)?;
            }
        }

        // TODO(David): Is this object RAII?
        // Maybe we should have some nicer termination detection
        //recorder.stop().context("Failed to stop audio recording")?;
    }
}