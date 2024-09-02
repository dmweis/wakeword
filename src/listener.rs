use anyhow::Context;
use cobra::Cobra;
use porcupine::Porcupine;
use pv_recorder::{PvRecorder, PvRecorderBuilder};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::mpsc::error::TrySendError;
use tracing::info;

use crate::{
    configuration::PicovoiceConfig, respeaker::ReSpeakerCommander,
    wakeword_validation::AudioBuffer, WakewordError, HUMAN_SPEECH_DETECTION_PROBABILITY_THRESHOLD,
    HUMAN_SPEECH_DETECTION_TIMEOUT,
};
use crate::{
    messages::{
        AudioSample, DetectionEndReason, VoiceProbability, WakeWordDetection, WakeWordDetectionEnd,
    },
    RECORDING_INITIAL_TIMEOUT,
};

pub enum AudioDetectorData {
    VoiceProbability(VoiceProbability),
    RecordingStarted(WakeWordDetection),
    WakeWordDetected(WakeWordDetection),
    RecordingEnd(WakeWordDetectionEnd),
}

pub struct Listener {
    /// Recording audio from microphone
    recorder: PvRecorder,
    /// WakeWord detector
    porcupine: Porcupine,
    /// Human speech detector
    cobra: Cobra,
    /// WakeWord models
    selected_keywords: Vec<(String, PathBuf)>,
    /// Keyword used for dismiss events
    dismiss_keyword: Option<String>,
    /// Sending raw audio recordings
    audio_sample_sender: tokio::sync::mpsc::Sender<AudioSample>,
    /// Sending wakeword events
    audio_detector_data: tokio::sync::mpsc::Sender<AudioDetectorData>,
    /// Privacy mode
    /// When this flag is true do not listen to audio
    privacy_mode_flag: Arc<AtomicBool>,

    last_human_speech_detected: Instant,
    // currently held audio samples
    audio_buffer: Vec<i16>,

    /// These could be grouped into an object
    recording_status: RecordingStatus,

    /// ReSpeaker LED ring commander
    respeaker_commander: ReSpeakerCommander,

    /// wake word buffer
    wake_word_buffer: AudioBuffer,
}

impl Listener {
    pub fn new(
        config: PicovoiceConfig,
        audio_sample_sender: tokio::sync::mpsc::Sender<AudioSample>,
        audio_detector_data: tokio::sync::mpsc::Sender<AudioDetectorData>,
        privacy_mode_flag: Arc<AtomicBool>,
        respeaker_commander: ReSpeakerCommander,
    ) -> anyhow::Result<Self> {
        let selected_keywords = config.keyword_pairs()?;

        info!("Configuring porcupine");
        let porcupine = config.build_porcupine()?;

        info!("Configuring cobra");
        let cobra = if let Some(cobra_lib_path) = config.cobra_lib_path {
            Cobra::new_with_library(config.access_key, cobra_lib_path)
                .map_err(WakewordError::CobraError)
                .context("Failed to create Cobra")?
        } else {
            Cobra::new(config.access_key)
                .map_err(WakewordError::CobraError)
                .context("Failed to create Cobra")?
        };

        info!("Configuring recorder");
        let mut recorder_builder = PvRecorderBuilder::new(porcupine.frame_length() as i32);
        recorder_builder.device_index(config.audio_device_index.unwrap_or(-1));

        if let Some(lib_path) = config.recorder_lib_path {
            recorder_builder.library_path(&lib_path);
        }

        let recorder = recorder_builder
            .init()
            .context("Failed to initialize pvrecorder")?;

        info!("Starting recorder");
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
            privacy_mode_flag,
            audio_buffer: vec![],
            // doesn't matter is we starting it to now
            last_human_speech_detected: Instant::now(),
            recording_status: RecordingStatus::NotActive,
            respeaker_commander,
            wake_word_buffer: AudioBuffer::default(),
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

    pub fn listener_loop(&mut self) -> anyhow::Result<()> {
        tracing::info!("Listening for wake words...");
        loop {
            let ts_now = chrono::Utc::now();
            let instant_now = Instant::now();
            let audio_frame = self.recorder.read().context("Failed to read audio frame")?;

            self.wake_word_buffer.insert(instant_now, &audio_frame);

            // skip in privacy mode
            if self.check_privacy_mode()? {
                self.respeaker_commander.off();
                continue;
            }

            // wake word detection
            let detected_wake_word = self.detect_wake_word(&audio_frame)?;
            if let Some(detected_wake_word) = detected_wake_word {
                // detect dismiss keywords
                if self.check_dismiss_keyword(&detected_wake_word, ts_now)? {
                    self.respeaker_commander.off();
                    continue;
                }

                // don't update wake word if we're already recording
                if !self.recording_status.active() {
                    self.respeaker_commander.listen();
                    let active_recording = ActiveRecording::new(ts_now, detected_wake_word.clone());

                    self.recording_status = RecordingStatus::Active(active_recording);

                    // only send event when we start recording
                    let event = AudioDetectorData::RecordingStarted(WakeWordDetection::new(
                        detected_wake_word.clone(),
                        ts_now,
                    ));
                    self.send_event(event)?;
                }

                // also bump this to prevent going to sleep if human detection is slow
                self.last_human_speech_detected = instant_now;

                tracing::info!("Detected {:?}", detected_wake_word);

                let event = AudioDetectorData::WakeWordDetected(WakeWordDetection::new(
                    detected_wake_word.clone(),
                    ts_now,
                ));
                self.send_event(event)?;
            }

            self.check_human_voice_probability(&audio_frame, ts_now)?;

            // Add sample to buffer
            if self.recording_status.active() {
                self.audio_buffer.extend_from_slice(&audio_frame);
            }

            // Check timeout
            let mut should_be_recording =
                self.last_human_speech_detected.elapsed() < HUMAN_SPEECH_DETECTION_TIMEOUT;

            if let Some(recording_initial_status) = self
                .recording_status
                .is_in_recording_initial_timeout(ts_now)
            {
                // if we are in initial 3 seconds do not time out
                if recording_initial_status {
                    should_be_recording = true;
                }
            }

            if self.recording_status.active() && !should_be_recording {
                // stop recording
                self.finish_recording()?;
                self.respeaker_commander.off();
            }
        }

        // TODO(David): Is this object RAII?
        // Maybe we should have some nicer termination detection
        //recorder.stop().context("Failed to stop audio recording")?;
    }

    fn check_privacy_mode(&mut self) -> anyhow::Result<bool> {
        // skip in privacy mode
        if self.privacy_mode_flag.load(Ordering::Relaxed) {
            // cancel recording if ongoing
            if let RecordingStatus::Active(recording_status) = self.recording_status.stop() {
                info!("Canceling recording because of privacy mode");
                let event = AudioDetectorData::RecordingEnd(WakeWordDetectionEnd::new(
                    recording_status.recording_triggering_wake_word,
                    recording_status.recording_triggering_timestamp,
                    DetectionEndReason::PrivacyModeActivated,
                ));
                self.send_event(event)?;
            }
            // clear buffer after
            self.audio_buffer.clear();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn check_dismiss_keyword(
        &mut self,
        detected_wake_word: &str,
        ts_now: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<bool> {
        if self
            .dismiss_keyword
            .as_ref()
            .is_some_and(|dismiss_keyword| dismiss_keyword == detected_wake_word)
        {
            info!("Dismiss keyword detected {:?}", self.dismiss_keyword);
            // cancel recording if ongoing
            if let RecordingStatus::Active(recording_status) = self.recording_status.stop() {
                info!("Canceling recording because of dismiss keyword");
                let event = AudioDetectorData::RecordingEnd(WakeWordDetectionEnd::new(
                    recording_status.recording_triggering_wake_word,
                    recording_status.recording_triggering_timestamp,
                    DetectionEndReason::Dismissed,
                ));
                self.send_event(event)?;
            }
            // clear after recording
            self.audio_buffer.clear();
            // send dismiss keyword detection
            let event = AudioDetectorData::WakeWordDetected(WakeWordDetection::new(
                detected_wake_word.to_owned(),
                ts_now,
            ));
            self.send_event(event)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn check_human_voice_probability(
        &mut self,
        audio_frame: &[i16],
        ts_now: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        // voice probability
        let voice_probability = self
            .cobra
            .process(audio_frame)
            .map_err(WakewordError::CobraError)
            .context("Cobra processing failed")?;

        let time_since_last_human_speech_detected_ms =
            self.last_human_speech_detected.elapsed().as_millis();

        // send event
        let event = AudioDetectorData::VoiceProbability(VoiceProbability::new(
            voice_probability,
            ts_now,
            time_since_last_human_speech_detected_ms as u64,
            self.recording_status.active(),
        ));
        self.send_event(event)?;

        // Check human speech presence
        let human_speech_detected =
            voice_probability > HUMAN_SPEECH_DETECTION_PROBABILITY_THRESHOLD;
        if human_speech_detected {
            self.last_human_speech_detected = Instant::now();
        }
        Ok(())
    }

    /// Finish recording and send data
    fn finish_recording(&mut self) -> anyhow::Result<()> {
        if let RecordingStatus::Active(recording_status) = self.recording_status.stop() {
            let audio_sample = AudioSample {
                data: self.audio_buffer.clone(),
                wake_word: recording_status.recording_triggering_wake_word.clone(),
                sample_rate: self.porcupine.sample_rate(),
                timestamp: recording_status.recording_triggering_timestamp,
            };
            // erase audio buffer after sending
            self.audio_buffer.clear();

            tracing::info!("Sending audio sample");
            if let Err(TrySendError::Closed(_)) = self.audio_sample_sender.try_send(audio_sample) {
                anyhow::bail!("Audio sample channel closed");
            }

            let event = AudioDetectorData::RecordingEnd(WakeWordDetectionEnd::new(
                recording_status.recording_triggering_wake_word.clone(),
                recording_status.recording_triggering_timestamp,
                DetectionEndReason::Finished,
            ));
            self.send_event(event)?;
        }
        Ok(())
    }
}

pub enum RecordingStatus {
    NotActive,
    Active(ActiveRecording),
}

impl RecordingStatus {
    fn active(&self) -> bool {
        matches!(self, RecordingStatus::Active(_))
    }

    fn stop(&mut self) -> RecordingStatus {
        let mut tmp = RecordingStatus::NotActive;
        std::mem::swap(self, &mut tmp);
        tmp
    }

    fn is_in_recording_initial_timeout(
        &self,
        ts_now: chrono::DateTime<chrono::Utc>,
    ) -> Option<bool> {
        match self {
            RecordingStatus::Active(status) => {
                let delta = status
                    .recording_triggering_timestamp
                    .signed_duration_since(ts_now);
                // timeout if delta bigger
                let timedout = delta > RECORDING_INITIAL_TIMEOUT;
                Some(timedout)
            }
            RecordingStatus::NotActive => None,
        }
    }
}

pub struct ActiveRecording {
    recording_triggering_timestamp: chrono::DateTime<chrono::Utc>,
    recording_triggering_wake_word: String,
}

impl ActiveRecording {
    fn new(
        recording_triggering_timestamp: chrono::DateTime<chrono::Utc>,
        recording_triggering_wake_word: String,
    ) -> Self {
        Self {
            recording_triggering_timestamp,
            recording_triggering_wake_word,
        }
    }
}
