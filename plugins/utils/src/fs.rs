use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 把敏感文件权限收到 `0600`，避免 umask 默认把配置和库暴露给同组用户。
pub fn restrict_mode_0600(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// purpose + PID + 序号，避免两个 Chromium 或崩溃残留共用 SingletonLock。
pub fn chromium_user_data_dir(purpose: &str) -> PathBuf {
    let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "chromiumoxide-runner-{purpose}-{}-{seq}",
        std::process::id()
    ))
}
