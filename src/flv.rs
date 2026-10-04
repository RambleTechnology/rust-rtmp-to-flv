use bytes::Bytes;
use rtmp_rs::media::{FlvTag, FlvTagType};

/// FLV header：固定 9 字节。
pub fn flv_header() -> Bytes {
    // "FLV" + version(1) + flags + DataOffset(9)
    // flags: audio=0x01, video=0x04。这里假设音视频都有。
    Bytes::from_static(&[
        b'F', b'L', b'V', 0x01, 0x05, // audio(0x01) | video(0x04)
        0x00, 0x00, 0x00, 0x09, // DataOffset = 9
        0x00, 0x00, 0x00, 0x00, // PreviousTagSize0 = 0
    ])
}

/// 把一个 tag 编码成 [11 字节 tag 头][data][4 字节 PreviousTagSize]。
pub fn encode_tag(tag: &FlvTag) -> Bytes {
    let n = tag.data.len() as u32;
    let mut out = Vec::with_capacity(11 + tag.data.len() + 4);

    // TagType：Audio=8, Video=9, Script=18
    out.push(match tag.tag_type {
        FlvTagType::Audio => 8,
        FlvTagType::Video => 9,
        FlvTagType::Script => 18,
    });

    // DataSize：3 字节大端（低 3 字节）
    out.extend_from_slice(&n.to_be_bytes()[1..]);

    // Timestamp：3 字节大端（低 3 字节）+ 1 字节扩展高位
    out.extend_from_slice(&tag.timestamp.to_be_bytes()[1..]);
    out.push((tag.timestamp >> 24) as u8);

    // StreamID：固定 0
    out.extend_from_slice(&[0, 0, 0]);

    // tag body
    out.extend_from_slice(&tag.data);

    // PreviousTagSize = 11 + data 长度
    out.extend_from_slice(&(11 + n).to_be_bytes());

    Bytes::from(out)
}
