use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use clap::Parser;
use flavors::parser::{TagHeader, TagType};
use log::{debug, info, warn};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::rml_amf0::{deserialize, Amf0Value};
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult,
};
use rml_rtmp::sessions::{PublishRequestType, StreamMetadata};
use rml_rtmp::time::RtmpTimestamp;
use simplelog::*;
use std::collections::VecDeque;
use std::io::{Cursor, SeekFrom};
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
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

    #[arg(short, long, default_value_t = true)]
    _loop: bool,
}

async fn connection_reader(
    mut stream: ReadHalf<TcpStream>,
    rx_queue: mpsc::UnboundedSender<Bytes>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut buffer = BytesMut::with_capacity(4096);

    loop {
        let bytes_read = stream.read_buf(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        let bytes = buffer.split_off(bytes_read);
        if rx_queue.send(buffer.freeze()).is_err() {
            break;
        }

        buffer = bytes;
    }

    info!("Reader disconnected");
    Ok(())
}

async fn read_file(
    file: &str,
    _loop: bool,
    tags: mpsc::UnboundedSender<(Duration, TagHeader, Vec<u8>)>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Open the file
    let start_time = Instant::now();
    let mut base_time: Duration = Duration::ZERO;

    'outer: loop {
        let file = File::open(file).await?;
        let mut flv_reader = FlvReader::new(file);
        let _header = flv_reader.read_header().await;
        debug!("starting file from {}", base_time.as_secs_f32());
        // Store the most recent tag timestamp
        let mut latest_tag_time: Duration = Duration::ZERO;
        'inner: loop {
            if let Ok((tag_header, tag_bytes)) = flv_reader.read_tag().await {
                // Setup our baseline tag time
                if tag_header.timestamp != 0 {
                    // Checking for zero here because the last tag in the file
                    // will have a timestamp of zero
                    latest_tag_time = Duration::from_millis(tag_header.timestamp as u64);
                }
                // This represents the tag time after N loops of the file
                let loop_adjusted_tag_time = latest_tag_time + base_time;
                let current_tag_time = start_time + loop_adjusted_tag_time;
                let now = Instant::now();

                if current_tag_time > now {
                    sleep(current_tag_time - now).await;
                }

                tags.send((current_tag_time - start_time, tag_header, tag_bytes))?;
            } else {
                debug!("File read complete");
                if !_loop {
                    break 'outer;
                }
                base_time += latest_tag_time;
                break 'inner;
            }
        }
    }
    Ok(())
}

// Iterate over the ClientSessionResults from a handle_input
// and either:
// 1. send them out the RTMP connection
// 2. put them in our local event queue for processing
async fn handle_session_results(
    rtmp_tx: &mut WriteHalf<TcpStream>,
    events: &mut VecDeque<ClientSessionEvent>,
    actions: impl IntoIterator<Item = ClientSessionResult>,
) -> Result<()> {
    for action in actions {
        match action {
            ClientSessionResult::OutboundResponse(packet) => {
                rtmp_tx.write_all(&packet.bytes).await?;
            }
            ClientSessionResult::RaisedEvent(ev) => {
                debug!("raised event {:?}", ev);
                events.push_back(ev);
            }
            ClientSessionResult::UnhandleableMessageReceived(msg) => {
                warn!(
                    "rtmp::client received unhandleable server message: {:?}",
                    msg
                );
                warn!("    data -> {:?}", msg.data);
            }
        }
    }
    Ok(())
}

// Return the first event in our event queue or, if the queue is empty, wait
// for one to arrive.  This will receive data from the rtmp receiver channel
// and route it to the session.
async fn wait_event(
    rtmp_tx: &mut WriteHalf<TcpStream>,
    rtmp_rx: &mut UnboundedReceiver<bytes::Bytes>,
    session: &mut ClientSession,
    events: &mut VecDeque<ClientSessionEvent>,
) -> Result<ClientSessionEvent> {
    if let Some(event) = events.pop_front() {
        return Ok(event);
    }
    loop {
        let bytes = rtmp_rx.recv().await.ok_or(anyhow!("Failed to read"))?;
        let actions = session.handle_input(&bytes)?;
        handle_session_results(rtmp_tx, events, actions).await?;
        if let Some(event) = events.pop_front() {
            return Ok(event);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    CombinedLogger::init(vec![TermLogger::new(
        LevelFilter::Info,
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

    let path_parts = url.path().split("/").collect::<Vec<&str>>();
    if path_parts.len() != 3 {
        return Err(anyhow!(
            "Path format is unusual. Should be /app/<stream_key>"
        ));
    }

    // unfortunately we can't use .origin() because
    // this is technically 'opaque' and would serialize
    // to 'null'
    let host = format!("{}:{}", url.host().unwrap(), port);

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
    let mut buff = [0; 4096];
    let bytes_after_handshake = loop {
        let bytes = stream.read(&mut buff).await?;
        match handshake.process_bytes(&buff[0..bytes])? {
            HandshakeProcessResult::InProgress { response_bytes } => {
                stream.write_all(&response_bytes).await?;
            }
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                stream.write_all(&response_bytes).await?;
                break remaining_bytes;
            }
        }
    };

    // 4. Spawn connection reader/writer tasks
    //    - this requires "splitting" the Tokio stream into read and write halves
    //    - then we can service the read task in the background to listen for
    //      commands from the server, while continuing to write tags in the foreground.
    let (stream_reader, mut stream_writer) = tokio::io::split(stream);
    let (read_bytes_sender, mut read_bytes_receiver) = mpsc::unbounded_channel();

    tokio::task::spawn(async { connection_reader(stream_reader, read_bytes_sender).await });

    let app_name = path_parts[1].to_string();
    let mut config = ClientSessionConfig::new();
    config.flash_version = "FMLE/3.0 (compatible; FMSc/1.0)".to_string();

    let tc_url = format!(
        "{}://{}:{}/{}",
        url.scheme(),
        url.host().unwrap(),
        port,
        app_name
    );
    config.tc_url = Some(tc_url);

    // let mut deserializer = ChunkDeserializer::new();
    // let mut serializer = ChunkSerializer::new();
    let (mut session, initial_results) = ClientSession::new(config.clone())?;

    let mut events = VecDeque::new();

    // The 'initial_results' here are some messages that we should send immediately.
    handle_session_results(&mut stream_writer, &mut events, initial_results).await?;

    let results = session.handle_input(&bytes_after_handshake)?;
    handle_session_results(&mut stream_writer, &mut events, results).await?;

    info!("sending connection request");

    let connect_result = session.request_connection(app_name)?;
    handle_session_results(&mut stream_writer, &mut events, vec![connect_result]).await?;

    // wait for connection result
    loop {
        match wait_event(
            &mut stream_writer,
            &mut read_bytes_receiver,
            &mut session,
            &mut events,
        )
        .await?
        {
            ClientSessionEvent::ConnectionRequestAccepted => {
                break;
            }
            ClientSessionEvent::ConnectionRequestRejected { description } => {
                return Err(anyhow!(description));
            }
            ev => {
                warn!("rtmp::client unexpected event: {:?}", ev);
                return Err(anyhow!("Unexpected event"));
            }
        }
    }

    info!("connection succeeded, attempting to publish...");

    // request publish:
    let stream_key = path_parts[2].to_string();
    let action = session.request_publishing(stream_key, PublishRequestType::Live)?;
    handle_session_results(&mut stream_writer, &mut events, vec![action]).await?;

    loop {
        match wait_event(
            &mut stream_writer,
            &mut read_bytes_receiver,
            &mut session,
            &mut events,
        )
        .await?
        {
            ClientSessionEvent::PublishRequestAccepted => {
                break;
            }
            ev => {
                warn!("rtmp::client unexpected event: {:?}", ev);
                return Err(anyhow!("Unexpected event"));
            }
        }
    }

    info!("publish succeeded");

    // now we can enter "normal operation" mode, where we do a few things:
    // 1. send media data
    // 2. service incoming events from the server
    // this will all happen in a big select loop.

    // this new queue and task will be used to generate media tags from an FLV file
    let (file_tx, mut file_rx) = mpsc::unbounded_channel::<(Duration, TagHeader, Vec<u8>)>();
    let reader = tokio::task::spawn(async move {
        let _ = read_file(&args.file, args._loop, file_tx).await;
    });

    let mut metadata_sent = false;

    loop {
        select! {
            file_read = file_rx.recv() => {
                if let Some((duration, tag, data)) = file_read {
                    debug!("Duration: {:?} TagType: {:?}, Len {}", duration, tag.tag_type, data.len());
                    // TODO handle timestamp wrap
                    let timestamp = RtmpTimestamp::new(duration.as_millis() as u32);
                    match tag.tag_type {
                        TagType::Video => {
                            let result = session.publish_video_data(data.into(), timestamp, false)?;
                            handle_session_results(&mut stream_writer, &mut events, vec![result]).await?;
                        }
                        TagType::Audio => {
                            let result = session.publish_audio_data(data.into(), timestamp, false)?;
                            handle_session_results(&mut stream_writer, &mut events, vec![result]).await?;
                        },
                        _ => {
                            let mut cursor = Cursor::new(data);
                            let results = deserialize(&mut cursor)?;
                            match results.get(0).unwrap() {
                                Amf0Value::Utf8String(cmd) => {
                                    match cmd.as_ref() {
                                        "@setDataFrame" => {
                                            // Only forward this once, in case we're looping
                                            if !metadata_sent {
                                                let _key = results.get(1).unwrap(); //  This should always be onMetaData
                                                let values = results.get(2).unwrap();
                                                let mut meta = StreamMetadata::new();
                                                meta.apply_metadata_values(values.clone().get_object_properties().unwrap());
                                                info!("sending metadata: {:?}", meta);
                                                let action = session.publish_metadata(&meta)?;
                                                handle_session_results(&mut stream_writer, &mut events, vec![action]).await?;
                                                metadata_sent = true;
                                            }
                                        }
                                        _ => {
                                            warn!("Unsupported command {}", cmd);
                                        }
                                    }
                                }
                                _ => {
                                    warn!("Unsupported script type: {:?}", results.get(0))
                                }
                            }
                        }
                    }
                } else {
                    info!("File is finished");
                    break
                }
            },
            event = wait_event(
                &mut stream_writer,
                &mut read_bytes_receiver,
                &mut session,
                &mut events,
            ) => {
                debug!("event received: {:?}", event);
            },
            _ = tokio::signal::ctrl_c() => {
                info!("exit signal received");
                break;
            }
        }
    }

    info!("Stopping publish");

    reader.abort();

    let results = session.stop_publishing()?;
    handle_session_results(&mut stream_writer, &mut events, results).await?;

    info!("All done!");

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
            let _ = read_file("ertmp-av1-avc-avc.flv", false, tx).await;
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
