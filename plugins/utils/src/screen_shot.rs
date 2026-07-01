use std::{
    borrow::Cow,
    sync::LazyLock,
    time::Duration,
};

use anyhow::Result;
use kovi::tokio::sync::OnceCell;
use playwright_rs::protocol::{
    BrowserContext, BrowserContextOptions, ScreenshotOptions, Viewport,
};
use playwright_rs::{Browser, LaunchOptions, Playwright};
use tracing::{debug, info};

use crate::BoundedResourcePool;

/// 默认视口大小，与原 chromiumoxide 配置保持一致
const VIEWPORT: Viewport = Viewport {
    width: 1920,
    height: 1080,
};

/// 同时最多存在的 BrowserContext 数量（含借出和 idle）。
const CONTEXT_POOL_MAX: usize = 8;

/// 等待 BrowserContext 的最长等待时间。
const CONTEXT_POOL_WAIT: Duration = Duration::from_secs(30);

/// idle BrowserContext 超过这个时间没被复用就被清理销毁。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 后台 cleanup 任务运行间隔。
const POOL_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

/// playwright 操作的默认超时（毫秒）。截图、设置内容等步骤超过此时长会被强制取消，
/// 避免浏览器偶发卡死永久占用一个 tokio task。
const DEFAULT_TIMEOUT_MS: f64 = 30_000.0;

/// 浏览器会话：持有 Playwright Server 与 Browser 的生命周期。
/// Playwright / Browser 在 playwright-rs 里都是远端 handle，
/// 内部走 Arc 共享，clone 廉价。
struct BrowserSession {
    /// 持有 Playwright 实例以维持其后台进程（webdriver pipe）存活
    _playwright: Playwright,
    /// 持有 Browser 引用以维持浏览器进程存活
    #[allow(dead_code)]
    browser: Browser,
}

/// 全局 Playwright 会话，惰性初始化。
static SESSION: OnceCell<BrowserSession> = OnceCell::const_new();

async fn get_session() -> Result<&'static BrowserSession> {
    SESSION
        .get_or_try_init(|| async {
            info!("screen_shot: launching browser");
            launch_session().await
        })
        .await
}

/// Chromium 启动参数：禁用沙箱已移除，并开启多项加固开关。
///
/// 注意：Chromium 沙箱在 root 用户下无法正常工作；若 bot 以 root 运行，
/// launch 会失败。推荐为 bot 创建普通用户。
fn chromium_launch_args() -> Vec<String> {
    vec![
        // 容器环境常见配置：避免 /dev/shm 不足
        "--disable-dev-shm-usage".to_string(),
        // 关闭 Chromium 后台网络服务，减少攻击面
        "--disable-background-networking".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-extensions".to_string(),
        "--disable-sync".to_string(),
        "--disable-translate".to_string(),
        "--no-first-run".to_string(),
        "--mute-audio".to_string(),
        "--password-store=basic".to_string(),
        "--use-mock-keychain".to_string(),
    ]
}

async fn launch_session() -> Result<BrowserSession> {
    let playwright = Playwright::launch().await?;
    let browser = playwright
        .chromium()
        .launch_with_options(
            LaunchOptions::new()
                .headless(true)
                .args(chromium_launch_args()),
        )
        .await?;
    Ok(BrowserSession {
        _playwright: playwright,
        browser,
    })
}

/// 全局 BrowserContext 资源池。
static BROWSER_POOL: LazyLock<BoundedResourcePool<BrowserContext>> = LazyLock::new(|| {
    BoundedResourcePool::new(CONTEXT_POOL_MAX, POOL_IDLE_TIMEOUT, POOL_CLEANUP_INTERVAL)
});

/// 暴露给外部的 BrowserContext guard 类型。
pub type ContextGuard = crate::bounded_pool::ResourceGuard<BrowserContext>;

/// 从池中获取一个 [`ContextGuard`]。
///
/// - 池中有 idle context → 复用最老的那个（FIFO）
/// - 池空 → 启动浏览器（如还没启动）并创建新 context
///
/// guard drop 时自动归还到 idle 池；被 [`ContextGuard::mark_unhealthy`] 标记过的
/// context 会直接丢弃，避免污染连接池。
///
/// # Example
/// ```no_run
/// use playwright_rs::protocol::ScreenshotOptions;
/// # async fn example() -> anyhow::Result<()> {
/// let context = utils::get_context().await?;
/// // context 实际上是 ContextGuard，但 Deref 到 BrowserContext
/// let page = context.new_page().await?;
/// page.goto("https://example.com", None).await?;
/// let bytes = page
///     .screenshot(Some(
///         ScreenshotOptions::builder().full_page(true).build(),
///     ))
///     .await?;
/// page.close().await;
/// // context 在 scope 结束时 drop，自动归还到池
/// # Ok(())
/// # }
/// ```
pub async fn get_context() -> Result<ContextGuard> {
    BROWSER_POOL
        .acquire(CONTEXT_POOL_WAIT, || async {
            let session = get_session().await?;
            let opts = BrowserContextOptions::builder().viewport(VIEWPORT).build();
            let context = session.browser.new_context_with_options(opts).await?;
            context.set_default_timeout(DEFAULT_TIMEOUT_MS).await;
            debug!("screen_shot: created new BrowserContext");
            Ok(context)
        })
        .await
}

/// 截图 HTML 内容，返回 PNG 数据。
/// - `html`: 要截图的 HTML 内容
/// - `selector`: 可选的 CSS 选择器，如果提供则只截图该元素，否则截图整个页面
///
/// 内部走 BrowserContext 资源池，池满 / 池空时按上述规则创建或复用。
pub async fn screenshot(
    html: Cow<'static, str>,
    selector: Option<Cow<'static, str>>,
) -> Result<Vec<u8>> {
    let mut guard = get_context().await?;
    // clone 一份 BrowserContext handle，避免后续可变借用 guard 时产生冲突。
    let ctx = BrowserContext::clone(&guard);
    let html_ref = html.as_ref();
    let selector_ref = selector.as_deref();
    // 截图偶尔会因浏览器上下文瞬时状态失败，做 1 次重试提高成功率。
    // 若重试后仍失败，认为该 context 已不健康，归还时直接丢弃，避免污染连接池。
    let res = crate::retry::retry_async(|| do_screenshot(&ctx, html_ref, selector_ref), 1).await;
    if res.is_err() {
        guard.mark_unhealthy();
        // 尝试优雅关闭异常 context；失败也无所谓，句柄释放后 playwright 会最终回收。
        let _ = ctx.close().await;
    }
    drop(guard);
    res
}

async fn do_screenshot(
    context: &BrowserContext,
    html: &str,
    selector: Option<&str>,
) -> Result<Vec<u8>> {
    let page = context.new_page().await?;
    let res = async {
        page.set_content(html, None).await?;

        let bytes = if let Some(selector) = selector {
            let locator = page.locator(selector).await;
            locator.screenshot(None).await?
        } else {
            let opts = ScreenshotOptions::builder().full_page(true).build();
            page.screenshot(Some(opts)).await?
        };
        Ok(bytes)
    }
    .await;
    let _ = page.close().await;

    res
}
