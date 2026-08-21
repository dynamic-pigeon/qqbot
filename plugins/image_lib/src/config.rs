use std::sync::LazyLock;
use std::time::Duration;

/// 未配置时每个群的图库总容量上限，单位 MiB。
pub(crate) const DEFAULT_MAX_GROUP_MIB: u64 = 500;
/// 未配置时单张图片大小上限，单位 MiB。
pub(crate) const DEFAULT_MAX_IMAGE_MIB: u64 = 15;
/// 未配置时「来只」滑动窗口长度，单位秒。
pub(crate) const DEFAULT_DRAW_WINDOW_SECS: u64 = 60;
/// 未配置时「来只」窗口内次数上限。
pub(crate) const DEFAULT_DRAW_MAX_PER_WINDOW: usize = 5;

/// 根目录 `config.toml` 的 `[image_lib]`。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub(crate) struct StaticConfig {
    /// 每个群的图库总容量上限，单位 MiB。
    pub max_group_mib: u64,
    /// 单张图片大小上限，单位 MiB。
    pub max_image_mib: u64,
    /// 「来只」滑动窗口长度，单位秒。
    pub draw_window_secs: u64,
    /// 「来只」窗口内次数上限。
    pub draw_max_per_window: usize,
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            max_group_mib: DEFAULT_MAX_GROUP_MIB,
            max_image_mib: DEFAULT_MAX_IMAGE_MIB,
            draw_window_secs: DEFAULT_DRAW_WINDOW_SECS,
            draw_max_per_window: DEFAULT_DRAW_MAX_PER_WINDOW,
        }
    }
}

impl StaticConfig {
    pub fn max_group_bytes(&self) -> u64 {
        self.max_group_mib.saturating_mul(1024 * 1024)
    }

    pub fn max_image_mib(&self) -> u64 {
        self.max_image_mib.max(1)
    }

    pub fn max_image_bytes(&self) -> usize {
        usize::try_from(self.max_image_mib().saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
    }

    pub fn draw_window(&self) -> Duration {
        Duration::from_secs(self.draw_window_secs.max(1))
    }

    pub fn draw_max_per_window(&self) -> usize {
        self.draw_max_per_window.max(1)
    }
}

pub(crate) fn static_config() -> &'static StaticConfig {
    static CONFIG: LazyLock<StaticConfig> = LazyLock::new(|| {
        utils::config::parse("image_lib")
            .unwrap_or_else(|error| panic!("解析 [image_lib] 配置失败: {error:#}"))
    });
    &CONFIG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_quota_is_500_mib() {
        assert_eq!(
            StaticConfig::default().max_group_bytes(),
            DEFAULT_MAX_GROUP_MIB * 1024 * 1024
        );
    }

    #[test]
    fn default_draw_limit_is_five_per_minute() {
        let config = StaticConfig::default();
        assert_eq!(config.draw_window().as_secs(), DEFAULT_DRAW_WINDOW_SECS);
        assert_eq!(config.draw_max_per_window(), DEFAULT_DRAW_MAX_PER_WINDOW);
    }

    #[test]
    fn default_image_limit_is_15_mib() {
        let config = StaticConfig::default();
        assert_eq!(config.max_image_mib(), DEFAULT_MAX_IMAGE_MIB);
        assert_eq!(
            config.max_image_bytes(),
            (DEFAULT_MAX_IMAGE_MIB * 1024 * 1024) as usize
        );
    }
}
