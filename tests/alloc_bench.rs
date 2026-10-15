//! 分配器内存基准：glibc 默认 / mimalloc / jemalloc 三种构建下跑同一负载，
//! 由 `scripts/alloc-bench/bench.sh` 外部采样进程 RSS/PSS。
//!
//! mock OneBot 与测试装置在 `tests/common/`（与 onebot_smoke.rs 共享），
//! 断言只保负载真实生效，不做行为校验。负载阶段：
//!   A 消息洪流 2000 条（采集入库的小分配风暴）
//!   B 词云 ×10（分词 + 布局的大分配后释放，分配器归还页差异的主要观测点）
//!   C B话榜 ×1 + !md ×3（Chrome 截图；无 CHROME 时走失败回退，三组一致）
//!   D 图库 6 张 512² 图入库 + 查重（OpenCV 感知哈希与 SIFT 大分配）
//!   E 静置（等 mimalloc/jemalloc 的延迟归还）
//!
//! 运行：`cargo test --release --test alloc_bench -- --ignored --nocapture`，
//! feature 决定待测分配器。词云依赖 msg_rank 5 秒刷盘，洪流后等一拍。

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::build_bot;
use kovi::config::kovi_conf::KoviConf;
use kovi::event::id::ID;
use kovi::tokio;
use tokio::time::timeout;

mod common;

use common::{ADMIN, MockOneBot, cropped_pattern_png, rgb_pattern_png};

const REPLY: Duration = Duration::from_secs(10);
const SLOW: Duration = Duration::from_secs(60);
const SIFT: Duration = Duration::from_secs(240);
const FLOOD_MESSAGES: usize = 2_000;
const WORDCLOUD_ROUNDS: usize = 10;
const GALLERY_IMAGES: usize = 6;

#[tokio::test(flavor = "current_thread")]
#[ignore = "分配器基准：重负载且依赖外部采样，见 scripts/alloc-bench/bench.sh"]
async fn allocator_bench_load() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let iso = common::IsolatedCwd::enter("allocbench", &repo);
    utils::config::preload();
    let server = MockOneBot::start().await;
    let bot = build_bot(KoviConf::new(ID::new(ADMIN), None, false), server.driver());
    let run = tokio::spawn(bot.run());
    let _abort = common::AbortOnDrop(run);

    server.wait_ready().await;
    common::wait_until_help_ready(&server, &["/wordle", "图库", "!md"]).await;

    println!("=== PHASE A: 洪流 {FLOOD_MESSAGES} 条群消息");
    let mut rng = Rng(0x5eed_1234);
    for i in 0..FLOOD_MESSAGES {
        server.emit_group(ADMIN, &flood_text(&mut rng, i)).await;
    }
    // 消息库 5 秒周期刷盘，洪流结束后等一拍再进词云，保证全部入账。
    tokio::time::sleep(Duration::from_secs(8)).await;

    println!("=== PHASE B: 词云 ×{WORDCLOUD_ROUNDS}");
    for round in 0..WORDCLOUD_ROUNDS {
        let pending = server.ask("/wordcloud once", REPLY).await;
        assert!(
            pending.text.contains("正在生成"),
            "round {round}: {pending:?}"
        );
        let cloud = timeout(SLOW, server.next_message())
            .await
            .unwrap_or_else(|_| panic!("round {round}: 词云图未在 {SLOW:?} 内发出"));
        assert!(cloud.has_image, "round {round}: 词云应带图: {cloud:?}");
        println!("--- wordcloud round {round} done");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    println!("=== PHASE C: B话榜 ×1 + !md ×3");
    let rank = server.ask("/今日B话榜", SLOW).await;
    println!("--- 今日B话榜: image={} text={}", rank.has_image, rank.text);
    for i in 0..3 {
        let md = server.ask("!md # 标题 *强调* 段落文本", SLOW).await;
        println!("--- !md {i}: image={} text={}", md.has_image, md.text);
        // !md 有 10 秒级全局限流,失败请求也占额度,间隔开才能每轮都走真实截图。
        tokio::time::sleep(Duration::from_secs(10)).await;
    }

    println!("=== PHASE D: 图库 {GALLERY_IMAGES} 张 512² 入库 + 查重");
    for i in 0..GALLERY_IMAGES {
        let path = gallery_image(&iso.root, i);
        let added = server
            .ask_with_image(ADMIN, "添加 基准图", &path, SIFT)
            .await
            .unwrap_or_else(|| panic!("图 {i} 添加无回复"));
        assert!(
            added.text.contains("添加") || added.text.contains("都已在"),
            "图 {i} 添加失败: {added:?}"
        );
        println!("--- gallery image {i} added");
    }
    let scan = server.ask("查重 基准图", SIFT).await;
    println!("--- 查重: image={} text={}", scan.has_image, scan.text);
    let crop_scan = server.ask("查裁剪 基准图", REPLY).await;
    println!(
        "--- 查裁剪: image={} text={}",
        crop_scan.has_image, crop_scan.text
    );
    // 查裁剪先回「正在全量扫」提示，SIFT 结果要等后台扫描完成后再发一条。
    let crop_result = timeout(SIFT, async {
        loop {
            let msg = server.next_message().await;
            if !msg.text.contains("正在全量扫") {
                return msg;
            }
        }
    })
    .await
    .expect("查裁剪结果未在时限内发出");
    println!(
        "--- 查裁剪结果: image={} text={}",
        crop_result.has_image, crop_result.text
    );
    let wipe = server.ask("删除 基准图", REPLY).await;
    assert!(
        wipe.text.contains("图库 确认"),
        "删除整库应先登记待确认: {wipe:?}"
    );
    common::assert_contains(&server, "图库 确认", "已清空", REPLY).await;

    println!("=== PHASE E: 静置 20s 等空闲页归还");
    tokio::time::sleep(Duration::from_secs(20)).await;
    println!("=== BENCH DONE");
}

/// 基准图序列：偶数张是 512² 全图，奇数张是前一张同源纹理的局部裁剪，
/// 感知哈希落在「疑似」区间，查重才会走 SIFT。
fn gallery_image(root: &Path, index: usize) -> PathBuf {
    let path = root.join(format!("fixtures/gallery-{index}.png"));
    let png = if index.is_multiple_of(2) {
        rgb_pattern_png(512, index as u32 * 37 + 5)
    } else {
        cropped_pattern_png(index as u32 * 37 - 32, 64, 320)
    };
    std::fs::write(&path, png).unwrap();
    path
}

/// 确定性伪随机（xorshift32），洪流内容可复现，三组构建负载严格一致。
struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

fn flood_text(rng: &mut Rng, i: usize) -> String {
    const CJK: [&str; 24] = [
        "今天",
        "天气",
        "不错",
        "周末",
        "出去",
        "玩",
        "这个",
        "问题",
        "感觉",
        "可以",
        "什么",
        "时候",
        "上线",
        "版本",
        "更新",
        "机器",
        "学习",
        "深度",
        "神经",
        "网络",
        "服务器",
        "内存",
        "分配",
        "测试",
    ];
    const EN: [&str; 16] = [
        "hello",
        "world",
        "rust",
        "allocator",
        "memory",
        "benchmark",
        "wordcloud",
        "message",
        "tokio",
        "runtime",
        "spawn",
        "future",
        "async",
        "channel",
        "buffer",
        "release",
    ];
    let words = 8 + rng.below(24);
    let mut parts = Vec::with_capacity(words + 1);
    parts.push(format!("msg{i}"));
    for _ in 0..words {
        if rng.below(3) == 0 {
            parts.push(EN[rng.below(EN.len())].to_string());
        } else {
            parts.push(CJK[rng.below(CJK.len())].to_string());
        }
    }
    parts.join(" ")
}
