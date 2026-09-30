//! 图库每日备份:日期目录快照 + 保护表 + 统一回收。
//!
//! 备份由两部分组成:`<root>/backups/<YYYY-MM-DD>/<群号>/index.db` 是
//! `VACUUM INTO` 出的一致快照(索引、别名、指纹、保护表都在里面);
//! 群库 `index.db` 的 `backup_refs` 表记录每个 hash 最后被哪天的备份指向。
//!
//! 删除命令只删索引行,从不动文件——被删出索引的 blob 成为孤儿,由每日
//! 对账任务统一回收,且回收前先查保护表:距今不足保留天数的记录视为
//! 「备份还指向这张图」,文件保留;过期记录连同文件一起清掉。因此误删
//! 的图在保护期内数据一直在原位,把快照 db 拷回即可恢复;超过保护期才
//! 真正释放磁盘。物理删除只有对账一个入口,与文件系统是否支持硬链接
//! 无关。
//!
//! 恢复(手动):停 bot,把某天 `<群号>/index.db` 拷回 `data/image_lib/<群号>/`
//! (若群目录残留 `index.db-journal` 之类的崩溃遗留热日志,拷回前一并
//! 删掉),快照引用的图在保护期内都还在正式 blobs 目录里,直接启动即可;
//! 超期的图数据已被回收,索引行会在对账时清掉。注意恢复等于回滚:
//! 快照日之后新入库的图不在快照的保护表里,恢复后失去保护,下次对账
//! 立即回收。

use std::{
    collections::BTreeSet,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

use super::Store;
use super::repo::refresh_backup_refs;

/// 保留最近多少个日期目录,即备份份数,也是孤儿文件的保护天数。
pub(super) const RETAINED_DAYS: usize = 7;

impl Store {
    pub(crate) async fn backup_daily_at(&self, today: i64) {
        if let Err(error) = self.run_daily_backup(today).await {
            tracing::error!("图库每日备份失败: {error}");
        }
    }

    async fn run_daily_backup(&self, today: i64) -> Result<()> {
        let backups = self.root.join("backups");
        kovi::tokio::fs::create_dir_all(&backups)
            .await
            .with_context(|| format!("创建备份目录失败: {}", backups.display()))?;
        clean_stale_tmp(&backups).await;

        let date = date_string(today);
        let final_dir = backups.join(&date);
        if kovi::tokio::fs::try_exists(&final_dir)
            .await
            .unwrap_or(false)
        {
            tracing::debug!("今日图库备份已存在,跳过: {date}");
        } else {
            let tmp_dir = backups.join(format!("{date}.tmp"));
            // rename 成日期名才算完整备份;写入前先清掉可能的上次残留。
            let _ = kovi::tokio::fs::remove_dir_all(&tmp_dir).await;
            kovi::tokio::fs::create_dir_all(&tmp_dir).await?;
            for group_id in self.list_group_ids().await? {
                if let Err(error) = self.backup_group(group_id, today, &tmp_dir).await {
                    tracing::error!("图库备份群 {group_id} 失败,已跳过: {error}");
                    let _ =
                        kovi::tokio::fs::remove_dir_all(tmp_dir.join(group_id.to_string())).await;
                }
            }
            kovi::tokio::fs::rename(&tmp_dir, &final_dir).await?;
            tracing::info!("图库每日备份完成: {date}");
        }

        prune_old_backups(&backups, today).await
    }

    /// 备份单个群:群锁内先做快照、快照落成后才刷保护记录——refs 指向
    /// 今天当且仅当今天的快照存在。反过来先刷表再 VACUUM,磁盘满时会
    /// 恰好停在「有保护期却无快照」的状态。代价是快照里带到的是前一天
    /// 的 refs,恢复出的库保护期最多少一天。
    async fn backup_group(&self, group_id: i64, today: i64, tmp_root: &Path) -> Result<()> {
        let group_tmp = tmp_root.join(group_id.to_string());
        let snapshot = group_tmp.join("index.db");
        kovi::tokio::fs::create_dir_all(&group_tmp).await?;
        self.with_group(group_id, {
            let snapshot = snapshot.clone();
            move |pool| async move {
                sqlx::query("VACUUM INTO ?")
                    .bind(snapshot.to_string_lossy().into_owned())
                    .execute(&pool)
                    .await?;
                refresh_backup_refs(&pool, today).await?;
                Ok(())
            }
        })
        .await?;
        utils::restrict_mode_0600(&snapshot).context("收紧备份数据库权限失败")?;
        Ok(())
    }
}

/// 删掉上次崩溃留下的 `<date>.tmp` 半成品,它们没有被 rename 成日期名,
/// 留着只会让下次同名备份多一步防御性删除。
async fn clean_stale_tmp(backups: &Path) {
    let Ok(mut entries) = kovi::tokio::fs::read_dir(backups).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let Some(date) = name.strip_suffix(".tmp") else {
                    return false;
                };
                looks_like_date(date)
            });
        if matches && kovi::tokio::fs::remove_dir_all(&path).await.is_ok() {
            tracing::warn!("清掉未完成的图库备份残留: {}", path.display());
        }
    }
}

/// 删除日期已过保留期的备份目录,与对账清保护记录用同一个谓词
/// (`目录日 + RETAINED_DAYS <= today`)。两者必须同日失效,「备份目录
/// 还在 ⟺ 其指向的文件还受保护」才在停机隔天等非连续备份下也成立;
/// 按份数滚动会在停机后把时间跨度拉长,让记录先于目录过期。名字不像
/// 日期的条目不动,避免误删人工放进去的东西。
async fn prune_old_backups(backups: &Path, today: i64) -> Result<()> {
    let mut dates = BTreeSet::new();
    let mut entries = kovi::tokio::fs::read_dir(backups).await?;
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str()
            && looks_like_date(name)
        {
            dates.insert(name.to_owned());
        }
    }
    while let Some(oldest) = dates.first()
        && date_epoch_day(oldest) + RETAINED_DAYS as i64 <= today
    {
        let oldest = dates.pop_first().expect("first 刚校验过");
        kovi::tokio::fs::remove_dir_all(backups.join(&oldest))
            .await
            .with_context(|| format!("删除过期备份失败: {oldest}"))?;
        tracing::info!("过期图库备份已删除: {oldest}");
    }
    Ok(())
}

/// `YYYY-MM-DD`,定宽,字典序即时间序。
fn looks_like_date(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        && name[5..7]
            .parse::<u32>()
            .is_ok_and(|month| (1..=12).contains(&month))
        && name[8..10]
            .parse::<u32>()
            .is_ok_and(|day| (1..=31).contains(&day))
}

/// UTC 当天的 epoch 天数。备份滚动只看目录名先后,不依赖时区时刻,
/// 用 UTC 免去本地时区换算。
pub(super) fn today_utc_days() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟早于 1970")
        .as_secs() as i64
        / 86_400
}

pub(super) fn date_string(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 日期目录名转回 epoch 天数;名字必须先通过 [`looks_like_date`]。
fn date_epoch_day(name: &str) -> i64 {
    let year: i64 = name[..4].parse().expect("looks_like_date 已校验");
    let month: u32 = name[5..7].parse().expect("looks_like_date 已校验");
    let day: u32 = name[8..10].parse().expect("looks_like_date 已校验");
    days_from_civil(year, month, day)
}

/// 公历年月日转 epoch 天数(Howard Hinnant days_from_civil),
/// 与 [`civil_from_days`] 互逆。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// epoch 天数转公历年月日(Howard Hinnant civil_from_days),纯整数运算,
/// 覆盖任意可表示日期。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(19_723), "2024-01-01");
        assert_eq!(date_string(19_782), "2024-02-29");
        assert_eq!(date_string(20_725), "2026-09-29");
        assert_eq!(date_string(-1), "1969-12-31");
    }

    #[test]
    fn date_roundtrip_survives_month_and_era_edges() {
        for days in [0, -1, -720, 19_723, 19_782, 20_725, 100_000, -100_000] {
            let name = date_string(days);
            assert!(
                looks_like_date(&name),
                "date_string 应产出合法目录名: {name}"
            );
            assert_eq!(date_epoch_day(&name), days, "roundtrip 失败: {name}");
        }
    }

    #[test]
    fn date_names_are_validated_not_matched_blindly() {
        assert!(looks_like_date("2026-09-29"));
        assert!(!looks_like_date("2026-9-29"));
        assert!(!looks_like_date("2026-13-01"));
        assert!(!looks_like_date("2026-00-10"));
        assert!(!looks_like_date("2026-09-29.tmp"));
        assert!(!looks_like_date("notes.txt"));
    }
}
