//! Secrets live in the macOS login Keychain, never in the SQLite file.
//!
//! Service is fixed to `ssh-client`; the account name encodes which session
//! the secret belongs to, e.g. `session:3:password`.

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

const SERVICE: &str = "ssh-client";

fn account(session_id: i64, kind: &str) -> String {
    format!("session:{session_id}:{kind}")
}

fn store(session_id: i64, kind: &str, secret: &str) -> Result<(), String> {
    let account = account(session_id, kind);
    // `set_generic_password` fails with errSecDuplicateItem on an existing
    // entry, so drop the old one first instead of branching on the error.
    let _ = delete_generic_password(SERVICE, &account);
    set_generic_password(SERVICE, &account, secret.as_bytes())
        .map_err(|e| format!("写入钥匙串失败（{kind}）: {e}"))
}

fn load(session_id: i64, kind: &str) -> Result<Option<String>, String> {
    match get_generic_password(SERVICE, &account(session_id, kind)) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| format!("钥匙串中的{kind}不是合法 UTF-8: {e}")),
        // Missing item is not an error: the user may simply not have saved one.
        Err(_) => Ok(None),
    }
}

pub fn set_password(session_id: i64, password: &str) -> Result<(), String> {
    store(session_id, "password", password)
}

pub fn password(session_id: i64) -> Result<Option<String>, String> {
    load(session_id, "password")
}

pub fn set_passphrase(session_id: i64, passphrase: &str) -> Result<(), String> {
    store(session_id, "passphrase", passphrase)
}

pub fn passphrase(session_id: i64) -> Result<Option<String>, String> {
    load(session_id, "passphrase")
}

/// Called when a session is deleted so no orphaned secrets stay behind.
pub fn forget(session_id: i64) {
    for kind in ["password", "passphrase"] {
        let _ = delete_generic_password(SERVICE, &account(session_id, kind));
    }
}
