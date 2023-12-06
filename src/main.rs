use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use clap::Parser;
use flavors::parser::TagHeader;
use log::{debug, info};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use simplelog::*;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::sleep;
use tokio::time::Instant;
use url::Url;

use crate::flv_reader::FlvReader;

mod flv_reader;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to FLV file on disk
    #[arg(short, long)]
    file: String,

    /// Path to RTMP server
    #[arg(short, long)]
    server: String,
}

async fn connection_reader(
    mut stream: ReadHalf<TcpStream>,
    manager: mpsc::UnboundedSender<Bytes>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buffer = BytesMut::with_capacity(4096);

    loop {
        let bytes_read = stream.read_buf(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        let bytes = buffer.split_off(bytes_read);
        if manager.send(buffer.freeze()).is_err() {
            break;
        }

        buffer = bytes;
    }

    info!("Reader disconnected");
    Ok(())
}

async fn read_file(
    file: &str,
    tags: mpsc::UnboundedSender<(Duration, TagHeader, Vec<u8>)>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Open the file
    let file = File::open(file).await?;

    let mut flv_reader = FlvReader::new(file);
    let _header = flv_reader.read_header().await;

    let start_time = Instant::now();
    loop {
        let (tag_header, tag_bytes) = flv_reader.read_tag().await?;
        // Setup our baseline tag time
        let current_tag_time = start_time + Duration::from_millis(tag_header.timestamp as u64);
        let now = Instant::now();

        if current_tag_time > now {
            sleep(current_tag_time - now).await;
        }

        tags.send((current_tag_time - start_time, tag_header, tag_bytes))?;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    CombinedLogger::init(vec![TermLogger::new(
        LevelFilter::Debug,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )])?;

    let args = Args::parse();

    let url = Url::parse(&args.server)?;

    if url.host().is_none() {
        return Err(anyhow!("Host not specified"));
    }

    info!("url: {:?}", url);

    let port = url.port().unwrap_or(1935);

    // unfortunately we can't use .origin() because
    // this is technically 'opaque' and would serialize
    // to 'null'
    let host = format!("{}://{}:{}", url.scheme(), url.host().unwrap(), port);

    debug!("Connecting to {}", &host);

    // 1. Open the socket connection
    let mut stream = TcpStream::connect(&host).await?;

    // 1a Optionally handle TLS negotiation
    if port == 443 || url.scheme().eq("rtmps") {
        return Err(anyhow!("uh, we need to implement TLS support"));
    }

    // 2. Send the first part of the RTMP handshake
    let mut handshake = Handshake::new(PeerType::Client);
    let c0_and_c1 = handshake.generate_outbound_p0_and_p1()?;

    debug!("sending c0+c1");
    stream.write_all(&c0_and_c1).await?;

    // 3. Read the rest of the handshake from the server
    let mut read_buffer = [0_u8; 1024];
    loop {
        let bytes_read = stream.read(&mut read_buffer).await?;
        let (is_finished, response_bytes) =
            match handshake.process_bytes(&read_buffer[..bytes_read]) {
                Err(x) => panic!("Error returned: {:?}", x),
                Ok(HandshakeProcessResult::InProgress {
                    response_bytes: bytes,
                }) => (false, bytes),
                Ok(HandshakeProcessResult::Completed {
                    response_bytes: bytes,
                    remaining_bytes: _,
                }) => (true, bytes),
            };

        if response_bytes.len() > 0 {
            stream.write_all(&response_bytes).await?;
        }

        if is_finished {
            debug!("Handshaking Completed!");
            break;
        } else {
            debug!("Handshake still in progress");
        }
    }

    // 4. Spawn connection reader/writer tasks
    //    - this requires "splitting" the Tokio stream into read and write halves
    //    - then we can service the read task in the background to listen for
    //      commands from the server, while continuing to write tags in the foreground.
    let (stream_reader, stream_writer) = tokio::io::split(stream);
    let (read_bytes_sender, mut read_bytes_receiver) = mpsc::unbounded_channel();

    tokio::task::spawn(async { connection_reader(stream_reader, read_bytes_sender).await });

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use tokio::select;

    #[tokio::test]
    async fn test_file_read() -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<(Duration, TagHeader, Vec<u8>)>();
        tokio::task::spawn(async {
            let _ = read_file("ertmp-av1-av1-av1.flv", tx).await;
        });

        loop {
            select! {
                Some((duration, tag, data)) = rx.recv() => {
                    println!("Duration: {:?} TagType: {:?}, Len {}", duration, tag.tag_type, data.len());
                }
            }
        }
    }
}
