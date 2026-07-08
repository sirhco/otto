use otto_llm::models_dev::{LoadOptions, load};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fetch_writes_cache_and_parses() {
    let server = MockServer::start().await;
    let body = include_str!("fixtures/models_dev_sample.json");
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("models.json");
    let opts = LoadOptions {
        cache_path: cache.clone(),
        source_url: server.uri(),
        fetch: true,
    };
    let reg = load(&opts).await;
    assert!(reg.lookup("anthropic", "claude-opus-4-8").is_some());
    assert!(cache.exists(), "cache should be written");
}

#[tokio::test]
async fn falls_back_to_embedded_when_fetch_disabled_and_no_cache() {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        cache_path: dir.path().join("models.json"),
        source_url: "http://127.0.0.1:1/".into(), // unreachable, but fetch disabled anyway
        fetch: false,
    };
    let reg = load(&opts).await; // must not hang or panic; returns embedded
    assert!(!reg.is_empty());
}
