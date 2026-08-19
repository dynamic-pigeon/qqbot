use std::{sync::LazyLock, time::Duration};

use anyhow::Result;
use askama::Template;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::TimeZone as _;
use kovi::RuntimeBot;
use kovi_onebot::MessageRegistrar as _;
use utils::RateLimiter;
use utils::command::{Command, CommandContext, CommandError, CommandResult, MessageScope};

mod user_info;

/// 每群连续两次 `#今日发言排行` 之间的最短间隔。
const RANK_COOLDOWN: Duration = Duration::from_secs(30);

/// 按群节流，避免连续刷排行打满 DB 连接池和 chromium。
static RANK_COOLDOWN_LIMITER: LazyLock<RateLimiter<i64>> =
    LazyLock::new(|| RateLimiter::new(RANK_COOLDOWN, 1));

pub(crate) fn daily_rank_command() -> Command {
    Command::new("#今日发言排行")
        .description("生成今日发言排行")
        .usage("#今日发言排行")
        .scope(MessageScope::Group)
        .handler(handle_daily_rank)
}

async fn handle_daily_rank(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.event().group_id.expect("群命令已通过范围校验");
    if let Err(hit) = RANK_COOLDOWN_LIMITER.try_acquire(group_id) {
        return Err(CommandError::user(format!(
            "刚跑完，{} 秒后再试一次",
            hit.retry_after_secs()
        )));
    }

    let html = gen_daily_rank_html(ctx.bot(), group_id)
        .await
        .map_err(CommandError::internal)?;
    let image = utils::screenshot(&html, utils::ScreenshotOptions::default())
        .await
        .map_err(CommandError::internal)?;
    let base64_image = STANDARD.encode(image);
    let message = kovi::Message::new().add_image(&format!("base64://{base64_image}"));
    ctx.reply(message);
    Ok(())
}

fn today_time_range() -> (i64, i64) {
    let now = chrono::Local::now();
    let today_midnight_naive = now.date_naive().and_hms_opt(0, 0, 0).unwrap();

    let today_start = chrono::Local
        .from_local_datetime(&today_midnight_naive)
        .single()
        .unwrap();

    let start_timestamp = today_start.timestamp();
    // 其实用啥都行，反正只要能保证时间范围是24小时就行了
    let end_timestamp = start_timestamp + 24 * 3600;

    (start_timestamp, end_timestamp)
}

/// 生成每日发言排行 HTML（仅展示前 5 名）
pub async fn gen_daily_rank_html(bot: &RuntimeBot, group_id: i64) -> Result<String> {
    let (start_timestamp, end_timestamp) = today_time_range();
    gen_rank_html_with_time_range(bot, group_id, start_timestamp, end_timestamp, 5).await
}

/// 生成时间范围发言排行 HTML
pub async fn gen_rank_html_with_time_range(
    bot: &RuntimeBot,
    group_id: i64,
    start_timestamp: i64,
    end_timestamp: i64,
    cnt: usize,
) -> Result<String> {
    let top = crate::db::msg_count_top_with_time_range(
        group_id,
        start_timestamp,
        end_timestamp,
        cnt as i64,
    )
    .await?;

    if top.is_empty() {
        anyhow::bail!("该时间范围暂无发言数据");
    }

    // 每个用户都要走群成员 API + 头像下载且各自带重试，串行拉取会让
    // 命令响应时间叠加上去，并行后总耗时约等于最慢的一个用户。
    let entries: Vec<(user_info::UserInfo, u32)> =
        futures::future::join_all(top.into_iter().map(|(user_id, cnt)| async move {
            match user_info::get_user_info(bot, group_id, user_id).await {
                Ok(info) => (info, cnt),
                Err(e) => {
                    tracing::error!("获取用户 {} 信息失败: {}", user_id, e);
                    // 使用 user_id 作为昵称占位
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

    let html = render_rank_html(&entries)?;
    Ok(html)
}

#[derive(Template)]
#[template(path = "rank.html")]
struct RankTemplate {
    date: String,
    items: Vec<RankItem>,
}

struct RankItem {
    rank: usize,
    medal: String,
    medal_color: &'static str,
    avatar_src: String,
    nickname: String,
    count: u32,
    bar_pct: u32,
}

fn render_rank_html(entries: &[(user_info::UserInfo, u32)]) -> Result<String> {
    let date_str = chrono::Local::now().format("%Y年%m月%d日").to_string();

    let medal_colors = ["#FFD700", "#C0C0C0", "#CD7F32"];
    let medals = ["🥇", "🥈", "🥉"];

    let items: Vec<RankItem> = entries
        .iter()
        .enumerate()
        .map(|(i, (info, cnt))| {
            let rank = i + 1;

            let (medal, medal_color) = if i < 3 {
                (medals[i].to_string(), medal_colors[i])
            } else {
                (rank.to_string(), "#6c757d")
            };

            let avatar_src = if info.avatar.is_empty() {
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
            };

            RankItem {
                rank,
                medal,
                medal_color,
                avatar_src,
                nickname: info.nickname.clone(),
                count: *cnt,
                bar_pct: bar_percent(entries, *cnt),
            }
        })
        .collect();

    let template = RankTemplate {
        date: date_str,
        items,
    };

    Ok(template.render()?)
}

/// 计算相对于第一名的进度条百分比
fn bar_percent(entries: &[(user_info::UserInfo, u32)], cnt: u32) -> u32 {
    let max = entries.first().map(|(_, c)| *c).unwrap_or(1).max(1);
    (cnt as u64 * 100 / max as u64) as u32
}

#[cfg(test)]
mod command_tests {
    use utils::command::{MessageScope, Permission, ResolveOutcome};

    #[test]
    fn daily_rank_is_a_public_group_command() {
        let tree = utils::command::CommandTree::new(vec![super::daily_rank_command()]).unwrap();
        let ResolveOutcome::Matched(command) = tree.resolve("#今日发言排行") else {
            panic!("expected rank command to resolve");
        };

        assert_eq!(command.scope(), MessageScope::Group);
        assert_eq!(command.permission(), Permission::Everyone);
        assert!(matches!(
            tree.resolve("#今日发言排行榜"),
            ResolveOutcome::Ignored
        ));
    }
}
