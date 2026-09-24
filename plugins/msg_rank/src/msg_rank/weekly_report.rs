use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicI64},
};

use anyhow::Result;
use askama::Template;
use kovi::chrono::{Datelike as _, Days, NaiveDate, Weekday};
use kovi::{Message, PluginBuilder as plugin, RuntimeBot, chrono};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};
use utils::command::{
    Command, CommandContext, CommandError, CommandResult, MessageScope, Permission,
};

use super::user_info::{self, UserInfo};
use crate::config::CONFIG;

/// 夜聊时段（本地时间 23:00–05:59）。
const NIGHT_HOURS: [u32; 7] = [23, 0, 1, 2, 3, 4, 5];
/// 早起时段（本地时间 06:00–08:59）。
const EARLY_HOURS: [u32; 3] = [6, 7, 8];
/// 加冕夜聊王/早起王所需的时段内最少消息数，避免一两条消息夺冠。
const MIN_KING_MESSAGES: u32 = 3;
/// 热词统计读取原文的上限，与词云一致。
const MAX_HOTWORD_INPUT_BYTES: usize = 2 * 1024 * 1024;
const HOTWORD_LIMIT: usize = 12;
/// 每日消息量柱状图从周一排到周日。
const DAY_LABELS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

/// `/周报` 裸命令直接出上周报告；父命令带 handler 时路由把裸命令交给父 handler，
/// 多出来的词会被判为未知子命令（见 `CommandTree::resolve`），不会误吃参数。
pub(crate) fn weekly_report_command(path: Arc<PathBuf>) -> Command {
    let once_path = Arc::clone(&path);
    Command::new("/周报")
        .description("查看上周群活跃周报；管理员可开关定时推送")
        .usage("/周报 [enable|disable|status]")
        .scope(MessageScope::Group)
        .handler(move |ctx| {
            let path = Arc::clone(&once_path);
            view_report(ctx, path)
        })
        .subcommand(
            Command::new("enable")
                .description("启用本群定时周报（同时开始采集）")
                .usage("/周报 enable")
                .permission(Permission::BotAdmin)
                .sync_handler(report_enable),
        )
        .subcommand(
            Command::new("disable")
                .description("停用本群定时周报（消息采集继续）")
                .usage("/周报 disable")
                .permission(Permission::BotAdmin)
                .sync_handler(report_disable),
        )
        .subcommand(
            Command::new("status")
                .description("查看本群定时周报状态")
                .usage("/周报 status")
                .permission(Permission::BotAdmin)
                .sync_handler(report_status),
        )
}

fn report_enable(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    CONFIG
        .modify(|config| config.enable_weekly_report(group_id))
        .map_err(CommandError::internal)?;
    ctx.reply("定时周报已启用");
    Ok(())
}

fn report_disable(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    CONFIG
        .modify(|config| config.disable_weekly_report(group_id))
        .map_err(CommandError::internal)?;
    ctx.reply("定时周报已停用");
    Ok(())
}

fn report_status(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    ctx.reply(if CONFIG.get().weekly_report_enabled(group_id) {
        "定时周报已启用"
    } else {
        "定时周报未启用"
    });
    Ok(())
}

async fn view_report(ctx: CommandContext, path: Arc<PathBuf>) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    if let Err(hit) = super::RANK_COOLDOWN_LIMITER.try_acquire(group_id) {
        return Err(CommandError::user(format!(
            "刚跑完，{} 秒后再试一次",
            hit.retry_after_secs()
        )));
    }

    let window = last_week_window(chrono::Local::now().date_naive());
    let rows = crate::db::msg_count_with_active_days(group_id, window.start, window.end)
        .await
        .map_err(CommandError::internal)?;
    if rows.is_empty() {
        return Err(CommandError::user(
            "上周暂无发言数据；管理员可先执行 /消息采集 enable 开启采集",
        ));
    }

    // 周报要加载 jieba 词典并截图，秒级耗时，回执后异步生成，不占住命令处理。
    let bot = Arc::clone(ctx.bot());
    ctx.reply("⏳ 正在生成上周周报...");
    kovi::spawn(async move {
        match make_report_image(&bot, &path, group_id, &window, rows).await {
            Ok(image) => bot.send_group_msg(
                group_id,
                Message::new().add_image(&utils::base64_image(&image)),
            ),
            Err(e) => {
                tracing::error!("生成周报失败: {e}, group_id: {group_id}");
                bot.send_group_msg(group_id, "周报生成失败，请稍后再试");
            }
        }
    });
    Ok(())
}

pub(crate) fn init(bot: Arc<RuntimeBot>, path: Arc<PathBuf>) -> Result<()> {
    let cron = crate::config::static_config().weekly_report_cron.clone();
    let last_fire_ts = AtomicI64::new(0);
    plugin::cron(&cron, move || {
        let bot = &bot;
        let path = &path;
        if crate::word_cloud::mark_cron_fire(&last_fire_ts, chrono::Local::now().timestamp()) {
            let bot = Arc::clone(bot);
            let path = Arc::clone(path);
            // 截图池并发槽位只有 2 且 acquire 超时 5 秒，逐群 spawn 会让第 3 个群起
            // 直接超时失败；同一 tick 内单任务顺序生成，每群都能出，代价是到群时间被拉平。
            kovi::spawn(async move {
                let config = CONFIG.get();
                for &group_id in &config.weekly_report_group {
                    send_weekly_report(&bot, group_id, &path).await;
                }
            });
        }
        async move {}
    })
    .unwrap();
    Ok(())
}

async fn send_weekly_report(bot: &RuntimeBot, group_id: i64, path: &Path) {
    let window = last_week_window(chrono::Local::now().date_naive());
    let rows = match crate::db::msg_count_with_active_days(group_id, window.start, window.end).await
    {
        Ok(rows) => rows,
        Err(e) => {
            notify_admin(bot, group_id, e).await;
            return;
        }
    };
    if rows.is_empty() {
        tracing::info!("群 {group_id} 上周无发言数据，跳过周报推送");
        return;
    }

    match make_report_image(bot, path, group_id, &window, rows).await {
        Ok(image) => bot.send_group_msg(
            group_id,
            Message::new()
                .add_text("📊 上周群活跃周报")
                .add_image(&utils::base64_image(&image)),
        ),
        Err(e) => notify_admin(bot, group_id, e).await,
    }
}

async fn notify_admin(bot: &RuntimeBot, group_id: i64, error: anyhow::Error) {
    tracing::error!("生成周报失败: {error}, group_id: {group_id}");
    if let Some(admin_id) = bot
        .get_main_admin()
        .ok()
        .and_then(|admin| admin.try_as_i64())
    {
        bot.send_private_msg(
            admin_id,
            format!("生成周报失败: {error}, group_id: {group_id}"),
        );
    }
}

async fn make_report_image(
    bot: &RuntimeBot,
    path: &Path,
    group_id: i64,
    window: &ReportWindow,
    rows: Vec<(i64, u32, u32)>,
) -> Result<Vec<u8>> {
    let html = gen_weekly_report_html(bot, path, group_id, window, &rows).await?;
    // 按卡片元素截取，图片不带视口边距。
    utils::screenshot(
        &html,
        utils::ScreenshotOptions::new().with_selector(".card"),
    )
    .await
}

#[derive(Clone, Copy)]
struct ReportWindow {
    start: i64,
    end: i64,
    monday: NaiveDate,
}

/// 上个完整自然周（周一开始），today 由调用方传入，测试才能用固定日期驱动。
fn last_week_window(today: NaiveDate) -> ReportWindow {
    let monday = today.week(Weekday::Mon).first_day() - Days::new(7);
    ReportWindow {
        start: super::local_midnight(monday).timestamp(),
        end: super::local_midnight(monday + Days::new(7)).timestamp(),
        monday,
    }
}

/// 头部日期标签；跨年的一周两边都带年份，同年只在前者带。
fn week_date_label(monday: NaiveDate) -> String {
    let sunday = monday + Days::new(6);
    if monday.year() == sunday.year() {
        format!(
            "{}年{} ~ {}",
            monday.format("%Y"),
            monday.format("%m月%d日"),
            sunday.format("%m月%d日")
        )
    } else {
        format!(
            "{} ~ {}",
            monday.format("%Y年%m月%d日"),
            sunday.format("%Y年%m月%d日")
        )
    }
}

async fn gen_weekly_report_html(
    bot: &RuntimeBot,
    path: &Path,
    group_id: i64,
    window: &ReportWindow,
    rows: &[(i64, u32, u32)],
) -> Result<String> {
    let total: u32 = rows.iter().map(|(_, count, _)| count).sum();
    let full_week_users = rows.iter().filter(|(_, _, days)| *days >= 7).count();
    let top_limit = crate::config::static_config().weekly_report_top();

    let night = crate::db::msg_count_top_at_local_hours(
        group_id,
        window.start,
        window.end,
        &NIGHT_HOURS,
        MIN_KING_MESSAGES,
    )
    .await?;
    let early = crate::db::msg_count_top_at_local_hours(
        group_id,
        window.start,
        window.end,
        &EARLY_HOURS,
        MIN_KING_MESSAGES,
    )
    .await?;

    // 榜单与两位「王者」的去重用户集合，各拉一次群成员信息与头像。
    let mut user_ids: Vec<i64> = rows.iter().take(top_limit).map(|row| row.0).collect();
    for king in [&night, &early] {
        if let Some((user_id, _)) = king
            && !user_ids.contains(user_id)
        {
            user_ids.push(*user_id);
        }
    }

    let infos: HashMap<i64, UserInfo> =
        kovi::futures_util::future::join_all(user_ids.into_iter().map(|user_id| async move {
            let info = user_info::get_user_info(bot, group_id, user_id)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("获取用户 {user_id} 信息失败: {e}");
                    fallback_user_info(user_id)
                });
            (user_id, info)
        }))
        .await
        .into_iter()
        .collect();

    let items: Vec<WeeklyItem> = rows
        .iter()
        .take(top_limit)
        .enumerate()
        .map(|(i, &(user_id, count, days))| {
            let info = &infos[&user_id];
            WeeklyItem {
                rank: i + 1,
                avatar_src: super::avatar_src(info),
                nickname: info.nickname.clone(),
                count,
                percent: super::percent_of_total(count, total),
                full_week: days >= 7,
            }
        })
        .collect();

    let king = |top: Option<(i64, u32)>| {
        top.map(|(user_id, count)| {
            let info = &infos[&user_id];
            King {
                avatar_src: super::avatar_src(info),
                nickname: info.nickname.clone(),
                count,
            }
        })
    };
    let night_owl = king(night);
    let early_bird = king(early);

    let daily = crate::db::msg_count_by_local_date(group_id, window.start, window.end).await?;
    let daily_map: HashMap<String, u32> = daily.into_iter().collect();

    let hot_words = collect_hot_words(path, group_id, window).await?;

    let template = WeeklyReportTemplate {
        date: week_date_label(window.monday),
        time: chrono::Local::now().format("%H:%M").to_string(),
        total,
        active_users: rows.len(),
        daily_avg: total / 7,
        full_week_users,
        bars: daily_bars(window.monday, &daily_map),
        items,
        night_owl,
        early_bird,
        hot_words,
    };
    Ok(template.render()?)
}

fn fallback_user_info(user_id: i64) -> UserInfo {
    UserInfo {
        user_id,
        nickname: user_id.to_string(),
        avatar: bytes::Bytes::new(),
        fetched_at: std::time::Instant::now(),
    }
}

async fn collect_hot_words(
    path: &Path,
    group_id: i64,
    window: &ReportWindow,
) -> Result<Vec<HotWord>> {
    let text = crate::db::select_text_from_time_range(
        group_id,
        window.start,
        window.end,
        MAX_HOTWORD_INPUT_BYTES,
    )
    .await?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let words = crate::word_cloud::top_words(path, &text, HOTWORD_LIMIT).await?;
    Ok(words
        .into_iter()
        .map(|(word, count)| HotWord { word, count })
        .collect())
}

/// 把按 `YYYY-MM-DD` 的每日计数铺满周一到周日七根柱，无数据的日子画灰柱。
fn daily_bars(monday: NaiveDate, counts: &HashMap<String, u32>) -> Vec<DailyBar> {
    let values: Vec<u32> = (0..7)
        .map(|offset| {
            let key = (monday + Days::new(offset)).format("%Y-%m-%d").to_string();
            counts.get(&key).copied().unwrap_or(0)
        })
        .collect();
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .into_iter()
        .enumerate()
        .map(|(i, count)| DailyBar {
            label: DAY_LABELS[i],
            count,
            height: count
                .checked_mul(100)
                .and_then(|scaled| scaled.checked_div(max))
                .unwrap_or(0),
            zero: count == 0,
        })
        .collect()
}

#[derive(Template)]
#[template(path = "weekly_report.html")]
struct WeeklyReportTemplate {
    date: String,
    time: String,
    total: u32,
    active_users: usize,
    daily_avg: u32,
    full_week_users: usize,
    bars: Vec<DailyBar>,
    items: Vec<WeeklyItem>,
    night_owl: Option<King>,
    early_bird: Option<King>,
    hot_words: Vec<HotWord>,
}

struct DailyBar {
    label: &'static str,
    count: u32,
    height: u32,
    zero: bool,
}

struct WeeklyItem {
    rank: usize,
    avatar_src: String,
    nickname: String,
    count: u32,
    percent: u32,
    full_week: bool,
}

struct King {
    avatar_src: String,
    nickname: String,
    count: u32,
}

struct HotWord {
    word: String,
    count: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use askama::Template as _;
    use utils::command::{MessageScope, Permission, ResolveOutcome};

    use super::{
        DailyBar, HotWord, King, WeeklyItem, WeeklyReportTemplate, daily_bars, last_week_window,
        week_date_label,
    };

    fn sample_template() -> WeeklyReportTemplate {
        WeeklyReportTemplate {
            date: week_date_label(kovi::chrono::NaiveDate::from_ymd_opt(2026, 9, 14).unwrap()),
            time: "10:00".into(),
            total: 70,
            active_users: 8,
            daily_avg: 10,
            full_week_users: 2,
            bars: vec![DailyBar {
                label: "一",
                count: 10,
                height: 100,
                zero: false,
            }],
            items: vec![WeeklyItem {
                rank: 1,
                avatar_src: "data:image/svg+xml;base64,x".into(),
                nickname: "阿明".into(),
                count: 21,
                percent: 30,
                full_week: true,
            }],
            night_owl: Some(King {
                avatar_src: "data:image/svg+xml;base64,x".into(),
                nickname: "夜猫子".into(),
                count: 5,
            }),
            early_bird: None,
            hot_words: vec![HotWord {
                word: "rust".into(),
                count: 9,
            }],
        }
    }

    #[test]
    fn weekly_report_commands_split_view_and_admin_subcommands() {
        let tree = utils::command::CommandTree::new(vec![super::weekly_report_command(
            std::sync::Arc::new(std::path::PathBuf::from("/tmp")),
        )])
        .unwrap();

        let utils::command::ResolveOutcome::Matched(command) = tree.resolve("/周报") else {
            panic!("`/周报` 应能直接解析");
        };
        assert_eq!(command.permission(), Permission::Everyone);
        assert_eq!(command.scope(), MessageScope::Group);

        for sub in ["enable", "disable", "status"] {
            let utils::command::ResolveOutcome::Matched(command) =
                tree.resolve(&format!("/周报 {sub}"))
            else {
                panic!("`/周报 {sub}` 应能解析");
            };
            assert_eq!(command.permission(), Permission::BotAdmin);
            assert_eq!(command.scope(), MessageScope::Group);
        }

        let ResolveOutcome::Error(error) = tree.resolve("/周报 乱输") else {
            panic!("`/周报 乱输` 应是未知子命令错误");
        };
        let utils::command::RouteError::UnknownSubcommand { subcommand, .. } = error else {
            panic!("`/周报 乱输` 应是未知子命令错误: {error:?}");
        };
        assert_eq!(subcommand, "乱输");
    }

    #[test]
    fn last_week_window_spans_previous_monday_to_sunday() {
        // 2026-09-22 是周二；本周一是 09-21，上周一应是 09-14。
        let today = kovi::chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let window = last_week_window(today);
        let monday = kovi::chrono::NaiveDate::from_ymd_opt(2026, 9, 14).unwrap();
        assert_eq!(window.monday, monday);
        assert_eq!(
            window.start,
            super::super::local_midnight(monday).timestamp()
        );
        assert_eq!(
            window.end,
            super::super::local_midnight(monday + kovi::chrono::Days::new(7)).timestamp()
        );
        assert_eq!(week_date_label(monday), "2026年09月14日 ~ 09月20日");
    }

    #[test]
    fn week_date_label_adds_years_on_both_ends_when_crossing_years() {
        // 2025-01-08 所在周的上周是 2024-12-30 ~ 2025-01-05。
        let today = kovi::chrono::NaiveDate::from_ymd_opt(2025, 1, 8).unwrap();
        let window = last_week_window(today);
        assert_eq!(
            week_date_label(window.monday),
            "2024年12月30日 ~ 2025年01月05日"
        );
    }

    #[test]
    fn daily_bars_fill_weekday_columns() {
        let monday = kovi::chrono::NaiveDate::from_ymd_opt(2026, 9, 14).unwrap();
        let mut counts = HashMap::new();
        counts.insert("2026-09-14".to_string(), 5);
        counts.insert("2026-09-15".to_string(), 10);

        let bars = daily_bars(monday, &counts);
        assert_eq!(bars.len(), 7);
        assert_eq!(bars[0].label, "一");
        assert_eq!(bars[0].count, 5);
        assert_eq!(bars[0].height, 50);
        assert_eq!(bars[1].count, 10);
        assert_eq!(bars[1].height, 100);
        assert!(bars[2..].iter().all(|bar| bar.zero && bar.count == 0));
    }

    #[test]
    fn template_renders_sections_and_escapes_optionals() {
        let html = sample_template().render().unwrap();
        assert!(html.contains("群活跃周报"));
        assert!(html.contains("阿明"));
        assert!(html.contains("全勤"));
        assert!(html.contains("夜聊王"));
        assert!(html.contains("夜猫子"));
        // 热词徽章带序号与词频，第一个是 top 高亮位。
        assert!(html.contains("chip top"));
        assert!(html.contains(">rust<"));
        assert!(html.contains(">9<"));
        // early_bird 为 None，不应渲染早起王卡片。
        assert!(!html.contains("早起王"));
    }
}
