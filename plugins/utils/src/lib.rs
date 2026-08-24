mod bounded_pool;
pub mod command;
pub mod config;
mod rate_limit;
mod rcu;
mod resource_manager;

#[cfg(feature = "markdown")]
mod markdown;
#[cfg(feature = "screenshot")]
mod screen_shot;

pub mod retry;
pub mod safe_url;

pub use bounded_pool::BoundedPool;
pub use rate_limit::{RateLimitHit, RateLimiter};
pub use rcu::{RcuCell, RcuReadGuard};
pub use resource_manager::{ManagedResource, ResourceManager};
pub use safe_url::{
    PRIVATE_NETWORK_PROTECTION_ENV, download_image_limited, is_public_ip,
    private_network_protection_enabled, read_response_limited, validate_image_url,
    validate_image_url_async, validate_image_url_async_with_options,
    validate_image_url_with_options,
};

#[cfg(feature = "markdown")]
pub use markdown::md_to_html;
#[cfg(feature = "markdown")]
pub use markdown::md_to_img;
#[cfg(feature = "screenshot")]
pub use screen_shot::{ScreenshotManager, ScreenshotOptions, screenshot};
