use std::future::Future;

use super::{MessageScope, Permission};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageSource {
    Group,
    Private,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AccessError {
    #[error("此命令只能在群聊中使用")]
    GroupOnly,
    #[error("此命令只能在私聊中使用")]
    PrivateOnly,
    #[error("管理员专用命令，普通用户无法使用")]
    PermissionDenied,
}

pub fn check_access(
    scope: MessageScope,
    permission: Permission,
    source: MessageSource,
    is_admin: bool,
) -> Result<(), AccessError> {
    match (scope, source) {
        (MessageScope::Group, MessageSource::Private) => return Err(AccessError::GroupOnly),
        (MessageScope::Private, MessageSource::Group) => return Err(AccessError::PrivateOnly),
        _ => {}
    }

    if permission == Permission::BotAdmin && !is_admin {
        return Err(AccessError::PermissionDenied);
    }
    Ok(())
}

pub async fn dispatch_if_allowed<F, Fut>(
    scope: MessageScope,
    permission: Permission,
    source: MessageSource,
    is_admin: bool,
    action: F,
) -> Result<(), AccessError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    check_access(scope, permission, source, is_admin)?;
    action().await;
    Ok(())
}
