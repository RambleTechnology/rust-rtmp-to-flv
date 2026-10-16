mod flv;
mod handler;
mod hub;

use std::sync::Arc;

use bytes::Bytes;
use rocket::fs::FileServer;
use rocket::http::{ContentType, Header, Status};
use rocket::request::Request;
use rocket::response::stream::ReaderStream;
use rocket::response::{self, Responder};
use rocket::serde::json::Json;
use rocket::{get, routes, State};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use rtmp_rs::server::{RtmpServer, ServerConfig};

use crate::handler::FlvHandler;
use crate::hub::{Hub, StreamInfo};

/// 用 `#[rocket::launch]` 而非 `#[tokio::main]`：后者会让 Rocket 运行在“别人的”
/// runtime 里并禁用强制关闭（Ctrl+C 不生效）。launch 展开为
/// `#[rocket::main] async fn main()`，此处 spawn 的 RTMP 任务落在 Rocket 自有 runtime 内。
#[rocket::launch]
async fn rocket() -> rocket::Rocket<rocket::Build> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let hub = Arc::new(Hub::new());

    // ---- RTMP 服务 ----
    let rtmp_bind = std::env::var("RTMP_BIND").unwrap_or_else(|_| "0.0.0.0:1935".into());
    let rtmp_addr: std::net::SocketAddr = rtmp_bind
        .parse()
        .unwrap_or_else(|e| panic!("RTMP_BIND {rtmp_bind} 解析失败: {e}"));
    let server = RtmpServer::new(
        ServerConfig::default().bind(rtmp_addr),
        FlvHandler { hub: hub.clone() },
    );
    tokio::spawn(async move {
        tracing::info!("RTMP 服务监听于 {rtmp_bind}");
        if let Err(e) = server.run().await {
            tracing::error!("RTMP 服务退出: {e:?}");
        }
    });

    // ---- HTTP 服务（Rocket）----
    let http_bind = std::env::var("HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let (address, port_str) = http_bind.split_once(':').unwrap_or(("0.0.0.0", "8080"));
    let port: u16 = port_str
        .parse()
        .unwrap_or_else(|e| panic!("HTTP_BIND 端口 {port_str} 非法: {e}"));
    let figment = rocket::Config::figment()
        .merge(("address", address))
        .merge(("port", port));

    tracing::info!("HTTP 控制台监听于 http://{http_bind}");
    rocket::custom(figment)
        .manage(hub)
        .mount("/", routes![list_streams, flv_stream])
        .mount("/", FileServer::from("static"))
}

/// GET /api/streams
#[get("/api/streams")]
fn list_streams(hub: &State<Arc<Hub>>) -> Json<Vec<StreamInfo>> {
    Json(hub.list())
}

/// GET /live/{key}
#[get("/live/<key>")]
fn flv_stream(
    hub: &State<Arc<Hub>>,
    key: &str,
) -> Result<
    FlvResponse<impl tokio_stream::Stream<Item = Bytes> + Send>,
    (Status, &'static str),
> {
    let Some((backlog, rx)) = hub.subscribe(key) else {
        return Err((Status::NotFound, "no such stream"));
    };

    // 1. FLV header
    // 2. 补发的顺序头 + 最近关键帧
    // 3. 直播中的帧
    let head = tokio_stream::iter(
        std::iter::once(flv::flv_header())
            .chain(backlog.into_iter().map(|tag| flv::encode_tag(&tag))),
    );

    let live = BroadcastStream::new(rx)
        .filter_map(|r| r.ok())
        .map(|tag| flv::encode_tag(&tag));

    Ok(FlvResponse(head.chain(live)))
}

/// 带 Content-Type / CORS 头的流式 FLV 响应。
/// Rocket 的 `Response` 本身不实现 `Responder`，需要包一层。
struct FlvResponse<S>(S)
where
    S: tokio_stream::Stream<Item = Bytes> + Send;

impl<'r, S> Responder<'r, 'r> for FlvResponse<S>
where
    S: tokio_stream::Stream<Item = Bytes> + Send + 'r,
{
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'r> {
        let body = ReaderStream::from(self.0.map(std::io::Cursor::new));
        Ok(rocket::response::Response::build()
            .header(ContentType::new("video", "x-flv"))
            .header(Header::new("Access-Control-Allow-Origin", "*"))
            .streamed_body(body)
            .finalize())
    }
}
