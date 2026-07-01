use anyhow::Result;
use kovi::{RuntimeBot, serde_json, tokio};
use kovi_onebot::OnebotTrait as _;

use crate::HTTP_CLIENT;

pub(crate) struct UserInfo {
    #[allow(dead_code)]
    pub user_id: i64,
    // QQ昵称或者群名片
    pub nickname: String,
    pub avatar: bytes::Bytes,
}

#[derive(serde::Deserialize)]
struct MemberInfoApiResponse {
    card: String,
    nickname: String,
    user_id: i64,
}

/// 通过 bot API 获取单个群成员的 UserInfo（群成员信息与头像并发获取）
pub(super) async fn get_user_info(
    bot: &RuntimeBot,
    group_id: i64,
    user_id: i64,
) -> Result<UserInfo> {
    let api_fut = async {
        let resp = bot
            .get_group_member_info(group_id, user_id, false)
            .await
            .map_err(|e| {
                tracing::error!("调用 API get_group_member_info 失败: {}", e);
                anyhow::anyhow!("获取群成员信息失败")
            })?;
        parse_member_info(resp).await
    };
    let avatar_fut = get_avatar(user_id);

    let (member, avatar) = tokio::join!(api_fut, avatar_fut);

    let (uid, nickname) = member?;
    let avatar = avatar.unwrap_or_else(|e| {
        tracing::warn!("获取头像失败，user_id={}: {}", user_id, e);
        bytes::Bytes::new()
    });

    Ok(UserInfo {
        user_id: uid,
        nickname,
        avatar,
    })
}

async fn parse_member_info(resp: kovi::ApiReturn) -> Result<(i64, String)> {
    if resp.status != "ok" {
        anyhow::bail!("API请求失败: {:?}", resp);
    }

    let item = serde_json::from_value::<MemberInfoApiResponse>(resp.data)?;
    let nickname = if item.card.is_empty() {
        item.nickname
    } else {
        item.card
    };

    Ok((item.user_id, nickname))
}

async fn get_avatar(user_id: i64) -> Result<bytes::Bytes> {
    let avatar_url = format!("https://q4.qlogo.cn/headimg_dl?dst_uin={user_id}&spec=640");
    let resp = HTTP_CLIENT.get(&avatar_url).send().await?;
    let resp = resp.error_for_status()?;
    let bytes = resp.bytes().await?;
    Ok(bytes)
}
