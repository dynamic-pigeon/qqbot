//! 集成测试共享的 mock OneBot 与测试装置。
//!
//! `tests/` 下每个顶层 .rs 是独立测试二进制；放在 `common/` 子目录里的
//! 本模块不会被单独编译成二进制，各测试 `mod common;` 引入即可共享。
//! mock 是最小 OneBot V11 正向 WS 服务端：api/event 双连接、回显 send 类
//! action、按 action 返回固定 data；事件 message_id 自增，避免依赖去重。

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use kovi::futures_util::{SinkExt, StreamExt};
use kovi::serde_json::{Value, json};
use kovi::tokio;
use kovi_onebot::{Host, OneBotDriver, OneBotDriverConfig, Server};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

pub const ADMIN: i64 = 10001;
pub const GROUP: i64 = 910_000_001;
pub const BOT_ID: i64 = 10000;
pub const READY: Duration = Duration::from_secs(10);

pub struct AbortOnDrop(pub tokio::task::JoinHandle<kovi::ExitEvent>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
pub struct Reply {
    pub text: String,
    pub has_image: bool,
}

enum ConnCmd {
    Send(Message),
}

pub struct MockOneBot {
    port: u16,
    event_cmd: Mutex<Option<mpsc::Sender<ConnCmd>>>,
    api_connects: AtomicUsize,
    event_connects: AtomicUsize,
    outgoing: Mutex<Vec<Value>>,
    notify: Notify,
    shutdown: Notify,
    next_message_id: AtomicI32,
    next_event_message_id: AtomicI32,
}

impl MockOneBot {
    pub async fn start() -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock onebot");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        let server = Arc::new(Self {
            port: addr.port(),
            event_cmd: Mutex::new(None),
            api_connects: AtomicUsize::new(0),
            event_connects: AtomicUsize::new(0),
            outgoing: Mutex::new(Vec::new()),
            notify: Notify::new(),
            shutdown: Notify::new(),
            next_message_id: AtomicI32::new(1),
            next_event_message_id: AtomicI32::new(1000),
        });
        let accept = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept.shutdown.notified() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let accept = Arc::clone(&accept);
                        tokio::spawn(async move { accept.handle_conn(stream).await });
                    }
                }
            }
        });
        server
    }

    pub fn driver(&self) -> OneBotDriver {
        OneBotDriver::new(OneBotDriverConfig {
            server: Server::new(
                Host::IpAddr("127.0.0.1".parse().expect("ip")),
                self.port,
                String::new(),
                false,
                "/".into(),
                false,
            ),
        })
    }

    pub async fn wait_ready(&self) {
        timeout(READY, async {
            loop {
                if self.api_connects.load(Ordering::SeqCst) > 0
                    && self.event_connects.load(Ordering::SeqCst) > 0
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("bot 未连上 mock api/event WS");
    }

    pub async fn ask(&self, text: &str, wait: Duration) -> Reply {
        timeout(wait, self.ask_inner(ADMIN, text))
            .await
            .unwrap_or_else(|_| panic!("{text} 在 {wait:?} 内没有 send_msg"))
    }

    pub async fn ask_from(&self, user_id: i64, text: &str, wait: Duration) -> Reply {
        timeout(wait, self.ask_inner(user_id, text))
            .await
            .unwrap_or_else(|_| panic!("{text} 在 {wait:?} 内没有 send_msg"))
    }

    pub async fn ask_private(&self, user_id: i64, text: &str, wait: Duration) -> Reply {
        timeout(wait, async {
            self.drain_messages().await;
            self.send_event(private_message(user_id, text, self.fresh_event_id()))
                .await;
            self.next_message().await
        })
        .await
        .unwrap_or_else(|_| panic!("私聊 {text} 在 {wait:?} 内没有 send_msg"))
    }

    pub async fn emit_group(&self, user_id: i64, text: &str) {
        self.send_event(group_message(user_id, text, self.fresh_event_id()))
            .await;
    }

    pub async fn ask_with_image(
        &self,
        user_id: i64,
        text: &str,
        image: &Path,
        wait: Duration,
    ) -> Option<Reply> {
        let file = format!("file://{}", image.display());
        let event = group_event(
            user_id,
            json!([
                {"type": "text", "data": {"text": text}},
                {"type": "image", "data": {"file": file}},
            ]),
            text,
            self.fresh_event_id(),
        );
        timeout(wait, async {
            self.drain_messages().await;
            self.send_event(event).await;
            self.next_message().await
        })
        .await
        .ok()
    }

    pub async fn ask_inner(&self, user_id: i64, text: &str) -> Reply {
        self.drain_messages().await;
        self.send_event(group_message(user_id, text, self.fresh_event_id()))
            .await;
        self.next_message().await
    }

    fn fresh_event_id(&self) -> i32 {
        self.next_event_message_id.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn drain_messages(&self) {
        self.outgoing.lock().await.clear();
    }

    pub async fn next_message(&self) -> Reply {
        loop {
            if let Some(api) = self.take_message().await {
                return summarize(&api);
            }
            self.notify.notified().await;
        }
    }

    async fn take_message(&self) -> Option<Value> {
        let mut outgoing = self.outgoing.lock().await;
        outgoing
            .iter()
            .position(|api| {
                matches!(
                    api["action"].as_str(),
                    Some(
                        "send_msg"
                            | "send_group_msg"
                            | "send_private_msg"
                            | "send_group_forward_msg"
                            | "send_forward_msg"
                    )
                )
            })
            .map(|idx| outgoing.remove(idx))
    }

    async fn send_event(&self, event: Value) {
        let tx = self.event_cmd.lock().await.clone();
        let Some(tx) = tx else {
            panic!("event WS 尚未连接");
        };
        tx.send(ConnCmd::Send(Message::text(event.to_string())))
            .await
            .expect("push event");
    }

    async fn handle_conn(&self, stream: TcpStream) {
        let mut path = String::new();
        let ws = match accept_hdr_async(stream, {
            #[allow(clippy::result_large_err)]
            |req: &Request, res: Response| {
                path = req.uri().path().to_string();
                Ok(res)
            }
        })
        .await
        {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let kind = path.trim_matches('/');
        let kind = kind.rsplit('/').next().unwrap_or(kind);
        match kind {
            "event" => self.serve_event(ws).await,
            "api" => self.serve_api(ws).await,
            _ => {}
        }
    }

    async fn serve_event(&self, ws: WebSocketStream<TcpStream>) {
        let (tx, rx) = mpsc::channel(16);
        *self.event_cmd.lock().await = Some(tx);
        self.event_connects.fetch_add(1, Ordering::SeqCst);
        run_ws_session(ws, rx).await;
    }

    async fn serve_api(&self, mut ws: WebSocketStream<TcpStream>) {
        self.api_connects.fetch_add(1, Ordering::SeqCst);
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let Ok(req) = kovi::serde_json::from_str::<Value>(text.as_ref()) else {
                        continue;
                    };
                    self.outgoing.lock().await.push(req.clone());
                    self.notify.notify_waiters();
                    let echo = req.get("echo").cloned().unwrap_or(json!(""));
                    let action = req.get("action").and_then(Value::as_str).unwrap_or("");
                    let data = match action {
                        "get_login_info" => json!({
                            "user_id": BOT_ID,
                            "nickname": "mock-bot"
                        }),
                        "get_group_member_info" => {
                            let user_id = req["params"]["user_id"].as_i64().unwrap_or(ADMIN);
                            json!({
                                "user_id": user_id,
                                "nickname": "tester",
                                "card": "tester",
                                "role": "member",
                                "group_id": GROUP,
                            })
                        }
                        "send_msg"
                        | "send_group_msg"
                        | "send_private_msg"
                        | "send_group_forward_msg"
                        | "send_forward_msg" => json!({
                            "message_id": self.next_message_id.fetch_add(1, Ordering::SeqCst)
                        }),
                        _ => json!({}),
                    };
                    let resp = json!({
                        "status": "ok",
                        "retcode": 0,
                        "data": data,
                        "echo": echo,
                    });
                    if ws.send(Message::text(resp.to_string())).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            }
        }
    }
}

impl Drop for MockOneBot {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
    }
}

async fn run_ws_session(mut ws: WebSocketStream<TcpStream>, mut cmd_rx: mpsc::Receiver<ConnCmd>) {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ConnCmd::Send(msg)) => {
                        if ws.send(msg).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_) | Message::Text(_))) | None | Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

fn group_message(user_id: i64, text: &str, message_id: i32) -> Value {
    group_event(
        user_id,
        json!([{"type": "text", "data": {"text": text}}]),
        text,
        message_id,
    )
}

fn now_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs() as i64
}

fn group_event(user_id: i64, message: Value, raw: &str, message_id: i32) -> Value {
    json!({
        // 事件时刻取真实时钟：消息库按事件时间入账，硬编码旧时间会落出「今日」窗口。
        "time": now_timestamp(),
        "self_id": BOT_ID,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "message_id": message_id,
        "group_id": GROUP,
        "user_id": user_id,
        "anonymous": null,
        "message": message,
        "raw_message": raw,
        "font": 0,
        "sender": {
            "user_id": user_id,
            "nickname": "tester",
            "card": "",
            "role": "member"
        }
    })
}

fn private_message(user_id: i64, text: &str, message_id: i32) -> Value {
    json!({
        "time": now_timestamp(),
        "self_id": BOT_ID,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "message_id": message_id,
        "user_id": user_id,
        "message": [{"type": "text", "data": {"text": text}}],
        "raw_message": text,
        "font": 0,
        "sender": {
            "user_id": user_id,
            "nickname": "tester"
        }
    })
}

/// 等所有插件命令注册进 /help（以 needles 为判定门槛），返回最终帮助文本。
/// msg_rank 要起 SQLite 比其他插件慢，只等部分插件会在慢机上对着没注册完的帮助断言。
pub async fn wait_until_help_ready(server: &MockOneBot, needles: &[&str]) -> String {
    let deadline = tokio::time::Instant::now() + READY;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        let reply = match timeout(Duration::from_secs(2), server.ask_inner(ADMIN, "/help")).await {
            Ok(reply) => reply,
            Err(_) => continue,
        };
        last = reply.text;
        if needles.iter().all(|needle| last.contains(needle)) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("插件未在 {READY:?} 内完成注册，最后帮助: {last}");
}

pub async fn assert_contains(server: &MockOneBot, cmd: &str, needle: &str, wait: Duration) {
    let reply = server.ask(cmd, wait).await;
    assert!(
        reply.text.contains(needle),
        "{cmd} 回复应含 {needle:?}，实际: {reply:?}"
    );
}

/// 把 cwd 切进临时目录并铺好共享配置：config.toml（来自 repo 的 example）、
/// data/wordle、data/msg_rank（含采集/词云配置）与 fixtures。
/// 各测试自己的词表、样图在返回的 root 上补充。
pub struct IsolatedCwd {
    previous: PathBuf,
    pub root: PathBuf,
}

impl IsolatedCwd {
    pub fn enter(tag: &str, repo: &Path) -> Self {
        let root = std::env::temp_dir().join(format!(
            "qqbot-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("data/wordle")).unwrap();
        std::fs::create_dir_all(root.join("data/msg_rank")).unwrap();
        std::fs::create_dir_all(root.join("fixtures")).unwrap();

        let config_src = repo.join("config.toml.example");
        assert!(config_src.is_file(), "缺少 {}", config_src.display());
        std::fs::copy(&config_src, root.join("config.toml")).unwrap();
        std::fs::write(
            root.join("data/msg_rank/config.json"),
            format!(
                r##"{{"record_group":[{GROUP}],"wordcloud_group":[{GROUP}],"tencent":null,"wordcloud_background":"#ffffff"}}"##
            ),
        )
        .unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        Self { previous, root }
    }
}

impl Drop for IsolatedCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// size×size 带纹理 RGB PNG。感知哈希会跳过纯色图、SIFT 需要角点，都要对比度。
pub fn rgb_pattern_png(size: u32, seed: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((1 + size * 3) * size) as usize);
    for y in 0..size {
        raw.push(0);
        for x in 0..size {
            push_pattern_pixel(&mut raw, x, y, seed);
        }
    }
    encode_png(size, size, &raw)
}

/// 同一纹理公式在偏移处的 crop，内容与整图局部一致，SIFT 可匹配。
pub fn cropped_pattern_png(seed: u32, offset: u32, crop: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((1 + crop * 3) * crop) as usize);
    for y in offset..offset + crop {
        raw.push(0);
        for x in offset..offset + crop {
            push_pattern_pixel(&mut raw, x, y, seed);
        }
    }
    encode_png(crop, crop, &raw)
}

fn push_pattern_pixel(raw: &mut Vec<u8>, x: u32, y: u32, seed: u32) {
    let wave = ((x / 8 + seed) % 32) * ((y / 8 + seed * 3) % 32);
    let v = (x.wrapping_mul(13) ^ y.wrapping_mul(7) ^ wave) as u8;
    raw.push(v);
    raw.push(v.wrapping_add(40));
    raw.push(220u8.wrapping_sub(v));
}

fn encode_png(width: u32, height: u32, raw: &[u8]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    write_png_chunk(&mut out, b"IDAT", &zlib_store(raw));
    write_png_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_png_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// uncompressed deflate，多块存储（单块上限 65535 字节，512² 图放不下）。
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + data.len() + 4);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = data.chunks(65_535).peekable();
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(u8::from(last));
        let len = u16::try_from(chunk.len()).expect("chunk fits one deflate block");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &byte in data {
        s1 = (s1 + u32::from(byte)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    (s2 << 16) | s1
}

fn summarize(api: &Value) -> Reply {
    let mut text_parts = Vec::new();
    let mut has_image = false;
    match api["action"].as_str() {
        Some("send_group_forward_msg" | "send_forward_msg") => {
            if let Value::Array(nodes) = &api["params"]["messages"] {
                for node in nodes {
                    collect_message(&node["data"]["content"], &mut text_parts, &mut has_image);
                }
            }
        }
        _ => collect_message(&api["params"]["message"], &mut text_parts, &mut has_image),
    }
    Reply {
        text: text_parts.join(""),
        has_image,
    }
}

fn collect_message(message: &Value, text_parts: &mut Vec<String>, has_image: &mut bool) {
    match message {
        Value::String(s) => text_parts.push(s.clone()),
        Value::Array(segs) => {
            for seg in segs {
                match seg["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = seg["data"]["text"].as_str() {
                            text_parts.push(t.to_owned());
                        }
                    }
                    Some("image") => *has_image = true,
                    Some("node") => {
                        collect_message(&seg["data"]["content"], text_parts, has_image);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}
