//! 共享的 Chromium 启动/关闭：独立 profile、轮询 CDP handler、超时关闭后删目录。

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use kovi::futures_util::StreamExt as _;
use kovi::tokio;

use crate::ResourceManager;

const DEFAULT_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);
const WINDOW_WIDTH: u32 = 1920;
const WINDOW_HEIGHT: u32 = 1080;

/// 一次已启动的 Chromium。可通过 [`Deref`] 当成 [`Browser`] 用。
pub struct ChromiumInstance {
    browser: Browser,
    user_data_dir: PathBuf,
    purpose: String,
    lifecycle_timeout: Duration,
}

impl Deref for ChromiumInstance {
    type Target = Browser;

    fn deref(&self) -> &Self::Target {
        &self.browser
    }
}

impl ChromiumInstance {
    /// 关闭浏览器；`wait` 成功后再删 profile，避免杀掉仍占用目录的进程。
    pub async fn close(self) {
        tracing::info!(purpose = %self.purpose, "关闭 Chromium");
        let ChromiumInstance {
            mut browser,
            user_data_dir,
            lifecycle_timeout,
            purpose: _,
        } = self;
        let _ = tokio::time::timeout(lifecycle_timeout, browser.close()).await;
        if tokio::time::timeout(lifecycle_timeout, browser.wait())
            .await
            .is_ok()
        {
            let _ = tokio::fs::remove_dir_all(user_data_dir).await;
        }
    }
}

/// 启动一次 Chromium 的参数。`purpose` 写入独立 user-data-dir。
#[derive(Clone)]
pub struct ChromiumLaunch {
    purpose: String,
    flags: Vec<String>,
    kv_args: Vec<(String, String)>,
    lifecycle_timeout: Duration,
}

impl ChromiumLaunch {
    /// `purpose` 用来隔离 profile，避免两个 Chromium 抢同一把 SingletonLock。
    pub fn new(purpose: impl Into<String>) -> Self {
        Self {
            purpose: purpose.into(),
            flags: Vec::new(),
            kv_args: Vec::new(),
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
        }
    }

    /// 布尔启动参数。chromiumoxide 会再拼一层 `--`，这里只传 flag 名。
    pub fn flags<I, S>(mut self, flags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.flags.extend(
            flags
                .into_iter()
                .map(|flag| strip_dashes(flag.as_ref()).to_owned()),
        );
        self
    }

    /// `key=value` 启动参数。UA 这类带值的 flag 必须走这里。
    pub fn arg(mut self, key: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.kv_args
            .push((strip_dashes(key.as_ref()).to_owned(), value.into()));
        self
    }

    /// chromiumoxide 把 `From<&str>` 当成无值 flag；UA 必须用 key/value。
    pub fn user_agent(self, ua: impl Into<String>) -> Self {
        self.arg("user-agent", ua)
    }

    pub fn lifecycle_timeout(mut self, timeout: Duration) -> Self {
        self.lifecycle_timeout = timeout;
        self
    }

    pub async fn launch(&self) -> Result<ChromiumInstance> {
        tracing::info!(purpose = %self.purpose, "启动 Chromium");
        let user_data_dir = profile_dir(&self.purpose);
        let config = BrowserConfig::builder()
            .user_data_dir(&user_data_dir)
            .window_size(WINDOW_WIDTH, WINDOW_HEIGHT)
            .args(self.flags.iter().map(String::as_str))
            .args(
                self.kv_args
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .build()
            .map_err(anyhow::Error::msg)?;
        let (browser, mut handler) =
            tokio::time::timeout(self.lifecycle_timeout, Browser::launch(config))
                .await
                .with_context(|| format!("启动 Chromium 超时 purpose={}", self.purpose))??;

        // chromiumoxide 要求持续轮询 handler stream，否则 CDP 事件不会被处理。
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });

        Ok(ChromiumInstance {
            browser,
            user_data_dir,
            purpose: self.purpose.clone(),
            lifecycle_timeout: self.lifecycle_timeout,
        })
    }

    /// 按需启动，空闲 `idle_timeout` 后关闭。
    pub fn managed(self, idle_timeout: Duration) -> ResourceManager<ChromiumInstance> {
        ResourceManager::new_with_destructor(
            idle_timeout,
            move || {
                let launch = self.clone();
                async move { launch.launch().await }
            },
            ChromiumInstance::close,
        )
    }
}

fn strip_dashes(flag: &str) -> &str {
    flag.strip_prefix("--").unwrap_or(flag)
}

static PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// purpose + PID + 序号，避免两个 Chromium 或崩溃残留共用 SingletonLock。
fn profile_dir(purpose: &str) -> PathBuf {
    let seq = PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "chromiumoxide-runner-{purpose}-{}-{seq}",
        std::process::id()
    ))
}
