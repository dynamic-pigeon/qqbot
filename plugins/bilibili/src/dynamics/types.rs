use serde::Deserialize;

#[derive(Debug)]
pub enum DynamicItem {
    Video {
        id: String,
        bvid: String,
        title: String,
        cover_url: String,
        summary: Option<RichText>,
        author: DynamicAuthor,
    },
    Draw {
        id: String,
        pics: Vec<Pic>,
        summary: Option<RichText>,
        author: DynamicAuthor,
    },
    /// 新版 B 站动态主体结构（`MAJOR_TYPE_OPUS`）。DRAW/Word 等图文动态目前都用这个。
    Opus {
        id: String,
        title: String,
        summary: Option<RichText>,
        pics: Vec<String>,
        jump_url: String,
        author: DynamicAuthor,
    },
    Word {
        id: String,
        text: String,
        pics: Vec<Pic>,
        author: DynamicAuthor,
    },
    Article {
        id: i64,
        title: String,
        summary: RichText,
        covers: Vec<String>,
        label: String,
        author: DynamicAuthor,
    },
    Live {
        id: i64,
        title: String,
        cover_url: String,
        room_id: i64,
        author: DynamicAuthor,
    },
    /// B 站返回了不在已知类型表里的 major_type / dynamic_type（含转发动态）；
    /// 保留动态 id + 作者供排查，不再递归解析内部内容。
    Other {
        id: String,
        author: DynamicAuthor,
    },
}

#[derive(Debug, Default, Clone)]
pub struct DynamicAuthor {
    pub name: String,
    pub pub_action: String,
}

#[derive(Debug, Default, Clone)]
pub struct RichText {
    pub text: String,
}

#[derive(Debug)]
pub struct Pic {
    pub src: String,
}

pub struct DynamicsPage {
    pub items: Vec<DynamicItem>,
    pub has_more: bool,
    pub next_offset: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct ItemRaw {
    #[serde(default)]
    pub id_str: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub modules: ModulesRaw,
}

#[derive(Default, Deserialize)]
pub struct ModulesRaw {
    #[serde(default)]
    pub module_author: Option<AuthorRaw>,
    #[serde(default)]
    pub module_dynamic: Option<DynamicBodyRaw>,
}

#[derive(Default, Deserialize)]
pub struct AuthorRaw {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub pub_action: String,
}

#[derive(Default, Deserialize)]
pub struct DynamicBodyRaw {
    #[serde(default)]
    pub desc: Option<DescRaw>,
    #[serde(default)]
    pub major: Option<MajorRaw>,
}

#[derive(Default, Deserialize)]
pub struct DescRaw {
    #[serde(default)]
    pub text: String,
}

#[derive(Default, Deserialize)]
pub struct MajorRaw {
    #[serde(default)]
    pub archive: Option<ArchiveRaw>,
    #[serde(default)]
    pub draw: Option<DrawRaw>,
    #[serde(default)]
    pub article: Option<ArticleRaw>,
    #[serde(default)]
    pub live: Option<LiveRaw>,
    #[serde(default)]
    pub opus: Option<OpusRaw>,
}

#[derive(Default, Deserialize)]
pub struct OpusRaw {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: Option<DescRaw>,
    #[serde(default)]
    pub pics: Vec<OpusPicRaw>,
    #[serde(default)]
    pub jump_url: String,
}

#[derive(Default, Deserialize)]
pub struct OpusPicRaw {
    #[serde(default)]
    pub url: String,
}

#[derive(Default, Deserialize)]
pub struct ArchiveRaw {
    #[serde(default)]
    pub bvid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cover: String,
}

#[derive(Default, Deserialize)]
pub struct DrawRaw {
    #[serde(default)]
    pub items: Vec<PicRaw>,
}

#[derive(Default, Deserialize)]
pub struct PicRaw {
    #[serde(default)]
    pub src: String,
}

#[derive(Default, Deserialize)]
pub struct ArticleRaw {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub covers: Vec<String>,
}

#[derive(Default, Deserialize)]
pub struct LiveRaw {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub jump_url: String,
}