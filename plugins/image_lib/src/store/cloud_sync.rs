//! 图库云同步:每日备份落成后把图库根目录用 rclone 镜像到远端。
//!
//! 顺序上夹在备份与对账之间,云端拿到的 blobs 始终覆盖现有全部快照。
//! 正式库的 index.db 不上传——运行中被拷出的库可能是损坏副本,云端
//! 恢复一律以 backups/ 下的 VACUUM 快照为准,见 backup.rs 的恢复步骤。
//! 镜像与本地同构:本地回收的图次日在云端删除,云端大小随各群容量
//! 上限封顶;超过 `cloud_sync_max_gib` 的体积闸门时跳过同步并报错,
//! 防止涨过对象存储免费额度。完成日志按 rclone 的 JSON 操作行报出
//! 新增/删除文件数与耗时。

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use kovi::tokio::io::{AsyncBufReadExt as _, BufReader};
use kovi::tokio::process::Command;

use super::Store;

/// rclone 单次运行上限;超时杀进程放弃本轮,明日备份后重试。
const SYNC_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const GIB: u64 = 1024 * 1024 * 1024;
/// 失败日志里保留的 stderr 行数上限。
const STDERR_LINES: usize = 30;

/// 一轮同步的统计,来自 rclone JSON 日志的操作行。
#[derive(Default)]
struct SyncStats {
    copied: u64,
    deleted: u64,
    bytes: u64,
}

impl Store {
    pub(super) async fn cloud_sync(&self, destination: &str) {
        if let Err(error) = self.run_cloud_sync(destination).await {
            tracing::error!("图库云同步失败({destination}): {error}");
        }
    }

    async fn run_cloud_sync(&self, destination: &str) -> Result<()> {
        // 空库跳过:sync 会把远端清成同构,新装或目标填错时不至于一轮
        // 就把云端内容全删掉。
        if self.list_group_ids().await?.is_empty() {
            tracing::info!("图库为空,跳过云同步");
            return Ok(());
        }
        // 体积闸门:云端随本地涨,本地不越过免费额度云端就不会。统计含
        // 未上传的正式 index.db,偏保守方向。
        let limit = crate::config::static_config().cloud_sync_limit_bytes();
        let used = dir_size(&self.root).await?;
        if used > limit {
            anyhow::bail!(
                "图库体积 {} GiB 超过云同步闸门 {} GiB,跳过本轮;要继续可调大 cloud_sync_max_gib",
                used / GIB,
                limit / GIB
            );
        }
        run_rclone(&self.root, destination).await
    }
}

/// 递归统计目录内全部普通文件的大小,不含符号链接。
async fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let mut entries = kovi::tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("列出目录失败: {}", dir.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                total += entry.metadata().await?.len();
            }
        }
    }
    Ok(total)
}

async fn run_rclone(root: &Path, destination: &str) -> Result<()> {
    let started = Instant::now();
    let mut child = Command::new("rclone")
        .arg("sync")
        .arg(root)
        .arg(destination)
        // 桶级令牌没有 HeadBucket/CreateBucket 权限,上传前的桶存在性
        // 预检必然 403,跳过预检直接写;对非 s3 后端是惰性参数。
        .arg("--s3-no-check-bucket")
        // 群目录下的正式 index.db 与热日志不进云端;快照库在
        // backups/<日期>/<群号>/index.db,层级更深,不受这些规则影响。
        // *.tmp 兼挡入库半成品与未 rename 完的备份目录。排除式不能带
        // 前导 /:rclone 的 /=*/ 式样会静默匹配不到任何路径。
        .arg("--exclude=[0-9]*/index.db")
        .arg("--exclude=*.db-journal")
        .arg("--exclude=*.db-wal")
        .arg("--exclude=*.db-shm")
        .arg("--exclude=*.tmp")
        // JSON 日志的操作行用来统计增删;--stats 0 去掉周期性统计噪声。
        .arg("-v")
        .arg("--use-json-log")
        .arg("--stats")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 rclone 失败,运行环境装有 rclone 吗")?;
    let handle = child.stderr.take().expect("stderr 已设 piped");
    let mut stats = SyncStats::default();
    let mut tail = VecDeque::new();
    let status = match kovi::tokio::time::timeout(SYNC_TIMEOUT, async {
        // 先读到 EOF 再收尸:管道在 rclone 退出时关闭,顺序反了会卡死。
        let mut lines = BufReader::new(handle).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            classify(&line, &mut stats, &mut tail);
        }
        child.wait().await
    })
    .await
    {
        Ok(status) => status.context("等待 rclone 退出失败")?,
        Err(_) => {
            child.start_kill().context("终止超时的 rclone 失败")?;
            let _ = child.wait().await;
            anyhow::bail!("同步超过一小时,已终止");
        }
    };
    if status.success() {
        tracing::info!(
            "图库云同步完成({destination}): 新增 {} 个({}), 删除 {} 个, 用时 {}",
            stats.copied,
            crate::commands::format_bytes(stats.bytes),
            stats.deleted,
            format_duration(started.elapsed())
        );
        Ok(())
    } else {
        anyhow::bail!(
            "rclone 异常退出({status}): {}",
            tail.make_contiguous().join("\n")
        )
    }
}

/// 归类一行 rclone JSON 日志:操作行累计进统计,任何行滚动保留最后
/// STDERR_LINES 行,失败时错误就在末尾。
fn classify(line: &str, stats: &mut SyncStats, tail: &mut VecDeque<String>) {
    if line.contains("\"msg\":\"Copied") {
        stats.copied += 1;
        stats.bytes += copied_size(line);
    } else if line.contains("\"msg\":\"Deleted\"") {
        stats.deleted += 1;
    }
    tail.push_back(line.to_owned());
    while tail.len() > STDERR_LINES {
        tail.pop_front();
    }
}

/// 取操作行的 `"size":<n>` 字段,解析不出按 0。
fn copied_size(line: &str) -> u64 {
    line.split("\"size\":")
        .nth(1)
        .and_then(|rest| {
            rest.chars()
                .take_while(|byte| byte.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        // 整数取整:按浮点 {:.0} 会把 1.53 分进成 2m。
        let total = duration.as_secs();
        format!("{}m{}s", total / 60, total % 60)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use kovi::tokio;

    use super::{STDERR_LINES, SyncStats, classify, copied_size, dir_size, format_duration};

    #[tokio::test]
    async fn dir_size_sums_nested_regular_files() {
        let root = std::env::temp_dir().join(format!(
            "image_lib_dir_size_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("123/blobs")).unwrap();
        std::fs::create_dir_all(root.join("backups/2026-10-26")).unwrap();
        std::fs::write(root.join("123/blobs/aaa"), [0u8; 10]).unwrap();
        std::fs::write(root.join("123/index.db"), [0u8; 7]).unwrap();
        std::fs::write(root.join("backups/2026-10-26/bbb"), [0u8; 5]).unwrap();

        let total = dir_size(&root).await.unwrap();
        assert_eq!(total, 22);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn copied_size_reads_json_field() {
        let line = r#"{"level":"info","msg":"Copied (new)","size":131072,"object":"f1"}"#;
        assert_eq!(copied_size(line), 131072);
        assert_eq!(copied_size(r#"{"msg":"Deleted","object":"f2"}"#), 0);
    }

    #[test]
    fn format_duration_splits_minutes() {
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_secs(92)), "1m32s");
    }

    #[test]
    fn classify_counts_ops_and_caps_tail() {
        let mut stats = SyncStats::default();
        let mut tail = VecDeque::new();
        for noise in 0..STDERR_LINES + 10 {
            classify(
                &format!("{{\"msg\":\"noise {noise}\"}}"),
                &mut stats,
                &mut tail,
            );
        }
        classify(r#"{"msg":"Copied (new)","size":10}"#, &mut stats, &mut tail);
        classify(r#"{"msg":"Deleted","object":"x"}"#, &mut stats, &mut tail);
        assert_eq!(stats.copied, 1);
        assert_eq!(stats.bytes, 10);
        assert_eq!(stats.deleted, 1);
        assert_eq!(tail.len(), STDERR_LINES);
    }
}
