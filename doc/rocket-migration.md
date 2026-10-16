# Rocket 替换 axum —— 可行性验证与迁移记录

> 分支：`verify/rocket-replace-axum`
> 验证时间：2026-10-16
> 结论：**可行。RTMP → HTTP-FLV 主功能与 flv.js 播放完全正常，axum 可完全移除。**
>
> 本文档只存在于本分支。`main` 分支的代码与 `readme.md`（仍描述 axum）未作任何改动，
> 因此本分支上 `readme.md` 的架构图与依赖表与实际代码不一致，以本文为准。

## 1. 为什么可行

axum 在本项目里只承担 HTTP 层，与流媒体核心完全解耦：

| 模块 | 职责 | 是否依赖 web 框架 |
|---|---|---|
| `src/handler.rs` | RTMP 回调（on_publish / on_media_tag / on_unpublish） | 否 |
| `src/hub.rs` | broadcast 广播 + 顺序头/关键帧缓存 | 否 |
| `src/flv.rs` | FlvTag → FLV 字节编码 | 否 |
| `src/main.rs` | 启动两个服务 + 路由 | **是（唯一接触点）** |

Web 框架只是把 `Hub` 吐出的 `FlvTag` 流包装成 HTTP 响应，换框架不影响推流接收、
广播、补发、编码任何一环。

## 2. 改动清单

| 文件 | 改动 |
|---|---|
| `Cargo.toml` | 移除 `axum` / `tower-http` / `serde_json` / `anyhow`；新增 `rocket = { version = "0.5", features = ["json"] }` |
| `src/main.rs` | HTTP 层整体重写为 Rocket |
| `src/hub.rs` | 新增 `session_id` 归属与 `remove_by_session()`（见 §5 问题 1） |
| `src/handler.rs` | 实现 `on_disconnect()`（见 §5 问题 1） |

依赖树已无 axum：`cargo tree | grep -c axum` = 0。残留的 `hyper` 由 `rocket_http` 自身引入，与 axum 无关。
`serde_json` 由 Rocket 的 `json` feature 内部提供，无需直接依赖；`anyhow` 在改用 `#[rocket::launch]` 后不再被使用。

## 3. 框架对应关系

| 能力 | axum 写法 | Rocket 写法 |
|---|---|---|
| 流列表 JSON | `axum::Json(hub.list())` | `rocket::serde::json::Json`（需 `json` feature） |
| 共享状态 | `.with_state(hub.clone())` | `.manage(hub)` + `&State<Arc<Hub>>` 请求守卫 |
| 路径参数 | `Path(key): Path<String>` | `<key>` 段 + `key: &str` |
| 流式响应体 | `Body::from_stream(head.chain(live))` | `ReaderStream::from(s.map(io::Cursor::new))` + `.streamed_body()` |
| 自定义响应头 | `Response::builder().header(...)` | `Response::build().header(...)`（需自建 Responder，见 §4） |
| 静态目录 | `tower_http::services::ServeDir` | `rocket::fs::FileServer::from("static")` |
| 监听地址端口 | `TcpListener::bind(HTTP_BIND)` | `Config::figment().merge(("address", ..)).merge(("port", ..))`，需把 `0.0.0.0:8080` 拆开 |

## 4. Rocket 迁移踩坑记录

**坑 1：`Response` 不实现 `Responder`。**
这是最大的障碍。axum 里可以直接 `Response::builder().header(..).body(..)` 造一个任意响应，
Rocket 0.5.1 的 `Response` 只是 `Responder::respond_to()` 的**返回值**，本身没有 `Responder` 实现，
不能直接当 handler 返回类型。实测报错：

```
error[E0277]: the trait bound `rocket::Response<'static>: Responder<'_, '_>` is not satisfied
```

同时 Rocket 的元组 Responder 只有 `(ContentType, R)` 和 `(Status, R)` 两种组合，
`Header` 无法进元组，所以也没法用 `(ContentType, Header, ByteStream)` 一并带上 CORS 头。

**解法**：写一个 15 行的 newtype 自己实现 `Responder`，见 [src/main.rs:106-122](../src/main.rs#L106-L122)。
内部用 `ReaderStream::from(stream.map(io::Cursor::new))` 把字节流适配成 `AsyncRead`，
这与 Rocket 自带 `ByteStream` 的内部实现一致，只是额外挂了两个 header。

**坑 2：`ByteStream` 的 item 约束比想象宽松。**
不必把 `Bytes` 转成 `Vec<u8>`——其约束是 `S::Item: AsRef<[u8]>`，`Bytes` 直接满足，
省掉每帧一次内存拷贝。

**坑 3：静态目录会不会抢走 `/live/<key>`。**
Rocket 路由按 rank 升序尝试，`FileServer` 的 `/<path..>` 排在 `/live/<key>` 之前，
看似会截胡。实际安全：`FileServer` 对不存在的文件返回 `Outcome::forward`（放行）而非 404，
请求会继续流向后面的路由。启动日志与运行日志均可验证：

```
(FileServer: static) GET /<path..> [10]
(flv_stream) GET /live/<key>
(list_streams) GET /api/streams
Matched: (list_streams) GET /api/streams → Success(200 OK)
```

`/` 仍由 FileServer 正常返回 `index.html`，`/flv.js` 等静态资源同样可用。

## 5. 本次一并修复的两个问题

### 问题 1：推流端异常断开后，流条目永久残留

**现象**：用 `taskkill /F` 强杀 ffmpeg 后，`/api/streams` 仍列出该流，
`/live/{key}` 继续返回 200 但再无新帧，形成僵尸流。

**根因**：`FlvHandler` 只实现了 `on_unpublish`，而它仅在客户端显式发送 unpublish 命令时触发；
异常断开不走这条路径。rtmp-rs 在读写循环结束后无条件调用的其实是 `on_disconnect`
（`rtmp-rs-0.6.0/src/server/connection.rs:331`），原代码没有实现它（默认空实现），
所以 `Hub` 里的条目从不清理。

> 注意：这是 `main` 分支同样存在的既有缺陷，与换不换框架无关（`hub.rs`/`handler.rs` 当时与 main 零差异）。

**修法**：`Hub` 记住每条流归属的 RTMP 会话，断连时按会话整体回收。

- [src/hub.rs:19](../src/hub.rs#L19) `StreamEntry` 增加 `session_id: u64`
- [src/hub.rs:63-68](../src/hub.rs#L63-L68) 新增 `remove_by_session()`
- [src/handler.rs:35-37](../src/handler.rs#L35-L37) 实现 `on_disconnect()`

选择按 `session_id` 而非按 `stream_key` 删除，是因为一条 RTMP 连接可以推多条流，
按 key 删无法覆盖这种情况。未引入空闲超时轮询兜底——`on_disconnect` 已覆盖正常与异常两条关闭路径。

### 问题 2：Rocket 运行在自定义 runtime 内，强制关闭被禁用

**现象**：启动日志出现

```
WARN rocket::server: Rocket is executing inside of a custom runtime.
INFO Rocket's runtime is enabled via `#[rocket::main]` or `#[launch]`.
Forced shutdown is disabled. Runtime settings may be suboptimal.
```

**根因**：为了与 RTMP 服务共存，最初保留了 `#[tokio::main]` 再手动 `rocket.launch().await`，
于是 Rocket 不是 runtime 的所有者，无法安装它的 `ForceShutdown` fairing，Ctrl+C 优雅关闭失效。

**修法**：改用 `#[rocket::launch]` + `async fn`，见 [src/main.rs:26](../src/main.rs#L26)。
它展开为 `#[rocket::main] async fn main()`，RTMP 服务在函数体内 `tokio::spawn`，
落在 Rocket 自有 runtime 中。Rocket 的 codegen 本身就支持这个用法——
它会检测 launch 函数里的 spawn 调用，若非 async 则警告
"task is being spawned outside an async context"（`rocket_codegen-0.5.1/src/attribute/entry/launch.rs:96`）。

配套地，原先用于 `?` 冒泡的 `anyhow` 不再需要，配置解析失败改为带明确信息的 panic。

**修复后启动日志**：警告消失，且 `force = true`：

```
shutdown: ctrlc = true, force = true, grace = 2s, mercy = 3s
```

## 6. 端到端验证记录

测试环境：Windows，rustc 1.93.1，ffmpeg 7.0.2，rtmp-rs 0.6.0，Rocket 0.5.1。
测试源无现成 mp4，用 ffmpeg lavfi 合成 320x240 H.264 + AAC 44100 的 6 秒素材循环推送。

### 复现命令

**1. 合成测试源**（仓库里没有 mp4，`readme.md` 提到的 `cmzw_h264.mp4` 未入库；有现成 H.264 素材可跳过这步）

```bash
ffmpeg -f lavfi -i "testsrc=size=320x240:rate=30:duration=6" \
       -f lavfi -i "sine=frequency=800:sample_rate=44100:duration=6" \
       -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 30 \
       -c:a aac -b:a 64k src6.mp4
```

`-g 30` 保证每秒一个关键帧，新观众补发才明显。

**2. 推流（RTMP → 1935）**

```bash
# 用合成素材循环推
ffmpeg -re -stream_loop -1 -i src6.mp4 -c copy -f flv rtmp://localhost:1935/live/live_test

# 或用真实素材（等价于 readme.md 里的写法）
ffmpeg -re -stream_loop -1 -i .\cmzw_h264.mp4 -c copy -f flv rtmp://localhost:1935/live/live_test
```

流 key 必须以 `live_` 开头，否则被 `on_publish` 拒绝；`-re` 不能省，否则 ffmpeg 全速发送导致丢帧。

**3. ffplay / ffprobe 拉 RTMP 流（1935，服务端播放路径）**

```bash
ffplay rtmp://localhost:1935/live/live_test

# 只验证链路、不弹窗口
ffprobe -i rtmp://localhost:1935/live/live_test
```

实测 ffprobe 输出，证明 RTMP 播放路径可用：

```
Input #0, flv, from 'rtmp://localhost:1935/live/live_doc':
  Stream #0:0: Video: h264 (High), yuv420p(progressive), 320x240, 30.30 fps
  Stream #0:1: Audio: aac (LC), 44100 Hz, mono, fltp
```

这条路径**不需要项目实现 `on_play`**，由 rtmp-rs 的 registry 自动把已推送的流中继给播放端，
服务端日志可见：

```
Subscriber added stream=live/live_doc subscribers=1 catchup_frames=31
Playing started session_id=2 stream_name="live_doc"
```

注意：直播流没有 EOF，`ffplay -autoexit` 不会自行退出，用 `q` 或 Ctrl+C 关闭。

**4. flv.js 播放地址（HTTP-FLV → 8080）**

| 用途 | 地址 |
|---|---|
| 拉流地址（flv.js 的 `url`） | `http://localhost:8080/live/live_test` |
| Web 控制台 | `http://localhost:8080/` |
| 流列表 JSON | `http://localhost:8080/api/streams` |

```js
const player = flvjs.createPlayer({ type: "flv", url: "http://localhost:8080/live/live_test", isLive: true });
player.attachMediaElement(document.querySelector("video"));
player.load();
player.play();
```

**5. 命令行快速验证 FLV 字节流**

```bash
curl -D - --max-time 5 -o capture.flv http://localhost:8080/live/live_test
head -c 3 capture.flv     # 应为 FLV
ffprobe capture.flv       # 应解析出 h264 + aac
```

### 协议与字节层

| 验证项 | 结果 |
|---|---|
| RTMP 接入 | 日志识别 `codec="H.264/AVC"` 与 `codec="AAC"` |
| `/live/live_test` 响应头 | `200` + `content-type: video/x-flv` + `transfer-encoding: chunked` + `access-control-allow-origin: *` |
| FLV 头 9 字节 | `46 4c 56 01 05 00 00 00 09 00 00 00 00` 正确 |
| ffprobe 解析抓取字节 | `format_name=flv`，`h264 320x240` + `aac 44100`，`nb_streams=2` |
| `/live/{未知key}` | `404` + `no such stream`（证明走的是业务 handler 而非 FileServer） |

### flv.js 真实浏览器播放

用 Playwright 驱动 Chromium 打开 `http://localhost:8080/`，加载 flv.js 后点"播放"：

| 指标 | 修复前 | 修复后 |
|---|---|---|
| videoWidth / Height | 320 / 240 | 320 / 240 |
| readyState | 4（HAVE_ENOUGH_DATA） | 4 |
| paused / error | false / null | false / null |
| 3~4 秒墙钟内 currentTime 推进 | 4.002s | 3.011s |

时间推进量与墙钟耗时基本 1:1，说明实时性正常、无累积卡顿。

### 多观众与生命周期

| 验证项 | 结果 |
|---|---|
| 3 个观众并发拉流 | `viewers: 3`，各自独立收满 ~146KB，互不干扰 |
| 持续吞吐 | ~1.7–1.9 MB / 5s（约 3.5Mbps）稳定 |
| 中途加入补发 | 后加入的 curl 首包即为完整 FLV 头 + 顺序头 + 关键帧，ffprobe 可独立解析 |
| 观众断开回收 | 关闭浏览器后 `viewers` 回落至 0，无 broadcast receiver 泄漏 |
| 优雅 EOF 清理 | `-stream_loop` 自然结束，条目正确移除 |

### 修复专项回归

| 场景 | 结果 |
|---|---|
| `taskkill /F` 强杀唯一推流端 | `/api/streams` → `[]`，`/live/{key}` → `404`（修复前永久 200） |
| 两个独立会话分别推 a / b，杀掉 a | 仅剩 `live_b`，会话隔离正确，无误删 |
| Rocket runtime 警告 | 消失，`force = true` |

## 7. 遗留事项

1. **`readme.md` 未同步**：按约定本分支只新增本文档，`readme.md` 保持 main 的 axum 描述。
   若本分支要转正，需同步 §架构、§依赖、§接口三处。
2. **控制台 flv.js 走 CDN**：`static/index.html` 从 jsdelivr 引入 flv.js，本机浏览器侧被
   `ERR_CONNECTION_RESET`（curl 可通，属网络环境问题）。验证时改为注入仓库自带的
   `static/flv.js` 本地副本完成播放，**未改动 `index.html`**。若需离线可用，把脚本 src 换成 `./flv.js` 即可。
3. **`StreamInfo.flv_url` 硬编码 `http://localhost:8080`**（[src/hub.rs:113](../src/hub.rs#L113)）：换框架未触及，
   但跨机器访问控制台时该地址不可用，建议改为按请求 Host 拼接。
