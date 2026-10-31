//! "Paste any AI API address and it works": detection and a real request
//! against mock OpenAI-style, Ollama and Anthropic-style servers.

use serde_json::json;
use whisker_agent::{ApiConfig, ApiFormat, Llm, detect};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_style_server_at_bare_address() {
    let s = MockServer::start().await;
    Mock::given(method("GET")).and(path("/v1/models")).and(header("authorization", "Bearer k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [{"id": "b-model", "object": "model"}, {"id": "a-model", "object": "model"}]})))
        .mount(&s).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": [{"message": {"role": "assistant", "content": "hi there"}, "finish_reason": "stop"}]})))
        .mount(&s).await;

    // User typed just the address, no /v1.
    let d = detect(&s.uri(), Some("k")).await.unwrap();
    assert_eq!(d.format, ApiFormat::OpenAiChat);
    assert_eq!(d.endpoint, format!("{}/v1/chat/completions", s.uri()));
    assert_eq!(d.models, vec!["a-model", "b-model"]);

    let api = ApiConfig { url: s.uri(), api_key: Some("k".into()), model: "a-model".into(), ..Default::default() };
    // Format left Unknown: the client detects it by itself on first use.
    assert_eq!(Llm::new(api, 100, Default::default()).unwrap().test().await.unwrap(), "hi there");
}

#[tokio::test]
async fn wrong_key_is_reported() {
    let s = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&s).await;
    let e = detect(&s.uri(), Some("bad")).await.unwrap_err();
    assert!(e.to_string().contains("rejected the API key"), "{e}");
}

#[tokio::test]
async fn ollama_native() {
    let s = MockServer::start().await;
    Mock::given(method("GET")).and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{"name": "llama3:8b"}]})))
        .mount(&s).await;
    let d = detect(&s.uri(), None).await.unwrap();
    assert_eq!(d.service, "Ollama");
    assert_eq!(d.models, vec!["llama3:8b"]);
    assert!(d.endpoint.ends_with("/v1/chat/completions"));
}

#[tokio::test]
async fn anthropic_style_server() {
    let s = MockServer::start().await;
    // Doesn't speak OpenAI's bearer auth…
    Mock::given(method("GET")).and(path("/v1/models")).and(header("x-api-key", "k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1", "type": "model"}]})))
        .mount(&s).await;
    Mock::given(method("POST")).and(path("/v1/messages")).and(header("x-api-key", "k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn"})))
        .mount(&s).await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&s).await;

    let d = detect(&s.uri(), Some("k")).await.unwrap();
    assert_eq!(d.format, ApiFormat::AnthropicMessages);
    let api = ApiConfig { url: s.uri(), api_key: Some("k".into()), model: "m1".into(), format: d.format, endpoint: d.endpoint, service: d.service };
    assert_eq!(Llm::new(api, 100, Default::default()).unwrap().test().await.unwrap(), "hello");
}

#[tokio::test]
async fn adapts_to_max_completion_tokens() {
    let s = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions")).and(wiremock::matchers::body_partial_json(json!({"max_tokens": 100})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "Unsupported parameter: 'max_tokens'. Use 'max_completion_tokens' instead."}})))
        .mount(&s).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions")).and(wiremock::matchers::body_partial_json(json!({"max_completion_tokens": 100})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]})))
        .mount(&s).await;
    let api = ApiConfig {
        url: s.uri(), model: "m".into(), format: ApiFormat::OpenAiChat,
        endpoint: format!("{}/v1/chat/completions", s.uri()), ..Default::default()
    };
    assert_eq!(Llm::new(api, 100, Default::default()).unwrap().test().await.unwrap(), "ok");
}
