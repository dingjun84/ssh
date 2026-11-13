//! SFTP panel backend.
//!
//! Everything here rides on the SSH session that the terminal already
//! established: `SessionSlot` hands out the live `russh` handle, we open one
//! extra channel, ask for the `sftp` subsystem, and wrap it in a
//! `SftpSession`. Dialling a second connection would duplicate auth, keepalive
//! and host-key policy, so it is deliberately avoided.
//!
//! The worker is a single task with one command queue. SFTP requests are
//! serialised on purpose — a shared `SftpSession` would otherwise interleave
//! request ids from concurrent operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver, UnboundedSender,
};

use crate::ssh::SessionSlot;

/// 64 KiB matches the default SFTP packet payload; larger reads just get
/// split by the protocol layer anyway.
const CHUNK: usize = 64 * 1024;

/// Do not spam the UI for every chunk — 150 ms keeps the bar smooth without
/// flooding the event channel on fast links.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub enum SftpEvent {
    Listing { path: String, entries: Vec<Entry> },
    Progress { label: String, done: u64, total: u64 },
    Done(String),
    Error(String),
}

#[derive(Debug)]
pub enum Command {
    List(String),
    /// Read `remote` from the host and store it at `local`.
    Download { remote: String, local: PathBuf },
    /// Send `local` to `remote` on the host.
    Upload { local: PathBuf, remote: String },
}

/// Sending half handed to the UI. Dropping it stops the worker.
pub struct Handle {
    tx: UnboundedSender<Command>,
}

impl Handle {
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

/// Spawns the SFTP worker for one SSH session. The returned handle is cheap to
/// keep on the tab; the receiver carries both listings and transfer progress.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    slot: Arc<SessionSlot>,
) -> (Handle, UnboundedReceiver<SftpEvent>) {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (evt_tx, evt_rx) = unbounded_channel();

    rt.spawn(async move {
        run(slot, cmd_rx, evt_tx).await;
    });

    (Handle { tx: cmd_tx }, evt_rx)
}

struct Live {
    /// `SessionSlot` generation this session was built for. A reconnect
    /// invalidates the channel, so the generation must be re-checked on every
    /// command instead of trusting the cached session forever.
    generation: u64,
    session: SftpSession,
}

async fn run(
    slot: Arc<SessionSlot>,
    mut commands: UnboundedReceiver<Command>,
    events: UnboundedSender<SftpEvent>,
) {
    let mut live: Option<Live> = None;

    while let Some(command) = commands.recv().await {
        let outcome = match command {
            Command::List(path) => match attach(&slot, &mut live).await {
                Ok(active) => list(active, &path).await.map(|(path, entries)| {
                    let _ = events.send(SftpEvent::Listing { path, entries });
                }),
                Err(err) => Err(err),
            },
            Command::Download { remote, local } => {
                match attach(&slot, &mut live).await {
                    Ok(active) => download(active, &remote, &local, &events)
                        .await
                        .map(|done| {
                            let _ = events.send(SftpEvent::Done(done));
                        }),
                    Err(err) => Err(err),
                }
            },
            Command::Upload { local, remote } => {
                match attach(&slot, &mut live).await {
                    Ok(active) => upload(active, &local, &remote, &events)
                        .await
                        .map(|done| {
                            let _ = events.send(SftpEvent::Done(done));
                        }),
                    Err(err) => Err(err),
                }
            },
        };

        if let Err(err) = outcome {
            // The channel is the usual casualty (dropped connection, server
            // restart), so throw the cached session away and re-attach on the
            // next command.
            live = None;
            let _ = events.send(SftpEvent::Error(err));
        }
    }

    if let Some(active) = live {
        let _ = active.session.close().await;
    }
}

/// Returns a usable SFTP session, opening the subsystem on first use and after
/// every reconnect.
async fn attach<'a>(
    slot: &Arc<SessionSlot>,
    live: &'a mut Option<Live>,
) -> Result<&'a mut Live, String> {
    let Some((generation, handle)) = slot.current() else {
        return Err("SSH 还没连上，无法打开 SFTP".into());
    };

    if !matches!(live.as_ref(), Some(active) if active.generation == generation)
    {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| format!("打开 SFTP 通道失败: {e}"))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| format!("服务器拒绝 sftp 子系统: {e}"))?;
        let session = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| format!("SFTP 协议协商失败: {e}"))?;
        *live = Some(Live {
            generation,
            session,
        });
    }

    Ok(live.as_mut().expect("live session was just installed"))
}

async fn list(active: &mut Live, path: &str) -> Result<(String, Vec<Entry>), String> {
    // `canonicalize` is what turns "." or "~" into a path worth showing in the
    // breadcrumb; fall back to the raw input when the server has no such
    // extension (it is optional in the protocol).
    let resolved = active
        .session
        .canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_string());

    let entries = active
        .session
        .read_dir(&resolved)
        .await
        .map_err(|e| format!("读取目录 {resolved} 失败: {e}"))?;

    let mut out = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let attrs = entry.metadata();
        let kind = attrs.file_type();
        out.push(Entry {
            name,
            is_dir: kind.is_dir(),
            is_symlink: kind.is_symlink(),
            size: attrs.len(),
        });
    }

    // Directories first, then case-insensitive name order.
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok((resolved, out))
}

async fn download(
    active: &mut Live,
    remote: &str,
    local: &Path,
    events: &UnboundedSender<SftpEvent>,
) -> Result<String, String> {
    let label = basename(remote);
    let mut source = active
        .session
        .open(remote)
        .await
        .map_err(|e| format!("打开远端文件 {remote} 失败: {e}"))?;

    let total = source.metadata().await.map(|m| m.len()).unwrap_or(0);

    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建本地目录 {} 失败: {e}", parent.display()))?;
    }
    let mut sink = tokio::fs::File::create(local)
        .await
        .map_err(|e| format!("创建本地文件 {} 失败: {e}", local.display()))?;

    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let n = source
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取远端数据失败: {e}"))?;
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入本地文件失败: {e}"))?;
        done += n as u64;
        if last.elapsed() >= PROGRESS_INTERVAL {
            let _ = events.send(SftpEvent::Progress {
                label: label.clone(),
                done,
                total,
            });
            last = Instant::now();
        }
    }

    sink.flush()
        .await
        .map_err(|e| format!("刷新本地文件失败: {e}"))?;
    let _ = source.close().await;
    let _ = events.send(SftpEvent::Progress {
        label,
        done,
        total: done.max(total),
    });

    Ok(format!("已下载 {remote} → {}", local.display()))
}

async fn upload(
    active: &mut Live,
    local: &Path,
    remote: &str,
    events: &UnboundedSender<SftpEvent>,
) -> Result<String, String> {
    let label = basename(remote);
    let meta = tokio::fs::metadata(local)
        .await
        .map_err(|e| format!("读取本地文件 {} 失败: {e}", local.display()))?;
    let total = meta.len();

    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("打开本地文件 {} 失败: {e}", local.display()))?;
    let mut sink = active
        .session
        .create(remote)
        .await
        .map_err(|e| format!("创建远端文件 {remote} 失败: {e}"))?;

    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let n = source
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取本地数据失败: {e}"))?;
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入远端文件失败: {e}"))?;
        done += n as u64;
        if last.elapsed() >= PROGRESS_INTERVAL {
            let _ = events.send(SftpEvent::Progress {
                label: label.clone(),
                done,
                total,
            });
            last = Instant::now();
        }
    }

    sink.flush()
        .await
        .map_err(|e| format!("刷新远端文件失败: {e}"))?;
    // `close` waits for the server's acknowledgement; dropping the handle
    // instead would swallow the final write errors.
    let _ = sink.close().await;
    let _ = events.send(SftpEvent::Progress {
        label,
        done,
        total: done.max(total),
    });

    Ok(format!(
        "已上传 {} → {remote}",
        local.display()
    ))
}

/// Joins a remote (POSIX) path. `Path::join` would use `\` on Windows and
/// mangle the name, so remote paths are assembled by hand.
pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

/// Parent of a remote path, never escaping `/`.
pub fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// Human-readable size for the panel.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_remote_handles_root_and_trailing_slashes() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("", "etc"), "/etc");
        assert_eq!(join_remote("/home/root/", "file"), "/home/root/file");
        assert_eq!(join_remote("/home/root", "file"), "/home/root/file");
    }

    #[test]
    fn parent_remote_stops_at_root() {
        assert_eq!(parent_remote("/home/root/a"), "/home/root");
        assert_eq!(parent_remote("/home/"), "/");
        assert_eq!(parent_remote("/"), "/");
        assert_eq!(parent_remote("relative"), "/");
    }

    #[test]
    fn basename_ignores_trailing_slash() {
        assert_eq!(basename("/var/log/syslog"), "syslog");
        assert_eq!(basename("/var/log/"), "log");
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 M");
    }
}
