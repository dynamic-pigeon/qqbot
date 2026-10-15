//! 用内存 OneBot V11 正向 WS 冒烟：current_thread 下各插件命令能否正常回复。
//!
//! 配置、词库和样图都写在临时目录，不读 gitignore 的 `data/` / `config.toml`。
//! 词云读库依赖 msg_rank 5 秒刷盘，所以 `/wordcloud once` 前会等一拍。
//! mock OneBot 与测试装置在 `tests/common/`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::build_bot;
use kovi::config::kovi_conf::KoviConf;
use kovi::event::id::ID;
use kovi::tokio;
use tokio::time::timeout;

mod common;

use common::{ADMIN, MockOneBot, rgb_pattern_png, wait_until_help_ready};

const STRANGER: i64 = 20002;
const REPLY: Duration = Duration::from_secs(8);
const SLOW: Duration = Duration::from_secs(45);

/// 帮助列表必须出现的命令，同时也是注册完成的判定门槛。
const HELP_NEEDLES: [&str; 8] = [
    "/help", "/wordle", "/live", "/dynamic", "图库", "!md", "/查卡", "/周报",
];

async fn assert_contains(server: &MockOneBot, cmd: &str, needle: &str) {
    common::assert_contains(server, cmd, needle, REPLY).await;
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_commands_reply_on_mock_onebot() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let iso = common::IsolatedCwd::enter("smoke", &repo);
    let (sample_a, sample_b) = write_smoke_fixtures(&iso.root);
    utils::config::preload();
    let server = MockOneBot::start().await;
    let bot = build_bot(KoviConf::new(ID::new(ADMIN), None, false), server.driver());
    let run = tokio::spawn(bot.run());
    let _abort = common::AbortOnDrop(run);

    server.wait_ready().await;
    let help = wait_until_help_ready(&server, &HELP_NEEDLES).await;
    assert!(help.contains("📚 可用命令"), "{help}");
    for needle in HELP_NEEDLES {
        assert!(help.contains(needle), "帮助缺少 {needle}: {help}");
    }

    assert_contains(&server, "/help /wordle", "开始一局").await;
    assert_contains(&server, "/live list", "本群尚未订阅任何直播间").await;
    assert_contains(&server, "/dynamic list", "本群尚未订阅任何动态").await;
    assert_contains(&server, "/dynamic add 1", "已为本群订阅").await;
    assert_contains(&server, "/dynamic rm 1", "已取消").await;
    assert_contains(&server, "图库", "本群还没有图库").await;
    assert_contains(&server, "来只 猫", "「猫」里还没有图").await;
    assert_contains(&server, "/消息采集 status", "消息采集已启用").await;
    assert_contains(&server, "/wordcloud status", "词云功能已启用").await;
    assert_contains(&server, "/wordcloud disable", "停用成功").await;
    assert_contains(&server, "/wordcloud status", "词云功能未启用").await;
    assert_contains(&server, "/wordcloud enable", "启用成功").await;
    assert_contains(&server, "/wordcloud status", "词云功能已启用").await;
    assert_contains(&server, "/查卡", "缺少参数 `卡片名称`").await;
    assert_contains(&server, "!md", "缺少参数 `Markdown 内容`").await;

    // 周报放在 B 话榜之前：两者共享每群 30 秒冷却，先测周报才能走到真实分支
    // 而不是「刚跑完」。测试消息都落在今天，上周窗口必然为空。
    assert_contains(&server, "/周报", "上周暂无发言数据").await;
    assert_contains(&server, "/周报 status", "定时周报未启用").await;
    assert_contains(&server, "/周报 enable", "定时周报已启用").await;
    assert_contains(&server, "/周报 status", "定时周报已启用").await;
    assert_contains(&server, "/周报 disable", "定时周报已停用").await;
    let weekly_denied = server.ask_from(STRANGER, "/周报 enable", REPLY).await;
    assert!(
        weekly_denied.text.contains("管理员专用"),
        "非管理员不应能开周报推送: {}",
        weekly_denied.text
    );

    let rank = server.ask("/今日B话榜", SLOW).await;
    assert!(
        rank.has_image || rank.text.contains("命令执行失败") || rank.text.contains("刚跑完"),
        "排行不应挂死: {rank:?}"
    );

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

    let added = server
        .ask_with_image(ADMIN, "添加 猫", &sample_a, REPLY)
        .await
        .expect("添加 猫 应回复");
    assert!(
        added.text.contains("添加") || added.text.contains("都已在"),
        "添加失败: {added:?}"
    );
    let added_mask = server
        .ask_with_image(ADMIN, "添加 猫", &sample_b, REPLY)
        .await
        .expect("添加第二张图应回复");
    assert!(
        added_mask.text.contains("添加") || added_mask.text.contains("都已在"),
        "添加第二张图失败: {added_mask:?}"
    );
    assert_contains(&server, "图库", "猫").await;
    assert_contains(&server, "别名 喵 猫", "别名").await;
    let draw = server.ask("来只 喵", REPLY).await;
    assert!(
        draw.has_image || draw.text.contains("没有图") || draw.text.contains("频繁"),
        "来只: {draw:?}"
    );
    let scan = server.ask("查重 猫", SLOW).await;
    assert!(
        !scan.text.is_empty() || scan.has_image,
        "查重应有回复: {scan:?}"
    );
    let hashed = server.ask("哈希 aa", REPLY).await;
    assert!(
        hashed.text.contains("没有") || hashed.text.contains("哈希") || hashed.has_image,
        "哈希: {hashed:?}"
    );
    assert_contains(&server, "取消别名 喵", "已取消别名").await;

    assert_contains(&server, "删除 狗", "「狗」不存在").await;
    let wipe = server.ask("删除 猫", REPLY).await;
    assert!(
        wipe.text.contains("张图") && wipe.text.contains("图库 确认"),
        "删除整库应先登记待确认: {wipe:?}"
    );
    assert_contains(&server, "图库 取消", "已取消清空「猫」").await;
    assert_contains(&server, "图库 确认", "没有待确认的清空操作").await;

    let removed = server
        .ask_with_image(ADMIN, "删除", &sample_a, REPLY)
        .await
        .expect("删除带图应回复");
    assert!(
        removed.text.contains("已从「猫」删除这张图"),
        "按本消息图片删除失败: {removed:?}"
    );

    assert_contains(&server, "删除 猫", "图库 确认").await;
    assert_contains(&server, "图库 确认", "已清空「猫」").await;
    assert_contains(&server, "图库", "本群还没有图库").await;

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
    let guess = server.ask("/wordle guess crane", SLOW).await;
    assert!(
        guess.has_image || guess.text.contains("猜"),
        "wordle guess: {guess:?}"
    );

    for i in 0..8 {
        // 夹杂英文，词云在没有 CJK 字体时也能用内置英文字体画出图。
        server
            .emit_group(
                ADMIN,
                &format!("闲聊消息 {i} 今天天气不错 词云测试 weather wordcloud hello"),
            )
            .await;
    }
    tokio::time::sleep(Duration::from_secs(6)).await;
    server.drain_messages().await;
    let pending = server.ask("/wordcloud once", REPLY).await;
    assert!(
        pending.text.contains("正在生成"),
        "once 应先回正在生成: {pending:?}"
    );
    let cloud = timeout(SLOW, server.next_message())
        .await
        .unwrap_or_else(|_| panic!("/wordcloud once 在 {SLOW:?} 内没有发出词云图"));
    assert!(
        cloud.has_image && cloud.text.contains("临时词云"),
        "once 应发出词云图: {cloud:?}"
    );

    let md = server.ask("!md **hi**", SLOW).await;
    assert!(
        md.has_image || md.text.contains("命令执行失败") || md.text.contains("过于频繁"),
        "!md 不应挂死: {md:?}"
    );

    let card = server.ask("/查卡 青眼白龙", SLOW).await;
    assert!(
        card.has_image
            || card.text.contains("未找到卡片")
            || card.text.contains("获取卡片图片失败")
            || card.text.contains("命令执行失败")
            || card.text.contains("查询过于频繁"),
        "/查卡 不应挂死: {card:?}"
    );
}

/// 冒烟自己的词表与样图：词表给 wordle（crane 在答案表里），两张 16² 样图给图库用例。
fn write_smoke_fixtures(root: &Path) -> (PathBuf, PathBuf) {
    write_word_lists(&root.join("data/wordle"));
    let sample_a = root.join("fixtures/a.png");
    let sample_b = root.join("fixtures/b.png");
    std::fs::write(&sample_a, rgb_pattern_png(16, 1)).unwrap();
    std::fs::write(&sample_b, rgb_pattern_png(16, 200)).unwrap();
    (sample_a, sample_b)
}

fn write_word_lists(dir: &Path) {
    let answers: Vec<String> = std::iter::once("crane".to_string())
        .chain((0..2_000).map(fake_word))
        .collect();
    let allowed: Vec<String> = std::iter::once("crane".to_string())
        .chain((0..12_000).map(fake_word))
        .collect();
    std::fs::write(dir.join("answers.txt"), answers.join("\n")).unwrap();
    std::fs::write(dir.join("allowed.txt"), allowed.join("\n")).unwrap();
}

fn fake_word(mut i: usize) -> String {
    let mut word = String::with_capacity(5);
    for _ in 0..5 {
        word.push((b'a' + (i % 26) as u8) as char);
        i /= 26;
    }
    word
}
