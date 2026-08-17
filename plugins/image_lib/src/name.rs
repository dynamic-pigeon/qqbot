use utils::command::CommandError;

pub const MAX_NAME_CHARS: usize = 32;

pub fn parse_library_name(name: &str) -> Result<&str, CommandError> {
    if name.is_empty() {
        return Err(CommandError::MissingArgument {
            name: "库名".to_owned(),
        });
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(CommandError::user(format!(
            "库名不能超过 {MAX_NAME_CHARS} 个字符"
        )));
    }
    if name == "." || name == ".." {
        return Err(CommandError::user("库名包含非法字符"));
    }
    if name.chars().any(is_forbidden_name_char) {
        return Err(CommandError::user("库名包含非法字符"));
    }
    Ok(name)
}

fn is_forbidden_name_char(ch: char) -> bool {
    // 库名只进 JSON 键，但仍拒绝路径和控制/格式字符，避免「看起来一样」的同名库。
    ch.is_control()
        || matches!(
            ch,
            '/' | '\\'
                | '\0'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_cjk_and_ascii() {
        assert_eq!(parse_library_name("猫").unwrap(), "猫");
        assert_eq!(parse_library_name("Cat").unwrap(), "Cat");
    }

    #[test]
    fn rejects_empty_too_long_and_path_chars() {
        assert!(matches!(
            parse_library_name(""),
            Err(CommandError::MissingArgument { .. })
        ));
        let long = "喵".repeat(MAX_NAME_CHARS + 1);
        assert!(parse_library_name(&long).is_err());
        assert!(parse_library_name("a/b").is_err());
        assert!(parse_library_name("a\\b").is_err());
        assert!(parse_library_name("a\nb").is_err());
        assert!(parse_library_name("..").is_err());
        assert!(parse_library_name("猫\u{200B}").is_err());
    }

    #[test]
    fn names_are_case_sensitive() {
        assert_eq!(parse_library_name("Cat").unwrap(), "Cat");
        assert_ne!(parse_library_name("Cat").unwrap(), "cat");
    }
}
