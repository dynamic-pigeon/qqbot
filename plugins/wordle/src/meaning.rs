//! 单词中文释义表。
//!
//! 内置数据覆盖官方 2315 个答案词，由 ECDICT（skywind3000/ECDICT，MIT）生成，
//! 编译期嵌入二进制；`data/wordle/meanings.txt` 可覆盖或补充（自定义词库场景），
//! 两处格式相同：每行 `word<TAB>释义`。

use std::collections::HashMap;

/// 内置释义数据，编译期嵌入，运行时离线可用。
const EMBEDDED_MEANINGS: &str = include_str!("../assets/meanings.txt");

/// 解析释义表文本：每行 `word<TAB>释义`，词或释义为空的行跳过。
pub fn parse(content: &str) -> HashMap<String, String> {
    content
        .lines()
        .filter_map(|line| {
            let (word, meaning) = line.split_once('\t')?;
            let meaning = meaning.trim();
            if word.is_empty() || meaning.is_empty() {
                return None;
            }
            Some((word.to_owned(), meaning.to_owned()))
        })
        .collect()
}

/// 内置表的解析结果。
pub fn embedded() -> HashMap<String, String> {
    parse(EMBEDDED_MEANINGS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_tab_separated_lines() {
        let map = parse("crane\tn. 鹤, 起重机\nperky\t a. 快活的 \n");
        assert_eq!(map["crane"], "n. 鹤, 起重机");
        assert_eq!(map["perky"], "a. 快活的", "释义两侧空白被去除");
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let map =
            parse("没有分隔符\n\t释义为空\nword\t释义\nword2\t释义2\nword2\t后出现的同词覆盖前者");
        assert_eq!(map.len(), 2, "无 TAB 与空释义的行跳过");
        assert_eq!(map["word2"], "后出现的同词覆盖前者");
    }

    #[test]
    fn embedded_table_is_well_formed() {
        let map = embedded();
        assert!(!map.is_empty(), "内置释义表不能为空");
        for (word, meaning) in &map {
            assert!(
                word.len() == 5 && word.bytes().all(|b| b.is_ascii_lowercase()),
                "key 应为小写 5 字母词: {word:?}"
            );
            assert!(
                !meaning.contains(['\t', '\n']),
                "释义不能含制表符或换行: {word:?}"
            );
        }
    }
}
