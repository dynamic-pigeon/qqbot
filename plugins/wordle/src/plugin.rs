//! QQ 插件适配层：群公开局 Wordle。
//!
//! 一个群同时只有一局进行中（公开局，全群共猜一个答案）；
//! 私聊以用户号为会话 key，等价于单人局。反馈以图片形式发出：
//! 6×5 网格，色块内嵌字母，配色为标准 Wordle 绿黄灰。
//! 图片由纯 Rust 渲染（`render` 模块），不依赖浏览器。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin, tokio::sync::OnceCell};
use kovi_onebot::MessageRegistrar as _;
use utils::command::{Command, CommandContext, CommandError, CommandResult, CommandRouter};

use crate::game::{Game, SubmitError, WORD_LEN, pick_answer};
use crate::render::render_board_png;
use crate::words::{WordList, load_or_download};

/// 会话无活动超过此时长后作废，下次 start 直接开新局。
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// 词库只加载一次；下载失败不缓存，下次命令可重试。
static WORD_LIST: LazyLock<OnceCell<std::sync::Arc<WordList>>> = LazyLock::new(OnceCell::new);

struct Session {
    game: Game,
    last_active: Instant,
}

/// 会话表：公开局 key 为群号，私聊 key 为用户号。
static SESSIONS: LazyLock<Mutex<HashMap<i64, Session>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn word_list() -> Result<std::sync::Arc<WordList>, anyhow::Error> {
    WORD_LIST
        .get_or_try_init(|| async {
            load_or_download(Path::new("data"))
                .await
                .map(std::sync::Arc::new)
        })
        .await
        .cloned()
}

pub async fn run() {
    let bot = plugin::get_runtime_bot();
    CommandRouter::new("wordle", bot)
        .register(wordle_command())
        .install()
        .expect("注册 /wordle 命令失败");
}

fn wordle_command() -> Command {
    Command::new("/wordle")
        .description("群内英文 Wordle 猜词游戏（公开局，全群共猜）")
        .usage("/wordle start [hard] | guess <单词> | status")
        .subcommand(
            Command::new("start")
                .description("开始一局新的猜词（加 hard 为严格模式）")
                .usage("/wordle start [hard]")
                .handler(handle_start),
        )
        .subcommand(
            Command::new("guess")
                .description("猜一个 5 字母单词")
                .usage("/wordle guess <单词>")
                .handler(handle_guess),
        )
        .subcommand(
            Command::new("status")
                .description("查看当前局面的网格图")
                .usage("/wordle status")
                .handler(handle_status),
        )
}

async fn handle_start(ctx: CommandContext) -> CommandResult {
    let hard = ctx.arg(0).is_some_and(|arg| arg == "hard");
    ctx.ensure_no_extra_args(1)?;
    let words = word_list().await.map_err(CommandError::internal)?;
    let key = session_key(&ctx);

    let mut sessions = lock_sessions();
    expire_sessions(&mut sessions);
    if sessions.get(&key).is_some_and(|s| !s.game.is_over()) {
        return Err(CommandError::user("本群已有一局进行中，先把它猜完吧"));
    }
    let game = if hard {
        Game::new_adversarial(words.answers.clone())
    } else {
        Game::new(pick_answer(&words.answers, rand::random()).to_owned())
    };
    // 渲染是毫秒级的纯 CPU 操作，持锁完成即可，无并发窗口。
    let png = render_board_png(&game);
    sessions.insert(
        key,
        Session {
            game,
            last_active: Instant::now(),
        },
    );
    drop(sessions);

    let note = if hard {
        "🔤 开局（严格模式）！答案不固定，随你的猜测动态变化"
    } else {
        "🔤 开局！/wordle guess <单词> 开始猜"
    };
    reply_with_image(&ctx, &png, note);
    Ok(())
}

async fn handle_guess(ctx: CommandContext) -> CommandResult {
    let guess = ctx
        .arg(0)
        .ok_or_else(|| CommandError::MissingArgument {
            name: "单词".to_owned(),
        })?
        .to_ascii_lowercase();
    ctx.ensure_no_extra_args(1)?;
    let words = word_list().await.map_err(CommandError::internal)?;
    let key = session_key(&ctx);

    let (png, note) = {
        let mut sessions = lock_sessions();
        submit_guess(&mut sessions, key, &guess, &words)?
    };
    match note {
        Some(note) => reply_with_image(&ctx, &png, &note),
        None => reply_with_image(&ctx, &png, ""),
    }
    Ok(())
}

/// 提交一次猜测并生成回复内容（PNG 网格 + 可选的附注文字）。
///
/// 纯逻辑、不涉及网络，便于单测所有分支。
fn submit_guess(
    sessions: &mut HashMap<i64, Session>,
    key: i64,
    guess: &str,
    words: &WordList,
) -> Result<(Vec<u8>, Option<String>), CommandError> {
    expire_sessions(sessions);
    let Some(session) = sessions.get_mut(&key) else {
        return Err(CommandError::user("本群还没开局，先发 /wordle start"));
    };
    match session.game.submit(guess, &words.allowed) {
        Err(SubmitError::InvalidLength) => {
            return Err(CommandError::user(format!("请输入恰好 {WORD_LEN} 个字母")));
        }
        Err(SubmitError::NotInWordList) => {
            return Err(CommandError::user(format!("“{guess}”不在词表中")));
        }
        Err(SubmitError::GameOver) => {
            return Err(CommandError::user("本局已结束，发 /wordle start 开新局"));
        }
        Ok(_) => {}
    }
    session.last_active = Instant::now();
    let png = render_board_png(&session.game);
    let note = session.game.result_note();
    Ok((png, note))
}

async fn handle_status(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let key = session_key(&ctx);
    // 渲染与次数统计在同一把锁内完成，避免两次加锁之间会话被
    // 并发 start 覆盖或 TTL 清理，导致图片与文字不一致。
    let (png, count) = {
        let mut sessions = lock_sessions();
        expire_sessions(&mut sessions);
        let Some(session) = sessions.get_mut(&key) else {
            return Err(CommandError::user("本群还没开局，先发 /wordle start"));
        };
        if session.game.is_over() {
            return Err(CommandError::user("本局已结束，发 /wordle start 开新局"));
        }
        session.last_active = Instant::now();
        let png = render_board_png(&session.game);
        let count = session.game.guesses_count();
        (png, count)
    };

    reply_with_image(&ctx, &png, &format!("当前第 {count} 次猜测"));
    Ok(())
}

fn session_key(ctx: &CommandContext) -> i64 {
    // 群消息按群号共享一局；私聊（含群临时会话之外的）按用户号独立。
    ctx.event().group_id.unwrap_or(ctx.event().user_id)
}

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<i64, Session>> {
    SESSIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn expire_sessions(sessions: &mut HashMap<i64, Session>) {
    sessions.retain(|_, s| s.last_active.elapsed() < SESSION_TTL);
}

/// 把渲染好的网格发到群里；`note` 非空时附加一行文字。
fn reply_with_image(ctx: &CommandContext, png: &[u8], note: &str) {
    let base64_img = base64::engine::general_purpose::STANDARD.encode(png);
    let mut message = Message::new().add_image(&format!("base64://{base64_img}"));
    if !note.is_empty() {
        message = message.add_text(note);
    }
    ctx.reply(message);
}

#[cfg(test)]
mod tests {
    use utils::command::{CommandTree, ResolveOutcome};

    use super::*;

    fn test_words() -> WordList {
        let answers = vec!["crane".to_owned()];
        let allowed: std::collections::HashSet<String> = [
            "crane", "slate", "serve", "other", "night", "focus", "happy",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        WordList { answers, allowed }
    }

    fn fresh_session(answer: &str) -> Session {
        Session {
            game: Game::new(answer.to_owned()),
            last_active: Instant::now(),
        }
    }

    #[test]
    fn submit_guess_requires_started_session() {
        let mut sessions = HashMap::new();
        let err = submit_guess(&mut sessions, 1, "slate", &test_words()).unwrap_err();
        assert!(err.to_string().contains("还没开局"), "{err}");
    }

    #[test]
    fn submit_guess_rejects_invalid_words_without_consuming() {
        let mut sessions = HashMap::new();
        sessions.insert(1, fresh_session("crane"));

        let err = submit_guess(&mut sessions, 1, "qqqqq", &test_words()).unwrap_err();
        assert!(err.to_string().contains("不在词表中"), "{err}");
        let err = submit_guess(&mut sessions, 1, "abcd", &test_words()).unwrap_err();
        assert!(err.to_string().contains("恰好 5 个字母"), "{err}");
        assert_eq!(sessions[&1].game.guesses_count(), 0, "非法输入不消耗次数");
    }

    #[test]
    fn submit_guess_win_note() {
        let mut sessions = HashMap::new();
        sessions.insert(1, fresh_session("crane"));

        let (png, note) = submit_guess(&mut sessions, 1, "slate", &test_words()).unwrap();
        assert!(note.is_none(), "未结束时无附注");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "反馈应为 PNG 图片");

        let (_, note) = submit_guess(&mut sessions, 1, "crane", &test_words()).unwrap();
        let note = note.expect("猜中应有附注");
        assert!(
            note.contains("🎉") && note.contains("CRANE") && note.contains("2 次"),
            "{note}"
        );
    }

    #[test]
    fn submit_guess_exhaust_then_game_over() {
        let mut sessions = HashMap::new();
        sessions.insert(1, fresh_session("crane"));
        let words = test_words();
        let mut last_note = None;
        for _ in 0..6 {
            let (_, note) = submit_guess(&mut sessions, 1, "other", &words).unwrap();
            last_note = note;
        }
        let note = last_note.expect("用尽应有附注");
        assert!(note.contains("😞") && note.contains("CRANE"), "{note}");

        let err = submit_guess(&mut sessions, 1, "slate", &words).unwrap_err();
        assert!(err.to_string().contains("本局已结束"), "{err}");
    }

    #[test]
    fn wordle_command_tree_resolves() {
        let tree = CommandTree::new(vec![wordle_command()]).unwrap();

        for (input, path) in [
            ("/wordle start", vec!["/wordle", "start"]),
            ("/wordle start hard", vec!["/wordle", "start"]),
            ("/wordle guess slate", vec!["/wordle", "guess"]),
            ("/wordle status", vec!["/wordle", "status"]),
        ] {
            let ResolveOutcome::Matched(command) = tree.resolve(input) else {
                panic!("{input} 应解析成功");
            };
            assert_eq!(command.path(), path);
            if input.contains("slate") {
                assert_eq!(command.args(), ["slate"]);
            }
            if input.contains("hard") {
                assert_eq!(command.args(), ["hard"]);
            }
        }

        assert!(matches!(tree.resolve("/wordle"), ResolveOutcome::Error(_)));
        assert!(matches!(
            tree.resolve("/wordle bogus"),
            ResolveOutcome::Error(_)
        ));
    }
}
