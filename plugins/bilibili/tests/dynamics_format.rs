use bilibili::dynamics::{
    DynamicAuthor, DynamicItem, Pic, RichText, author_of, collect_pics, count_pics_total,
    format_body, push_url,
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

#[test]
fn collect_pics_caps_at_three() {
    let item = word_with_pics(5);
    assert_eq!(collect_pics(&item).len(), 3);
}

#[test]
fn collect_pics_empty_when_no_pics() {
    let item = DynamicItem::Word {
        id: "1".into(),
        text: "x".into(),
        pics: vec![],
        author: DynamicAuthor::default(),
    };
    assert!(collect_pics(&item).is_empty());
}

#[test]
fn push_url_routes_by_type() {
    assert_eq!(
        push_url(&DynamicItem::Video {
            id: "123".into(),
            bvid: "BV1abc".into(),
            title: String::new(),
            cover_url: String::new(),
            summary: None,
            author: DynamicAuthor::default(),
        }),
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
fn format_body_for_video_uses_tougao_template() {
    let author = DynamicAuthor {
        name: "小明".into(),
        ..DynamicAuthor::default()
    };
    let item = DynamicItem::Video {
        id: "987654321".into(),
        bvid: "BV1xx411c7mD".into(),
        title: "我的新视频".into(),
        cover_url: "https://i0.hdslb.com/bfs/archive/cover.jpg".into(),
        summary: Some(RichText {
            text: "这是视频简介".into(),
        }),
        author: author.clone(),
    };
    let body = format_body(&author, &item);
    assert_eq!(
        body,
        "小明 投稿了视频：我的新视频\n这是视频简介\nhttps://www.bilibili.com/video/BV1xx411c7mD"
    );
    assert_eq!(
        collect_pics(&item),
        vec!["https://i0.hdslb.com/bfs/archive/cover.jpg".to_string()]
    );
}

#[test]
fn format_body_for_video_without_summary_omits_blank_line() {
    let author = DynamicAuthor {
        name: "小明".into(),
        ..DynamicAuthor::default()
    };
    let item = DynamicItem::Video {
        id: "1".into(),
        bvid: "BV1abc".into(),
        title: "无简介视频".into(),
        cover_url: String::new(),
        summary: None,
        author: author.clone(),
    };
    assert_eq!(
        format_body(&author, &item),
        "小明 投稿了视频：无简介视频\nhttps://www.bilibili.com/video/BV1abc"
    );
}

#[test]
fn format_body_for_video_with_empty_title_falls_back_to_summary() {
    let author = DynamicAuthor {
        name: "小明".into(),
        ..DynamicAuthor::default()
    };
    let item = DynamicItem::Video {
        id: "1".into(),
        bvid: "BV1abc".into(),
        title: String::new(),
        cover_url: String::new(),
        summary: Some(RichText {
            text: "只有简介".into(),
        }),
        author: author.clone(),
    };
    assert_eq!(
        format_body(&author, &item),
        "小明\n只有简介\nhttps://www.bilibili.com/video/BV1abc"
    );
}

#[test]
fn format_body_for_video_with_empty_name_falls_back_to_pub_action() {
    let author = DynamicAuthor {
        name: String::new(),
        pub_action: "发布了视频".into(),
    };
    let item = DynamicItem::Video {
        id: "1".into(),
        bvid: "BV1abc".into(),
        title: "标题".into(),
        cover_url: String::new(),
        summary: None,
        author: author.clone(),
    };
    assert_eq!(
        format_body(&author, &item),
        "发布了视频 投稿了视频：标题\nhttps://www.bilibili.com/video/BV1abc"
    );
}

#[test]
fn format_body_for_video_with_all_empty_author_and_title_returns_only_url() {
    let author = DynamicAuthor::default();
    let item = DynamicItem::Video {
        id: "1".into(),
        bvid: "BV1abc".into(),
        title: String::new(),
        cover_url: String::new(),
        summary: None,
        author: author.clone(),
    };
    // 全空时不应出现 leading blank line / 裸冒号
    assert_eq!(
        format_body(&author, &item),
        "https://www.bilibili.com/video/BV1abc"
    );
}

#[test]
fn format_body_for_draw_with_all_empty_author_returns_summary_and_url_only() {
    let author = DynamicAuthor::default();
    let item = DynamicItem::Draw {
        id: "1".into(),
        pics: vec![],
        summary: Some(RichText {
            text: "正文".into(),
        }),
        author: author.clone(),
    };
    assert_eq!(
        format_body(&author, &item),
        "正文\nhttps://t.bilibili.com/1"
    );
}

#[test]
fn format_body_for_draw_with_no_text_shows_image_count() {
    let author = DynamicAuthor {
        name: "小明".into(),
        pub_action: "发布了图文动态".into(),
    };
    let item = DynamicItem::Draw {
        id: "222".into(),
        pics: (0..3)
            .map(|i| Pic {
                src: format!("https://i0.hdslb.com/pic{i}.jpg"),
            })
            .collect(),
        summary: None,
        author: author.clone(),
    };
    let body = format_body(&author, &item);
    assert_eq!(
        body,
        "小明 发布了图文动态\n（共 3 张图片）\nhttps://t.bilibili.com/222"
    );
}

#[test]
fn format_body_for_article_with_only_title() {
    let author = DynamicAuthor {
        name: "小明".into(),
        pub_action: "发布了专栏".into(),
    };
    let item = DynamicItem::Article {
        id: 100,
        title: "深入理解 Rust 所有权".into(),
        summary: RichText {
            text: String::new(),
        },
        covers: vec!["https://i0.hdslb.com/cover.jpg".into()],
        label: String::new(),
        author: author.clone(),
    };
    let body = format_body(&author, &item);
    assert_eq!(
        body,
        "小明 发布了专栏\n深入理解 Rust 所有权\nhttps://www.bilibili.com/read/cv100"
    );
}

#[test]
fn format_body_for_non_video_keeps_legacy_header_summary_url() {
    let author = DynamicAuthor {
        name: "小明".into(),
        pub_action: "发布了文字动态".into(),
    };
    let item = DynamicItem::Word {
        id: "111".into(),
        text: "纯文字内容".into(),
        pics: vec![],
        author: author.clone(),
    };
    let body = format_body(&author, &item);
    assert_eq!(
        body,
        "小明 发布了文字动态\n纯文字内容\nhttps://t.bilibili.com/111"
    );
}

#[test]
fn author_of_returns_per_item_author_for_all_variants() {
    let video_author = DynamicAuthor {
        name: "UP主A".into(),
        pub_action: String::new(),
    };
    let video = DynamicItem::Video {
        id: "1".into(),
        bvid: String::new(),
        title: String::new(),
        cover_url: String::new(),
        summary: None,
        author: video_author.clone(),
    };
    assert_eq!(author_of(&video).name, "UP主A");

    let word_author = DynamicAuthor {
        name: "UP主B".into(),
        pub_action: String::new(),
    };
    let word = DynamicItem::Word {
        id: "2".into(),
        text: "hi".into(),
        pics: vec![],
        author: word_author.clone(),
    };
    assert_eq!(author_of(&word).name, "UP主B");

    let article_author = DynamicAuthor {
        name: "UP主C".into(),
        pub_action: String::new(),
    };
    let article = DynamicItem::Article {
        id: 3,
        title: String::new(),
        summary: RichText {
            text: String::new(),
        },
        covers: vec![],
        label: String::new(),
        author: article_author.clone(),
    };
    assert_eq!(author_of(&article).name, "UP主C");

    let draw_author = DynamicAuthor {
        name: "UP主D".into(),
        pub_action: String::new(),
    };
    let draw = DynamicItem::Draw {
        id: "4".into(),
        pics: vec![],
        summary: None,
        author: draw_author.clone(),
    };
    assert_eq!(author_of(&draw).name, "UP主D");

    let live_author = DynamicAuthor {
        name: "UP主E".into(),
        pub_action: String::new(),
    };
    let live = DynamicItem::Live {
        id: 5,
        title: String::new(),
        cover_url: String::new(),
        room_id: 0,
        author: live_author.clone(),
    };
    assert_eq!(author_of(&live).name, "UP主E");
}

#[test]
fn count_pics_total_reports_full_count_for_all_variants() {
    let video = DynamicItem::Video {
        id: "1".into(),
        bvid: String::new(),
        title: String::new(),
        cover_url: String::new(),
        summary: None,
        author: DynamicAuthor::default(),
    };
    assert_eq!(count_pics_total(&video), 1);

    let live = DynamicItem::Live {
        id: 2,
        title: String::new(),
        cover_url: String::new(),
        room_id: 0,
        author: DynamicAuthor::default(),
    };
    assert_eq!(count_pics_total(&live), 1);

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
    // collect_pics 截断到 MAX_PICS_PER_PUSH（=3）
    assert_eq!(collect_pics(&opus).len(), 3);
}

#[test]
fn push_dynamic_body_appends_hint_when_pics_exceed_limit() {
    let author = DynamicAuthor {
        name: "小明".into(),
        pub_action: String::new(),
    };
    let item = DynamicItem::Opus {
        id: "555".into(),
        title: "九图动态".into(),
        summary: Some(RichText {
            text: "今天拍了九张".into(),
        }),
        pics: (0..9)
            .map(|i| format!("https://i0.hdslb.com/pic{i}.jpg"))
            .collect(),
        jump_url: "https://www.bilibili.com/opus/555".into(),
        author: author.clone(),
    };
    let total = count_pics_total(&item);
    let sent = collect_pics(&item);
    let body = format_body(&author, &item);
    let full = if total > sent.len() {
        format!("{}\n（还有 {} 张图片未显示）", body, total - sent.len())
    } else {
        body
    };
    assert_eq!(
        full,
        "小明\n九图动态\n今天拍了九张\nhttps://www.bilibili.com/opus/555\n（还有 6 张图片未显示）"
    );
}
