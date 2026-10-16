use std::collections::HashMap;
use std::sync::Mutex;

use rtmp_rs::media::{FlvTag, FlvTagType};
use tokio::sync::broadcast;

/// 每个流的缓存：顺序头（SPS/PPS、AAC 配置）+ 最近一个关键帧。
#[derive(Default)]
struct StreamCache {
    sequence_headers: Vec<FlvTag>,
    last_keyframe: Option<FlvTag>,
}

struct StreamEntry {
    tx: broadcast::Sender<FlvTag>,
    cache: StreamCache,
    publisher: String,
    app: String,
    session_id: u64,
}

#[derive(Default)]
pub struct Hub {
    streams: Mutex<HashMap<String, StreamEntry>>,
}

/// 控制台展示用的流信息快照。
#[derive(serde::Serialize)]
pub struct StreamInfo {
    pub stream_key: String,
    pub publisher: String,
    pub app: String,
    pub viewers: usize,
    pub flv_url: String,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 推流开始：建通道与缓存，并记住归属会话以便断连时回收。
    pub fn create(&self, session_id: u64, key: &str, publisher: String, app: String) {
        let (tx, _) = broadcast::channel(1024);
        let entry = StreamEntry {
            tx,
            cache: StreamCache::default(),
            publisher,
            app,
            session_id,
        };
        self.streams.lock().unwrap().insert(key.to_string(), entry);
    }

    /// 推流结束：移除整条流。
    pub fn remove(&self, key: &str) {
        self.streams.lock().unwrap().remove(key);
    }

    /// 连接断开：移除该会话推送的全部流。
    /// 一条 RTMP 连接可以推多条流，而异常断开不会触发 `on_unpublish`，
    /// 只按 key 清理会留下永久残留的僵尸流。
    pub fn remove_by_session(&self, session_id: u64) {
        self.streams
            .lock()
            .unwrap()
            .retain(|_, entry| entry.session_id != session_id);
    }

    /// 收到一个媒体 tag：更新缓存并广播。
    pub fn publish(&self, key: &str, tag: FlvTag) {
        let mut guard = self.streams.lock().unwrap();
        let Some(entry) = guard.get_mut(key) else {
            return;
        };

        // 顺序头单独留一份，新客户端订阅时补发。
        if is_sequence_header(&tag) {
            entry.cache.sequence_headers.push(tag.clone());
        }
        // 关键帧只留最近一个。
        if tag.is_keyframe() {
            entry.cache.last_keyframe = Some(tag.clone());
        }

        // 没有接收者时返回 Err，直接忽略。
        let _ = entry.tx.send(tag);
    }

    /// 新客户端订阅：先拿补发用的 tag，再拿接收端。
    pub fn subscribe(&self, key: &str) -> Option<(Vec<FlvTag>, broadcast::Receiver<FlvTag>)> {
        let guard = self.streams.lock().unwrap();
        let entry = guard.get(key)?;

        let mut backlog = entry.cache.sequence_headers.clone();
        if let Some(kf) = &entry.cache.last_keyframe {
            backlog.push(kf.clone());
        }

        Some((backlog, entry.tx.subscribe()))
    }

    /// 控制台列表。
    pub fn list(&self) -> Vec<StreamInfo> {
        let guard = self.streams.lock().unwrap();
        guard
            .iter()
            .map(|(key, entry)| StreamInfo {
                stream_key: key.clone(),
                publisher: entry.publisher.clone(),
                app: entry.app.clone(),
                viewers: entry.tx.receiver_count(),
                flv_url: format!("http://localhost:8080/live/{key}"),
            })
            .collect()
    }
}

/// 判断是不是 AVC/AAC 的顺序头 tag。
fn is_sequence_header(tag: &FlvTag) -> bool {
    match tag.tag_type {
        FlvTagType::Video => tag.is_avc_sequence_header(),
        FlvTagType::Audio => tag.is_aac_sequence_header(),
        FlvTagType::Script => false,
    }
}
