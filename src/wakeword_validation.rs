use std::{
    collections::VecDeque,
    io::Cursor,
    time::{Duration, Instant},
};

use anyhow::Context;

const AUDIO_SAMPLE_RETENTION_PERIOD: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct AudioBuffer {
    samples: VecDeque<AudioSample>,
}

struct AudioSample {
    sample: Vec<i16>,
    time: Instant,
}

impl AudioBuffer {
    pub fn insert(&mut self, now: Instant, sample: &[i16]) {
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

    #[allow(dead_code)]
    pub fn contents_to_wav(&self, sample_rate: u32) -> anyhow::Result<Vec<u8>> {
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
