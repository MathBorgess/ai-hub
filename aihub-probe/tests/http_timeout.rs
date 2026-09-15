use std::time::{Duration, Instant};

use aihub_probe::{build_probe_http_client, http_failure_note, PROBE_HTTP_TIMEOUT};

#[tokio::test]
async fn probe_http_timeout_becomes_note_not_hang() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        while let Ok((_stream, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    let client = build_probe_http_client().expect("client");
    let url = format!("http://{addr}/");
    let start = Instant::now();
    let err = client.get(&url).send().await.expect_err("timeout");
    assert!(start.elapsed() < PROBE_HTTP_TIMEOUT + Duration::from_secs(2));
    assert!(err.is_timeout());

    assert_eq!(http_failure_note(&err), "HTTP request timed out");
}
