mod flv;
mod handler;
mod hub;

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::services::ServeDir;

use rtmp_rs::server::{RtmpServer, ServerConfig};

use crate::handler::FlvHandler;
use crate::hub::Hub;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let hub = Arc::new(Hub::new());

    // ---- HTTP 服务（axum）----
    let app = Router::new()
        .route("/api/streams", get(list_streams))
        .route("/live/{key}", get(flv_stream))
        .fallback_service(ServeDir::new("static"))
        .with_state(hub.clone());

    let http_bind = std::env::var("HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&http_bind).await?;
    tracing::info!("HTTP 控制台监听于 http://{http_bind}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // ---- RTMP 服务 ----
    let rtmp_bind = std::env::var("RTMP_BIND").unwrap_or_else(|_| "0.0.0.0:1935".into());
    let config = ServerConfig::default().bind(rtmp_bind.parse()?);
    let server = RtmpServer::new(config, FlvHandler { hub });
    tracing::info!("RTMP 服务监听于 {rtmp_bind}");

    server.run().await?;
    Ok(())
}

/// GET /api/streams
async fn list_streams(State(hub): State<Arc<Hub>>) -> impl IntoResponse {
    axum::Json(hub.list())
}

/// GET /live/{key}.flv
async fn flv_stream(State(hub): State<Arc<Hub>>, Path(key): Path<String>) -> Response {
    let Some((backlog, rx)) = hub.subscribe(&key) else {
        return (StatusCode::NOT_FOUND, "no such stream").into_response();
    };

    // 1. FLV header
    // 2. 补发的顺序头 + 最近关键帧
    // 3. 直播中的帧
    let head = tokio_stream::iter(
        std::iter::once(flv::flv_header())
            .chain(backlog.into_iter().map(|tag| flv::encode_tag(&tag)))
            .map(Ok::<Bytes, Infallible>),
    );

    let live = BroadcastStream::new(rx)
        .filter_map(|r| r.ok())
        .map(|tag| Ok::<Bytes, Infallible>(flv::encode_tag(&tag)));

    let body = Body::from_stream(head.chain(live));

    Response::builder()
        .header(header::CONTENT_TYPE, "video/x-flv")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .unwrap()
}
