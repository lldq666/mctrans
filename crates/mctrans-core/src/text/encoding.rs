//! 文件编码探测与保形写回：BOM → UTF-8 严格 → GB18030（GBK 超集）→ Big5。
//! JSON/SNBT 要求 UTF-8；.lang 允许 GBK/Big5，写回时保持原编码。

use crate::error::{McError, McResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enc {
    Utf8,
    Gb18030,
    Big5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bom {
    None,
    Utf8,
}

#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    pub enc: Enc,
    pub bom: Bom,
}

/// 解码文件字节。`utf8_only` 为 true 时（JSON/SNBT）非 UTF-8 直接报错。
pub fn decode(bytes: &[u8], utf8_only: bool) -> McResult<Decoded> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let s = std::str::from_utf8(&bytes[3..]).map_err(|e| McError::Decode {
            path: "data".into(),
            msg: format!("UTF-8 BOM 文件解码失败: {e}"),
        })?;
        return Ok(Decoded {
            text: s.to_string(),
            enc: Enc::Utf8,
            bom: Bom::Utf8,
        });
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(McError::Decode {
            path: "data".into(),
            msg: "检测到 UTF-16 编码，Minecraft 语言文件不支持".into(),
        });
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(Decoded {
            text: s.to_string(),
            enc: Enc::Utf8,
            bom: Bom::None,
        });
    }
    if utf8_only {
        return Err(McError::Decode {
            path: "data".into(),
            msg: "不是 UTF-8 编码（该格式强制要求 UTF-8）".into(),
        });
    }
    for enc in [Enc::Gb18030, Enc::Big5] {
        let label = match enc {
            Enc::Gb18030 => "GB18030",
            Enc::Big5 => "Big5",
            Enc::Utf8 => unreachable!(),
        };
        let (cow, _, had_errors) = encoding_rs::Encoding::for_label(label.as_bytes())
            .unwrap()
            .decode(bytes);
        if !had_errors {
            return Ok(Decoded {
                text: cow.into_owned(),
                enc,
                bom: Bom::None,
            });
        }
    }
    Err(McError::Decode {
        path: "data".into(),
        msg: "无法按 UTF-8/GBK/Big5 解码".into(),
    })
}

/// 按原编码写回字节。
pub fn encode(text: &str, enc: Enc, bom: Bom) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 3);
    if bom == Bom::Utf8 {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    match enc {
        Enc::Utf8 => out.extend_from_slice(text.as_bytes()),
        Enc::Gb18030 | Enc::Big5 => {
            let label = if enc == Enc::Gb18030 { "GB18030" } else { "Big5" };
            let e = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap();
            let (cow, _, _) = e.encode(text);
            out.extend_from_slice(&cow);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_with_bom() {
        let bytes = [0xEF, 0xBB, 0xBF].iter().copied().chain("你好".bytes()).collect::<Vec<_>>();
        let d = decode(&bytes, false).unwrap();
        assert_eq!(d.text, "你好");
        assert_eq!(d.bom, Bom::Utf8);
        let back = encode(&d.text, d.enc, d.bom);
        assert_eq!(back, bytes);
    }

    #[test]
    fn gbk_detected_and_preserved() {
        // "中文" 的 GBK 编码
        let (cow, _, _) = encoding_rs::Encoding::for_label(b"GB18030")
            .unwrap()
            .encode("泥土");
        let bytes = cow.into_owned();
        let d = decode(&bytes, false).unwrap();
        assert_eq!(d.text, "泥土");
        assert!(matches!(d.enc, Enc::Gb18030));
        let back = encode(&d.text, d.enc, d.bom);
        assert_eq!(back, bytes);
    }

    #[test]
    fn utf8_only_rejects_gbk() {
        let (cow, _, _) = encoding_rs::Encoding::for_label(b"GB18030")
            .unwrap()
            .encode("泥土");
        assert!(decode(&cow, true).is_err());
    }

    #[test]
    fn plain_ascii() {
        let d = decode(b"hello", false).unwrap();
        assert_eq!(d.text, "hello");
        assert_eq!(d.enc, Enc::Utf8);
    }
}
