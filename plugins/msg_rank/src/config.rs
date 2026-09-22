use std::sync::LazyLock;

use utils::JsonStore;

/// 根目录 `config.toml` 的 `[msg_rank]`。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct StaticConfig {
    pub retention_days: u64,
    pub wordcloud_concurrency: usize,
    pub wordcloud: Vec<WordCloudSchedule>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WordCloudSchedule {
    pub cron: String,
    pub days: i64,
    pub title: String,
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            retention_days: 8,
            wordcloud_concurrency: 1,
            wordcloud: vec![
                WordCloudSchedule {
                    cron: "0 21 * * *".into(),
                    days: 1,
                    title: "今日词云".into(),
                },
                WordCloudSchedule {
                    cron: "0 10 * * 6".into(),
                    days: 7,
                    title: "上周词云".into(),
                },
            ],
        }
    }
}

pub(crate) fn static_config() -> &'static StaticConfig {
    static PARSED: LazyLock<StaticConfig> =
        LazyLock::new(|| utils::config::parse_or_panic("msg_rank"));
    &PARSED
}

pub(crate) static CONFIG: JsonStore<Config> = JsonStore::new();

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Config {
    /// 采集消息的群。发言排行和词云都读这份记录。
    pub notify_group: Vec<i64>,
    /// 定时推送词云的群。空列表就是不开；缺字段时按空列表读。
    #[serde(default)]
    pub wordcloud_group: Vec<i64>,
    /// 词云背景色，支持 #RRGGBB 和常见颜色名。
    #[serde(default = "default_wordcloud_background")]
    pub wordcloud_background: String,
}

fn default_wordcloud_background() -> String {
    "#ffffff".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notify_group: vec![],
            wordcloud_group: vec![],
            wordcloud_background: default_wordcloud_background(),
        }
    }
}

impl Config {
    pub(crate) fn wordcloud_enabled(&self, group_id: i64) -> bool {
        self.wordcloud_group.contains(&group_id)
    }

    pub(crate) fn recording_enabled(&self, group_id: i64) -> bool {
        self.notify_group.contains(&group_id)
    }

    pub(crate) fn enable_recording(&mut self, group_id: i64) {
        if !self.notify_group.contains(&group_id) {
            self.notify_group.push(group_id);
        }
    }

    pub(crate) fn disable_recording(&mut self, group_id: i64) {
        self.notify_group.retain(|&id| id != group_id);
    }

    /// 开始采集，并打开定时词云。
    pub(crate) fn enable_wordcloud(&mut self, group_id: i64) {
        self.enable_recording(group_id);
        if !self.wordcloud_group.contains(&group_id) {
            self.wordcloud_group.push(group_id);
        }
    }

    /// 只关掉定时词云，采集继续。
    pub(crate) fn disable_wordcloud(&mut self, group_id: i64) {
        self.wordcloud_group.retain(|&id| id != group_id);
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn empty_wordcloud_group_means_disabled() {
        let config: Config =
            kovi::serde_json::from_str(r#"{"notify_group":[1],"wordcloud_group":[]}"#).unwrap();
        assert!(config.recording_enabled(1));
        assert!(!config.wordcloud_enabled(1));
    }

    #[test]
    fn disable_wordcloud_keeps_recording() {
        let mut config = Config::default();
        config.enable_wordcloud(1);
        assert!(config.notify_group.contains(&1));
        assert!(config.wordcloud_enabled(1));

        config.disable_wordcloud(1);
        assert!(config.notify_group.contains(&1));
        assert!(!config.wordcloud_enabled(1));

        config.enable_wordcloud(1);
        assert!(config.wordcloud_enabled(1));
        assert_eq!(config.notify_group.iter().filter(|&&id| id == 1).count(), 1);
    }

    #[test]
    fn enable_recording_does_not_turn_on_wordcloud() {
        let mut config = Config::default();
        config.enable_recording(1);
        assert!(config.recording_enabled(1));
        assert!(!config.wordcloud_enabled(1));

        config.enable_wordcloud(1);
        assert!(config.wordcloud_enabled(1));
        assert!(config.recording_enabled(1));
    }

    #[test]
    fn enable_recording_does_not_change_wordcloud_group() {
        let mut config = Config {
            notify_group: vec![1],
            wordcloud_group: vec![1],
            wordcloud_background: super::default_wordcloud_background(),
        };
        config.enable_recording(2);
        assert!(config.recording_enabled(1));
        assert!(config.recording_enabled(2));
        assert!(config.wordcloud_enabled(1));
        assert!(!config.wordcloud_enabled(2));
        assert_eq!(config.wordcloud_group, vec![1]);
    }
}
