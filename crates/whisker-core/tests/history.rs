//! History pages, reactions and forwarding.

use matrix_sdk::ruma::{event_id, room_id};
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use serde_json::json;
use whisker_core::timeline;
use wiremock::matchers::{body_partial_json, method, path_regex};
use wiremock::{Mock, ResponseTemplate};

fn text(id: &str, body: &str, ts: u64) -> serde_json::Value {
    json!({"type": "m.room.message", "event_id": id, "sender": "@bob:localhost", "origin_server_ts": ts,
           "content": {"msgtype": "m.text", "body": body}})
}

/// Rooms here aren't encrypted: answer the SDK's check before it sends.
async fn plain_room(server: &MatrixMockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.room.encryption/?$"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"errcode": "M_NOT_FOUND", "error": "no encryption"})))
        .mount(server.server())
        .await;
}

#[tokio::test]
async fn short_page_means_nothing_older() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id!("!new:localhost")).await;
    // A new room: fewer events than asked for, yet the server still hands
    // back an `end` token. That used to leave "Load older messages" up.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/messages$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "start": "t0", "end": "t1",
            "chunk": [
                {"type": "m.reaction", "event_id": "$r1", "sender": "@amy:localhost", "origin_server_ts": 4,
                 "content": {"m.relates_to": {"rel_type": "m.annotation", "event_id": "$m2", "key": "👍"}}},
                text("$m2", "second", 3),
                text("$m1", "first", 2),
            ]
        })))
        .mount(server.server())
        .await;
    let page = timeline::history(&room, None, 50).await.unwrap();
    assert_eq!(page.items.iter().map(|i| i.body.as_str()).collect::<Vec<_>>(), ["first", "second"]);
    assert_eq!(page.older, None);
    assert_eq!(page.reactions.len(), 1);
    assert_eq!(page.reactions[0].key, "👍");
    assert_eq!(page.reactions[0].target, "$m2");
}

#[tokio::test]
async fn full_page_keeps_the_token() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id!("!busy:localhost")).await;
    let chunk: Vec<_> = (0..4).rev().map(|i| text(&format!("$m{i}"), &format!("m{i}"), i)).collect();
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/messages$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"start": "t0", "end": "t1", "chunk": chunk})))
        .mount(server.server())
        .await;
    let page = timeline::history(&room, None, 4).await.unwrap();
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.older.as_deref(), Some("t1"));
}

#[tokio::test]
async fn reacts_with_an_annotation() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id!("!r:localhost")).await;
    plain_room(&server).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/send/m.reaction/.*"))
        .and(body_partial_json(json!({"m.relates_to": {"rel_type": "m.annotation", "event_id": "$m1", "key": "🎉"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id": "$react"})))
        .expect(1)
        .mount(server.server())
        .await;
    let id = timeline::react(&room, event_id!("$m1"), "🎉").await.unwrap();
    assert_eq!(id, "$react");
}

#[tokio::test]
async fn forwards_a_copy_without_the_reply() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let from = server.sync_joined_room(&client, room_id!("!from:localhost")).await;
    let to = server.sync_joined_room(&client, room_id!("!to:localhost")).await;
    plain_room(&server).await;
    // A reply with an image: the image goes, the reply link doesn't.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*from.*/event/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "m.room.message", "event_id": "$pic", "sender": "@bob:localhost", "origin_server_ts": 5,
            "room_id": "!from:localhost",
            "content": {"msgtype": "m.image", "body": "cat.png", "url": "mxc://localhost/cat",
                        "m.relates_to": {"m.in_reply_to": {"event_id": "$earlier"}}}
        })))
        .mount(server.server())
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*to.*/send/m.room.message/.*"))
        .and(body_partial_json(json!({"msgtype": "m.image", "url": "mxc://localhost/cat"})))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert!(body.get("m.relates_to").is_none(), "reply link was forwarded: {body}");
            ResponseTemplate::new(200).set_body_json(json!({"event_id": "$copy"}))
        })
        .expect(1)
        .mount(server.server())
        .await;
    let id = timeline::forward(&from, event_id!("$pic"), &to, None).await.unwrap();
    assert_eq!(id, "$copy");
}

#[tokio::test]
async fn room_admins_may_change_the_picture() {
    use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let id = room_id!("!pic:localhost");
    let f = EventFactory::new().room(id).sender(matrix_sdk::ruma::user_id!("@bob:localhost"));
    let me = client.user_id().unwrap().to_owned();
    let room = server
        .sync_room(&client, JoinedRoomBuilder::new(id).add_state_event(f.create(matrix_sdk::ruma::user_id!("@bob:localhost"), matrix_sdk::ruma::RoomVersionId::V11)).add_state_event(f.power_levels(&mut std::collections::BTreeMap::from([(me, 100.into())]))))
        .await;
    let d = whisker_core::rooms::details(&room).await;
    assert!(d.can_set_avatar, "admin can't change the picture");
    assert!(d.can_edit);
}

#[tokio::test]
async fn name_changes_in_history_are_not_joins() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id!("!names:localhost")).await;
    let member = |id: &str, old: &str, new: &str, ts: u64| {
        json!({"type": "m.room.member", "state_key": "@nully:localhost", "sender": "@nully:localhost", "event_id": id,
               "origin_server_ts": ts, "content": {"membership": "join", "displayname": new},
               "unsigned": {"prev_content": {"membership": "join", "displayname": old}}})
    };
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/messages$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "start": "t0",
            "chunk": [member("$n3", "B", "C", 3), member("$n2", "A", "B", 2), member("$n1", "nully", "A", 1)]
        })))
        .mount(server.server())
        .await;
    let page = timeline::history(&room, None, 50).await.unwrap();
    let lines: Vec<&str> = page.items.iter().map(|i| i.body.as_str()).collect();
    assert_eq!(lines, ["nully is now A", "A is now B", "B is now C"]);
}

#[tokio::test]
async fn name_changes_in_sync_are_not_joins() {
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::events::{AnySyncStateEvent, AnySyncTimelineEvent};
    use matrix_sdk::ruma::serde::Raw;
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let id = room_id!("!live:localhost");
    let ev: Raw<AnySyncTimelineEvent> = serde_json::from_value(json!({
        "type": "m.room.member", "state_key": "@nully:localhost", "sender": "@nully:localhost", "event_id": "$n1",
        "origin_server_ts": 1, "content": {"membership": "join", "displayname": "New"},
        "unsigned": {"prev_content": {"membership": "join", "displayname": "Old"}}
    })).unwrap();
    server.mock_sync().ok(move |b| { b.add_joined_room(matrix_sdk_test::JoinedRoomBuilder::new(id).add_timeline_event(ev.clone())); }).mount().await;
    let resp = client.sync_once(SyncSettings::default()).await.unwrap();
    let room = client.get_room(id).unwrap();
    let events = &resp.rooms.joined[id].timeline.events;
    println!("raw: {}", events[0].raw().json().get());
    let Ok(AnySyncTimelineEvent::State(st)) = events[0].raw().deserialize() else { panic!("not state") };
    let st: AnySyncStateEvent = st;
    let item = timeline::notice_item(&room, &st, &mut Default::default()).await.unwrap();
    assert_eq!(item.body, "Old is now New");
}
