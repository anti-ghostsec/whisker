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
        ask: None,
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

fn captcha(error: bool) -> ResponseTemplate {
    // "\x89PNG\r\n\x1a\n" plus a little: enough to pass as a PNG here.
    let mut body = json!({
        "flows": [{ "stages": ["org.anoncat.captcha"] }],
        "params": { "org.anoncat.captcha": { "image": "data:image/png;base64,iVBORw0KGgpwaWM=", "expires_in": 300 } },
        "session": "sess1"
    });
    if error {
        body["errcode"] = json!("M_FORBIDDEN");
        body["error"] = json!("Wrong answer");
    }
    ResponseTemplate::new(401).set_body_json(body)
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_through_the_servers_captcha() {
    let (server, mgr, vault, dir) = setup().await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register"))
        .and(body_partial_json(json!({"auth": {"type": "org.anoncat.captcha", "session": "sess1", "response": "k7fq2"}})))
        .respond_with(ok()).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register"))
        .and(body_partial_json(json!({"auth": {"type": "org.anoncat.captcha"}})))
        .respond_with(captcha(true)).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(captcha(false)).mount(server.server()).await;

    // Without a way to ask, it says what the server wants.
    let uri = server.uri();
    let e = mgr.login(req(&vault, &uri, None)).await.unwrap_err();
    assert!(format!("{e:#}").contains("captcha"), "{e:#}");

    // Wrong first, then right: the second ask says why.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let ask = move |step: whisker_core::SignupStep| -> futures_util::future::BoxFuture<'static, whisker_core::SignupAnswer> {
        let seen = s2.clone();
        Box::pin(async move {
            let whisker_core::SignupStep::Captcha { image, retry } = step else { panic!("{step:?}") };
            assert!(image.starts_with(b"\x89PNG"));
            let mut s = seen.lock().unwrap();
            s.push(retry);
            whisker_core::SignupAnswer::Text(if s.len() == 1 { "nope".into() } else { " k7fq2 ".into() })
        })
    };
    let mut r = req(&vault, &uri, None);
    r.ask = Some(&ask);
    mgr.login(r).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [None, Some("Wrong answer".to_owned())]);
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn other_steps_go_to_the_servers_page() {
    let (server, mgr, vault, dir) = setup().await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register"))
        .and(body_partial_json(json!({"auth": {"session": "sess1"}})))
        .respond_with(ok()).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(uiaa(&["m.login.terms"])).mount(server.server()).await;

    let uri = server.uri();
    let page = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let p2 = page.clone();
    let ask = move |step: whisker_core::SignupStep| -> futures_util::future::BoxFuture<'static, whisker_core::SignupAnswer> {
        let page = p2.clone();
        Box::pin(async move {
            let whisker_core::SignupStep::Fallback { url, .. } = step else { panic!("{step:?}") };
            *page.lock().unwrap() = url;
            whisker_core::SignupAnswer::Done
        })
    };
    let mut r = req(&vault, &uri, None);
    r.ask = Some(&ask);
    mgr.login(r).await.unwrap();
    assert_eq!(*page.lock().unwrap(), format!("{uri}/_matrix/client/v3/auth/m.login.terms/fallback/web?session=sess1"));
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn picks_captcha_and_terms_over_a_token_it_doesnt_have() {
    let (server, mgr, vault, dir) = setup().await;
    // anoncat's shape: an invite-token flow, or its captcha plus its rules.
    let flows = |completed: Vec<&str>| {
        ResponseTemplate::new(401).set_body_json(json!({
            "flows": [{ "stages": ["m.registration_token"] }, { "stages": ["org.anoncat.captcha", "m.login.terms"] }],
            "params": {
                "org.anoncat.captcha": { "image": "data:image/png;base64,iVBORw0KGgpwaWM=" },
                "m.login.terms": { "policies": { "rules": { "version": "1", "en": { "name": "Server rules", "url": "https://cat.example/rules" } } } }
            },
            "completed": completed,
            "session": "sess1"
        }))
    };
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register"))
        .and(body_partial_json(json!({"auth": {"type": "m.login.terms", "session": "sess1"}})))
        .respond_with(ok()).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register"))
        .and(body_partial_json(json!({"auth": {"type": "org.anoncat.captcha", "response": "k7fq2"}})))
        .respond_with(flows(vec!["org.anoncat.captcha"])).mount(server.server()).await;
    Mock::given(method("POST")).and(path("/_matrix/client/v3/register")).respond_with(flows(vec![])).mount(server.server()).await;

    let uri = server.uri();
    let steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = steps.clone();
    let ask = move |step: whisker_core::SignupStep| -> futures_util::future::BoxFuture<'static, whisker_core::SignupAnswer> {
        let steps = s2.clone();
        Box::pin(async move {
            let answer = match &step {
                whisker_core::SignupStep::Captcha { .. } => whisker_core::SignupAnswer::Text("k7fq2".into()),
                whisker_core::SignupStep::Terms { documents } => {
                    assert_eq!(documents[0].0, "Server rules");
                    whisker_core::SignupAnswer::Done
                }
                other => panic!("{other:?}"),
            };
            steps.lock().unwrap().push(format!("{step:?}").split(' ').next().unwrap().to_owned());
            answer
        })
    };
    let mut r = req(&vault, &uri, None);
    r.ask = Some(&ask);
    mgr.login(r).await.unwrap();
    assert_eq!(*steps.lock().unwrap(), ["Captcha", "Terms"]);
    mgr.lock_all().await;
    std::fs::remove_dir_all(dir).ok();
}
