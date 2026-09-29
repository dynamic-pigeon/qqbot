//! 图库每日备份:日期目录快照 + 滚动保留七天。
//!
//! 布局 `<root>/backups/<YYYY-MM-DD>/<群号>/`,内含 `index.db`(VACUUM INTO
//! 的一致快照)和 `blobs/<sha256>`(硬链接到正式 blob,同盘零拷贝,不占
//! 图片空间)。正式目录里删图的各条路径只 `remove_file` 自己那份链接,
//! 备份还链着的图数据就仍在:误删的图在保留窗口内可恢复;备份过期滚动
//! 删除时最后一个链接消失,磁盘空间才真正释放。
//!
//! 恢复(手动):停 bot,把某天 `<群号>/index.db` 拷回 `data/image_lib/<群号>/`,
//! 该快照引用而正式 blobs 目录缺失的图从备份 `blobs/` 复制回去,再启动。

use std::{
    collections::BTreeSet,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

use super::Store;

/// 保留最近多少个日期目录,每天一份,即保留天数。
const RETAINED_DAYS: usize = 7;

impl Store {
    /// 每日任务入口:先备份当日快照,失败只记日志,不影响主流程。
    pub(crate) async fn backup_daily(&self) {
        self.backup_daily_at(today_utc_days()).await;
    }

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
                if let Err(error) = self.backup_group(group_id, &tmp_dir).await {
                    tracing::error!("图库备份群 {group_id} 失败,已跳过: {error}");
                    let _ =
                        kovi::tokio::fs::remove_dir_all(tmp_dir.join(group_id.to_string())).await;
                }
            }
            kovi::tokio::fs::rename(&tmp_dir, &final_dir).await?;
            tracing::info!("图库每日备份完成: {date}");
        }

        prune_old_backups(&backups).await
    }

    /// 备份单个群:群锁内 VACUUM INTO 出一致快照,锁外把盘上 blob 逐个
    /// 硬链接进备份目录。链接比快照便宜得多,链的是当时盘上的全部 blob,
    /// 不查索引——被删命令刚清掉行、文件还在的图也一并保底。
    async fn backup_group(&self, group_id: i64, tmp_root: &Path) -> Result<()> {
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
                Ok(())
            }
        })
        .await?;
        utils::restrict_mode_0600(&snapshot).context("收紧备份数据库权限失败")?;

        let blobs = self.blobs_dir(group_id);
        let backup_blobs = group_tmp.join("blobs");
        kovi::tokio::fs::create_dir_all(&backup_blobs).await?;
        let mut entries = kovi::tokio::fs::read_dir(&blobs)
            .await
            .with_context(|| format!("列出图片目录失败: {}", blobs.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // staged 临时文件都带扩展名,正式 blob 是 64 位十六进制裸名。
            if path.extension().is_some() {
                continue;
            }
            let name = entry.file_name();
            if let Err(error) = kovi::tokio::fs::hard_link(&path, backup_blobs.join(&name)).await {
                // 备份期间被并发删掉的图链不上是正常竞态,其余错误不致命:
                // 该图恢复时缺失,但不拖垮整份备份。
                tracing::warn!(
                    "图库备份链接图片失败 group_id={group_id} file={}: {error}",
                    name.to_string_lossy()
                );
            }
        }
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

/// 只保留最近 RETAINED_DAYS 个日期目录。目录名排序即时间序(定宽
/// YYYY-MM-DD);名字不像日期的条目不动,避免误删人工放进去的东西。
async fn prune_old_backups(backups: &Path) -> Result<()> {
    let mut dates = BTreeSet::new();
    let mut entries = kovi::tokio::fs::read_dir(backups).await?;
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str()
            && looks_like_date(name)
        {
            dates.insert(name.to_owned());
        }
    }
    while dates.len() > RETAINED_DAYS {
        let oldest = dates.pop_first().expect("len > RETAINED_DAYS");
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
    fn date_names_are_validated_not_matched_blindly() {
        assert!(looks_like_date("2026-09-29"));
        assert!(!looks_like_date("2026-9-29"));
        assert!(!looks_like_date("2026-13-01"));
        assert!(!looks_like_date("2026-00-10"));
        assert!(!looks_like_date("2026-09-29.tmp"));
        assert!(!looks_like_date("notes.txt"));
    }
}
