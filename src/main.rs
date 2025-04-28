// src/main.rs

// Import the generated gRPC module
pub mod eventpb;

use std::{env, error::Error};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use serde_json::{to_value, to_vec};
use nom_teltonika::{parser::tcp_frame, TeltonikaFrame};
use eventpb::event::{EventMessage, event_service_client::EventServiceClient};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // --- Configuration ---
    let grpc_addr = env::var("GRPC_ADDR").unwrap_or_else(|_| "http://tsb-backend:50051".into());
    let tcp_addr  = env::var("TCP_ADDR").unwrap_or_else(|_| "0.0.0.0:7494".into());

    // --- Channel for outbound gRPC messages ---
    let (tx, rx) = mpsc::channel(128);
    let outbound = ReceiverStream::new(rx);

    // --- Prepare tasks: TCP listener and gRPC client/ack logging ---
    let tcp_task = start_tcp_listener(&tcp_addr, tx);
    let ack_task = async {
        // Wait until TCP listener is spawned to show readiness
        println!("Starting gRPC client to {}...", grpc_addr);
        let mut ack_stream = start_grpc_client(&grpc_addr, outbound).await?;
        while let Ok(Some(ack)) = ack_stream.message().await {
            println!("Received ack: {}", ack.status);
        }
        Ok::<(), Box<dyn Error>>(())
    };

    println!("Starting TCP listener on {} and gRPC client concurrently...", tcp_addr);
    let (tcp_res, ack_res) = tokio::join!(tcp_task, ack_task);

    // Propagate any errors
    tcp_res?;
    ack_res?;
    Ok(())
}

/// Sets up the gRPC client, opens the bi-directional stream, and returns the ACK stream
async fn start_grpc_client(
    grpc_addr: &str,
    outbound: ReceiverStream<EventMessage>,
) -> Result<tonic::codec::Streaming<eventpb::event::Ack>, Box<dyn Error>> {
    let mut client = EventServiceClient::connect(grpc_addr.to_string()).await?;
    let response = client.stream_events(outbound).await?;
    Ok(response.into_inner())
}

/// Listens on TCP, prints listener startup, spawns a task per connection, and forwards parsed events into `tx`
async fn start_tcp_listener(
    listen_addr: &str,
    tx: mpsc::Sender<EventMessage>,
) -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind(listen_addr).await?;
    println!("Listening on {}", listen_addr);
    loop {
        let (socket, _) = listener.accept().await?;
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_client(socket, tx_clone).await {
                eprintln!("Error handling client: {:?}", err);
            }
        });
    }
}

/// Handles a single Teltonika TCP connection: handshake, parse frames, send EventMessage
async fn handle_client(
    mut stream: tokio::net::TcpStream,
    tx: mpsc::Sender<EventMessage>,
) -> std::io::Result<()> {
    // 1) IMEI handshake
    let imei = {
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await?;
        let len = u16::from_be_bytes(buf) as usize;
        let mut data = vec![0u8; len];
        stream.read_exact(&mut data).await?;
        String::from_utf8(data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    };
    println!("Client IMEI: {}", imei);

    // ACK IMEI
    stream.write_all(&[1]).await?;
    stream.flush().await?;

    // 2) Frame-reading loop
    loop {
        // Read and validate preamble
        let mut preamble = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut preamble).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                println!("Client {} disconnected", imei);
                return Ok(());
            } else {
                return Err(e);
            }
        }
        if preamble != [0, 0, 0, 0] {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad preamble"));
        }

        // Read length, payload, and CRC
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let data_len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; data_len];
        stream.read_exact(&mut payload).await?;
        let mut crc = [0u8; 4];
        stream.read_exact(&mut crc).await?;

        // Assemble full frame, parse, and forward records
        let mut frame = Vec::with_capacity(8 + data_len + 4);
        frame.extend_from_slice(&preamble);
        frame.extend_from_slice(&len_buf);
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc);

        match tcp_frame(&frame) {
            Ok((_, TeltonikaFrame::AVL(avl))) => {
                println!("→ {:?}, {} record(s)", avl.codec, avl.records.len());
                for record in &avl.records {
                    let mut json_val = to_value(&record).unwrap();
                    json_val["device_imei"] = imei.clone().into();
                    let data = to_vec(&json_val).unwrap();
                    let msg = EventMessage { r#type: format!("{:?}", avl.codec), payload: data };
                    if tx.send(msg).await.is_err() {
                        eprintln!("gRPC send channel closed");
                    }
                }
                // ACK record count
                let ack = (avl.records.len() as u32).to_be_bytes();
                stream.write_all(&ack).await?;
                stream.flush().await?;
            }
            Ok((_, TeltonikaFrame::GPRS(gprs))) => {
                println!("→ GPRS {:?}", gprs.command_responses);
                let ack = (gprs.command_responses.len() as u32).to_be_bytes();
                stream.write_all(&ack).await?;
                stream.flush().await?;
            }
            Err(e) => {
                eprintln!("Parse error: {:?}" , e);
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e)));
            }
        }
    }
}