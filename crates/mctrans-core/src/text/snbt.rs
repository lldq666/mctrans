//! SNBT 结构编辑器（FTB Quests 的 quests/lang 数据格式）。
//! 两种模式：
//! - `Map`：lang 映射表，顶层 compound 的字符串值全部可译（含嵌套）。
//! - `Fields`：chapter/task 数据，仅白名单 key（title/subtitle/description/text…）下的字符串值可译。
//! 保形改写：只重写目标字符串的字符区间，其余字节不动。

use std::collections::HashMap;

use crate::error::{McError, McResult};

#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// 以 "/" 分隔的键路径，如 `title`、`quests/0/title`。
    pub pointer: String,
    /// 解码后的字符串值。
    pub value: String,
}

#[derive(Debug, Clone)]
pub enum SnbtMode {
    /// FTB Quests lang 表：全部字符串值。
    Map,
    /// 章节/任务数据：仅这些 key 下的字符串值。
    Fields(Vec<String>),
}

struct Span {
    pointer: String,
    start: usize,
    end: usize,
    quote: u8,
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn err(&self, msg: &str) -> McError {
        McError::Decode {
            path: "snbt".into(),
            msg: format!("偏移 {}: {msg}", self.i),
        }
    }

    fn scan(&mut self, mode: &SnbtMode, out: &mut Vec<Span>) -> McResult<()> {
        self.ws();
        if self.peek().is_none() {
            return Ok(());
        }
        self.value(String::new(), mode, out)?;
        Ok(())
    }

    fn value(&mut self, ptr: String, mode: &SnbtMode, out: &mut Vec<Span>) -> McResult<()> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(ptr, mode, out),
            Some(b'[') => self.array(ptr, mode, out),
            Some(b'"') | Some(b'\'') => {
                let s = self.string()?;
                if mode_allows(&ptr, mode) {
                    out.push(Span {
                        pointer: ptr,
                        start: s.0,
                        end: s.1,
                        quote: s.2,
                    });
                }
                Ok(())
            }
            Some(c) if c == b'-' || c == b'+' || c.is_ascii_digit() || c == b'.' => self.number(),
            Some(b't') => self.lit("true"),
            Some(b'f') => self.lit("false"),
            _ => Err(self.err("不是合法的 SNBT 值")),
        }
    }

    fn number(&mut self) -> McResult<()> {
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') {
                self.i += 1;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn lit(&mut self, word: &str) -> McResult<()> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(())
        } else {
            Err(self.err(&format!("不是 {word}")))
        }
    }

    fn object(&mut self, ptr: String, mode: &SnbtMode, out: &mut Vec<Span>) -> McResult<()> {
        self.i += 1; // {
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            self.ws();
            let key = self.key()?;
            let child = if ptr.is_empty() {
                key
            } else {
                format!("{ptr}/{key}")
            };
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("键后缺少 ':'"));
            }
            self.i += 1;
            self.value(child, mode, out)?;
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(self.err("compound 缺少 ',' 或 '}'")),
            }
        }
    }

    fn array(&mut self, ptr: String, mode: &SnbtMode, out: &mut Vec<Span>) -> McResult<()> {
        self.i += 1; // [
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(());
        }
        let mut idx = 0usize;
        loop {
            let child = if ptr.is_empty() {
                idx.to_string()
            } else {
                format!("{ptr}/{idx}")
            };
            self.value(child, mode, out)?;
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    idx += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(self.err("数组缺少 ',' 或 ']'")),
            }
        }
    }

    /// 键：带引号的字符串或裸标识符。
    fn key(&mut self) -> McResult<String> {
        match self.peek() {
            Some(b'"') | Some(b'\'') => {
                let (s, e, _) = self.string()?;
                let raw = self.slice(s, e)?;
                decode_snbt_string(raw)
            }
            _ => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-' | b'+') {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                if self.i == start {
                    return Err(self.err("compound 键非法"));
                }
                Ok(self.slice(start, self.i)?.to_string())
            }
        }
    }

    /// 扫描字符串字面量，返回 (内容起, 内容止, 引号字符)。
    fn string(&mut self) -> McResult<(usize, usize, u8)> {
        let quote = self.peek().ok_or_else(|| self.err("意外的文件结尾"))?;
        let start = self.i + 1;
        self.i += 1;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i += 2,
                c if c == quote => {
                    let end = self.i;
                    self.i += 1;
                    return Ok((start, end, quote));
                }
                _ => self.i += 1,
            }
        }
        Err(self.err("字符串未闭合"))
    }

    fn slice(&self, s: usize, e: usize) -> McResult<&'a str> {
        std::str::from_utf8(&self.b[s..e]).map_err(|_| self.err("字符串内容不是 UTF-8"))
    }
}

fn mode_allows(ptr: &str, mode: &SnbtMode) -> bool {
    match mode {
        SnbtMode::Map => true,
        SnbtMode::Fields(keys) => {
            let last = ptr.rsplit('/').next().unwrap_or(ptr);
            keys.iter().any(|k| k == last)
        }
    }
}

/// 解析 SNBT，取出可译字符串单元。
pub fn parse_units(src: &str, mode: &SnbtMode) -> McResult<Vec<Unit>> {
    let mut sc = Scanner::new(src);
    let mut spans = Vec::new();
    sc.scan(mode, &mut spans)?;
    sc.ws();
    if sc.i != src.len() {
        return Err(McError::Decode {
            path: "snbt".into(),
            msg: format!("根值之后还有多余内容（偏移 {}）", sc.i),
        });
    }
    let mut units = Vec::with_capacity(spans.len());
    for s in spans {
        let raw = std::str::from_utf8(&sc.b[s.start..s.end]).map_err(|_| {
            McError::Decode {
                path: "snbt".into(),
                msg: "字符串内容不是 UTF-8".into(),
            }
        })?;
        units.push(Unit {
            pointer: s.pointer,
            value: decode_snbt_string(raw)?,
        });
    }
    Ok(units)
}

/// 按 updates 重写目标字符串，其余字节不动。
pub fn rewrite(src: &str, updates: &[(String, String)], mode: &SnbtMode) -> McResult<String> {
    if updates.is_empty() {
        return Ok(src.to_string());
    }
    let mut sc = Scanner::new(src);
    let mut spans: Vec<Span> = Vec::new();
    sc.scan(&SnbtMode::Map, &mut spans)?; // 全量扫描以获得所有字符串区间
    sc.ws();
    if sc.i != src.len() {
        return Err(McError::Decode {
            path: "snbt".into(),
            msg: format!("根值之后还有多余内容（偏移 {}）", sc.i),
        });
    }
    let map: HashMap<&str, &str> = updates.iter().map(|(p, v)| (p.as_str(), v.as_str())).collect();
    let mut out = String::with_capacity(src.len() + 64);
    let mut last = 0usize;
    let mut applied = 0usize;
    for s in &spans {
        let new_v = match map.get(s.pointer.as_str()) {
            Some(v) => *v,
            None => continue,
        };
        out.push_str(&src[last..s.start]);
        out.push_str(&escape_snbt_content(new_v, s.quote as char));
        last = s.end;
        applied += 1;
    }
    out.push_str(&src[last..]);
    if applied != updates.len() {
        return Err(McError::Other(format!(
            "SNBT 重写时找不到目标指针（应用 {applied}/{}）",
            updates.len()
        )));
    }
    parse_units(&out, mode)?;
    Ok(out)
}

/// 解码 SNBT 字符串内容（已去引号）：\' \" \\ \n \r \t \uXXXX。
pub fn decode_snbt_string(raw: &str) -> McResult<String> {
    let b = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
            if i >= b.len() {
                return Err(dec_err("转义不完整"));
            }
            match b[i] {
                b'\'' => out.push('\''),
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let hex = raw.get(i + 1..i + 5).ok_or_else(|| dec_err("\\u 转义被截断"))?;
                    let cp = u32::from_str_radix(hex, 16).map_err(|_| dec_err("\\u 码点非法"))?;
                    out.push(char::from_u32(cp).ok_or_else(|| dec_err("\\u 码点非法"))?);
                    i += 4;
                }
                _ => return Err(dec_err("未知转义")),
            }
            i += 1;
        } else {
            let l = utf8_len(b[i]);
            let end = (i + l).min(b.len());
            out.push_str(std::str::from_utf8(&b[i..end]).map_err(|_| dec_err("非 UTF-8 字符"))?);
            i = end;
        }
    }
    Ok(out)
}

fn dec_err(msg: &str) -> McError {
    McError::Decode {
        path: "snbt".into(),
        msg: msg.to_string(),
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// 编码字符串内容为 SNBT 字面量内部（不含首尾引号，保持原引号风格）。
pub fn escape_snbt_content(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            c if c == quote => out.push_str(&format!("\\{c}")),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 编码字符串为 SNBT 字面量（含首尾引号）。
pub fn escape_snbt_value(s: &str, quote: char) -> String {
    format!("{quote}{}{quote}", escape_snbt_content(s, quote))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_mode_roundtrip() {
        let src = r#"{
  chapter1: {
    title: "Getting Started",
    desc: 'Read &6manual\nfirst'
  }
}"#;
        let units = parse_units(src, &SnbtMode::Map).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].pointer, "chapter1/title");
        assert_eq!(units[1].value, "Read &6manual\nfirst");
        let out = rewrite(src, &[], &SnbtMode::Map).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn fields_mode_whitelist() {
        let src = "{title: \"Chapter\", id: \"keepme\", subtitle: \"Sub\"}";
        let units = parse_units(
            src,
            &SnbtMode::Fields(vec!["title".into(), "subtitle".into()]),
        )
        .unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].pointer, "title");
        let out = rewrite(
            src,
            &[("title".into(), "章节".into())],
            &SnbtMode::Fields(vec!["title".into(), "subtitle".into()]),
        )
        .unwrap();
        assert_eq!(out, "{title: \"章节\", id: \"keepme\", subtitle: \"Sub\"}");
    }

    #[test]
    fn nested_lists_and_numbers() {
        let src = "{quests: [{id: 1b, tasks: [\"a\"]}, {x: -1.5f}]}";
        let units = parse_units(src, &SnbtMode::Map).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].pointer, "quests/0/tasks/0");
    }

    #[test]
    fn escape_preserved() {
        let src = "{a: 'it\\'s \"quoted\"'}";
        let units = parse_units(src, &SnbtMode::Map).unwrap();
        assert_eq!(units[0].value, "it's \"quoted\"");
        let out = rewrite(src, &[("a".into(), "它说 \"嗨\"".into())], &SnbtMode::Map).unwrap();
        let units2 = parse_units(&out, &SnbtMode::Map).unwrap();
        assert_eq!(units2[0].value, "它说 \"嗨\"");
    }

    #[test]
    fn invalid_rejected() {
        assert!(parse_units("{a: }", &SnbtMode::Map).is_err());
        assert!(parse_units("{a: 'x}", &SnbtMode::Map).is_err());
    }
}
