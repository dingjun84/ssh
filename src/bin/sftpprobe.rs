//! Real-connection probe for the SFTP panel.
//!
//! Reuses the exact modules the GUI uses (`ssh.rs` opens the session, `sftp.rs`
//! runs the worker) so a pass here means the panel's code paths work, not just
//! that some parallel implementation does. Point it at any host:
//!
//! ```text
//! LAB_HOST=127.0.0.1 LAB_PORT=2244 LAB_USER=lab LAB_PASS=labpass \
//!   cargo run --bin sftpprobe --offline
//! ```
//!
//! Exit code 0 = every check passed.
//!
//! The GUI modules are pulled in with `#[path]` instead of being shared through
//! a lib target, so most of their surface is unused from here.
#![allow(dead_code)]

#[path = "../hostkey.rs"]
mod hostkey;
#[path = "../sftp.rs"]
mod sftp;
#[path = "../ssh.rs"]
mod ssh;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let host = env_or("LAB_HOST", "127.0.0.1");
    let port: u16 = env_or("LAB_PORT", "2244")
        .parse()
        .map_err(|e| format!("LAB_PORT 不是端口号: {e}"))?;
    let user = env_or("LAB_USER", "lab");
    let password = env_or("LAB_PASS", "labpass");
    let dir = PathBuf::from(env_or("LAB_OUT", "/tmp/sftp-lab/out"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("建本地目录失败: {e}"))?;

    println!("== 1. SSH 会话 ==");
    let rt = tokio::runtime::Handle::current();
    let (_transport, mut events, slot) = ssh::spawn(
        &rt,
        ssh::SshRequest {
            host: host.clone(),
            port,
            user,
            auth: ssh::Auth::Password(password),
            cols: 120,
            rows: 32,
        },
    );
    wait_connected(&mut events).await?;
    println!("   已连接 {host}:{port}");

    println!("== 2. SFTP 子系统 ==");
    let (handle, mut sftp_events) = sftp::spawn(&rt, slot.clone());

    handle.send(sftp::Command::List("/".into()));
    let entries = expect_listing(&mut sftp_events).await?;
    println!("   目录 / → {} 项", entries.len());
    for entry in &entries {
        println!(
            "     {:<12} {:>6}  {}",
            if entry.is_dir { "dir" } else { "file" },
            sftp::human_size(entry.size),
            entry.name
        );
    }
    assert_entry(&entries, "hello.txt", false)?;
    assert_entry(&entries, "sub", true)?;
    assert_entry(&entries, "big.bin", false)?;

    println!("== 3. 小文件下载 ==");
    let hello = dir.join("hello.txt");
    let _ = std::fs::remove_file(&hello);
    handle.send(sftp::Command::Download {
        remote: "/hello.txt".into(),
        local: hello.clone(),
    });
    expect_done(&mut sftp_events).await?;
    let text = std::fs::read_to_string(&hello).map_err(|e| format!("读回下载文件失败: {e}"))?;
    if text != "hello from the sftp lab\n" {
        return Err(format!("下载内容不符: {text:?}"));
    }
    println!("   hello.txt 内容一致");

    println!("== 4. 4 MiB 大文件下载（分块 + 进度）==");
    let big = dir.join("big.bin");
    let _ = std::fs::remove_file(&big);
    let started = std::time::Instant::now();
    handle.send(sftp::Command::Download {
        remote: "/big.bin".into(),
        local: big.clone(),
    });
    let progress = expect_done_counting(&mut sftp_events).await?;
    let got = std::fs::read(&big).map_err(|e| format!("读回 big.bin 失败: {e}"))?;
    let want = std::fs::read("/tmp/sftp-lab/root/big.bin")
        .map_err(|e| format!("读源文件 big.bin 失败: {e}"))?;
    if got != want {
        return Err(format!(
            "big.bin 字节不一致: 收到 {} 字节，期望 {} 字节",
            got.len(),
            want.len()
        ));
    }
    println!(
        "   {} 字节逐字节一致，{} 条进度事件，耗时 {:.2}s",
        got.len(),
        progress,
        started.elapsed().as_secs_f64()
    );

    println!("== 5. 上传到根目录并回落校验 ==");
    let source = dir.join("upload.txt");
    let payload = "uploaded by sftpprobe\n第二行：中文与 emoji 🚀\n";
    std::fs::write(&source, payload).map_err(|e| format!("写本地源文件失败: {e}"))?;
    handle.send(sftp::Command::Upload {
        local: source.clone(),
        remote: "/uploaded.txt".into(),
    });
    expect_done(&mut sftp_events).await?;
    let landed = std::fs::read_to_string("/tmp/sftp-lab/root/uploaded.txt")
        .map_err(|e| format!("读远端落盘文件失败: {e}"))?;
    if landed != payload {
        return Err(format!("上传内容不符: {landed:?}"));
    }
    println!("   服务端落盘内容一致（含 UTF-8 与 emoji）");

    println!("== 6. 上传到子目录 ==");
    handle.send(sftp::Command::Upload {
        local: source.clone(),
        remote: "/sub/uploaded2.txt".into(),
    });
    expect_done(&mut sftp_events).await?;
    if !Path::new("/tmp/sftp-lab/root/sub/uploaded2.txt").exists() {
        return Err("子目录上传未落盘".into());
    }
    println!("   /sub/uploaded2.txt 已落盘");

    println!("== 7. 进入子目录列目录 ==");
    handle.send(sftp::Command::List("/sub".into()));
    let sub = expect_listing(&mut sftp_events).await?;
    assert_entry(&sub, "uploaded2.txt", false)?;
    println!("   子目录列表正常：{:?}", sub.iter().map(|e| e.name.as_str()).collect::<Vec<_>>());

    println!("== 8. 错误路径 ==");
    handle.send(sftp::Command::Download {
        remote: "/does-not-exist.txt".into(),
        local: dir.join("nope.txt"),
    });
    match expect_error(&mut sftp_events).await {
        Ok(message) => println!("   缺文件按预期报错: {message}"),
        Err(e) => return Err(e),
    }
    // A failed command drops the cached session; the next one must re-attach
    // on its own instead of staying poisoned.
    handle.send(sftp::Command::List("/".into()));
    expect_listing(&mut sftp_events).await?;
    println!("   报错后仍能继续列目录（会话已自动重建）");

    println!("\n全部通过。");
    Ok(())
}

async fn wait_connected(events: &mut UnboundedReceiver<ssh::SshEvent>) -> Result<(), String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), events.recv()).await {
            Ok(Some(ssh::SshEvent::HostKey {
                fingerprint,
                learned,
            })) => {
                println!(
                    "   host key {fingerprint} ({})",
                    if learned { "首次信任" } else { "已记录" }
                );
            },
            Ok(Some(ssh::SshEvent::Connected)) => return Ok(()),
            Ok(Some(ssh::SshEvent::Error(err))) => return Err(format!("连接失败: {err}")),
            Ok(Some(ssh::SshEvent::Closed)) | Ok(None) => {
                return Err("连接在握手后立即关闭".into());
            },
            Ok(Some(_)) => {},
            Err(_) => return Err("连接超时（15s）".into()),
        }
    }
}

async fn next(events: &mut UnboundedReceiver<sftp::SftpEvent>) -> Result<sftp::SftpEvent, String> {
    tokio::time::timeout(Duration::from_secs(60), events.recv())
        .await
        .map_err(|_| "等待 SFTP 事件超时（60s）".to_string())?
        .ok_or_else(|| "SFTP worker 提前退出".to_string())
}

async fn expect_listing(
    events: &mut UnboundedReceiver<sftp::SftpEvent>,
) -> Result<Vec<sftp::Entry>, String> {
    loop {
        match next(events).await? {
            sftp::SftpEvent::Overwrite(c) => { if let Some(tx) = c.reply.lock().unwrap().take() { let _ = tx.send(true); } },
            sftp::SftpEvent::Listing { path, entries } => {
                println!("   列表 {path} 返回 {} 项", entries.len());
                return Ok(entries);
            },
            sftp::SftpEvent::Progress { .. } => {},
            sftp::SftpEvent::Done(message) => println!("   {message}"),
            sftp::SftpEvent::Error(err) => return Err(format!("列目录失败: {err}")),
        }
    }
}

async fn expect_done(events: &mut UnboundedReceiver<sftp::SftpEvent>) -> Result<(), String> {
    loop {
        match next(events).await? {
            sftp::SftpEvent::Overwrite(c) => { if let Some(tx) = c.reply.lock().unwrap().take() { let _ = tx.send(true); } },
            sftp::SftpEvent::Done(message) => {
                println!("   {message}");
                return Ok(());
            },
            sftp::SftpEvent::Progress { .. } => {},
            sftp::SftpEvent::Error(err) => return Err(format!("传输失败: {err}")),
            sftp::SftpEvent::Listing { .. } => {},
        }
    }
}

async fn expect_done_counting(
    events: &mut UnboundedReceiver<sftp::SftpEvent>,
) -> Result<usize, String> {
    let mut ticks = 0;
    loop {
        match next(events).await? {
            sftp::SftpEvent::Overwrite(c) => { if let Some(tx) = c.reply.lock().unwrap().take() { let _ = tx.send(true); } },
            sftp::SftpEvent::Progress { done, total, .. } => {
                ticks += 1;
                if ticks == 1 || done == total {
                    println!("   进度 {done}/{total}");
                }
            },
            sftp::SftpEvent::Done(message) => {
                println!("   {message}");
                return Ok(ticks);
            },
            sftp::SftpEvent::Error(err) => return Err(format!("传输失败: {err}")),
            sftp::SftpEvent::Listing { .. } => {},
        }
    }
}

async fn expect_error(events: &mut UnboundedReceiver<sftp::SftpEvent>) -> Result<String, String> {
    loop {
        match next(events).await? {
            sftp::SftpEvent::Overwrite(c) => { if let Some(tx) = c.reply.lock().unwrap().take() { let _ = tx.send(true); } },
            sftp::SftpEvent::Error(err) => return Ok(err),
            sftp::SftpEvent::Done(message) => {
                return Err(format!("本应失败却成功了: {message}"));
            },
            sftp::SftpEvent::Progress { .. } | sftp::SftpEvent::Listing { .. } => {},
        }
    }
}

fn assert_entry(entries: &[sftp::Entry], name: &str, is_dir: bool) -> Result<(), String> {
    match entries.iter().find(|entry| entry.name == name) {
        Some(entry) if entry.is_dir == is_dir => Ok(()),
        Some(entry) => Err(format!(
            "{name} 类型不对：期望 is_dir={is_dir}，实际 {}",
            entry.is_dir
        )),
        None => Err(format!("列表里没有 {name}")),
    }
}
