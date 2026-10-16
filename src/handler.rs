use std::sync::Arc;

use rtmp_rs::media::FlvTag;
use rtmp_rs::protocol::message::PublishParams;
use rtmp_rs::session::{SessionContext, StreamContext};
use rtmp_rs::{AuthResult, RtmpHandler};

use crate::hub::Hub;

pub struct FlvHandler {
    pub hub: Arc<Hub>,
}

impl RtmpHandler for FlvHandler {
    async fn on_publish(&self, ctx: &SessionContext, params: &PublishParams) -> AuthResult {
        // 流 key 约定：非空且以 live_ 开头
        if params.stream_key.is_empty() || !params.stream_key.starts_with("live_") {
            return AuthResult::Reject("invalid stream key".into());
        }

        self.hub.create(
            ctx.session_id,
            &params.stream_key,
            ctx.peer_addr.to_string(),
            ctx.app.clone(),
        );
        AuthResult::Accept
    }

    async fn on_unpublish(&self, ctx: &StreamContext) {
        self.hub.remove(&ctx.stream_key);
    }

    /// 连接关闭（含被强杀等异常断开）：回收该会话遗留的所有流。
    async fn on_disconnect(&self, ctx: &SessionContext) {
        self.hub.remove_by_session(ctx.session_id);
    }

    async fn on_media_tag(&self, ctx: &StreamContext, tag: &FlvTag) -> bool {
        // 只处理推流方向，避免把自己的帧再收一遍
        if ctx.is_publishing {
            self.hub.publish(&ctx.stream_key, tag.clone());
        }
        true
    }
}
