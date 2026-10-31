//! Hidden vaults: the vault folder shouldn't reveal how many vaults exist
//! or which files are real.

use matrix_sdk::test_utils::mocks::MatrixMockServer;
use std::path::Path;
use std::time::Duration;
use whisker_core::{AccountKind, AccountManager, ClientEvent, LoginRequest};

/// Every file under `dir`, relative.
fn files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files(&p).into_iter().map(|f| format!("{}/{f}", e.file_name().to_string_lossy())));
        } else {
            out.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn account_stores_live_in_ram_and_inside_the_vault() {
    let server = MatrixMockServer::new().await;
    server.mock_versions().ok().mount().await;
    wiremock::Mock::given(wiremock::matchers::path("/_matrix/client/v3/login"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "user_id": "@example:localhost", "access_token": "tok", "device_id": "DEV"
        })))
        .mount(server.server())
        .await;
    server.mock_upload_keys().ok().mount().await;
    server.mock_query_keys().ok().mount().await;
    server.mock_sync().ok(|_| {}).mount().await;

    let dir = std::env::temp_dir().join(format!("whisker-test-{}", whisker_core::vault::random_secret()));
    let mgr = AccountManager::new(&dir);
    let vault = mgr.create_vault("pass phrase").await.unwrap();
    let mut rx = mgr.subscribe();
    let id = mgr
        .login(LoginRequest {
            vault: &vault,
            register: false,
            token: None,
            homeserver: &server.uri(),
            username: "example",
            password: "wordpass",
            kind: AccountKind::Human,
            tags: Default::default(),
            proxy: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !matches!(rx.recv().await, Ok(ClientEvent::Synced { account }) if account == id) {}
    })
    .await
    .unwrap();
    let key = mgr.client(id).await.unwrap().encryption().ed25519_key().await.unwrap();

    // Running: the store is in the RAM folder, and only vault files are on disk.
    let running = whisker_core::stores::runtime_root(&dir).join(id.to_string());
    assert!(whisker_core::stores::has_store(&running.join("store")));
    assert!(files(&dir).iter().all(|f| f.starts_with("vaults/") && f.ends_with(".vault")), "{:?}", files(&dir));

    // Locked: the RAM copy is gone; unlocking brings back the same device.
    mgr.lock_all().await;
    assert!(!running.exists());
    mgr.unlock("pass phrase").await.unwrap();
    assert_eq!(mgr.client(id).await.unwrap().encryption().ed25519_key().await.unwrap(), key);
    mgr.lock_all().await;

    // A store folder left by an older version moves into the vault.
    let path = std::fs::read_dir(dir.join("vaults"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| whisker_core::vault::Vault::open::<serde_json::Value>(p, "pass phrase").is_ok())
        .unwrap();
    let (v, profile, att) = whisker_core::vault::Vault::open_with::<serde_json::Value>(&path, "pass phrase").unwrap();
    let legacy = dir.join("stores").join(id.to_string());
    whisker_core::stores::unpack(&att[&format!("store/{id}")], &legacy).unwrap();
    v.save(&profile).unwrap();
    drop(v);
    mgr.unlock("pass phrase").await.unwrap();
    assert_eq!(mgr.client(id).await.unwrap().encryption().ed25519_key().await.unwrap(), key);
    assert!(!dir.join("stores").exists(), "old store folder still there");
    mgr.lock_all().await;
    let (_, _, att) = whisker_core::vault::Vault::open_with::<serde_json::Value>(&path, "pass phrase").unwrap();
    assert!(att.contains_key(&format!("store/{id}")));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn vault_folder_does_not_reveal_hidden_vaults() {
    let dir = std::env::temp_dir().join(format!("whisker-test-{}", whisker_core::vault::random_secret()));
    let mgr = AccountManager::new(&dir);
    mgr.create_vault("main passphrase").await.unwrap();
    mgr.create_vault("hidden passphrase").await.unwrap();
    mgr.lock_all().await;

    let files: Vec<_> = std::fs::read_dir(dir.join("vaults")).unwrap().flatten().map(|e| e.path()).collect();
    assert!(files.iter().all(|p| p.extension().is_some_and(|x| x == "vault")), "{files:?}");
    // Decoys outnumber the two real vaults, so the count says nothing.
    assert!(files.len() >= 5, "only {} files", files.len());
    let meta: Vec<_> = files.iter().map(|p| std::fs::metadata(p).unwrap()).collect();
    for m in &meta {
        assert!(whisker_core::vault::is_stepped(m.len()), "size {}", m.len());
        assert_eq!(m.modified().unwrap(), meta[0].modified().unwrap());
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777, 0o600);
    }

    // Each passphrase opens its own vault and nothing else.
    assert_eq!(mgr.unlock("main passphrase").await.unwrap().len(), 1);
    assert_eq!(mgr.unlock("hidden passphrase").await.unwrap().len(), 1);
    assert!(mgr.unlock("wrong").await.unwrap().is_empty());
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}
