use std::sync::{Arc, OnceLock};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use zenoh::{prelude::r#async::*, Session as ZenohSession};

use crate::WakewordError;

const TRACING_ZENOH_TOPIC_FULL: &str = "/tracing/full";
const TRACING_ZENOH_TOPIC_JSON: &str = "/tracing/json";

static GLOBAL_ZENOH_SESSION: OnceLock<Arc<ZenohSession>> = OnceLock::new();

/// This method panics in case it fails to set the global zenoh session
pub fn set_global_tracing_zenoh_subscriber(session: Arc<ZenohSession>) {
    GLOBAL_ZENOH_SESSION.set(session).unwrap();
}

pub fn setup_tracing(verbosity_level: u8, topic_prefix: &str) {
    let filter = match verbosity_level {
        0 => tracing::level_filters::LevelFilter::INFO,
        1 => tracing::level_filters::LevelFilter::DEBUG,
        2 => tracing::level_filters::LevelFilter::TRACE,
        _ => tracing::level_filters::LevelFilter::TRACE,
    };

    let stderr_writer = fmt::Layer::default()
        .with_thread_names(true)
        .with_writer(std::io::stderr);

    let tracing_zenoh_topic_full = format!("{}{}", topic_prefix, TRACING_ZENOH_TOPIC_FULL);
    let zenoh_full_writer = start_log_writer(&tracing_zenoh_topic_full);
    let zenoh_full_layer = fmt::Layer::default()
        .with_thread_names(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(move || zenoh_full_writer.clone());

    let tracing_zenoh_topic_json = format!("{}{}", topic_prefix, TRACING_ZENOH_TOPIC_JSON);
    let zenoh_json_writer = start_log_writer(&tracing_zenoh_topic_json);
    let zenoh_json_layer = fmt::Layer::default()
        .json()
        .with_thread_names(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_span_list(true)
        .with_writer(move || zenoh_json_writer.clone());

    let subscriber = fmt::Subscriber::builder()
        // subscriber configuration
        .with_env_filter(EnvFilter::from_default_env())
        .with_max_level(filter)
        .finish()
        // add additional writers
        .with(stderr_writer)
        .with(zenoh_full_layer)
        .with(zenoh_json_layer);

    tracing::subscriber::set_global_default(subscriber).expect("unable to set global subscriber");
}

#[derive(Clone)]
struct LogWriter {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let data = buf.to_vec();
        // ignore errors
        _ = self.sender.try_send(data);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn start_log_writer(topic: &str) -> LogWriter {
    let (sender, receiver) = tokio::sync::mpsc::channel(100);
    let log_writer = LogWriter { sender };

    tokio::spawn({
        let topic = topic.to_owned();
        async move { repeat_loop(receiver, &topic).await }
    });

    log_writer
}

async fn repeat_loop(mut receiver: tokio::sync::mpsc::Receiver<Vec<u8>>, topic: &str) {
    loop {
        if let Err(err) = publisher_loop(&mut receiver, topic).await {
            tracing::error!("Log publisher loop failed {:?}", err);
        }
    }
}

async fn publisher_loop(
    receiver: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    topic: &str,
) -> anyhow::Result<()> {
    if let Some(zenoh_session) = GLOBAL_ZENOH_SESSION.get() {
        let publisher = zenoh_session
            .declare_publisher(topic.to_owned())
            .congestion_control(CongestionControl::Drop)
            .priority(Priority::DataLow)
            .res()
            .await
            .map_err(WakewordError::ZenohError)?;

        while let Some(data) = receiver.recv().await {
            publisher
                .put(data)
                .res()
                .await
                .map_err(WakewordError::ZenohError)?;
        }
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Ok(())
}
