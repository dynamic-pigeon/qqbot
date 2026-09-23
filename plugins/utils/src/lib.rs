mod bounded_pool;
pub mod command;
pub mod config;
mod fs;
mod hash;
mod http;
mod json_store;
mod rate_limit;
mod resource_manager;

#[cfg(feature = "chromium")]
mod chromium;
#[cfg(feature = "markdown")]
mod markdown;
#[cfg(feature = "screenshot")]
mod screen_shot;

pub mod retry;
pub mod safe_url;

pub use bounded_pool::BoundedPool;
pub use fs::restrict_mode_0600;
pub use hash::{base64_image, hex_encode, sha256_hex, sha256_hex_file};
pub use http::http_client;
pub use json_store::JsonStore;
pub use rate_limit::{RateLimitHit, RateLimiter};
pub use resource_manager::{ManagedResource, ResourceManager};
pub use safe_url::{
    PRIVATE_NETWORK_PROTECTION_ENV, QQ_IMAGE_HOSTS, download_image_limited, download_image_to_file,
    https_image_url_from_data, is_public_ip, private_network_protection_enabled,
    read_response_limited, validate_image_url, validate_image_url_async,
    validate_image_url_async_with_options, validate_image_url_with_options,
};

#[cfg(feature = "chromium")]
pub use chromium::{ChromiumInstance, ChromiumLaunch};
#[cfg(feature = "markdown")]
pub use markdown::md_to_html;
#[cfg(feature = "markdown")]
pub use markdown::md_to_img;
#[cfg(feature = "screenshot")]
pub use screen_shot::{ScreenshotOptions, screenshot};
