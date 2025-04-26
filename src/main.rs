use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

use nom_teltonika::parser::tcp_frame;
use nom_teltonika::TeltonikaFrame;

fn handle_client(mut stream: TcpStream) -> io::Result<()> {
    let peer = stream.peer_addr().ok();

    // ——— 1) Read exactly the IMEI handshake ————————————————————————
    // 2-byte big-endian length
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let imei_len = u16::from_be_bytes(len_buf) as usize;

    // then that many ASCII bytes
    let mut imei_buf = vec![0u8; imei_len];
    stream.read_exact(&mut imei_buf)?;
    let imei = String::from_utf8(imei_buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    println!("[{:?}] IMEI: {}", peer, imei);

    // ——— 2) ACK the IMEI ————————————————————————————————————————
    stream.write_all(&[1])?;
    stream.flush()?;

    // ——— 3) Loop reading full frames —————————————————————————————
    loop {
        // 3.1 preamble
        let mut preamble = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut preamble) {
            // clean EOF on client disconnect
            if e.kind() == io::ErrorKind::UnexpectedEof {
                println!("[{:?}] disconnected", peer);
                return Ok(());
            } else {
                return Err(e);
            }
        }
        if preamble != [0, 0, 0, 0] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad preamble",
            ));
        }

        // 3.2 length
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let data_len = u32::from_be_bytes(len_buf) as usize;

        // 3.3 payload
        let mut payload = vec![0u8; data_len];
        stream.read_exact(&mut payload)?;

        // 3.4 CRC
        let mut crc_buf = [0u8; 4];
        stream.read_exact(&mut crc_buf)?;

        // re-assemble into one contiguous buffer
        let mut frame_buf = Vec::with_capacity(8 + data_len + 4);
        frame_buf.extend_from_slice(&preamble);
        frame_buf.extend_from_slice(&len_buf);
        frame_buf.extend_from_slice(&payload);
        frame_buf.extend_from_slice(&crc_buf);

        // 3.5 parse with nom-teltonika
        match tcp_frame(&frame_buf) {
            Ok((_, TeltonikaFrame::AVL(avl))) => {
                println!(
                    "[{:?}] → Codec={:?}, {} record(s)",
                    peer,
                    avl.codec,
                    avl.records.len()
                );
                for rec in avl.records.iter() {
                    println!("    {:?}", rec);
                }
                // ACK how many we handled
                let ack = (avl.records.len() as u32).to_be_bytes();
                stream.write_all(&ack)?;
                stream.flush()?;
            }
            Ok((_, TeltonikaFrame::GPRS(gprs))) => {
                println!("[{:?}] → GPRS responses: {:?}", peer, gprs.command_responses);
                // ACK how many command responses
                let ack = (gprs.command_responses.len() as u32).to_be_bytes();
                stream.write_all(&ack)?;
                stream.flush()?;
            }
            Err(e) => {
                eprintln!("[{:?}] parse error: {:?}", peer, e);
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{:?}", e)));
            }
        }
    }
}

fn main() -> io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:7494")?;
    println!("Listening on 0.0.0.0:7494");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                std::thread::spawn(|| {
                    if let Err(e) = handle_client(stream) {
                        eprintln!("Connection error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("Accept error: {}", e),
        }
    }
    Ok(())
}