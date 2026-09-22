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

/// 每群连续两次 B 话榜之间的最短间隔。
const RANK_COOLDOWN: Duration = Duration::from_secs(30);

/// 榜单展示的名次数量。
const RANK_TOP: usize = 5;

/// 按群节流，避免连续刷排行打满 DB 连接池和 chromium。
static RANK_COOLDOWN_LIMITER: LazyLock<RateLimiter<i64>> =
    LazyLock::new(|| RateLimiter::new(RANK_COOLDOWN, 1));

/// 一档 B 话榜：子命令名、根直连触发词、帮助描述与时间范围。
/// 子命令名带「B话榜」后缀：expose_as_root 会让名字成为裸词触发词，
/// 用「今日」这类常见句首词会在群聊里误触发。
struct RankSpan {
    sub: &'static str,
    trigger: &'static str,
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
        sub: "今日B话榜",
        trigger: "/今日B话榜",
        description: "看看今天的群友发了多少消息！",
        kind: RankKind::Today,
    },
    RankSpan {
        sub: "昨日B话榜",
        trigger: "/昨日B话榜",
        description: "看看昨天的群友发了多少消息！",
        kind: RankKind::Yesterday,
    },
    RankSpan {
        sub: "前日B话榜",
        trigger: "/前日B话榜",
        description: "看看前天的群友发了多少消息！",
        kind: RankKind::BeforeYesterday,
    },
    RankSpan {
        sub: "本周B话榜",
        trigger: "/本周B话榜",
        description: "看看本周的群友发了多少消息！",
        kind: RankKind::ThisWeek,
    },
    RankSpan {
        sub: "上周B话榜",
        trigger: "/上周B话榜",
        description: "看看上周的群友发了多少消息！",
        kind: RankKind::LastWeek,
    },
    RankSpan {
        sub: "本月B话榜",
        trigger: "/本月B话榜",
        description: "看看这个月的群友发了多少消息！",
        kind: RankKind::ThisMonth,
    },
];

/// 六档榜单挂在 `/B话榜` 父命令下；子命令 expose_as_root，
/// `/今日B话榜` 这类直连写法可用，而 `/help` 根列表只占一行。
pub(crate) fn rank_command() -> Command {
    let parent = Command::new("/B话榜")
        .description("群友发言排行（今日/昨日/前日/本周/上周/本月）")
        .usage("/B话榜 <档位>")
        .scope(MessageScope::Group);
    RANK_SPANS.iter().fold(parent, |parent, span| {
        let kind = span.kind;
        parent.subcommand(
            Command::new(span.sub)
                .alias(span.trigger)
                .description(span.description)
                .usage(span.trigger)
                .handler(move |ctx| handle_rank(ctx, kind))
                .expose_as_root(),
        )
    })
}

async fn handle_rank(ctx: CommandContext, kind: RankKind) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    if let Err(hit) = RANK_COOLDOWN_LIMITER.try_acquire(group_id) {
        return Err(CommandError::user(format!(
            "刚跑完，{} 秒后再试一次",
            hit.retry_after_secs()
        )));
    }

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

/// 周窗口从周一开始。today 由调用方传入，测试才能用固定日期驱动。
fn week_window(
    today: NaiveDate,
    weeks_ago: u32,
    title: &'static str,
    period: &'static str,
) -> RankWindow {
    let monday = today.week(Weekday::Mon).first_day() - Days::new(u64::from(weeks_ago) * 7);
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

/// 生成一档 B 话榜 HTML（前 5 名）。
async fn gen_rank_html(bot: &RuntimeBot, group_id: i64, window: &RankWindow) -> Result<String> {
    let top = crate::db::msg_count_top_with_time_range(
        group_id,
        window.start,
        window.end,
        RANK_TOP as i64,
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
                    (
                        user_info::UserInfo {
                            user_id,
                            nickname: user_id.to_string(),
                            avatar: bytes::Bytes::new(),
                            fetched_at: std::time::Instant::now(),
                        },
                        cnt,
                    )
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
        let tree = utils::command::CommandTree::new(vec![super::rank_command()]).unwrap();
        for trigger in [
            "/今日B话榜",
            "/昨日B话榜",
            "/前日B话榜",
            "/本周B话榜",
            "/上周B话榜",
            "/本月B话榜",
            "/B话榜 今日B话榜",
            "/B话榜 上周B话榜",
        ] {
            let ResolveOutcome::Matched(command) = tree.resolve(trigger) else {
                panic!("{trigger} 应能解析");
            };
            assert_eq!(command.scope(), MessageScope::Group);
            assert_eq!(command.permission(), Permission::Everyone);
        }
        // 裸的「今日」是常见聊天句首词，不应误触发。
        assert!(matches!(tree.resolve("今日"), ResolveOutcome::Ignored));
        // 光打父命令应提示选择子命令，而不是被忽略。
        let ResolveOutcome::Error(error) = tree.resolve("/B话榜") else {
            panic!("`/B话榜` 应返回缺少子命令错误");
        };
        let utils::command::RouteError::MissingSubcommand { available, .. } = error else {
            panic!("`/B话榜` 应是缺少子命令错误: {error:?}");
        };
        assert_eq!(
            available,
            [
                "今日B话榜",
                "昨日B话榜",
                "前日B话榜",
                "本周B话榜",
                "上周B话榜",
                "本月B话榜"
            ]
        );
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
