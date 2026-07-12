# chromiumoxide 迁移实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 `plugins/utils` 的截图实现从 `playwright-rs` 迁移到 `chromiumoxide = "0.9"`，保持对外 `screenshot(html, selector)` 接口不变。

**Architecture:** 恢复 git 历史中基于 `chromiumoxide` 的 `ScreenshotManager` 结构：全局单例 Browser，每次截图新建 Page，截图完成后关闭 Page；首次失败时重启浏览器并重试一次。

**Tech Stack:** Rust 2024, Tokio, chromiumoxide 0.9, futures

## Global Constraints

- 依赖版本：`chromiumoxide = "0.9"`，`futures = "0.3"`。
- 移除 `playwright-rs` 及其 workspace 声明。
- 对外接口只保留 `utils::screenshot(html, selector)`；移除 `ContextGuard` / `get_context`。
- 保持沙箱开启，沿用当前 Chromium 加固启动参数。
- 截图相关操作设置 30 秒超时，避免卡死。
- Chromium 来源：依赖运行环境已安装的 Chrome/Chromium。
- 每次提交前运行 `cargo fmt`。

---

## File Structure

| 文件 | 职责 | 变更类型 |
|---|---|---|
| `plugins/utils/Cargo.toml` | utils crate 依赖声明 | 修改 |
| `Cargo.toml` | workspace 依赖声明 | 修改 |
| `plugins/utils/src/screen_shot.rs` | 截图核心实现 | 重写 |
| `plugins/utils/src/lib.rs` | utils 对外导出 | 修改 |
| `plugins/utils/src/bounded_pool.rs` | 资源池实现 | 不修改，保留但不再被截图使用 |
| `plugins/utils/tests/markdown.rs` | 现有截图相关测试 | 运行/验证 |

---

### Task 1: 更新依赖声明

**Files:**
- Modify: `plugins/utils/Cargo.toml`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: 无
- Produces: `plugins/utils` 依赖 `chromiumoxide` 和 `futures`，不再依赖 `playwright-rs`

- [ ] **Step 1: 修改 `plugins/utils/Cargo.toml`**

将 `playwright-rs.workspace = true` 替换为 `chromiumoxide = "0.9"` 和 `futures.workspace = true`（或 `futures = "0.3"`）。

```toml
[package]
name = "utils"
version = "0.1.0"
edition = "2024"

[dependencies]
pulldown-cmark.workspace = true
kovi.workspace = true
anyhow.workspace = true
askama.workspace = true
chromiumoxide = "0.9"
futures.workspace = true
tracing.workspace = true
crossbeam-epoch.workspace = true
reqwest.workspace = true
ammonia = "4"
base64.workspace = true

[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt"] }

[build-dependencies]
base64.workspace = true
```

- [ ] **Step 2: 修改根目录 `Cargo.toml`**

在 `[workspace.dependencies]` 中移除 `playwright-rs` 整行，并确保 `futures = "0.3"` 存在（通常已有）。

```toml
# 移除以下整行
# playwright-rs = { version = "0.14", default-features = false, features = [
#     "rustls-tls-webpki-roots",
#     "macros",
# ] }
```

- [ ] **Step 3: 运行 `cargo check` 观察初始状态**

```bash
cd /Users/bytedance/Public/hm/qqbot
cargo check -p utils
```

Expected: 大量 `playwright_rs` 未找到的错误（正常，因为 `screen_shot.rs` 还未改）。

- [ ] **Step 4: Commit**

```bash
git add plugins/utils/Cargo.toml Cargo.toml
git commit -m "chore(utils): replace playwright-rs with chromiumoxide 0.9 deps

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: 重写 `screen_shot.rs`

**Files:**
- Modify: `plugins/utils/src/screen_shot.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub async fn screenshot(html: &str, selector: Option<&str>) -> Result<Vec<u8>>`
  - `pub struct ScreenshotManager`

- [ ] **Step 1: 用 chromiumoxide 实现替换 `plugins/utils/src/screen_shot.rs` 全部内容**

```rust
use std::time::Duration;

use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, ScreenshotParams, Viewport};
use futures::StreamExt;
use kovi::tokio::{self, sync::OnceCell};
use tracing::{error, info};

/// 截图默认超时：避免浏览器偶发卡死永久占用一个 tokio task。
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);

/// 对外入口：将 HTML 渲染为 PNG。
/// - `html`: 完整 HTML 内容
/// - `selector`: 若提供，只截取该 CSS 选择器匹配的元素；否则截取全页
pub async fn screenshot(html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
    let manager = get_manager().await?;
    manager.screenshot(html, selector).await
}

async fn get_manager() -> Result<&'static ScreenshotManager> {
    static MANAGER: OnceCell<ScreenshotManager> = OnceCell::const_new();
    MANAGER.get_or_try_init(ScreenshotManager::init).await
}

pub struct ScreenshotManager {
    browser: tokio::sync::RwLock<Browser>,
    restart_lock: tokio::sync::Mutex<()>,
}

impl ScreenshotManager {
    pub async fn init() -> Result<Self> {
        let browser = Self::launch_browser().await?;
        info!("chromiumoxide browser launched");
        Ok(Self {
            browser: tokio::sync::RwLock::new(browser),
            restart_lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn launch_browser() -> Result<Browser> {
        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .window_size(1920, 1080)
                .arg("--disable-dev-shm-usage")
                .arg("--disable-background-networking")
                .arg("--disable-default-apps")
                .arg("--disable-extensions")
                .arg("--disable-sync")
                .arg("--disable-translate")
                .arg("--no-first-run")
                .arg("--mute-audio")
                .arg("--password-store=basic")
                .arg("--use-mock-keychain")
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .await?;

        // chromiumoxide 要求持续轮询 handler stream，否则 CDP 事件不会被处理。
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        Ok(browser)
    }

    pub async fn screenshot(&self, html: &str, selector: Option<&str>) -> Result<Vec<u8>> {
        let browser = self.browser.read().await;
        match tokio::time::timeout(
            SCREENSHOT_TIMEOUT,
            Self::do_screenshot(&browser, html, selector),
        )
        .await
        {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => {
                error!("截图失败，尝试重启浏览器后重试: {}", e);
                drop(browser);
                self.restart_browser().await?;
                let browser = self.browser.read().await;
                tokio::time::timeout(
                    SCREENSHOT_TIMEOUT,
                    Self::do_screenshot(&browser, html, selector),
                )
                .await
                .map_err(|_| anyhow::anyhow!("截图超时"))?
            }
            Err(_) => {
                drop(browser);
                Err(anyhow::anyhow!("截图超时"))
            }
        }
    }

    async fn restart_browser(&self) -> Result<()> {
        let _lock = self.restart_lock.lock().await;
        let mut browser = self.browser.write().await;
        info!("restarting chromiumoxide browser");
        let _ = browser.close().await;
        let _ = browser.wait().await;
        *browser = Self::launch_browser().await?;
        Ok(())
    }

    async fn do_screenshot(
        browser: &Browser,
        html: &str,
        selector: Option<&str>,
    ) -> Result<Vec<u8>> {
        let page = browser.new_page("about:blank").await?;

        let res = async {
            page.set_content(html).await?;

            let bytes = if let Some(selector) = selector {
                let bounding_box = page
                    .find_element(selector)
                    .await
                    .map_err(|e| anyhow::anyhow!("找不到元素 {}: {}", selector, e))?
                    .bounding_box()
                    .await?;
                let viewport = Viewport {
                    x: bounding_box.x,
                    y: bounding_box.y,
                    width: bounding_box.width,
                    height: bounding_box.height,
                    scale: 1.0,
                };
                page.screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .clip(viewport)
                        .capture_beyond_viewport(true)
                        .build(),
                )
                .await?
            } else {
                page.screenshot(
                    ScreenshotParams::builder()
                        .format(CaptureScreenshotFormat::Png)
                        .full_page(true)
                        .build(),
                )
                .await?
            };
            Ok(bytes)
        }
        .await;

        let _ = page.close().await;
        res
    }
}
```

注意：以上代码基于 `chromiumoxide 0.8` 的 API 结构。0.9 的 import 路径或 builder 方法可能有微调，下一步用 `cargo check` 修正。

- [ ] **Step 2: 运行 `cargo check` 并修正 API 差异**

```bash
cd /Users/bytedance/Public/hm/qqbot
cargo check -p utils
```

Expected: 可能出现 import 错误、builder 方法不存在等。根据错误信息调整：
- 如果 `BrowserConfig::builder()` 不存在，改用 `BrowserConfig::new()` 或对应 API。
- 如果 `page.find_element` 返回 `Option`，用 `.ok_or_else(...)` 转换。
- 如果 `ScreenshotParams::builder()` 方法名不同，改成 0.9 对应的方法。
- 如果 `CaptureScreenshotFormat::Png` 路径不同，调整 import。

- [ ] **Step 3: Commit**

```bash
git add plugins/utils/src/screen_shot.rs
git commit -m "refactor(utils): rewrite screenshot with chromiumoxide

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: 调整对外导出

**Files:**
- Modify: `plugins/utils/src/lib.rs`

**Interfaces:**
- Consumes: `screen_shot::screenshot`
- Produces: 简化的 public API

- [ ] **Step 1: 修改 `plugins/utils/src/lib.rs`**

移除 `ContextGuard` 和 `get_context` 导出，只保留 `screenshot`。

```rust
mod bounded_pool;
mod markdown;
mod retry;
mod screen_shot;

pub use bounded_pool::{BoundedPool, BoundedResourcePool, ResourceGuard};
pub use markdown::{md_to_html, md_to_img};
pub use retry::retry_async;
pub use screen_shot::screenshot;
```

- [ ] **Step 2: 运行 `cargo check` 确认无未使用导出**

```bash
cargo check -p utils
```

Expected: 通过。

- [ ] **Step 3: Commit**

```bash
git add plugins/utils/src/lib.rs
git commit -m "refactor(utils): remove unused ContextGuard/get_context exports

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: 格式化与静态检查

**Files:**
- Modify: 所有已变更文件

- [ ] **Step 1: 运行 cargo fmt**

```bash
cd /Users/bytedance/Public/hm/qqbot
cargo fmt
```

- [ ] **Step 2: 运行 cargo clippy**

```bash
cargo clippy -p utils -- -D warnings
```

Expected: 无 warning。若出现 `BoundedResourcePool` 等未使用警告，确认是 `pub` 导出后消除，或保留在 `lib.rs` 的 `pub use` 中。

- [ ] **Step 3: Commit 格式化结果**

```bash
git add -A
git commit -m "style(utils): cargo fmt and clippy for chromiumoxide migration

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: 编译与测试验证

**Files:**
- Test: `plugins/utils/tests/markdown.rs`
- 运行环境需要已安装 Chrome/Chromium

- [ ] **Step 1: 运行 utils crate 测试**

```bash
cd /Users/bytedance/Public/hm/qqbot
cargo test -p utils
```

Expected: 所有测试通过。若环境无 Chrome，会报错提示找不到浏览器；此时需先安装 Chromium。

- [ ] **Step 2: 全 workspace 编译**

```bash
cargo check --workspace
```

Expected: 全 workspace 通过。

- [ ] **Step 3: Commit（如有测试或配置调整）**

```bash
git add -A
git commit -m "test(utils): verify chromiumoxide screenshot tests pass

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: 运行期验证（需要 QQ 环境或手动触发）

**Files:**
- 调用方：`plugins/utils/src/markdown/mod.rs`
- 调用方：`plugins/msg_rank/src/msg_rank/mod.rs`

- [ ] **Step 1: 验证 Markdown 截图**

启动 bot 后，在群里发送一条包含 Markdown/KaTeX/代码块的测试消息（取决于 bot 命令），触发 `md_to_img`。

Expected: 返回 PNG 图片，内容包含正确渲染的 Markdown、公式、代码高亮。

- [ ] **Step 2: 验证发言排行截图**

触发 `#今日发言排行` 命令。

Expected: 返回 PNG 图片，显示今日发言排行。

- [ ] **Step 3: 失败重试验证**

可手动 kill Chromium 进程或构造错误场景，验证失败后会重启浏览器并重试一次。

- [ ] **Step 4: 最终提交或 amend**

如无代码变更，无需额外提交；如有调整，正常 commit。

---

## 回滚方案

若迁移后出现无法解决的稳定性问题，可直接 revert 本次相关 commits 回到 `playwright-rs` 版本：

```bash
git revert --no-commit <first-migration-commit>..HEAD
git commit -m "revert: rollback to playwright-rs"
```

---

## Self-Review

1. **Spec coverage**: 依赖替换、screen_shot.rs 重写、导出调整、格式化/检查、测试、运行期验证均已覆盖。
2. **Placeholder scan**: 无 TBD/TODO；代码块为具体实现；失败重试和 API 调整步骤给出具体命令。
3. **Type consistency**: `screenshot(html: &str, selector: Option<&str>)` 接口保持不变，调用方无需修改。
