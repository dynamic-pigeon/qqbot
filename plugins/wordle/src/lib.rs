//! 英文 Wordle 核心逻辑：词库加载与标准判定。
//!
//! 本 crate 的 lib 不依赖任何 QQ 机器人框架，CLI 与 QQ 插件共用；
//! 插件适配层（`plugin` 模块）与 PNG 渲染（`render` 模块）仅在 `qq` feature 下编译。

pub mod game;
pub mod words;

#[cfg(feature = "qq")]
pub mod plugin;
#[cfg(feature = "qq")]
pub mod render;

// `#[kovi::plugin]` 宏生成的辅助符号以 `crate::` 路径引用，必须在 crate 根展开。
#[cfg(feature = "qq")]
#[kovi::plugin]
async fn main() {
    plugin::run().await;
}
