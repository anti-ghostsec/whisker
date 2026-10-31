//! Public room directory search and joining by address.

use matrix_sdk::ruma::room_id;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use serde_json::json;
use whisker_core::timeline;
use wiremock::matchers::{body_partial_json, method, path, path_regex, query_param};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
async fn searches_directory() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/publicRooms"))
        .and(query_param("server", "matrix.org"))
        .and(body_partial_json(json!({"filter": {"generic_search_term": "rust"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [{
                "room_id": "!rust:matrix.org", "name": "Rust", "canonical_alias": "#rust:matrix.org",
                "topic": "The Rust language", "num_joined_members": 4200,
                "world_readable": true, "guest_can_join": false
            }],
            "next_batch": "p2"
        })))
        .mount(server.server())
        .await;

    let (rooms, next) = timeline::public_rooms(&client, Some("matrix.org"), "rust", None).await.unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].name, "Rust");
    assert_eq!(rooms[0].alias.as_deref(), Some("#rust:matrix.org"));
    assert_eq!(rooms[0].members, 4200);
    assert_eq!(next.as_deref(), Some("p2"));

    assert!(timeline::public_rooms(&client, Some("not a server!"), "", None).await.is_err());
}

#[tokio::test]
async fn joins_by_address() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let id = room_id!("!rust:matrix.org");
    // The address is looked up first, then joined via the room's server.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/directory/room/(%23|#)rust:matrix.org$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "room_id": id, "servers": ["matrix.org"] })))
        .mount(server.server())
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/v3/join/(%23|#)rust:matrix.org$"))
        .and(query_param("via", "matrix.org"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "room_id": id })))
        .mount(server.server())
        .await;
    let room = timeline::join(&client, "#rust:matrix.org").await.unwrap();
    assert_eq!(room.room_id(), id);
    assert!(timeline::join(&client, "rust").await.is_err());
}

#[tokio::test]
async fn falls_back_to_asking_the_other_server() {
    use whisker_core::rooms;
    let own = MatrixMockServer::new().await;
    let client = own.client_builder().build().await;
    // Our homeserver won't fetch other directories over federation.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/publicRooms"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"errcode": "M_FORBIDDEN", "error": "not allowed"})))
        .mount(own.server())
        .await;
    // The other server answers anonymous GETs.
    let other = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/publicRooms"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [
                {"room_id": "!a:o", "name": "Cats", "topic": "meow", "num_joined_members": 3},
                {"room_id": "!b:o", "name": "Dogs", "num_joined_members": 5}
            ],
            "next_batch": "n2"
        })))
        .mount(&other)
        .await;
    let host = other.address().to_string(); // 127.0.0.1:port

    let (rooms, next) = rooms::public_rooms(&client, &format!("http://{host}/"), "cat", None).await.unwrap();
    assert_eq!(rooms.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["Cats"]);
    assert_eq!(next.as_deref(), Some("n2"));
}

#[tokio::test]
async fn creates_encrypted_private_room() {
    use whisker_core::rooms::{NewRoom, create_room};
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .and(body_partial_json(json!({
            "name": "Plans", "preset": "private_chat", "invite": ["@friend:localhost"],
            "initial_state": [{"type": "m.room.encryption", "state_key": ""}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"room_id": "!new:localhost"})))
        .mount(server.server())
        .await;
    let room = create_room(&client, NewRoom {
        name: "Plans".into(), topic: String::new(), public: false, alias: String::new(), encrypted: true,
        invite: vec!["@friend:localhost".try_into().unwrap()],
    }).await.unwrap();
    assert_eq!(room.room_id(), "!new:localhost");
}
