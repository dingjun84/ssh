//! Host key verification, OpenSSH-compatible.
//!
//! We trust `~/.ssh/known_hosts` as the single source of truth instead of
//! inventing a private store: entries learned here are also honoured by the
//! `ssh` command line, and vice versa. Trust is trust-on-first-use — the first
//! contact pins the key, and any later mismatch aborts the connection.

use std::path::{Path, PathBuf};

use russh::keys::known_hosts::{known_host_keys_path, learn_known_hosts_path};
use russh::keys::ssh_key::{HashAlg, PublicKey};

#[derive(Debug, Clone, PartialEq)]
pub enum Trust {
    /// Fingerprint already recorded and it matches.
    Known,
    /// First contact with this host; the key was just pinned.
    Learned,
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub fingerprint: String,
    pub trust: Trust,
}

/// Verifies `key` against the user's `~/.ssh/known_hosts`.
pub fn verify(host: &str, port: u16, key: &PublicKey) -> Result<Verdict, String> {
    verify_at(&known_hosts_path()?, host, port, key)
}

pub fn verify_at(
    path: &Path,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<Verdict, String> {
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let recorded = known_host_keys_path(host, port, path)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;

    // Compare fingerprints rather than `PublicKey` values: the derived
    // equality also compares the key *comment*, so a `known_hosts` entry and a
    // key straight off the wire would look different even when the key
    // material is identical.
    let mut same_algorithm = Vec::new();
    for (line, recorded_key) in recorded {
        if recorded_key.algorithm() != key.algorithm() {
            continue;
        }
        if recorded_key.fingerprint(HashAlg::Sha256).to_string() == fingerprint {
            return Ok(Verdict {
                fingerprint,
                trust: Trust::Known,
            });
        }
        same_algorithm.push((line, recorded_key));
    }

    if let Some((line, recorded_key)) = same_algorithm.first() {
        let recorded_fp = recorded_key.fingerprint(HashAlg::Sha256).to_string();
        return Err(format!(
            "主机密钥已变更！服务器 {host}:{port} 出示 {fingerprint}，\
             但 {} 第 {line} 行记录的是 {recorded_fp}。\
             可能是中间人攻击；确认无误后删除该行再重连。",
            path.display()
        ));
    }

    // Either the host is new, or only keys of another algorithm are recorded.
    // Both count as first contact for this algorithm, same as OpenSSH.
    learn_known_hosts_path(host, port, key, path)
        .map_err(|e| format!("无法向 {} 记录主机密钥: {e}", path.display()))?;
    Ok(Verdict {
        fingerprint,
        trust: Trust::Learned,
    })
}

fn known_hosts_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SSH_KNOWN_HOSTS") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or("拿不到 HOME 目录")?;
    Ok(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two throwaway ed25519 public keys, used only to prove that pinning
    /// distinguishes one key from another.
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMxlWrDpSig+VPIWxamOH5T/yEipqSCFuKEjl2L7NEw4 test1";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAdJIKdmPX6zes+C1/5uyKzye9WA8q1/5xxkgHjfFUtQ test2";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("ssh-client-hostkey-{nanos}"));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn known_hosts(&self) -> PathBuf {
            self.0.join("known_hosts")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key(openssh: &str) -> PublicKey {
        PublicKey::from_openssh(openssh).unwrap()
    }

    #[test]
    fn first_contact_pins_and_second_matches() {
        let dir = TempDir::new();
        let path = dir.known_hosts();
        let key = key(KEY_A);

        let v = verify_at(&path, "example.com", 22, &key).unwrap();
        assert_eq!(v.trust, Trust::Learned);
        assert!(v.fingerprint.starts_with("SHA256:"), "{}", v.fingerprint);

        let v = verify_at(&path, "example.com", 22, &key).unwrap();
        assert_eq!(v.trust, Trust::Known);
    }

    #[test]
    fn changed_key_is_rejected() {
        let dir = TempDir::new();
        let path = dir.known_hosts();

        verify_at(&path, "example.com", 22, &key(KEY_A)).unwrap();
        let err = verify_at(&path, "example.com", 22, &key(KEY_B)).unwrap_err();
        assert!(err.contains("主机密钥已变更"), "{err}");
    }

    #[test]
    fn non_standard_port_is_namespaced() {
        let dir = TempDir::new();
        let path = dir.known_hosts();
        let key = key(KEY_A);

        verify_at(&path, "example.com", 2222, &key).unwrap();
        // Same host on 22 is a different endpoint and must be learned again.
        let v = verify_at(&path, "example.com", 22, &key).unwrap();
        assert_eq!(v.trust, Trust::Learned);
    }
}
