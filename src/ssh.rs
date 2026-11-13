use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use iced_term::Transport;
use russh::client;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::ChannelMsg;
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver, UnboundedSender,
};

use crate::hostkey;

/// Messages the UI needs to push towards the remote host.
enum Outbound {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// Messages the remote host pushes towards the UI.
#[derive(Debug, Clone)]
pub enum SshEvent {
    Data(Vec<u8>),
    /// Emitted right after the host key has been verified (or pinned).
    HostKey {
        fingerprint: String,
        learned: bool,
    },
    Connected,
    Error(String),
    Closed,
}

/// Bridges the terminal widget to an SSH channel. The widget only knows this
/// trait, so it never learns whether bytes go to a local PTY or a cipher.
pub struct SshTransport {
    tx: UnboundedSender<Outbound>,
}

impl Transport for SshTransport {
    fn write(&self, data: &[u8]) {
        let _ = self.tx.send(Outbound::Data(data.to_vec()));
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(Outbound::Resize { cols, rows });
    }
}

pub enum Auth {
    Password(String),
    Key {
        path: String,
        passphrase: Option<String>,
    },
}

pub struct SshRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    pub cols: u16,
    pub rows: u16,
}

pub(crate) struct Handler {
    host: String,
    port: u16,
    /// Filled in by `check_server_key` so the caller can surface the
    /// fingerprint (or the rejection reason) once the handshake settles.
    verdict: Arc<StdMutex<Option<Result<hostkey::Verdict, String>>>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let outcome = hostkey::verify(&self.host, self.port, &key.public_key());
        let accept = outcome.is_ok();
        *self.verdict.lock().unwrap() = Some(outcome);
        // Returning false makes russh abort the handshake; the concrete reason
        // travels through the shared slot instead of a generic "unknown key".
        Ok(accept)
    }
}

/// A place to park the live session handle so other subsystems (SFTP) can open
/// their own channel on the *same* connection instead of dialing a second one.
///
/// `russh`'s `Handle` is not `Clone`, so it is shared behind an `Arc`. The
/// slot must only be borrowed for the duration of one operation — holding it
/// would keep a dead connection alive after the shell loop exits.
pub(crate) struct SessionSlot {
    inner: StdMutex<Option<(u64, Arc<client::Handle<Handler>>)>>,
    counter: AtomicU64,
}

impl SessionSlot {
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            inner: StdMutex::new(None),
            counter: AtomicU64::new(0),
        })
    }

    /// Every successful handshake gets a new generation so consumers can tell
    /// a live channel from one that belonged to a previous connection.
    fn next_generation(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn publish(&self, generation: u64, handle: Arc<client::Handle<Handler>>) {
        *self.inner.lock().unwrap() = Some((generation, handle));
    }

    /// Clears the slot only if it still belongs to `generation`; a reconnect
    /// may already have installed a newer handle by the time the old loop ends.
    fn retire(&self, generation: u64) {
        let mut slot = self.inner.lock().unwrap();
        if matches!(slot.as_ref(), Some((current, _)) if *current == generation) {
            *slot = None;
        }
    }

    pub(crate) fn current(&self) -> Option<(u64, Arc<client::Handle<Handler>>)> {
        self.inner.lock().unwrap().clone()
    }
}

/// Starts a connection attempt in the background and returns the transport
/// handle, the event stream to feed into the UI, and the session slot that
/// later lets SFTP attach to the finished connection.
pub fn spawn(
    handle: &tokio::runtime::Handle,
    req: SshRequest,
) -> (
    Arc<SshTransport>,
    UnboundedReceiver<SshEvent>,
    Arc<SessionSlot>,
) {
    let (out_tx, out_rx) = unbounded_channel();
    let (evt_tx, evt_rx) = unbounded_channel();
    let transport = Arc::new(SshTransport { tx: out_tx });
    let slot = SessionSlot::empty();

    let worker_slot = slot.clone();
    handle.spawn(async move {
        let result = run(req, out_rx, evt_tx.clone(), worker_slot).await;
        if let Err(err) = result {
            let _ = evt_tx.send(SshEvent::Error(err));
        }
        let _ = evt_tx.send(SshEvent::Closed);
    });

    (transport, evt_rx, slot)
}

async fn run(
    req: SshRequest,
    mut out_rx: UnboundedReceiver<Outbound>,
    evt: UnboundedSender<SshEvent>,
    slot: Arc<SessionSlot>,
) -> Result<(), String> {
    let config = Arc::new(client::Config {
        // Matches `ServerAliveInterval 30`: probe only after 30s of silence,
        // and give up after 4 unanswered probes (~2 minutes).
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 4,
        nodelay: true,
        ..Default::default()
    });

    let verdict_slot = Arc::new(StdMutex::new(None));
    let handler = Handler {
        host: req.host.clone(),
        port: req.port,
        verdict: verdict_slot.clone(),
    };

    let connected = client::connect(config, (req.host.as_str(), req.port), handler).await;

    // A rejected host key surfaces as a generic handshake failure, so consult
    // the slot first to report the real reason (mismatch vs. unreachable).
    let last_verdict = verdict_slot.lock().unwrap().clone();
    let mut session = match (connected, last_verdict) {
        (_, Some(Err(reason))) => return Err(reason),
        (Ok(session), _) => session,
        (Err(e), _) => {
            return Err(format!("连接 {}:{} 失败: {e}", req.host, req.port));
        },
    };

    if let Some(Ok(verdict)) = verdict_slot.lock().unwrap().clone() {
        let _ = evt.send(SshEvent::HostKey {
            fingerprint: verdict.fingerprint,
            learned: verdict.trust == hostkey::Trust::Learned,
        });
    }

    match &req.auth {
        Auth::Password(password) => {
            let result = session
                .authenticate_password(req.user.clone(), password.clone())
                .await
                .map_err(|e| format!("认证过程出错: {e}"))?;
            if let client::AuthResult::Failure { remaining_methods, .. } = result {
                return Err(format!("密码认证被拒绝，服务端允许: {:?}", remaining_methods));
            }
        },
        Auth::Key { path, passphrase } => {
            let key = load_secret_key(path, passphrase.as_deref())
                .map_err(|e| format!("读取私钥 {path} 失败: {e}"))?;

            // RSA keys need an explicit hash algorithm. Passing `None` falls
            // back to `ssh-rsa` (SHA-1), which OpenSSH 8.8+ disables by
            // default, so the server answers with a bare rejection. Ask the
            // server what it accepts via `server-sig-algs` instead.
            let hash_alg = session
                .best_supported_rsa_hash()
                .await
                .map_err(|e| format!("协商签名算法失败: {e}"))?
                .flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            let result = session
                .authenticate_publickey(req.user.clone(), key)
                .await
                .map_err(|e| format!("认证过程出错: {e}"))?;
            if let client::AuthResult::Failure { remaining_methods, .. } = result {
                return Err(format!("公钥认证被拒绝，服务端允许: {:?}", remaining_methods));
            }
        },
    }

    // Authentication is done, so the handle can now be shared: subsystems
    // (SFTP) open their own channel on this same connection rather than
    // dialling the host again.
    let generation = slot.next_generation();
    let session = Arc::new(session);
    slot.publish(generation, session.clone());

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| format!("打开会话通道失败: {e}"))?;

    channel
        .request_pty(
            false,
            "xterm-256color",
            req.cols as u32,
            req.rows as u32,
            0,
            0,
            &[],
        )
        .await
        .map_err(|e| format!("申请 PTY 失败: {e}"))?;

    channel
        .request_shell(false)
        .await
        .map_err(|e| format!("启动远端 shell 失败: {e}"))?;

    let (mut read_half, write_half) = channel.split();
    let _ = evt.send(SshEvent::Connected);

    // Unattended smoke test: type a command into the remote shell so the write
    // direction can be verified without a keyboard. Delayed, because bytes sent
    // before the shell prints its first prompt get swallowed by the PTY setup.
    if let Ok(probe) = std::env::var("SSH_PROBE") {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let _ = write_half
            .data_bytes(format!("{probe}\n").into_bytes())
            .await;
    }

    // Single task, `select!` over both directions. An earlier version spawned
    // a second task for the write side and it would sometimes never be polled
    // (the writes silently vanished while reads kept flowing), so the two
    // halves now live in one loop.
    loop {
        tokio::select! {
            incoming = read_half.wait() => match incoming {
                Some(ChannelMsg::Data { data }) => {
                    if evt.send(SshEvent::Data(data.to_vec())).is_err() {
                        break;
                    }
                },
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = evt.send(SshEvent::Data(data.to_vec()));
                },
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {},
            },
            outbound = out_rx.recv() => match outbound {
                Some(Outbound::Data(data)) => {
                    if write_half.data_bytes(data).await.is_err() {
                        break;
                    }
                },
                Some(Outbound::Resize { cols, rows }) => {
                    let _ = write_half
                        .window_change(cols as u32, rows as u32, 0, 0)
                        .await;
                },
                Some(Outbound::Close) => {
                    let _ = write_half.eof().await;
                    break;
                },
                None => break,
            },
        }
    }

    // The session handle must outlive the channel or russh tears the
    // transport down; keep it in scope until the loop finishes.
    slot.retire(generation);
    drop(session);
    Ok(())
}
