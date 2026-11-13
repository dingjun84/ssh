// Minimal russh smoke test used to isolate integration bugs from library
// bugs. Two write strategies, selected by WRITE_MODE:
//   whole -> write on the Channel, then read on the same handle
//   split -> split() into halves, spawn a writer task, read in the main task
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use tokio::sync::mpsc::unbounded_channel;

struct H;

impl client::Handler for H {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = std::env::args().nth(1).unwrap_or("43.139.142.161".into());
    let user = std::env::args().nth(2).unwrap_or("root".into());
    let key_path = std::env::args()
        .nth(3)
        .unwrap_or_else(|| format!("{}/.ssh/id_rsa", std::env::var("HOME").unwrap()));
    let mode = std::env::var("WRITE_MODE").unwrap_or_else(|_| "whole".into());

    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 4,
        ..Default::default()
    });

    let mut session = client::connect(config, (host.as_str(), 22), H).await?;
    let key = load_secret_key(&key_path, None)?;
    let hash = session.best_supported_rsa_hash().await?.flatten();
    let auth = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await?;
    eprintln!("auth ok = {}", auth.success());

    let channel = session.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    channel.request_shell(false).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    let mut out = Vec::new();

    if mode == "split" {
        let (mut read_half, write_half) = channel.split();
        let (tx, mut rx) = unbounded_channel::<Vec<u8>>();

        let writer = tokio::spawn(async move {
            let _session = session;
            while let Some(data) = rx.recv().await {
                if let Err(e) = write_half.data_bytes(data).await {
                    eprintln!("write error: {e}");
                    break;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
        tx.send(b"echo RUSSH_WRITE_OK\n".to_vec())?;
        eprintln!("queued command (split)");

        loop {
            match tokio::time::timeout_at(deadline, read_half.wait()).await {
                Ok(Some(ChannelMsg::Data { data })) => out.extend_from_slice(&data),
                Ok(Some(_)) => {},
                Ok(None) | Err(_) => break,
            }
        }
        writer.abort();
    } else {
        let mut channel = channel;
        tokio::time::sleep(Duration::from_millis(500)).await;
        channel.data_bytes("echo RUSSH_WRITE_OK\n").await?;
        eprintln!("sent command (whole)");

        loop {
            match tokio::time::timeout_at(deadline, channel.wait()).await {
                Ok(Some(ChannelMsg::Data { data })) => out.extend_from_slice(&data),
                Ok(Some(_)) => {},
                Ok(None) | Err(_) => break,
            }
        }
    }

    let echoed = out.windows(14).any(|w| w == b"RUSSH_WRITE_OK");
    println!("--- received {} bytes, echoed = {echoed} ---", out.len());
    println!("{}", String::from_utf8_lossy(&out));
    Ok(())
}
