//! 图库云同步:每日备份落成后把图库根目录用 rclone 镜像到远端。
//!
//! 顺序上夹在备份与对账之间,云端拿到的 blobs 始终覆盖现有全部快照。
//! 正式库的 index.db 不上传——运行中被拷出的库可能是损坏副本,云端
//! 恢复一律以 backups/ 下的 VACUUM 快照为准,见 backup.rs 的恢复步骤。
//! 镜像与本地同构:本地回收的图次日在云端删除,云端大小随各群容量
//! 上限封顶;超过 `cloud_sync_max_gib` 的体积闸门时跳过同步并报错,
//! 防止涨过对象存储免费额度。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use kovi::tokio::io::AsyncReadExt as _;
use kovi::tokio::process::Command;

use super::Store;

/// rclone 单次运行上限;超时杀进程放弃本轮,明日备份后重试。
const SYNC_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// 失败日志里保留的 rclone stderr 尾部。
const STDERR_TAIL: usize = 2000;
const GIB: u64 = 1024 * 1024 * 1024;

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
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 rclone 失败,运行环境装有 rclone 吗")?;
    let mut handle = child.stderr.take().expect("stderr 已设 piped");
    let mut stderr = Vec::new();
    let status = match kovi::tokio::time::timeout(SYNC_TIMEOUT, async {
        let status = child.wait().await;
        let _ = handle.read_to_end(&mut stderr).await;
        status
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
        tracing::info!("图库云同步完成: {destination}");
        Ok(())
    } else {
        anyhow::bail!(
            "rclone 异常退出({status}):{}",
            tail(&String::from_utf8_lossy(&stderr))
        )
    }
}

fn tail(text: &str) -> &str {
    if text.len() <= STDERR_TAIL {
        return text;
    }
    let mut start = text.len() - STDERR_TAIL;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    use super::dir_size;

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
}
