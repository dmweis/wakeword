use clap::Parser;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use zenoh::config::Config as ZenohConfig;
use zenoh::prelude::r#async::*;

#[derive(Parser)]
#[command(author, version)]
struct Args {
    /// Topic to listen on
    #[arg(long, default_value = "wakeword/event/wake_word_audio_wav")]
    topic: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Args = Args::parse();

    let zenoh_config = ZenohConfig::default();
    let zenoh_session = zenoh::open(zenoh_config).res().await.unwrap().into_arc();

    println!("Creating subscriber on {}", args.topic);

    let file_subscriber = zenoh_session
        .declare_subscriber(args.topic)
        .res()
        .await
        .unwrap();

    let mut counter = 0;
    loop {
        println!("Waiting for message");
        let msg = file_subscriber.recv_async().await.unwrap();
        println!("Received new file");
        let msg: Vec<u8> = msg.value.try_into()?;
        let filename = format!("tmp/recording_{}.wav", counter);
        let mut file = File::create(&filename).await?;
        file.write_all(&msg).await?;
        drop(file);
        println!("Saved file as {}", &filename);
        counter += 1;
    }
}
