use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::{
    EventLoadingFinished, EventResponseReceived, GetResponseBodyParams,
};
use futures::StreamExt as _;
use kovi::tokio::{self, sync::OnceCell};

const BROWSER_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(10);
const BROWSER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const PAGE_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(200);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub(super) async fn fetch_space_body(uid: u64, offset: Option<&str>) -> Result<String> {
    manager().await.fetch(uid, offset).await
}

async fn manager() -> &'static BrowserManager {
    static MANAGER: OnceCell<BrowserManager> = OnceCell::const_new();
    MANAGER
        .get_or_init(|| async { BrowserManager::new() })
        .await
}

struct BrowserManager {
    browser: utils::ResourceManager<Browser>,
    request_lock: tokio::sync::Mutex<()>,
}

impl BrowserManager {
    fn new() -> Self {
        Self {
            browser: utils::ResourceManager::new_with_destructor(
                IDLE_TIMEOUT,
                || async {
                    tracing::info!("启动 Bilibili 匿名动态 Chromium 后备");
                    Self::launch_browser().await
                },
                Self::close_browser,
            ),
            request_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn launch_browser() -> Result<Browser> {
        let config = BrowserConfig::builder()
            .window_size(1920, 1080)
            .arg("--disable-dev-shm-usage")
            .arg("--disable-default-apps")
            .arg("--disable-extensions")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--no-first-run")
            .arg("--mute-audio")
            .arg("--password-store=basic")
            .arg("--use-mock-keychain")
            // B 站风控认 UA；headless 默认带 HeadlessChrome，和 HTTP 直连指纹不一致。
            .arg(user_agent_arg(super::fetch::user_agent()))
            .build()
            .map_err(anyhow::Error::msg)?;
        let (browser, mut handler) =
            tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, Browser::launch(config))
                .await
                .context("启动 Chromium 超时")??;
        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });
        Ok(browser)
    }

    async fn close_browser(mut browser: Browser) {
        tracing::info!("关闭 Bilibili 匿名动态 Chromium 后备");
        let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.close()).await;
        let _ = tokio::time::timeout(BROWSER_LIFECYCLE_TIMEOUT, browser.wait()).await;
    }

    async fn fetch(&self, uid: u64, offset: Option<&str>) -> Result<String> {
        let _request = self.request_lock.lock().await;
        let browser = self.browser.get().await?;
        fetch_with_browser(&browser, uid, offset).await
    }
}

async fn fetch_with_browser(browser: &Browser, uid: u64, offset: Option<&str>) -> Result<String> {
    let page = browser.new_page("about:blank").await?;
    // 超时只包住抓取阶段、不包住 close：超时 drop 掉抓取 future 后仍需显式
    // 关闭 tab（chromiumoxide 的 Page 没有 Drop 自动关闭），否则风控期反复
    // 超时会在浏览器里累积僵尸 tab。
    let operation = tokio::time::timeout(BROWSER_REQUEST_TIMEOUT, async {
        // 启动参数覆盖进程默认 UA；tab 上再 CDP override，保证 goto 发出的
        // 空间页请求和 navigator.userAgent 都用同一份 BILIBILI_USER_AGENT。
        page.set_user_agent(super::fetch::user_agent()).await?;
        for attempt in 0..2 {
            match capture_dynamic_body(&page, uid, offset).await {
                Ok(body) => return Ok(body),
                Err(error) if attempt == 0 => {
                    tracing::warn!("空间页动态响应捕获失败，重新加载后重试: {error}");
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    })
    .await;
    let _ = tokio::time::timeout(PAGE_CLOSE_TIMEOUT, page.close()).await;
    operation.context("Chromium 动态请求超时")?
}

async fn capture_dynamic_body(
    page: &chromiumoxide::Page,
    uid: u64,
    offset: Option<&str>,
) -> Result<String> {
    let mut responses = page.event_listener::<EventResponseReceived>().await?;
    let mut finished = page.event_listener::<EventLoadingFinished>().await?;
    page.goto(format!("https://space.bilibili.com/{uid}/dynamic"))
        .await?;

    let mut request_id = None;
    let deadline = Instant::now() + DISCOVERY_TIMEOUT;
    while Instant::now() < deadline {
        match tokio::time::timeout(DISCOVERY_INTERVAL, responses.next()).await {
            Ok(Some(event)) if validate_api_url(&event.response.url, uid, offset).is_ok() => {
                request_id = Some(event.request_id.clone());
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => anyhow::bail!("Chromium 网络事件流已关闭"),
            Err(_) => {}
        }
        page.evaluate_expression(
            "(() => {
                const target = document.scrollingElement || document.documentElement;
                target.scrollTop = target.scrollHeight;
                window.dispatchEvent(new Event('scroll'));
                return true;
            })()",
        )
        .await?;
    }
    let request_id = request_id.context("空间页未发出目标动态 API 请求")?;

    let deadline = Instant::now() + DISCOVERY_TIMEOUT;
    while Instant::now() < deadline {
        match tokio::time::timeout(DISCOVERY_INTERVAL, finished.next()).await {
            Ok(Some(event)) if event.request_id == request_id => {
                let response = page
                    .execute(GetResponseBodyParams::new(request_id))
                    .await?
                    .result;
                let bytes = if response.base64_encoded {
                    STANDARD.decode(response.body)?
                } else {
                    response.body.into_bytes()
                };
                if bytes.len() > MAX_RESPONSE_BYTES {
                    anyhow::bail!("Chromium 动态响应超过 {} bytes", MAX_RESPONSE_BYTES);
                }
                return String::from_utf8(bytes).context("Chromium 动态响应不是 UTF-8");
            }
            Ok(Some(_)) | Err(_) => continue,
            Ok(None) => anyhow::bail!("Chromium 加载事件流已关闭"),
        }
    }
    anyhow::bail!("目标动态 API 响应未完成")
}

/// chromiumoxide 把 `From<&str>` 当成无值 flag（再拼一层 `--`），
/// UA 必须用 key/value，否则 `--user-agent=...` 会变成非法启动参数。
fn user_agent_arg(ua: &str) -> (&str, &str) {
    ("user-agent", ua)
}

fn validate_api_url(observed: &str, uid: u64, offset: Option<&str>) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(observed).context("浏览器动态 API URL 无效")?;
    if url.scheme() != "https"
        || url.host_str() != Some("api.bilibili.com")
        || url.path() != "/x/polymer/web-dynamic/v1/feed/space"
    {
        anyhow::bail!("浏览器动态 API URL 不在允许范围内")
    }
    let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    if params.get("host_mid").map(String::as_str) != Some(uid.to_string().as_str())
        || params.get("offset").map(String::as_str) != Some(offset.unwrap_or_default())
    {
        anyhow::bail!("浏览器动态 API URL 参数与请求不匹配")
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_launch_arg_is_key_value() {
        let ua = "Mozilla/5.0 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
        assert_eq!(user_agent_arg(ua), ("user-agent", ua));
    }

    #[test]
    fn observed_url_rejects_unexpected_host() {
        assert!(
            validate_api_url(
                "https://example.com/x/polymer/web-dynamic/v1/feed/space",
                2,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn observed_url_validates_requested_offset() {
        let url = validate_api_url(
            "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/space?\
             host_mid=2&offset=next-page&features=itemOpusStyle&wts=1&w_rid=signed",
            2,
            Some("next-page"),
        )
        .unwrap();
        assert_eq!(url.host_str(), Some("api.bilibili.com"));
        assert!(validate_api_url(url.as_str(), 2, Some("wrong-page")).is_err());
    }
}
