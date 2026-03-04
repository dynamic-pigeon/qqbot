use std::{cmp::Reverse, sync::Arc};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::TimeZone as _;
use futures::TryFutureExt as _;
use help_msg::register_help;
use itertools::izip;
use kovi::{PluginBuilder as plugin, RuntimeBot, log};

mod user_info;

pub async fn init() -> Result<()> {
    register_help("今日发言排行", "今日发言排行", "#今日发言排行").await;
    let bot = crate::plugin::get_runtime_bot();
    plugin::on_group_msg(move |event| {
        let bot = Arc::clone(&bot);
        async move {
            let text = event.borrow_text().unwrap_or_default();
            if text.trim() != "#今日发言排行" {
                return;
            }
            match gen_daily_rank_html(&bot, event.group_id)
                .and_then(async |html| utils::screenshot(html.into(), None).await)
                .await
            {
                Ok(image) => {
                    let base64_image = STANDARD.encode(image);
                    let msg = kovi::Message::new().add_image(&format!("base64://{}", base64_image));
                    event.reply(msg);
                }
                Err(e) => {
                    log::error!("生成发言排行失败: {}", e);
                    event.reply(format!("❌ 生成发言排行失败: {}", e));
                }
            }
        }
    });
    Ok(())
}

async fn today_msg_cnt(group_id: i64) -> Result<Vec<(i64, u32)>> {
    let now = chrono::Local::now();
    let today_midnight_naive = now.date_naive().and_hms_opt(0, 0, 0).unwrap();

    let today_start = chrono::Local
        .from_local_datetime(&today_midnight_naive)
        .single()
        .unwrap();

    let start_timestamp = today_start.timestamp();
    // 其实用啥都行，反正只要能保证时间范围是24小时就行了
    let end_timestamp = start_timestamp + 24 * 3600;
    let msg_cnts =
        crate::db::msg_count_with_time_range(group_id, start_timestamp, end_timestamp).await?;

    Ok(msg_cnts)
}

/// 生成每日发言排行 HTML（仅展示前 5 名）
pub async fn gen_daily_rank_html(bot: &RuntimeBot, group_id: i64) -> Result<String> {
    // 获取今日各用户发言数
    let mut msg_cnts = today_msg_cnt(group_id).await?;

    // 按发言数降序排序，取前 5
    msg_cnts.sort_by_key(|&(_, cnt)| Reverse(cnt));
    let top5 = msg_cnts.into_iter().take(5).collect::<Vec<_>>();

    if top5.is_empty() {
        anyhow::bail!("今日暂无发言数据");
    }

    // 获取每个用户的 UserInfo
    let mut entries: Vec<(user_info::UserInfo, u32)> = Vec::with_capacity(top5.len());
    for (user_id, cnt) in top5 {
        match user_info::get_user_info(bot, group_id, user_id).await {
            Ok(info) => entries.push((info, cnt)),
            Err(e) => {
                log::error!("获取用户 {} 信息失败: {}", user_id, e);
                // 使用 user_id 作为昵称占位
                entries.push((
                    user_info::UserInfo {
                        user_id,
                        nickname: user_id.to_string(),
                        avatar: bytes::Bytes::new(),
                    },
                    cnt,
                ));
            }
        }
    }

    let html = render_rank_html(&entries);
    Ok(html)
}

fn render_rank_html(entries: &[(user_info::UserInfo, u32)]) -> String {
    let date_str = chrono::Local::now().format("%Y年%m月%d日").to_string();

    // 徽章：前三名特殊颜色
    let medal_colors = ["#FFD700", "#C0C0C0", "#CD7F32"]
        .into_iter()
        .chain((0..).map(|_| "#6c757d"));
    let medals = ["🥇", "🥈", "🥉"]
        .into_iter()
        .map(|s| s.to_string())
        .chain((4..).map(|n| n.to_string()));

    let mut rows = String::new();
    for (i, ((info, cnt), medal, medal_color)) in
        izip!(entries.iter(), medals, medal_colors).enumerate()
    {
        let rank = i + 1;

        // 将头像转为 base64 data URL（若为空则使用占位 SVG）
        let avatar_src = if info.avatar.is_empty() {
            format!(
                "data:image/svg+xml;base64,{}",
                STANDARD.encode(
                    format!(
                        r##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="80"><circle cx="40" cy="40" r="40" fill="#888"/><text x="50%" y="55%" text-anchor="middle" fill="white" font-size="28" font-family="sans-serif">{}</text></svg>"##,
                        &info.nickname.chars().next().unwrap_or('?')
                    )
                )
            )
        } else {
            format!("data:image/jpeg;base64,{}", STANDARD.encode(&info.avatar))
        };

        let nickname = html_escape(&info.nickname);

        rows.push_str(&format!(
            r#"
        <div class="rank-item rank-{rank}">
            <div class="rank-badge" style="color:{medal_color};">{medal}</div>
            <div class="avatar-wrap">
                <img class="avatar" src="{avatar_src}" alt="avatar" />
            </div>
            <div class="user-info">
                <span class="nickname">{nickname}</span>
                <span class="count">{cnt} 条消息</span>
            </div>
            <div class="bar-wrap">
                <div class="bar" style="width:{bar_pct}%; background:{medal_color};"></div>
            </div>
        </div>"#,
            rank = rank,
            medal_color = medal_color,
            medal = medal,
            avatar_src = avatar_src,
            nickname = nickname,
            cnt = cnt,
            bar_pct = bar_percent(entries, *cnt),
        ));
    }

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<title>每日发言排行 - {date}</title>
<style>
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{
    background: linear-gradient(135deg, #1a1a2e 0%, #16213e 50%, #0f3460 100%);
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    font-family: "PingFang SC", "Microsoft YaHei", "Segoe UI", sans-serif;
    padding: 24px;
  }}
  .card {{
    background: rgba(255,255,255,0.07);
    backdrop-filter: blur(12px);
    border: 1px solid rgba(255,255,255,0.15);
    border-radius: 20px;
    width: 480px;
    padding: 32px 28px;
    box-shadow: 0 24px 64px rgba(0,0,0,0.5);
  }}
  .header {{
    text-align: center;
    margin-bottom: 28px;
  }}
  .header h1 {{
    color: #fff;
    font-size: 22px;
    font-weight: 700;
    letter-spacing: 2px;
  }}
  .header .date {{
    color: rgba(255,255,255,0.5);
    font-size: 13px;
    margin-top: 6px;
  }}
  .rank-item {{
    display: flex;
    align-items: center;
    gap: 14px;
    background: rgba(255,255,255,0.06);
    border-radius: 14px;
    padding: 14px 16px;
    margin-bottom: 12px;
    position: relative;
    transition: background 0.2s;
  }}
  .rank-item:last-child {{ margin-bottom: 0; }}
  .rank-1 {{ background: rgba(255,215,0,0.10); border: 1px solid rgba(255,215,0,0.25); }}
  .rank-2 {{ background: rgba(192,192,192,0.08); border: 1px solid rgba(192,192,192,0.2); }}
  .rank-3 {{ background: rgba(205,127,50,0.08); border: 1px solid rgba(205,127,50,0.2); }}
  .rank-badge {{
    font-size: 26px;
    width: 32px;
    text-align: center;
    flex-shrink: 0;
    line-height: 1;
  }}
  .avatar-wrap {{
    flex-shrink: 0;
  }}
  .avatar {{
    width: 52px;
    height: 52px;
    border-radius: 50%;
    object-fit: cover;
    border: 2px solid rgba(255,255,255,0.2);
    display: block;
  }}
  .user-info {{
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }}
  .nickname {{
    color: #fff;
    font-size: 15px;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }}
  .count {{
    color: rgba(255,255,255,0.55);
    font-size: 12px;
  }}
  .bar-wrap {{
    position: absolute;
    bottom: 0;
    left: 0;
    right: 0;
    height: 3px;
    border-radius: 0 0 14px 14px;
    background: rgba(255,255,255,0.05);
    overflow: hidden;
  }}
  .bar {{
    height: 100%;
    border-radius: 0 0 14px 14px;
    opacity: 0.7;
    transition: width 0.5s ease;
  }}
  .footer {{
    text-align: center;
    color: rgba(255,255,255,0.3);
    font-size: 11px;
    margin-top: 20px;
  }}
</style>
</head>
<body>
  <div class="card">
    <div class="header">
      <h1>🏆 每日发言排行</h1>
      <div class="date">{date}</div>
    </div>
    {rows}
    <div class="footer">Top 5 · 数据截至今日</div>
  </div>
</body>
</html>
"#,
        date = date_str,
        rows = rows,
    )
}

/// 计算相对于第一名的进度条百分比
fn bar_percent(entries: &[(user_info::UserInfo, u32)], cnt: u32) -> u32 {
    let max = entries.first().map(|(_, c)| *c).unwrap_or(1).max(1);
    (cnt as u64 * 100 / max as u64) as u32
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
