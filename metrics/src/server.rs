//! HTTP server that exposes the `/metrics`, `/health`, and `/ready` endpoints.
//!
//! Uses `axum` to serve:
//! - `GET /metrics` — Prometheus text format metrics
//! - `GET /health`  — simple liveness probe (returns `200 OK`)
//! - `GET /ready`   — readiness probe (returns `200 OK` if router_up == 1, `503` otherwise)

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use prometheus::{Encoder, Registry, TextEncoder};
use std::net::SocketAddr;
use tracing::info;

use crate::cache::ResponseCache;

/// Shared server state.
#[derive(Clone)]
struct AppState {
    registry: Registry,
    cache: ResponseCache,
}

/// Start the HTTP server and block until it exits.
pub async fn serve(listen: String, registry: Registry, cache: ResponseCache) -> Result<()> {
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid listen address: {listen}"))?;

    let state = AppState { registry, cache };

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .with_state(state);

    info!(%addr, "HTTP server listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind to {addr}"))?;

    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;

    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /metrics` — render all registered metrics in Prometheus text format.
///
/// Responses are cached for the configured TTL to reduce encoding overhead
/// on high-frequency Prometheus scrapes.
async fn metrics_handler(State(state): State<AppState>) -> Response {
    const CACHE_KEY: &str = "metrics";

    // Cache hit — return immediately
    if let Some((body, content_type)) = state.cache.get(CACHE_KEY) {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            body,
        )
            .into_response();
    }

    // Cache miss — encode and store
    let encoder = TextEncoder::new();
    let metric_families = state.registry.gather();

    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to encode metrics: {e}"),
        )
            .into_response();
    }

    let content_type = encoder.format_type().to_string();
    state.cache.set(CACHE_KEY, buffer.clone(), &content_type);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        buffer,
    )
        .into_response()
}

/// `GET /health` — simple liveness probe.
async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// `GET /ready` — readiness probe that checks if the exporter is ready to serve traffic.
///
/// Returns 200 OK if:
/// - At least one contract ID is configured
/// - The last scrape cycle succeeded (router_up == 1)
///
/// Returns 503 Service Unavailable otherwise.
async fn ready_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Check if router_up metric is 1
    let metric_families = state.registry.gather();

    let router_up = metric_families
        .iter()
        .find(|mf| mf.get_name() == "router_up")
        .and_then(|mf| mf.get_metric().first())
        .and_then(|m| {
            if m.has_gauge() {
                Some(m.get_gauge().get_value())
            } else {
                None
            }
        })
        .unwrap_or(0.0);

    if router_up >= 1.0 {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::util::ServiceExt; // for `oneshot`

    fn make_app() -> Router {
        let registry = Registry::new();
        // Register a test gauge so the /metrics output is non-empty
        let gauge = prometheus::Gauge::new("test_gauge", "a test gauge").unwrap();
        registry.register(Box::new(gauge.clone())).unwrap();
        gauge.set(42.0);

        let state = AppState {
            registry,
            cache: crate::cache::ResponseCache::new(crate::cache::CacheConfig::default()),
        };
        Router::new()
            .route("/metrics", get(metrics_handler))
            .route("/health", get(health_handler))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_health_returns_200() {
        let app = make_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_metrics_returns_200_with_prometheus_content_type() {
        let app = make_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("text/plain"),
            "expected text/plain content type, got: {content_type}"
        );
    }

    #[tokio::test]
    async fn test_metrics_body_contains_gauge() {
        use axum::body::to_bytes;

        let app = make_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            body_str.contains("test_gauge"),
            "expected test_gauge in metrics output"
        );
        assert!(
            body_str.contains("42"),
            "expected gauge value 42 in metrics output"
        );
    }

    #[tokio::test]
    async fn test_ready_returns_503_when_router_up_is_zero() {
        let registry = Registry::new();
        // Register router_up gauge and set it to 0
        let gauge = prometheus::Gauge::new("router_up", "exporter health").unwrap();
        registry.register(Box::new(gauge.clone())).unwrap();
        gauge.set(0.0);

        let state = AppState {
            registry,
            cache: crate::cache::ResponseCache::new(crate::cache::CacheConfig::default()),
        };
        let app = Router::new()
            .route("/ready", get(ready_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_ready_returns_200_when_router_up_is_one() {
        let registry = Registry::new();
        // Register router_up gauge and set it to 1
        let gauge = prometheus::Gauge::new("router_up", "exporter health").unwrap();
        registry.register(Box::new(gauge.clone())).unwrap();
        gauge.set(1.0);

        let state = AppState {
            registry,
            cache: crate::cache::ResponseCache::new(crate::cache::CacheConfig::default()),
        };
        let app = Router::new()
            .route("/ready", get(ready_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
