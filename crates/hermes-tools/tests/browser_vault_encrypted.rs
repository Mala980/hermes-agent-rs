//! Integration verification for the encrypted `browser_vault` file backend.
//!
//! Mirrors the in-module unit tests in `tools::browser` through the public
//! API only, so the suite can run even while other `#[cfg(test)]` modules in
//! this crate are mid-edit by sibling workers.

use hermes_tools::tools::browser::{FileVaultBackend, VaultBackend, VaultItemKind};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn backend_in(dir: &Path) -> FileVaultBackend {
    // `path` is the legacy plaintext location; key + encrypted store live in
    // its parent directory.
    FileVaultBackend::new(dir.join("vault.json"))
}

fn mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

#[test]
fn roundtrip_and_no_secret_in_list() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_path = dir.path().join("vault.json");
    let backend = backend_in(dir.path());
    let rt = rt();
    let handle = rt
        .block_on(backend.save_login(
            "Example",
            "user@example.com",
            "https://example.com",
            "s3cr3t",
        ))
        .unwrap();
    assert!(!handle.is_empty());
    let items = rt.block_on(backend.list_items()).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].identifier.as_deref(), Some("user@example.com"));
    assert_eq!(items[0].kind, VaultItemKind::Login);
    let list_json = serde_json::to_string(&items).unwrap();
    assert!(!list_json.contains("s3cr3t"));
    // Secret round-trips for fill.
    let item = rt.block_on(backend.get_item(&handle)).unwrap();
    assert_eq!(item.secret, "s3cr3t");
    // Same origin+identifier replaces instead of duplicating.
    rt.block_on(backend.save_login("Example", "user@example.com", "https://example.com", "n3w"))
        .unwrap();
    assert_eq!(rt.block_on(backend.list_items()).unwrap().len(), 1);
    // Encrypted store holds no plaintext secret; no legacy file was created.
    let enc_path = dir.path().join("vault.json.enc");
    assert!(enc_path.exists());
    let enc_bytes = std::fs::read(&enc_path).unwrap();
    assert!(!enc_bytes.windows(6).any(|w| w == b"s3cr3t"));
    assert!(!legacy_path.exists());
    #[cfg(unix)]
    {
        assert_eq!(mode(&enc_path), 0o600);
        assert_eq!(mode(&dir.path().join("vault.key")), 0o600);
    }
}

#[test]
fn envelope_shape_and_decrypt_roundtrip_via_fill() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_in(dir.path());
    let rt = rt();
    rt.block_on(backend.save_login("E", "u", "https://example.com", "pw"))
        .unwrap();
    let enc_path = dir.path().join("vault.json.enc");
    let raw = std::fs::read(&enc_path).unwrap();
    let env: Value = serde_json::from_str(&String::from_utf8(raw).unwrap()).unwrap();
    assert_eq!(env["format"], "hermes-vault/1");
    assert_eq!(env["nonce"].as_str().unwrap().len(), 32);
    assert!(!env["nonce"].as_str().unwrap().is_empty());
    assert!(!env["payload"].as_str().unwrap().is_empty());
}

#[test]
fn tamper_detection_ciphertext_and_tag() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_in(dir.path());
    let rt = rt();
    rt.block_on(backend.save_login("Example", "u", "https://example.com", "s3cr3t"))
        .unwrap();
    let enc_path = dir.path().join("vault.json.enc");
    let pristine = std::fs::read(&enc_path).unwrap();

    for tamper in ["ciphertext", "tag"] {
        let mut env: Value =
            serde_json::from_str(&String::from_utf8(pristine.clone()).unwrap()).unwrap();
        let payload = env["payload"].as_str().unwrap().to_string();
        let mut chars: Vec<char> = payload.chars().collect();
        let idx = if tamper == "ciphertext" {
            0
        } else {
            chars.len() - 1
        };
        chars[idx] = if chars[idx] == 'a' { 'b' } else { 'a' };
        env["payload"] = Value::String(chars.into_iter().collect());
        std::fs::write(&enc_path, serde_json::to_vec(&env).unwrap()).unwrap();
        let err = rt.block_on(backend.list_items()).unwrap_err();
        assert!(
            err.to_string().contains("decryption failed"),
            "tamper {tamper} was not detected: {err}"
        );
        std::fs::write(&enc_path, &pristine).unwrap();
    }
    // Store is healthy again.
    assert_eq!(rt.block_on(backend.list_items()).unwrap().len(), 1);
}

#[test]
fn permissions_enforced() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let backend = backend_in(dir.path());
    rt().block_on(backend.save_login("Example", "u", "https://example.com", "pw"))
        .unwrap();
    #[cfg(unix)]
    {
        assert_eq!(mode(dir.path()), 0o700, "vault dir");
        assert_eq!(mode(&dir.path().join("vault.key")), 0o600, "vault.key");
        assert_eq!(
            mode(&dir.path().join("vault.json.enc")),
            0o600,
            "vault.json.enc"
        );
    }
}

#[test]
fn key_file_reused_across_instances() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt();
    let backend = backend_in(dir.path());
    rt.block_on(backend.save_login("Example", "u", "https://example.com", "pw"))
        .unwrap();
    let key_before = std::fs::read(dir.path().join("vault.key")).unwrap();
    assert_eq!(key_before.len(), 64);
    let backend2 = backend_in(dir.path());
    assert_eq!(rt.block_on(backend2.list_items()).unwrap().len(), 1);
    assert_eq!(
        std::fs::read(dir.path().join("vault.key")).unwrap(),
        key_before
    );
}

#[test]
fn migration_from_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_path = dir.path().join("vault.json");
    let backend = backend_in(dir.path());
    // Simulate an old plaintext vault (cleartext `secret`, legacy shape).
    let legacy = json!([{
        "handle": "legacy1",
        "label": "Old",
        "kind": "login",
        "origin": "https://old.example",
        "allowed_origins": [],
        "identifier": "olduser",
        "secret": "oldsecret",
        "has_otp": false,
    }]);
    std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

    let rt = rt();
    let items = rt.block_on(backend.list_items()).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].handle, "legacy1");
    let item = rt.block_on(backend.get_item("legacy1")).unwrap();
    assert_eq!(item.secret, "oldsecret");
    // Legacy file renamed, never deleted silently.
    assert!(!legacy_path.exists());
    assert!(dir.path().join("vault.json.migrated").exists());
    let enc = std::fs::read(dir.path().join("vault.json.enc")).unwrap();
    assert!(!enc.windows(9).any(|w| w == b"oldsecret"));
    // Second load is idempotent.
    assert_eq!(rt.block_on(backend.list_items()).unwrap().len(), 1);
}

#[test]
fn migration_merges_with_encrypted_store() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_path = dir.path().join("vault.json");
    let backend = backend_in(dir.path());
    let rt = rt();
    let new_handle = rt
        .block_on(backend.save_login("New", "n", "https://new.example", "newsecret"))
        .unwrap();
    let legacy = json!([
        {
            "handle": new_handle,
            "label": "New",
            "kind": "login",
            "origin": "https://new.example",
            "allowed_origins": [],
            "identifier": "n",
            "secret": "stale",
            "has_otp": false,
        },
        {
            "handle": "legacy2",
            "label": "Old",
            "kind": "login",
            "origin": "https://old.example",
            "allowed_origins": [],
            "identifier": "o",
            "secret": "oldsecret",
            "has_otp": false,
        },
    ]);
    std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let items = rt.block_on(backend.list_items()).unwrap();
    assert_eq!(items.len(), 2);
    // The encrypted copy wins for the duplicated handle.
    assert_eq!(
        rt.block_on(backend.get_item(&new_handle)).unwrap().secret,
        "newsecret"
    );
}

#[tokio::test]
async fn keyfile_based_no_unlock_gate() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend_in(dir.path());
    // Key-file based: no unlock gate (unlike external manager backends).
    assert!(!backend.needs_unlock());
    assert!(backend.is_unlocked());
    assert!(backend.unlock().await.is_err());
    // default_path stays the legacy location (migration source).
    let p: PathBuf = FileVaultBackend::default_path();
    assert!(p.ends_with("browser_vault.json"));
}
