//! End-to-end against matrix-sdk's mock homeserver: log in, sync, and see
//! the rooms (with names and message previews) appear.

use matrix_sdk::ruma::{room_id, user_id};
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::JoinedRoomBuilder;
use matrix_sdk_test::event_factory::EventFactory;
use std::time::Duration;
use whisker_core::{AccountKind, AccountManager, ClientEvent, LoginRequest};

#[tokio::test(flavor = "multi_thread")]
async fn rooms_show_up_after_first_sync() {
    let server = MatrixMockServer::new().await;
    server.mock_versions().ok().mount().await;
    server.mock_login().ok().mount().await;
    server.mock_upload_keys().ok().mount().await;
    server.mock_query_keys().ok().mount().await;

    let room = room_id!("!cats:matrix.org");
    let f = EventFactory::new().room(room).sender(user_id!("@alice:matrix.org"));
    server
        .mock_sync()
        .ok(|b| {
            b.add_joined_room(
                JoinedRoomBuilder::new(room)
                    .add_state_event(f.room_name("Cat room"))
                    .add_timeline_event(f.text_msg("meow")),
            );
        })
        .mount()
        .await;

    let dir = std::env::temp_dir().join(format!("whisker-test-{}", whisker_core::vault::random_secret()));
    let mgr = AccountManager::new(&dir);
    let vault = mgr.create_vault("test passphrase").await.unwrap();
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

    // Before this fix, nothing refreshed the list after the first sync.
    let mut saw_message = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await.unwrap() {
                ClientEvent::Message { body, is_text, .. } if body == "meow" => {
                    assert!(is_text);
                    saw_message = true;
                }
                ClientEvent::Synced { account } if account == id => break,
                _ => {}
            }
        }
    })
    .await
    .expect("no sync within 20s");
    assert!(saw_message, "message handler didn't fire");

    let rooms = mgr.rooms(id).await;
    assert_eq!(rooms.len(), 1, "{rooms:?}");
    assert_eq!(rooms[0].name, "Cat room");
    assert_eq!(rooms[0].latest.as_ref().map(|l| l.body.as_str()), Some("meow"));

    // The account is stored in the vault and comes back after a relock.
    mgr.lock_all().await;
    assert!(mgr.accounts().await.is_empty());

    // Unlocking must not wait on the server: make every request hang.
    server.server().reset().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(120)))
        .mount(server.server())
        .await;
    let opened = tokio::time::timeout(Duration::from_secs(10), mgr.unlock("test passphrase"))
        .await
        .expect("unlock waited on a hanging server")
        .unwrap();
    assert_eq!(opened, vec![vault]);
    assert_eq!(mgr.accounts().await.len(), 1);

    // Logging out doesn't wait on the (still hanging) server, a second
    // click is harmless, and the account stays gone after a relock.
    tokio::time::timeout(Duration::from_secs(5), mgr.logout(id))
        .await
        .expect("logout waited on the server")
        .unwrap();
    assert!(mgr.accounts().await.is_empty());
    mgr.logout(id).await.unwrap();
    mgr.lock_all().await;
    mgr.unlock("test passphrase").await.unwrap();
    assert!(mgr.accounts().await.is_empty());

    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}
