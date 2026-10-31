//! Hidden vaults: the vault folder shouldn't reveal how many vaults exist
//! or which files are real.

use whisker_core::AccountManager;

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
        assert_eq!(m.len(), meta[0].len());
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
