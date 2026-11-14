//! SFTP file-window backend.
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

pub type Reply = Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>;

#[derive(Debug, Clone)]
pub struct Conflict {
    pub path: String,
    pub reply: Reply,
}

#[derive(Debug, Clone)]
pub enum SftpEvent {
    Overwrite(Conflict),
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
                    Ok(active) => transfer(active, &local, &remote, false, &events)
                        .await
                        .map(|done| {
                            let _ = events.send(SftpEvent::Done(done));
                        }),
                    Err(err) => Err(err),
                }
            },
            Command::Upload { local, remote } => {
                match attach(&slot, &mut live).await {
                    Ok(active) => transfer(active, &local, &remote, true, &events)
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

// Walk explicitly so directory depth never grows the async call stack.
async fn transfer(active: &mut Live, local: &Path, remote: &str, uploading: bool,
    events: &UnboundedSender<SftpEvent>) -> Result<String, String> {
    let mut pending = vec![(local.to_path_buf(), remote.to_string())];
    let mut skipped = 0;
    while let Some((local, remote)) = pending.pop() {
        let local_meta = match tokio::fs::symlink_metadata(&local).await {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("读取 {} 失败: {e}", local.display())),
        };
        let remote_meta = match active.session.symlink_metadata(&remote).await {
            Ok(m) => Some(m),
            Err(russh_sftp::client::error::Error::Status(s))
                if s.status_code == russh_sftp::protocol::StatusCode::NoSuchFile => None,
            Err(e) => return Err(format!("读取 {remote} 失败: {e}")),
        };
        // Never follow links while recursively copying or replacing destinations.
        if local_meta.as_ref().is_some_and(|m| m.file_type().is_symlink())
            || remote_meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
            return Err(format!("暂不传输或覆盖符号链接: {} / {remote}", local.display()));
        }
        let is_dir = if uploading {
            local_meta.as_ref().ok_or("本地源不存在")?.is_dir()
        } else {
            remote_meta.as_ref().ok_or("远程源不存在")?.is_dir()
        };
        if uploading && !is_dir && !local_meta.as_ref().is_some_and(|m| m.is_file()) {
            return Err(format!("只支持普通文件与目录: {}", local.display()));
        }
        if !uploading && !is_dir && !remote_meta.as_ref().is_some_and(|m| m.file_type().is_file()) {
            return Err(format!("只支持普通文件与目录: {remote}"));
        }
        let target_dir = if uploading { remote_meta.as_ref().map(|m| m.is_dir()) }
            else { local_meta.as_ref().map(|m| m.is_dir()) };
        if let Some(target_dir) = target_dir {
            if target_dir != is_dir {
                return Err(format!("文件与目录类型冲突，请先更名: {} / {remote}", local.display()));
            }
            {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let conflict = Conflict { path: if uploading { remote.clone() } else { local.display().to_string() },
                    reply: Arc::new(std::sync::Mutex::new(Some(tx))) };
                events.send(SftpEvent::Overwrite(conflict)).map_err(|_| "窗口已关闭")?;
                if !rx.await.unwrap_or(false) { skipped += 1; continue; }
            }
        }
        if is_dir {
            if uploading {
                if target_dir.is_none() { active.session.create_dir(&remote).await.map_err(|e| e.to_string())?; }
                let mut entries = tokio::fs::read_dir(&local).await.map_err(|e| e.to_string())?;
                while let Some(e) = entries.next_entry().await.map_err(|e| e.to_string())? {
                    let name = e.file_name().into_string().map_err(|_| "文件名不是 UTF-8")?;
                    pending.push((e.path(), join_remote(&remote, &name)));
                }
            } else {
                tokio::fs::create_dir_all(&local).await.map_err(|e| e.to_string())?;
                for entry in active.session.read_dir(&remote).await.map_err(|e| e.to_string())? {
                    let name = entry.file_name();
                    if name == "." || name == ".." { continue; }
                    if name.contains('/') || name.contains('\\') { return Err("远程文件名不安全".into()); }
                    pending.push((local.join(&name), join_remote(&remote, &name)));
                }
            }
        } else if uploading { upload(active, &local, &remote, events).await?; }
        else { download(active, &remote, &local, events).await?; }
    }
    Ok(format!("{}完成: {} · 跳过 {skipped} 项", if uploading { "上传" } else { "下载" }, local.display()))
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
    sink.close().await.map_err(|e| format!("关闭远程文件失败: {e}"))?;
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

// Use the OS SFTP subsystem over stdio: exercises the real protocol and disk
// writes without network access, SSH credentials, or Python dependencies.
#[cfg(all(test, target_os = "macos"))]
mod transfer_tests {
    use super::*;

    async fn copy(live: &mut Live, local: &Path, remote: &str, upload: bool,
        approve: bool) -> (Result<String, String>, usize) {
        let (tx, mut rx) = unbounded_channel();
        let transfer = transfer(live, local, remote, upload, &tx);
        tokio::pin!(transfer);
        let mut conflicts = 0;
        loop {
            tokio::select! {
                result = &mut transfer => return (result, conflicts),
                event = rx.recv() => if let Some(SftpEvent::Overwrite(c)) = event {
                    conflicts += 1;
                    // The worker must remain paused until a decision arrives.
                    assert!(tokio::time::timeout(Duration::from_millis(20), &mut transfer).await.is_err());
                    c.reply.lock().unwrap().take().unwrap().send(approve).unwrap();
                },
            }
        }
    }

    #[tokio::test]
    async fn recursive_roundtrip_and_overwrite_decisions() {
        let mut child = tokio::process::Command::new("/usr/libexec/sftp-server")
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .kill_on_drop(true).spawn().unwrap();
        let stream = tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let session = SftpSession::new(stream).await.unwrap();
        let mut live = Live { generation: 0, session };
        let root = std::env::temp_dir().join(format!("ssh-recursive-test-{}", std::process::id()));
        let source = root.join("source");
        let remote = root.join("remote");
        let target = root.join("download");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(source.join("中文/empty")).unwrap();
        let payload = vec![42u8; CHUNK * 3 + 17];
        std::fs::write(source.join("中文/data.bin"), &payload).unwrap();
        std::fs::write(source.join("hello.txt"), "original").unwrap();
        let remote_path = remote.to_str().unwrap();
        let (result, conflicts) = copy(&mut live, &source, remote_path, true, true).await;
        result.unwrap(); assert_eq!(conflicts, 0);
        assert!(remote.join("中文/empty").is_dir());
        assert_eq!(std::fs::read(remote.join("中文/data.bin")).unwrap(), payload);
        copy(&mut live, &target, remote_path, false, true).await.0.unwrap();
        assert!(target.join("中文/empty").is_dir());
        assert_eq!(std::fs::read(target.join("中文/data.bin")).unwrap(), payload);

        std::fs::write(source.join("hello.txt"), "updated").unwrap();
        let remote_file = remote.join("hello.txt");
        let local_file = source.join("hello.txt");
        let (result, conflicts) = copy(&mut live, &local_file, remote_file.to_str().unwrap(), true, false).await;
        result.unwrap(); assert_eq!(conflicts, 1);
        assert_eq!(std::fs::read_to_string(&remote_file).unwrap(), "original");
        copy(&mut live, &local_file, remote_file.to_str().unwrap(), true, true).await.0.unwrap();
        assert_eq!(std::fs::read_to_string(&remote_file).unwrap(), "updated");

        let downloaded = target.join("hello.txt");
        copy(&mut live, &downloaded, remote_file.to_str().unwrap(), false, false).await.0.unwrap();
        assert_eq!(std::fs::read_to_string(&downloaded).unwrap(), "original");
        copy(&mut live, &downloaded, remote_file.to_str().unwrap(), false, true).await.0.unwrap();
        assert_eq!(std::fs::read_to_string(&downloaded).unwrap(), "updated");
        // Declining an existing directory skips its entire subtree.
        let (_, conflicts) = copy(&mut live, &source, remote_path, true, false).await;
        assert_eq!(conflicts, 1);
        // File/directory conflicts fail without replacing either side.
        assert!(copy(&mut live, &local_file, remote_path, true, true).await.0.is_err());
        assert!(remote.is_dir());
        live.session.close().await.unwrap();
        child.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
