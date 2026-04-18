use anyhow::{Ok, Result};
use kovi::{RuntimeBot, serde_json};
use tracing;

use crate::HTTP_CLIENT;

pub(crate) struct UserInfo {
    #[allow(dead_code)]
    pub user_id: i64,
    // QQ昵称或者群名片
    pub nickname: String,
    pub avatar: bytes::Bytes,
}

#[derive(serde::Deserialize)]
struct UserInfoApiResponse {
    card: String,
    nickname: String,
    user_id: i64,
}

/// 通过 bot API 获取单个群成员的 UserInfo
pub(super) async fn get_user_info(
    bot: &RuntimeBot,
    group_id: i64,
    user_id: i64,
) -> Result<UserInfo> {
    let resp = bot
        .get_group_member_info(group_id, user_id, false)
        .await
        .map_err(|e| {
            tracing::error!("调用 API get_group_member_info 失败: {}", e);
            anyhow::anyhow!("获取群成员信息失败")
        })?;

    parse_api_res(resp).await
}

async fn parse_api_res(resp: kovi::ApiReturn) -> Result<UserInfo> {
    if resp.status != "ok" {
        anyhow::bail!("API请求失败: {:?}", resp);
    }

    let item = serde_json::from_value::<UserInfoApiResponse>(resp.data)?;
    let avatar = get_avatar(item.user_id).await?;
    let nickname = if item.card.is_empty() {
        item.nickname
    } else {
        item.card
    };

    Ok(UserInfo {
        user_id: item.user_id,
        nickname,
        avatar,
    })
}

async fn get_avatar(user_id: i64) -> Result<bytes::Bytes> {
    let avatar_url = format!("https://q4.qlogo.cn/headimg_dl?dst_uin={user_id}&spec=640");
    let resp = HTTP_CLIENT.get(&avatar_url).send().await?;
    let bytes = resp.bytes().await?;
    Ok(bytes)
}
