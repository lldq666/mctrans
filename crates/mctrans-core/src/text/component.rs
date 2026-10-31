//! FTB Quests / MC 文本组件：字符串里嵌套的 JSON（{"text":"…","extra":[…]}）。
//! 只翻译 text 字段，其余键（color、hoverEvent、clickEvent 等）原样保留。

use super::json_edit::{self, Unit};

/// 判断字符串是否为 JSON 文本组件（对象或数组，且能 parse）。
pub fn is_component(s: &str) -> bool {
    let t = s.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    matches!(t.chars().last(), Some('}') | Some(']')) && json_edit::parse_units(t, &|_| true).is_ok()
}

fn accept_text(pointer: &str) -> bool {
    json_edit::last_segment(pointer) == "text"
}

/// 从组件中抽取待译 text 单元。
pub fn text_units(component: &str) -> Result<Vec<Unit>, String> {
    json_edit::parse_units(component, &accept_text).map_err(|e| e.to_string())
}

/// 按 updates 重写组件的 text 字段。
pub fn rewrite_texts(component: &str, updates: &[(String, String)]) -> Result<String, String> {
    json_edit::rewrite(component, updates, &accept_text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_and_extract() {
        let src = r#"{"text":"Hello ","color":"gold","extra":[{"text":"world"},{"text":"!"}]}"#;
        assert!(is_component(src));
        let units = text_units(src).unwrap();
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].pointer, "/text");
        assert_eq!(units[0].value, "Hello ");
        assert_eq!(units[2].pointer, "/extra/1/text");
    }

    #[test]
    fn rewrite_keeps_other_keys() {
        let src = r#"{"text":"Hello","color":"gold","hoverEvent":{"action":"show_text","value":"tip"}}"#;
        let out = rewrite_texts(src, &[("/text".into(), "你好".into())]).unwrap();
        assert_eq!(
            out,
            r#"{"text":"你好","color":"gold","hoverEvent":{"action":"show_text","value":"tip"}}"#
        );
    }

    #[test]
    fn plain_string_not_component() {
        assert!(!is_component("Hello world"));
        assert!(!is_component("{unclosed"));
        assert!(!is_component("123"));
    }

    #[test]
    fn array_component_lore() {
        let src = r#"[{"text":"line1"},{"text":"line2"}]"#;
        let units = text_units(src).unwrap();
        assert_eq!(units.len(), 2);
        let out = rewrite_texts(
            src,
            &[("/0/text".into(), "行一".into()), ("/1/text".into(), "行二".into())],
        )
        .unwrap();
        assert_eq!(out, r#"[{"text":"行一"},{"text":"行二"}]"#);
    }
}
