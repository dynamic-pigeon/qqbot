use std::{sync::LazyLock, time::Duration};

use anyhow::Result;
use askama::Template;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::RuntimeBot;
use kovi::chrono::{self, Datelike as _, Days, Months, NaiveDate, TimeZone as _, Weekday};
use kovi_onebot::MessageRegistrar as _;
use utils::RateLimiter;
use utils::command::{Command, CommandContext, CommandError, CommandResult, MessageScope};

mod user_info;
pub(crate) mod weekly_report;

/// 每群连续两次 B 话榜之间的最短间隔。
const RANK_COOLDOWN: Duration = Duration::from_secs(30);

/// 按群节流，避免连续刷排行打满 DB 连接池和 chromium。
static RANK_COOLDOWN_LIMITER: LazyLock<RateLimiter<i64>> =
    LazyLock::new(|| RateLimiter::new(RANK_COOLDOWN, 1));

/// 按群节流；冷却中返回用户可见的错误。
pub(super) fn acquire_rank_cooldown(group_id: i64) -> CommandResult {
    if let Err(hit) = RANK_COOLDOWN_LIMITER.try_acquire(group_id) {
        return Err(CommandError::user(format!(
            "刚跑完，{} 秒后再试一次",
            hit.retry_after_secs()
        )));
    }
    Ok(())
}

/// 一档 B 话榜：命令名（带 `/` 的触发词）、不带斜杠的裸词别名、帮助描述与时间范围。
struct RankSpan {
    name: &'static str,
    bare: &'static str,
    description: &'static str,
    kind: RankKind,
}

#[derive(Clone, Copy)]
enum RankKind {
    Today,
    Yesterday,
    BeforeYesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
}

const RANK_SPANS: [RankSpan; 6] = [
    RankSpan {
        name: "/今日B话榜",
        bare: "今日B话榜",
        description: "看看今天的群友发了多少消息！",
        kind: RankKind::Today,
    },
    RankSpan {
        name: "/昨日B话榜",
        bare: "昨日B话榜",
        description: "看看昨天的群友发了多少消息！",
        kind: RankKind::Yesterday,
    },
    RankSpan {
        name: "/前日B话榜",
        bare: "前日B话榜",
        description: "看看前天的群友发了多少消息！",
        kind: RankKind::BeforeYesterday,
    },
    RankSpan {
        name: "/本周B话榜",
        bare: "本周B话榜",
        description: "看看本周的群友发了多少消息！",
        kind: RankKind::ThisWeek,
    },
    RankSpan {
        name: "/上周B话榜",
        bare: "上周B话榜",
        description: "看看上周的群友发了多少消息！",
        kind: RankKind::LastWeek,
    },
    RankSpan {
        name: "/本月B话榜",
        bare: "本月B话榜",
        description: "看看这个月的群友发了多少消息！",
        kind: RankKind::ThisMonth,
    },
];

/// 六档榜单是六个独立根命令，折进 /help 的「B话榜」分组（/help B话榜 展开）；
/// `/B话榜` 本体保留为入口，直接回档位用法。
pub(crate) fn rank_commands() -> Vec<Command> {
    let mut commands = vec![
        Command::new("/B话榜")
            .description("群友发言排行（今日/昨日/前日/本周/上周/本月）")
            .usage("/B话榜 <档位>")
            .scope(MessageScope::Group)
            .group("B话榜")
            .sync_handler(rank_usage),
    ];
    commands.extend(RANK_SPANS.iter().map(|span| {
        let kind = span.kind;
        Command::new(span.name)
            .alias(span.bare)
            .description(span.description)
            .usage(span.name)
            .scope(MessageScope::Group)
            .group("B话榜")
            .handler(move |ctx| handle_rank(ctx, kind))
    }));
    commands
}

fn rank_usage(ctx: CommandContext) -> CommandResult {
    let spans: Vec<&str> = RANK_SPANS.iter().map(|span| span.name).collect();
    ctx.reply(format!(
        "用法: /B话榜 <档位>\n可用档位: {}",
        spans.join(" | ")
    ));
    Ok(())
}

async fn handle_rank(ctx: CommandContext, kind: RankKind) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    acquire_rank_cooldown(group_id)?;

    let window = rank_window(kind);
    let html = gen_rank_html(ctx.bot(), group_id, &window)
        .await
        .map_err(CommandError::internal)?;
    // 按卡片元素截取，图片不带视口边距。
    let image = utils::screenshot(
        &html,
        utils::ScreenshotOptions::new().with_selector(".card"),
    )
    .await
    .map_err(CommandError::internal)?;
    let message = kovi::Message::new().add_image(&utils::base64_image(&image));
    ctx.reply(message);
    Ok(())
}

struct RankWindow {
    start: i64,
    end: i64,
    title: &'static str,
    period: &'static str,
    date_label: String,
}

fn rank_window(kind: RankKind) -> RankWindow {
    let today = chrono::Local::now().date_naive();
    match kind {
        RankKind::Today => day_window(today, "今日B话榜", "今日"),
        RankKind::Yesterday => day_window(today - Days::new(1), "昨日B话榜", "昨日"),
        RankKind::BeforeYesterday => day_window(today - Days::new(2), "前日B话榜", "前日"),
        RankKind::ThisWeek => week_window(today, 0, "本周B话榜", "本周"),
        RankKind::LastWeek => week_window(today, 1, "上周B话榜", "上周"),
        RankKind::ThisMonth => month_window(today, "本月B话榜", "本月"),
    }
}

/// 夏令时切换日的本地午夜可能不存在或有歧义；此时按 UTC 解释兜底，
/// 只影响有夏令时的时区，误差为当地偏移量级。
fn local_midnight(date: NaiveDate) -> chrono::DateTime<chrono::Local> {
    let naive = date.and_hms_opt(0, 0, 0).expect("00:00:00 合法");
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .unwrap_or_else(|| naive.and_utc().with_timezone(&chrono::Local))
}

fn day_window(date: NaiveDate, title: &'static str, period: &'static str) -> RankWindow {
    RankWindow {
        start: local_midnight(date).timestamp(),
        // 结束取次日本地午夜而不是 +24h，夏令时切换日也对齐真实的一天。
        end: local_midnight(date + Days::new(1)).timestamp(),
        title,
        period,
        date_label: date.format("%Y年%m月%d日").to_string(),
    }
}

/// today 所在周的周一往前推 `weeks_ago` 周。
fn monday_weeks_ago(today: NaiveDate, weeks_ago: u32) -> NaiveDate {
    today.week(Weekday::Mon).first_day() - Days::new(u64::from(weeks_ago) * 7)
}

/// 周窗口从周一开始。today 由调用方传入，测试才能用固定日期驱动。
fn week_window(
    today: NaiveDate,
    weeks_ago: u32,
    title: &'static str,
    period: &'static str,
) -> RankWindow {
    let monday = monday_weeks_ago(today, weeks_ago);
    RankWindow {
        start: local_midnight(monday).timestamp(),
        end: local_midnight(monday + Days::new(7)).timestamp(),
        title,
        period,
        date_label: format!(
            "{} ~ {}",
            monday.format("%m月%d日"),
            (monday + Days::new(6)).format("%m月%d日")
        ),
    }
}

/// today 由调用方传入，测试才能用固定日期驱动。
fn month_window(today: NaiveDate, title: &'static str, period: &'static str) -> RankWindow {
    let first = today.with_day(1).expect("每月都有 1 号");
    RankWindow {
        start: local_midnight(first).timestamp(),
        end: local_midnight(first + Months::new(1)).timestamp(),
        title,
        period,
        date_label: first.format("%Y年%m月").to_string(),
    }
}

/// 生成一档 B 话榜 HTML。
async fn gen_rank_html(bot: &RuntimeBot, group_id: i64, window: &RankWindow) -> Result<String> {
    let top = crate::db::msg_count_top_with_time_range(
        group_id,
        window.start,
        window.end,
        crate::config::static_config().rank_top(),
    )
    .await?;

    if top.is_empty() {
        anyhow::bail!("该时间范围暂无发言数据");
    }

    let total =
        crate::db::msg_count_total_with_time_range(group_id, window.start, window.end).await?;

    // 每个用户都要走群成员 API + 头像下载且各自带重试，串行拉取会让
    // 命令响应时间叠加上去，并行后总耗时约等于最慢的一个用户。
    let entries: Vec<(user_info::UserInfo, u32)> =
        kovi::futures_util::future::join_all(top.into_iter().map(|(user_id, cnt)| async move {
            match user_info::get_user_info(bot, group_id, user_id).await {
                Ok(info) => (info, cnt),
                Err(e) => {
                    tracing::error!("获取用户 {} 信息失败: {}", user_id, e);
                    (user_info::fallback_user_info(user_id), cnt)
                }
            }
        }))
        .await;

    let html = render_rank_html(&entries, total, window)?;
    Ok(html)
}

#[derive(Template)]
#[template(path = "rank.html")]
struct RankTemplate {
    title: &'static str,
    period: &'static str,
    date: String,
    time: String,
    total: u32,
    items: Vec<RankItem>,
}

struct RankItem {
    rank: usize,
    avatar_src: String,
    nickname: String,
    count: u32,
    percent: u32,
}

fn render_rank_html(
    entries: &[(user_info::UserInfo, u32)],
    total: u32,
    window: &RankWindow,
) -> Result<String> {
    let now = chrono::Local::now();

    let items: Vec<RankItem> = entries
        .iter()
        .enumerate()
        .map(|(i, (info, cnt))| RankItem {
            rank: i + 1,
            avatar_src: avatar_src(info),
            nickname: info.nickname.clone(),
            count: *cnt,
            percent: percent_of_total(*cnt, total),
        })
        .collect();

    let template = RankTemplate {
        title: window.title,
        period: window.period,
        date: window.date_label.clone(),
        time: now.format("%H:%M").to_string(),
        total,
        items,
    };

    Ok(template.render()?)
}

fn avatar_src(info: &user_info::UserInfo) -> String {
    if info.avatar.is_empty() {
        format!(
            "data:image/svg+xml;base64,{}",
            STANDARD.encode(
                format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="80"><circle cx="40" cy="40" r="40" fill="#888"/><text x="50%" y="55%" text-anchor="middle" fill="white" font-size="28" font-family="sans-serif">{}</text></svg>"##,
                    info.nickname.chars().next().unwrap_or('?')
                )
            )
        )
    } else {
        format!("data:image/jpeg;base64,{}", STANDARD.encode(&info.avatar))
    }
}

/// 占时间范围内总消息量的百分比，驱动占比列与进度条宽度。
fn percent_of_total(cnt: u32, total: u32) -> u32 {
    let total = (total as u64).max(1);
    (cnt as u64 * 100 / total) as u32
}

#[cfg(test)]
mod command_tests {
    use kovi::chrono::{Days, Months, NaiveDate};
    use utils::command::{MessageScope, Permission, ResolveOutcome};

    #[test]
    fn rank_commands_are_public_group_commands() {
        let tree = utils::command::CommandTree::new(super::rank_commands()).unwrap();
        for trigger in [
            "/今日B话榜",
            "/昨日B话榜",
            "/前日B话榜",
            "/本周B话榜",
            "/上周B话榜",
            "/本月B话榜",
            "今日B话榜",
            "上周B话榜",
        ] {
            let ResolveOutcome::Matched(command) = tree.resolve(trigger) else {
                panic!("{trigger} 应能解析");
            };
            assert_eq!(command.scope(), MessageScope::Group);
            assert_eq!(command.permission(), Permission::Everyone);
        }
        // 裸的「今日」是常见聊天句首词，不应误触发。
        assert!(matches!(tree.resolve("今日"), ResolveOutcome::Ignored));
        // 光打 `/B话榜` 直接命中入口命令，由 handler 回档位用法。
        let ResolveOutcome::Matched(entry) = tree.resolve("/B话榜") else {
            panic!("`/B话榜` 应解析为入口命令");
        };
        assert_eq!(entry.path(), ["/B话榜"]);
        assert_eq!(entry.scope(), MessageScope::Group);
        assert!(matches!(
            tree.resolve("#今日发言排行"),
            ResolveOutcome::Ignored
        ));
    }

    #[test]
    fn week_and_month_windows_align_to_boundaries() {
        // 固定 today 驱动，测试不随真实日期漂移；2026-09-22 是周二。
        let today = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        let window = super::week_window(today, 0, "本周B话榜", "本周");
        assert_eq!(window.start, super::local_midnight(monday).timestamp());
        assert_eq!(
            window.end,
            super::local_midnight(monday + Days::new(7)).timestamp()
        );
        assert_eq!(window.date_label, "09月21日 ~ 09月27日");

        let last = super::week_window(today, 1, "上周B话榜", "上周");
        assert_eq!(
            last.start,
            super::local_midnight(monday - Days::new(7)).timestamp()
        );

        let month = super::month_window(today, "本月B话榜", "本月");
        let first = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        assert_eq!(month.start, super::local_midnight(first).timestamp());
        assert_eq!(
            month.end,
            super::local_midnight(first + Months::new(1)).timestamp()
        );
    }
}
