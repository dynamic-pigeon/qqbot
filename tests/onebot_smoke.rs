//! 用内存 OneBot V11 正向 WS 冒烟：current_thread 下各插件命令能否正常回复。
//!
//! 不改生产订阅配置。图库 sqlite 写在 `data/image_lib/<GROUP>`，用例结束会删掉。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

use bot::build_bot;
use kovi::config::kovi_conf::KoviConf;
use kovi::event::id::ID;
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

const ADMIN: i64 = 10001;
const STRANGER: i64 = 20002;
const GROUP: i64 = 910_000_001;
const BOT_ID: i64 = 10000;
const READY: Duration = Duration::from_secs(10);
const REPLY: Duration = Duration::from_secs(8);
const SLOW: Duration = Duration::from_secs(45);

#[tokio::test(flavor = "current_thread")]
async fn plugin_commands_reply_on_mock_onebot() {
    let _ = utils::config::value();
    let _cleanup = GroupDataCleanup(format!("data/image_lib/{GROUP}"));
    let server = MockOneBot::start().await;
    let bot = build_bot(KoviConf::new(ID::new(ADMIN), None, false), server.driver());
    let run = tokio::spawn(bot.run());
    let _abort = AbortOnDrop(run);

    server.wait_ready().await;
    let help = wait_until_help_ready(&server).await;
    assert!(help.contains("📚 可用命令"), "{help}");
    for needle in [
        "/help", "/wordle", "/live", "/dynamic", "图库", "!md", "/查卡",
    ] {
        assert!(help.contains(needle), "帮助缺少 {needle}: {help}");
    }

    assert_contains(&server, "/help /wordle", "开始一局").await;
    assert_contains(&server, "/live list", "本群尚未订阅任何直播间").await;
    assert_contains(&server, "/dynamic list", "本群尚未订阅任何动态").await;
    assert_contains(&server, "图库", "本群还没有图库").await;
    assert_contains(&server, "来只 猫", "「猫」里还没有图").await;
    assert_contains(&server, "/wordcloud status", "词云功能未启用").await;
    assert_contains(&server, "/查卡", "缺少参数 `卡片名称`").await;
    assert_contains(&server, "!md", "缺少参数 `Markdown 内容`").await;
    assert_contains(&server, "#今日发言排行", "命令执行失败").await;

    let denied = server.ask_from(STRANGER, "/wordcloud status", REPLY).await;
    assert!(
        denied.text.contains("管理员专用"),
        "非管理员应被拒绝: {}",
        denied.text
    );

    let private = server.ask_private(ADMIN, "/live list", REPLY).await;
    assert!(
        private.text.contains("只能在群聊中使用"),
        "群命令私聊应被拒绝: {}",
        private.text
    );

    let start = server.ask("/wordle start", SLOW).await;
    assert!(
        start.has_image && start.text.contains("开局"),
        "wordle 开局应带图: {start:?}"
    );
    let status = server.ask("/wordle status", REPLY).await;
    assert!(
        status.has_image && status.text.contains("当前第"),
        "wordle status 应带图: {status:?}"
    );
    assert_contains(&server, "/wordle guess qqqqq", "不在词表中").await;

    let md = server.ask("!md **hi**", SLOW).await;
    assert!(
        md.has_image || md.text.contains("命令执行失败") || md.text.contains("过于频繁"),
        "!md 不应挂死: {md:?}"
    );

    let card = server.ask("/查卡 青眼白龙", SLOW).await;
    assert!(
        card.has_image
            || card.text.contains("未找到卡片")
            || card.text.contains("命令执行失败")
            || card.text.contains("查询过于频繁"),
        "/查卡 不应挂死: {card:?}"
    );
}

async fn wait_until_help_ready(server: &MockOneBot) -> String {
    let deadline = tokio::time::Instant::now() + READY;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        let reply = match timeout(Duration::from_secs(2), server.ask_inner(ADMIN, "/help")).await {
            Ok(reply) => reply,
            Err(_) => continue,
        };
        last = reply.text;
        if last.contains("/wordle") && last.contains("图库") && last.contains("!md") {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("插件未在 {READY:?} 内完成注册，最后帮助: {last}");
}

async fn assert_contains(server: &MockOneBot, cmd: &str, needle: &str) {
    let reply = server.ask(cmd, REPLY).await;
    assert!(
        reply.text.contains(needle),
        "{cmd} 回复应含 {needle:?}，实际: {reply:?}"
    );
}

struct GroupDataCleanup(String);

impl Drop for GroupDataCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<kovi::ExitEvent>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct Reply {
    text: String,
    has_image: bool,
}

enum ConnCmd {
    Send(Message),
}

struct MockOneBot {
    port: u16,
    event_cmd: Mutex<Option<mpsc::Sender<ConnCmd>>>,
    api_connects: AtomicUsize,
    event_connects: AtomicUsize,
    outgoing: Mutex<Vec<Value>>,
    notify: Notify,
    shutdown: Notify,
    next_message_id: AtomicI32,
}

impl MockOneBot {
    async fn start() -> Arc<Self> {
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
        });
        let accept = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept.shutdown.notified() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        let accept = Arc::clone(&accept);
                        tokio::spawn(async move { accept.handle_conn(stream).await });
                    }
                }
            }
        });
        server
    }

    fn driver(&self) -> OneBotDriver {
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

    async fn wait_ready(&self) {
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

    async fn ask(&self, text: &str, wait: Duration) -> Reply {
        timeout(wait, self.ask_inner(ADMIN, text))
            .await
            .unwrap_or_else(|_| panic!("{text} 在 {wait:?} 内没有 send_msg"))
    }

    async fn ask_from(&self, user_id: i64, text: &str, wait: Duration) -> Reply {
        timeout(wait, self.ask_inner(user_id, text))
            .await
            .unwrap_or_else(|_| panic!("{text} 在 {wait:?} 内没有 send_msg"))
    }

    async fn ask_private(&self, user_id: i64, text: &str, wait: Duration) -> Reply {
        timeout(wait, async {
            self.drain_messages().await;
            self.send_event(private_message(user_id, text)).await;
            self.next_message().await
        })
        .await
        .unwrap_or_else(|_| panic!("私聊 {text} 在 {wait:?} 内没有 send_msg"))
    }

    async fn ask_inner(&self, user_id: i64, text: &str) -> Reply {
        self.drain_messages().await;
        self.send_event(group_message(user_id, text)).await;
        self.next_message().await
    }

    async fn drain_messages(&self) {
        self.outgoing.lock().await.clear();
    }

    async fn next_message(&self) -> Reply {
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
                    Some("send_msg" | "send_group_msg" | "send_private_msg")
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
                        "send_msg" | "send_group_msg" | "send_private_msg" => json!({
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

fn group_message(user_id: i64, text: &str) -> Value {
    json!({
        "time": 1_700_000_000,
        "self_id": BOT_ID,
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "message_id": 1,
        "group_id": GROUP,
        "user_id": user_id,
        "anonymous": null,
        "message": [{"type": "text", "data": {"text": text}}],
        "raw_message": text,
        "font": 0,
        "sender": {
            "user_id": user_id,
            "nickname": "tester",
            "card": "",
            "role": "member"
        }
    })
}

fn private_message(user_id: i64, text: &str) -> Value {
    json!({
        "time": 1_700_000_000,
        "self_id": BOT_ID,
        "post_type": "message",
        "message_type": "private",
        "sub_type": "friend",
        "message_id": 1,
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

fn summarize(api: &Value) -> Reply {
    let message = &api["params"]["message"];
    let mut text_parts = Vec::new();
    let mut has_image = false;
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
                    Some("image") => has_image = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Reply {
        text: text_parts.join(""),
        has_image,
    }
}
