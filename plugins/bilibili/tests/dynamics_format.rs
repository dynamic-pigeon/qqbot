use bilibili::dynamics::{
    DynamicAuthor, DynamicItem, Pic, RichText, collect_pics, count_pics_total, format_body,
    push_url,
};

fn word_with_pics(n: usize) -> DynamicItem {
    DynamicItem::Word {
        id: "1".into(),
        text: "x".into(),
        pics: (0..n)
            .map(|i| Pic {
                src: format!("u{i}"),
            })
            .collect(),
        author: DynamicAuthor::default(),
    }
}

fn video(author: DynamicAuthor, title: &str, summary: Option<&str>, bvid: &str) -> DynamicItem {
    DynamicItem::Video {
        id: "1".into(),
        bvid: bvid.into(),
        title: title.into(),
        cover_url: String::new(),
        summary: summary.map(|text| RichText { text: text.into() }),
        author,
    }
}

#[test]
fn collect_pics_caps_and_empty() {
    assert_eq!(collect_pics(&word_with_pics(5)).len(), 3);
    assert!(collect_pics(&word_with_pics(0)).is_empty());
}

#[test]
fn push_url_routes_by_type() {
    assert_eq!(
        push_url(&video(DynamicAuthor::default(), "", None, "BV1abc")),
        "https://www.bilibili.com/video/BV1abc"
    );
    assert_eq!(
        push_url(&DynamicItem::Article {
            id: 999,
            title: String::new(),
            summary: RichText {
                text: String::new()
            },
            covers: vec![],
            label: String::new(),
            author: DynamicAuthor::default(),
        }),
        "https://www.bilibili.com/read/cv999"
    );
}

#[test]
fn format_body_for_video() {
    let named = DynamicAuthor {
        name: "小明".into(),
        ..DynamicAuthor::default()
    };
    let cases = [
        (
            named.clone(),
            "我的新视频",
            Some("这是视频简介"),
            "小明 投稿了视频：我的新视频\n这是视频简介\nhttps://www.bilibili.com/video/BV1abc",
        ),
        (
            named.clone(),
            "无简介视频",
            None,
            "小明 投稿了视频：无简介视频\nhttps://www.bilibili.com/video/BV1abc",
        ),
        (
            named,
            "",
            Some("只有简介"),
            "小明\n只有简介\nhttps://www.bilibili.com/video/BV1abc",
        ),
        (
            DynamicAuthor {
                name: String::new(),
                pub_action: "发布了视频".into(),
            },
            "标题",
            None,
            "发布了视频 投稿了视频：标题\nhttps://www.bilibili.com/video/BV1abc",
        ),
        (
            DynamicAuthor::default(),
            "",
            None,
            "https://www.bilibili.com/video/BV1abc",
        ),
    ];
    for (author, title, summary, expected) in cases {
        let item = video(author.clone(), title, summary, "BV1abc");
        assert_eq!(format_body(&author, &item), expected);
    }
}

#[test]
fn format_body_for_word_draw_article() {
    let empty = DynamicAuthor::default();
    let draw = DynamicItem::Draw {
        id: "1".into(),
        pics: vec![],
        summary: Some(RichText {
            text: "正文".into(),
        }),
        author: empty.clone(),
    };
    assert_eq!(format_body(&empty, &draw), "正文\nhttps://t.bilibili.com/1");

    let author = DynamicAuthor {
        name: "小明".into(),
        pub_action: "发布了图文动态".into(),
    };
    let draw_pics = DynamicItem::Draw {
        id: "222".into(),
        pics: (0..3)
            .map(|i| Pic {
                src: format!("https://i0.hdslb.com/pic{i}.jpg"),
            })
            .collect(),
        summary: None,
        author: author.clone(),
    };
    assert_eq!(
        format_body(&author, &draw_pics),
        "小明 发布了图文动态\n（共 3 张图片）\nhttps://t.bilibili.com/222"
    );

    let word_author = DynamicAuthor {
        name: "小明".into(),
        pub_action: "发布了文字动态".into(),
    };
    let word = DynamicItem::Word {
        id: "111".into(),
        text: "纯文字内容".into(),
        pics: vec![],
        author: word_author.clone(),
    };
    assert_eq!(
        format_body(&word_author, &word),
        "小明 发布了文字动态\n纯文字内容\nhttps://t.bilibili.com/111"
    );

    let article_author = DynamicAuthor {
        name: "小明".into(),
        pub_action: "发布了专栏".into(),
    };
    let article = DynamicItem::Article {
        id: 100,
        title: "深入理解 Rust 所有权".into(),
        summary: RichText {
            text: String::new(),
        },
        covers: vec!["https://i0.hdslb.com/cover.jpg".into()],
        label: String::new(),
        author: article_author.clone(),
    };
    assert_eq!(
        format_body(&article_author, &article),
        "小明 发布了专栏\n深入理解 Rust 所有权\nhttps://www.bilibili.com/read/cv100"
    );
}

#[test]
fn count_pics_total_is_uncapped_collect_pics_is_capped() {
    let video = DynamicItem::Video {
        id: "1".into(),
        bvid: String::new(),
        title: String::new(),
        cover_url: String::new(),
        summary: None,
        author: DynamicAuthor::default(),
    };
    assert_eq!(count_pics_total(&video), 1);

    let opus = DynamicItem::Opus {
        id: "3".into(),
        title: String::new(),
        summary: None,
        pics: (0..9)
            .map(|i| format!("https://i0.hdslb.com/pic{i}.jpg"))
            .collect(),
        jump_url: String::new(),
        author: DynamicAuthor::default(),
    };
    assert_eq!(count_pics_total(&opus), 9);
    assert_eq!(collect_pics(&opus).len(), 3);
}
