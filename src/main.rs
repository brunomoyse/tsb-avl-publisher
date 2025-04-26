// src/main.rs
use std::env;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, SocketAddr};
use std::sync::Arc;
use std::thread;

use lapin::{
    options::{BasicPublishOptions, QueueDeclareOptions},
    types::FieldTable,
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use nom_teltonika::parser::tcp_frame;
use nom_teltonika::TeltonikaFrame;
use serde_json::to_vec;
use tokio::runtime::Runtime;

fn main() -> io::Result<()> {
    // 0) Build a small Tokio runtime to drive RabbitMQ
    let rt = Arc::new(Runtime::new().expect("failed to create Tokio runtime"));
    let amqp_addr =
        env::var("AMQP_ADDR").unwrap_or_else(|_| "amqp://127.0.0.1:5672/%2f".into());
    let queue_name = env::var("AMQP_QUEUE").unwrap_or_else(|_| "teltonika.records".into());

    // 1) Connect & declare queue on that runtime
    let channel: Channel = {
        let rt = rt.clone();
        rt.block_on(async {
            let conn = Connection::connect(&amqp_addr, ConnectionProperties::default())
                .await
                .expect("AMQP connect failed");
            let ch = conn.create_channel().await.expect("create_channel");
            ch.queue_declare(
                &queue_name,
                QueueDeclareOptions { durable: true, ..Default::default() },
                FieldTable::default(),
            )
            .await
            .expect("queue_declare");
            ch
        })
    };
    let channel = Arc::new(channel);

    // 2) Your existing TCP listener
    let listener = TcpListener::bind("0.0.0.0:7494")?;
    println!("Listening on 0.0.0.0:7494");

    for conn in listener.incoming() {
        let stream = conn?;
        let peer = stream.peer_addr().unwrap_or_else(|_| SocketAddr::from(([0,0,0,0],0)));
        let channel = channel.clone();
        let queue_name = queue_name.clone();
        let rt = rt.clone();

        thread::spawn(move || {
            if let Err(e) = handle_client(stream, peer, channel, queue_name, rt) {
                eprintln!("[{}] error: {}", peer, e);
            }
        });
    }

    Ok(())
}

fn handle_client(
    mut stream: TcpStream,
    peer: SocketAddr,
    channel: Arc<Channel>,
    queue: String,
    rt: Arc<Runtime>,
) -> io::Result<()> {
    // —— 1) IMEI handshake —————————————————————————————
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let imei_len = u16::from_be_bytes(len_buf) as usize;
    let mut imei_buf = vec![0u8; imei_len];
    stream.read_exact(&mut imei_buf)?;
    let imei = String::from_utf8(imei_buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    println!("[{}] IMEI: {}", peer, imei);

    // —— 2) ACK IMEI ———————————————————————————————
    stream.write_all(&[1])?;
    stream.flush()?;

    // —— 3) Loop reading frames —————————————————————————
    loop {
        // 3.1 preamble
        let mut preamble = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut preamble) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                println!("[{}] disconnected", peer);
                return Ok(());
            } else {
                return Err(e);
            }
        }
        if preamble != [0, 0, 0, 0] {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad preamble"));
        }

        // 3.2 length
        let mut len32 = [0u8; 4];
        stream.read_exact(&mut len32)?;
        let data_len = u32::from_be_bytes(len32) as usize;

        // 3.3 payload
        let mut payload = vec![0u8; data_len];
        stream.read_exact(&mut payload)?;

        // 3.4 CRC
        let mut crc32 = [0u8; 4];
        stream.read_exact(&mut crc32)?;

        // Reassemble full frame buffer
        let mut frame_buf = Vec::with_capacity(8 + data_len + 4);
        frame_buf.extend_from_slice(&preamble);
        frame_buf.extend_from_slice(&len32);
        frame_buf.extend_from_slice(&payload);
        frame_buf.extend_from_slice(&crc32);

        // 3.5 parse with nom-teltonika
        match tcp_frame(&frame_buf) {
            Ok((_, TeltonikaFrame::AVL(avl))) => {
                println!("[{}] → {:?}, {} record(s)", peer, avl.codec, avl.records.len());

                // Publish each record to RabbitMQ
                for record in &avl.records {
                    // merge IMEI into payload
                    let mut json = serde_json::to_value(&record).unwrap();
                    json["device_imei"] = imei.clone().into();
                    let payload = to_vec(&json).unwrap();

                    // block_on a small future that publishes + awaits confirmation
                    let confirm = {
                        let ch = channel.clone();
                        let q = queue.clone();
                        rt.block_on(async move {
                            let publisher = ch
                                .basic_publish(
                                    "",
                                    &q,
                                    BasicPublishOptions::default(),
                                    &payload,
                                    BasicProperties::default()
                                        .with_content_type("application/json".into()),
                                )
                                .await?;
                            // `publisher.await` is a `Future<Output = Result<PublisherConfirm, Error>>`
                            publisher.await
                        })
                    }
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                    // optionally inspect `confirm`
                    let _ = confirm; 
                }

                // ACK how many you processed
                let ack = (avl.records.len() as u32).to_be_bytes();
                stream.write_all(&ack)?;
                stream.flush()?;
            }
            Ok((_, TeltonikaFrame::GPRS(gprs))) => {
                println!("[{}] → GPRS {:?}", peer, gprs.command_responses);
                let ack = (gprs.command_responses.len() as u32).to_be_bytes();
                stream.write_all(&ack)?;
                stream.flush()?;
            }
            Err(e) => {
                eprintln!("[{}] parse error: {:?}", peer, e);
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{:?}", e)));
            }
        }
    }
}