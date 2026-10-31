//! 文本处理层：保形结构编辑器 + 编码探测。

pub mod component;
pub mod encoding;
pub mod json_edit;
pub mod lang;
pub mod snbt;

pub use encoding::{decode, encode, Bom, Decoded, Enc};
