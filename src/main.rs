//! This application listens to wakewords and sends recordings of audio with wakewords in them
//! 
//! Based on examples for [porcupine](https://github.com/Picovoice/porcupine/blob/master/demo/rust/micdemo/src/main.rs)
//! and [cobra](https://github.com/Picovoice/cobra/blob/main/demo/rust/micdemo/src/main.rs)
//! By the excellent folks at https://picovoice.ai/

mod configuration;

use anyohow::Context;
use chrono::prelude::*;
use cobra::Cobra;
use porcupine::{BuiltinKeywords, PorcupineBuilder};
use pv_recorder::PvRecorderBuilder;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;

static LISTENING: AtomicBool = AtomicBool::new(false);

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

fn porcupine_demo(
    config: PicovoiceConfig,
    keywords_or_paths: KeywordsOrPaths,
    audio_device_index: i32,
) {
    let mut porcupine_builder = match keywords_or_paths {
        KeywordsOrPaths::Keywords(ref keywords) => {
            PorcupineBuilder::new_with_keywords(config.access_key, keywords)
        }
        KeywordsOrPaths::KeywordPaths(ref keyword_paths) => {
            PorcupineBuilder::new_with_keyword_paths(config.access_key, keyword_paths)
        }
    };

    let cobra = Cobra::new(config.access_key).context("Failed to create Cobra")?;

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
        .device_index(audio_device_index)
        .init()
        .context("Failed to initialize pvrecorder")?;

    recorder.start().context("Failed to start audio recording")?;

    LISTENING.store(true, Ordering::SeqCst);
    ctrlc::set_handler(|| {
        LISTENING.store(false, Ordering::SeqCst);
    })
    .context("Unable to setup signal handler")?;

    tracing::info!("Listening for wake words...");

    let mut audio_buffer = Vec::new();
    while LISTENING.load(Ordering::SeqCst) {
        let frame = recorder.read().context("Failed to read audio frame")?;

        let keyword_index = porcupine.process(&frame).context("Failed to process audio frame")?;
        if keyword_index >= 0 {
            tracing::info!(
                "Detected {}",
                keywords_or_paths.get(keyword_index as usize)
            );
        }

        let voice_probability = cobra.process(&frame).unwrap();
        print_voice_activity(voice_probability);

        /// record if we think we are hearing humans after a wakeword
        if false {
            audio_data.extend_from_slice(&frame);
        }
    }

    tracing::info!("\nStopping...");
    recorder.stop().context("Failed to stop audio recording")?;

    // write to wav file
    if let Some(output_path) = output_path {
        let wavspec = hound::WavSpec {
            channels: 1,
            sample_rate: porcupine.sample_rate(),
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(output_path, wavspec)
            .context("Failed to open output audio file")?;
        for sample in audio_data {
            writer.write_sample(sample).unwrap();
        }
    }
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

fn show_audio_devices() {
    let audio_devices = PvRecorderBuilder::default().get_available_devices();
    match audio_devices {
        Ok(audio_devices) => {
            for (idx, device) in audio_devices.iter().enumerate() {
                println!("index: {idx}, device name: {device:?}");
            }
        }
        Err(err) => panic!("Failed to get audio devices: {}", err),
    };
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
    utilities::setup_tracing(args.verbose);
    let args: Args = Args::parse();
    let app_config = get_configuration(&args.config)?;



    if args.show_audio_devices {
         show_audio_devices();
    }



    let keywords_or_paths: KeywordsOrPaths = {
        if let Some(keyword_paths) = &app_config.picovoice.keyword_paths {
            KeywordsOrPaths::KeywordPaths(
                keyword_paths
            )
        } else let Some(keywords) = &app_config.picovoice.keywords {
            KeywordsOrPaths::Keywords(
                keywords.iter()
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

    porcupine_demo(
        audio_device_index,
        access_key,
        keywords_or_paths,
        sensitivities,
        model_path,
        output_path,
    );
}

fn show_audio_devices() {
    let audio_devices = PvRecorderBuilder::default().get_available_devices();
    match audio_devices {
        Ok(audio_devices) => {
            for (idx, device) in audio_devices.iter().enumerate() {
                info!("index: {idx}, device name: {device:?}");
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
