use std::{
    collections::VecDeque,
    io::Cursor,
    time::{Duration, Instant},
};

use anyhow::Context;
use async_openai::{
    config::OpenAIConfig,
    types::{AudioInput, CreateTranscriptionRequestArgs},
    Client,
};
use tracing::{error, info};

use crate::{VOICE_TO_TEXT_TRANSCRIBE_MODEL, VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE};

const AUDIO_SAMPLE_RETENTION_PERIOD: Duration = Duration::from_secs(5);

pub struct WakeWordValidator {
    buffer: AudioBuffer,
    sample_rate: u32,
    open_ai_client: Client<OpenAIConfig>,
}

impl WakeWordValidator {
    pub fn new(open_ai_client: Client<OpenAIConfig>, sample_rate: u32) -> Self {
        Self {
            buffer: Default::default(),
            sample_rate,
            open_ai_client,
        }
    }

    pub fn insert(&mut self, now: Instant, sample: &[i16]) {
        self.buffer.insert(now, sample);
    }

    pub fn contains_wakeword(
        &self,
        wakeword: &str,
    ) -> anyhow::Result<tokio::sync::oneshot::Receiver<bool>> {
        let wav_file = self.buffer.contents_to_wav(self.sample_rate)?;
        let audio_input = AudioInput::from_vec_u8(String::from("recorded.wav"), wav_file);

        let request = CreateTranscriptionRequestArgs::default()
            .file(audio_input)
            .model(VOICE_TO_TEXT_TRANSCRIBE_MODEL)
            .language(VOICE_TO_TEXT_TRANSCRIBE_MODEL_ENGLISH_LANGUAGE)
            .prompt(format!(
                "This sample might contain the wake word {}",
                wakeword
            ))
            .build()?;

        // execute future
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn({
            let open_ai_client = self.open_ai_client.clone();
            let wakeword = wakeword.to_owned();
            async move {
                info!("starting validation for wakeword {:?}", &wakeword);
                match open_ai_client.audio().transcribe(request).await {
                    Ok(response) => {
                        info!(
                            "Transcribe for wakeword: {:?} returned {:?}",
                            wakeword, response.text
                        );
                        let contains = response.text.to_ascii_lowercase().contains(&wakeword);
                        // ignore error because we don't care if we failed to send
                        _ = tx.send(contains);
                    }
                    Err(err) => {
                        error!("Failed to transcribe wakeword buffer {:?}", err);
                    }
                }
            }
        });

        Ok(rx)
    }
}

#[derive(Debug, Default)]
struct AudioBuffer {
    samples: VecDeque<AudioSample>,
}

#[derive(Debug)]
struct AudioSample {
    sample: Vec<i16>,
    time: Instant,
}

impl AudioBuffer {
    fn insert(&mut self, now: Instant, sample: &[i16]) {
        // drain old
        while self.samples.front().is_some_and(|sample| {
            now.checked_duration_since(sample.time).unwrap_or_default()
                > AUDIO_SAMPLE_RETENTION_PERIOD
        }) {
            _ = self.samples.pop_front();
        }

        self.samples.push_back(AudioSample {
            sample: sample.to_owned(),
            time: now,
        });
    }

    fn contents_to_wav(&self, sample_rate: u32) -> anyhow::Result<Vec<u8>> {
        let sample: Vec<i16> = self
            .samples
            .iter()
            .flat_map(|sample| sample.sample.clone())
            .collect();

        let wavspec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut file = vec![];

        {
            let cursor = Cursor::new(&mut file);
            let mut writer = hound::WavWriter::new(cursor, wavspec)
                .context("Failed to open output audio file")?;
            for sample in sample {
                writer
                    .write_sample(sample)
                    .context("Failed to write sample")?;
            }
        }

        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_popping() {
        let start = Instant::now();
        let after_timeout = start + AUDIO_SAMPLE_RETENTION_PERIOD + AUDIO_SAMPLE_RETENTION_PERIOD;

        let a = [0];
        let b = [1];

        let mut buffer = AudioBuffer::default();

        // first insert should work
        buffer.insert(start, &a);
        assert_eq!(buffer.samples.len(), 1);

        // these inserts do not pop
        buffer.insert(start, &a);
        buffer.insert(start, &a);
        assert_eq!(buffer.samples.len(), 3);

        // this insert should pop all previous values
        buffer.insert(after_timeout, &b);
        assert_eq!(buffer.samples.len(), 1);
    }

    #[test]
    fn buffer_ordering() {
        let start = Instant::now();
        let after_timeout = start + AUDIO_SAMPLE_RETENTION_PERIOD + AUDIO_SAMPLE_RETENTION_PERIOD;

        let mut buffer = AudioBuffer::default();

        // insert outdated sample
        buffer.insert(start, &[100]);

        buffer.insert(after_timeout, &[0]);
        buffer.insert(after_timeout, &[1]);
        buffer.insert(after_timeout, &[2]);
        assert_eq!(buffer.samples.len(), 3);

        assert_eq!(&buffer.samples[0].sample, &[0]);
        assert_eq!(&buffer.samples[1].sample, &[1]);
        assert_eq!(&buffer.samples[2].sample, &[2]);
    }
}
