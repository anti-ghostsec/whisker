//! Registration against a mock homeserver: the usual "dummy" sign-up step,
//! a registration token, and a server that wants an email instead.

use matrix_sdk::test_utils::mocks::MatrixMockServer;
use serde_json::json;
use whisker_core::{AccountKind, AccountManager, LoginRequest};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

fn uiaa(stages: &[&str]) -> ResponseTemplate {
    ResponseTemplate::new(401).set_body_json(json!({
        "flows": [{ "stages": stages }],
        "params": {},
        "session": "sess1"
    }))
}

fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "user_id": "@newbie:localhost", "access_token": "tok", "device_id": "DEV1"
    }))
}

async fn setup() -> (MatrixMockServer, AccountManager, String, std::path::PathBuf) {
    let server = MatrixMockServer::new().await;
    server.mock_versions().ok().mount().await;
    server.mock_upload_keys().ok().mount().await;
    server.mock_query_keys().ok().mount().await;
    let dir = std::env::temp_dir().join(format!("whisker-reg-{}", whisker_core::vault::random_secret()));
    let mgr = AccountManager::new(&dir);
    let vault = mgr.create_vault("register test pw").await.unwrap();
    (server, mgr, vault, dir)
}

fn req<'a>(vault: &'a str, hs: &'a str, token: Option<&'a str>) -> LoginRequest<'a> {
    LoginRequest {
        vault,
        register: true,
        token,
        homeserver: hs,
        username: "newbie",
        password: "a long password",
        kind: AccountKind::Human,
        tags: Default::default(),
        proxy: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_through_dummy_stage() {
    let (server, mgr, vault, dir) = setup().await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).and(body_partial_json(json!({"auth": {"type": "m.login.dummy", "session": "sess1"}})))
        .respond_with(ok()).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(uiaa(&["m.login.dummy"])).mount(server.server()).await;

    let uri = server.uri();
    mgr.login(req(&vault, &uri, None)).await.unwrap();
    assert_eq!(mgr.accounts().await[0].user_id, "@newbie:localhost");
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_with_token() {
    let (server, mgr, vault, dir) = setup().await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register"))
        .and(body_partial_json(json!({"auth": {"type": "m.login.registration_token", "token": "invite-123"}})))
        .respond_with(ok()).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(uiaa(&["m.login.registration_token"])).mount(server.server()).await;

    let uri = server.uri();
    let e = mgr.login(req(&vault, &uri, None)).await.unwrap_err();
    assert!(format!("{e:#}").contains("registration token"), "{e:#}");
    mgr.login(req(&vault, &uri, Some("invite-123"))).await.unwrap();
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn explains_email_only_signup() {
    let (server, mgr, vault, dir) = setup().await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(uiaa(&["m.login.email.identity"])).mount(server.server()).await;
    let uri = server.uri();
    let e = format!("{:#}", mgr.login(req(&vault, &uri, None)).await.unwrap_err());
    assert!(e.contains("an email address") && e.contains("website"), "{e}");
    std::fs::remove_dir_all(dir).ok();
}

/// The typed server only says where its client API lives; the account is
/// made there.
#[tokio::test(flavor = "multi_thread")]
async fn follows_the_servers_well_known() {
    let (server, mgr, vault, dir) = setup().await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).and(body_partial_json(json!({"auth": {"type": "m.login.dummy", "session": "sess1"}})))
        .respond_with(ok()).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(uiaa(&["m.login.dummy"])).mount(server.server()).await;
    let front = wiremock::MockServer::start().await;
    Mock::given(method("GET")).and(path("/.well-known/matrix/client"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"m.homeserver": {"base_url": server.uri()}})))
        .mount(&front).await;

    let uri = front.uri();
    mgr.login(req(&vault, &uri, None)).await.unwrap();
    assert_eq!(mgr.accounts().await[0].user_id, "@newbie:localhost");
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}
