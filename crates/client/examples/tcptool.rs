//! Tiny TCP helper for end-to-end smoke tests of the tunnel (not part of the
//! product). Stands in for the system RDP service (echo) and the RDP client
//! (probe).
//!
//!   tcptool echo  <listen_addr>          # echo server (fake RDP service)
//!   tcptool probe <connect_addr> <msg>   # connect, send, verify the echo

use std::env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("echo") => {
            let addr = args.get(2).expect("usage: tcptool echo <addr>");
            let listener = TcpListener::bind(addr).await?;
            eprintln!("echo: listening on {addr}");
            loop {
                let (mut sock, peer) = listener.accept().await?;
                eprintln!("echo: connection from {peer}");
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        }
        Some("probe") => {
            let addr = args.get(2).expect("usage: tcptool probe <addr> <msg>");
            let msg = args.get(3).map(String::as_str).unwrap_or("ping");
            let mut sock = TcpStream::connect(addr).await?;
            sock.write_all(msg.as_bytes()).await?;
            let mut buf = vec![0u8; msg.len()];
            sock.read_exact(&mut buf).await?;
            let got = String::from_utf8_lossy(&buf);
            if got == msg {
                println!("PROBE OK: echoed {got:?}");
                Ok(())
            } else {
                eprintln!("PROBE FAIL: sent {msg:?}, got {got:?}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: tcptool echo <addr> | tcptool probe <addr> <msg>");
            std::process::exit(2);
        }
    }
}
