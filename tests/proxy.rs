//! End-to-end integration tests for trusted-proxy client IP resolution
//! as seen by the rate limiter: spoofed `X-Forwarded-For` values are
//! ignored unless the peer is a trusted proxy, and distinct resolved
//! clients get distinct token buckets.

mod common;

use common::TestServer;
use wallermax_server::config::AppConfig;

/// Config with a tiny, non-refilling rate limit bucket per client IP.
fn rate_limited_config() -> AppConfig {
    let mut config = AppConfig::default();
    config.middleware.rate_limit = true;
    config.rate_limit.capacity = 2;
    // Effectively no refill within a test's lifetime.
    config.rate_limit.refill_per_second = 0.001;
    config
}

/// Number of `429` responses for `count` sequential health probes.
async fn rejected(server: &TestServer, count: usize) -> usize {
    let mut rejected = 0;
    for _ in 0..count {
        let response = reqwest::get(server.url("/health"))
            .await
            .expect("request succeeds");
        if response.status().as_u16() == 429 {
            rejected += 1;
        }
    }
    rejected
}

#[tokio::test]
async fn without_trusted_proxies_spoofed_forwards_are_ignored() {
    let mut config = rate_limited_config();
    config.server.trusted_proxies = Vec::new();
    let server = TestServer::start_with_config(config).await;

    // Every request claims a different client IP; with no trusted
    // proxies the header is ignored, so all requests share the single
    // 127.0.0.1 bucket.
    let mut rejected = 0;
    for index in 0..5 {
        let response = reqwest::Client::new()
            .get(server.url("/health"))
            .header("X-Forwarded-For", format!("198.51.100.{index}"))
            .send()
            .await
            .expect("request succeeds");
        if response.status().as_u16() == 429 {
            rejected += 1;
        }
    }

    assert_eq!(rejected, 3, "all requests share the peer bucket");
}

#[tokio::test]
async fn untrusted_peers_cannot_join_the_forward_chain() {
    let mut config = rate_limited_config();
    // The peer (127.0.0.1) is NOT in this list.
    config.server.trusted_proxies = vec!["10.0.0.0/8".to_owned()];
    let server = TestServer::start_with_config(config).await;

    let rejected = rejected(&server, 4).await;

    // Same single bucket again: the header is ignored for untrusted peers.
    assert_eq!(rejected, 2);
}

#[tokio::test]
async fn trusted_proxies_honour_distinct_forwarded_clients() {
    let mut config = rate_limited_config();
    config.server.trusted_proxies = vec!["127.0.0.1".to_owned()];
    let server = TestServer::start_with_config(config).await;

    // Each request presents a distinct client behind the trusted proxy;
    // each gets its own bucket, so nothing is rejected.
    let mut rejected = 0;
    for index in 0..5 {
        let response = reqwest::Client::new()
            .get(server.url("/health"))
            .header("X-Forwarded-For", format!("198.51.100.{index}"))
            .send()
            .await
            .expect("request succeeds");
        if response.status().as_u16() == 429 {
            rejected += 1;
        }
    }

    assert_eq!(
        rejected, 0,
        "distinct forwarded clients get distinct buckets"
    );
}

#[tokio::test]
async fn trusted_proxy_clients_share_a_bucket_by_address() {
    let mut config = rate_limited_config();
    config.server.trusted_proxies = vec!["127.0.0.1".to_owned()];
    let server = TestServer::start_with_config(config).await;

    // Same forwarded client five times: one bucket, three rejections.
    let mut rejected = 0;
    for _ in 0..5 {
        let response = reqwest::Client::new()
            .get(server.url("/health"))
            .header("X-Forwarded-For", "198.51.100.99")
            .send()
            .await
            .expect("request succeeds");
        if response.status().as_u16() == 429 {
            rejected += 1;
        }
    }

    assert_eq!(rejected, 3);
}

#[tokio::test]
async fn the_rightmost_untrusted_entry_wins() {
    let mut config = rate_limited_config();
    config.server.trusted_proxies = vec!["127.0.0.1".to_owned(), "10.0.0.0/8".to_owned()];
    let server = TestServer::start_with_config(config).await;

    // "198.51.100.7" is the right-most untrusted hop; "203.0.113.1" (to
    // its left) must NOT be the bucket key. Hammering with the right-most
    // client constant exhausts its bucket only.
    let mut rejected = 0;
    for _ in 0..5 {
        let response = reqwest::Client::new()
            .get(server.url("/health"))
            .header("X-Forwarded-For", "203.0.113.1, 10.0.0.9, 198.51.100.7")
            .send()
            .await
            .expect("request succeeds");
        if response.status().as_u16() == 429 {
            rejected += 1;
        }
    }

    assert_eq!(rejected, 3, "the right-most untrusted client is the key");
}

#[tokio::test]
async fn cidr_blocks_cover_whole_proxy_networks() {
    let mut config = rate_limited_config();
    // The loopback peer as a /8 CIDR entry.
    config.server.trusted_proxies = vec!["127.0.0.0/8".to_owned()];
    let server = TestServer::start_with_config(config).await;

    let mut rejected = 0;
    for index in 0..5 {
        let response = reqwest::Client::new()
            .get(server.url("/health"))
            .header("X-Forwarded-For", format!("198.51.100.{index}"))
            .send()
            .await
            .expect("request succeeds");
        if response.status().as_u16() == 429 {
            rejected += 1;
        }
    }

    assert_eq!(rejected, 0, "CIDR trust covers the loopback peer");
}
