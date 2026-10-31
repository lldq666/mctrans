//! SQLite 缓存与运行状态。
//!
//! 设计要点：
//! - 内容寻址：缓存键 = sha256(src_lang \x1f tgt_lang \x1f 原文)，与包/文件路径无关，
//!   同一字符串在多个 jar 里只译一次；
//! - 单写者：`Mutex<Connection>` + WAL + busy_timeout，跨进程用 fs2 建议锁快速失败；
//! - 断点续跑：runs / file_states 记录每个文件的处理状态；
//! - 术语表：glossary_terms，供管线后处理与送译前注入。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{McError, McResult};

const SCHEMA_VERSION: i64 = 1;

pub struct Cache {
    conn: Mutex<Connection>,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CacheHit {
    pub translated: String,
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRecord {
    pub src_lang: String,
    pub tgt_lang: String,
    pub content: String,
    pub translated: String,
    pub provider: String,
    pub model: String,
    pub placeholder_ok: bool,
}

pub fn content_hash(src_lang: &str, tgt_lang: &str, content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(src_lang.as_bytes());
    h.update(b"\x1f");
    h.update(tgt_lang.as_bytes());
    h.update(b"\x1f");
    h.update(content.as_bytes());
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

impl Cache {
    /// 打开（或创建）数据库并执行迁移。
    pub fn open(path: &Path) -> McResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| McError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
        }
        // 跨进程建议锁：第二个进程快速失败，避免并发写冲突。
        let lock_path = path.with_extension("db.mctrans.lock");
        if lock_path.exists() {
            // 简单惰锁：存在且内容对应本机 pid 之外一律视为占用
            let content = std::fs::read_to_string(&lock_path).unwrap_or_default();
            if !content.trim().is_empty() {
                return Err(McError::OutputLocked { path: lock_path });
            }
        }
        std::fs::write(&lock_path, "").ok();

        let conn = Connection::open(path).map_err(McError::Sql)?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000i64).ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    fn migrate(conn: &Connection) -> McResult<()> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        conn.execute_batch(include_str!("schema.sql"))
            .map_err(|e| McError::Config(format!("数据库迁移失败: {e}")))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION).ok();
        Ok(())
    }

    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> McResult<T>) -> McResult<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    /// 查询缓存。preferred 为当前选中渠道 (provider, model) 时优先取它。
    pub fn get(
        &self,
        src_lang: &str,
        tgt_lang: &str,
        content: &str,
        preferred: Option<(&str, &str)>,
    ) -> McResult<Option<CacheHit>> {
        let sha = content_hash(src_lang, tgt_lang, content);
        self.with_conn(|c| {
            let source_id: Option<i64> = c
                .query_row(
                    "SELECT id FROM source_texts WHERE src_lang=?1 AND tgt_lang=?2 AND content_sha=?3",
                    params![src_lang, tgt_lang, sha],
                    |r| r.get(0),
                )
                .optional()
                .map_err(McError::Sql)?;
            let source_id = match source_id {
                Some(id) => id,
                None => return Ok(None),
            };
            let pick = |sql: &str, extra: Vec<Box<dyn rusqlite::ToSql>>| -> McResult<Option<CacheHit>> {
                let mut stmt = c.prepare(sql).map_err(McError::Sql)?;
                let mut params_vec: Vec<&dyn rusqlite::ToSql> = vec![&source_id];
                for b in &extra {
                    params_vec.push(b.as_ref());
                }
                let mut rows = stmt.query(params_vec.as_slice()).map_err(McError::Sql)?;
                if let Some(row) = rows.next().map_err(McError::Sql)? {
                    return Ok(Some(CacheHit {
                        translated: row.get(0).map_err(McError::Sql)?,
                        provider: row.get(1).map_err(McError::Sql)?,
                        model: row.get(2).map_err(McError::Sql)?,
                    }));
                }
                Ok(None)
            };
            if let Some((p, m)) = preferred {
                let hit = pick(
                    "SELECT translated, provider, model FROM translations \
                     WHERE source_id=?1 AND provider=?2 AND model=?3",
                    vec![Box::new(p.to_string()), Box::new(m.to_string())],
                )?;
                if hit.is_some() {
                    return Ok(hit);
                }
            }
            pick(
                "SELECT translated, provider, model FROM translations \
                 WHERE source_id=?1 AND placeholder_ok=1 ORDER BY id DESC LIMIT 1",
                vec![],
            )
        })
    }

    /// 写入缓存（幂等，同一 provider+model 覆盖）。
    pub fn put(
        &self,
        src_lang: &str,
        tgt_lang: &str,
        content: &str,
        translated: &str,
        provider: &str,
        model: &str,
        placeholder_ok: bool,
    ) -> McResult<()> {
        let sha = content_hash(src_lang, tgt_lang, content);
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO source_texts (src_lang, tgt_lang, content, content_sha, content_len) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(src_lang, tgt_lang, content_sha) DO NOTHING",
                params![src_lang, tgt_lang, content, sha, content.chars().count() as i64],
            )
            .map_err(McError::Sql)?;
            let source_id: i64 = c
                .query_row(
                    "SELECT id FROM source_texts WHERE src_lang=?1 AND tgt_lang=?2 AND content_sha=?3",
                    params![src_lang, tgt_lang, sha],
                    |r| r.get(0),
                )
                .map_err(McError::Sql)?;
            c.execute(
                "INSERT INTO translations (source_id, provider, model, translated, placeholder_ok) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(source_id, provider, model) DO UPDATE SET \
                 translated=excluded.translated, placeholder_ok=excluded.placeholder_ok",
                params![source_id, provider, model, translated, placeholder_ok as i64],
            )
            .map_err(McError::Sql)?;
            Ok(())
        })
    }

    // ---- runs / file_states：断点续跑 ----

    pub fn new_run(&self, tgt_lang: &str, mode: &str, pack_name: &str) -> McResult<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO runs (tgt_lang, mode, pack_name, status) VALUES (?1, ?2, ?3, 'running')",
                params![tgt_lang, mode, pack_name],
            )
            .map_err(McError::Sql)?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn finish_run(&self, run_id: i64, status: &str, stats_json: &str) -> McResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET status=?2, finished_at=datetime('now'), stats_json=?3 WHERE id=?1",
                params![run_id, status, stats_json],
            )
            .map_err(McError::Sql)?;
            Ok(())
        })
    }

    pub fn set_file_state(
        &self,
        run_id: i64,
        container: &str,
        kind: &str,
        encoding: Option<&str>,
        status: &str,
        units_total: i64,
        units_done: i64,
        error: Option<&str>,
    ) -> McResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO file_states (run_id, container, kind, encoding, status, units_total, units_done, error) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(run_id, container) DO UPDATE SET \
                 status=excluded.status, units_total=excluded.units_total, \
                 units_done=excluded.units_done, error=excluded.error, \
                 updated_at=datetime('now')",
                params![run_id, container, kind, encoding, status, units_total, units_done, error],
            )
            .map_err(McError::Sql)?;
            Ok(())
        })
    }

    pub fn done_files(&self, run_id: i64) -> McResult<std::collections::HashSet<String>> {
        self.with_conn(|c| {
            let mut stmt = c
                .prepare("SELECT container FROM file_states WHERE run_id=?1 AND status='done'")
                .map_err(McError::Sql)?;
            let rows = stmt
                .query_map(params![run_id], |r| r.get::<_, String>(0))
                .map_err(McError::Sql)?;
            let mut set = std::collections::HashSet::new();
            for r in rows {
                set.insert(r.map_err(McError::Sql)?);
            }
            Ok(set)
        })
    }

    /// resume 用：某个整合包最近一次 run 中已完成的文件集合。
    pub fn latest_run_done_files(&self, pack_name: &str) -> McResult<std::collections::HashSet<String>> {
        self.with_conn(|c| {
            let run_id: Option<i64> = c
                .query_row(
                    "SELECT id FROM runs WHERE pack_name=?1 ORDER BY id DESC LIMIT 1",
                    params![pack_name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(McError::Sql)?;
            match run_id {
                Some(rid) => {
                    let mut stmt = c
                        .prepare("SELECT container FROM file_states WHERE run_id=?1 AND status='done'")
                        .map_err(McError::Sql)?;
                    let rows = stmt
                        .query_map(params![rid], |r| r.get::<_, String>(0))
                        .map_err(McError::Sql)?;
                    let mut set = std::collections::HashSet::new();
                    for r in rows {
                        set.insert(r.map_err(McError::Sql)?);
                    }
                    Ok(set)
                }
                None => Ok(std::collections::HashSet::new()),
            }
        })
    }

    // ---- 术语表 ----

    pub fn glossary_list(&self) -> McResult<Vec<(String, String)>> {
        self.with_conn(|c| {
            let mut stmt = c
                .prepare("SELECT term_src, term_tgt FROM glossary_terms WHERE enabled=1 ORDER BY id")
                .map_err(McError::Sql)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(McError::Sql)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(McError::Sql)?);
            }
            Ok(out)
        })
    }

    pub fn glossary_upsert(&self, src: &str, tgt: &str) -> McResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO glossary_terms (term_src, term_tgt) VALUES (?1, ?2) \
                 ON CONFLICT(term_src, term_tgt) DO NOTHING",
                params![src, tgt],
            )
            .map_err(McError::Sql)?;
            Ok(())
        })
    }

    pub fn glossary_delete(&self, src: &str, tgt: &str) -> McResult<bool> {
        self.with_conn(|c| {
            let n = c
                .execute(
                    "DELETE FROM glossary_terms WHERE term_src=?1 AND term_tgt=?2",
                    params![src, tgt],
                )
                .map_err(McError::Sql)?;
            Ok(n > 0)
        })
    }

    // ---- 统计 / 导入导出 / 清理 ----

    pub fn stats(&self) -> McResult<CacheStats> {
        self.with_conn(|c| {
            let sources: i64 = c
                .query_row("SELECT COUNT(*) FROM source_texts", [], |r| r.get(0))
                .map_err(McError::Sql)?;
            let translations: i64 = c
                .query_row("SELECT COUNT(*) FROM translations", [], |r| r.get(0))
                .map_err(McError::Sql)?;
            let glossary: i64 = c
                .query_row("SELECT COUNT(*) FROM glossary_terms", [], |r| r.get(0))
                .map_err(McError::Sql)?;
            let runs: i64 = c
                .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
                .map_err(McError::Sql)?;
            let by_provider: Vec<(String, String, i64)> = {
                let mut stmt = c
                    .prepare("SELECT provider, model, COUNT(*) FROM translations GROUP BY provider, model")
                    .map_err(McError::Sql)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .map_err(McError::Sql)?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r.map_err(McError::Sql)?);
                }
                v
            };
            Ok(CacheStats {
                sources,
                translations,
                glossary,
                runs,
                by_provider,
            })
        })
    }

    pub fn export_jsonl(&self) -> McResult<Vec<ExportRecord>> {
        self.with_conn(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT s.src_lang, s.tgt_lang, s.content, t.translated, t.provider, t.model, t.placeholder_ok \
                     FROM translations t JOIN source_texts s ON s.id = t.source_id ORDER BY t.id",
                )
                .map_err(McError::Sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(ExportRecord {
                        src_lang: r.get(0)?,
                        tgt_lang: r.get(1)?,
                        content: r.get(2)?,
                        translated: r.get(3)?,
                        provider: r.get(4)?,
                        model: r.get(5)?,
                        placeholder_ok: r.get::<_, i64>(6)? != 0,
                    })
                })
                .map_err(McError::Sql)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(McError::Sql)?);
            }
            Ok(out)
        })
    }

    pub fn import_jsonl(&self, records: &[ExportRecord]) -> McResult<usize> {
        let mut n = 0usize;
        for r in records {
            self.put(
                &r.src_lang,
                &r.tgt_lang,
                &r.content,
                &r.translated,
                &r.provider,
                &r.model,
                r.placeholder_ok,
            )?;
            n += 1;
        }
        Ok(n)
    }

    /// 清空缓存。scope: "translations" 仅译文；"all" 含 runs/file_states/术语表。
    pub fn clear(&self, scope: &str) -> McResult<()> {
        self.with_conn(|c| {
            c.execute_batch("DELETE FROM translations; DELETE FROM source_texts;")
                .map_err(McError::Sql)?;
            if scope == "all" {
                c.execute_batch("DELETE FROM file_states; DELETE FROM runs; DELETE FROM glossary_terms; DELETE FROM provider_stats;")
                    .map_err(McError::Sql)?;
            }
            Ok(())
        })
    }

    /// 完整性自检。
    pub fn integrity_check(&self) -> McResult<()> {
        self.with_conn(|c| {
            let ok: String = c
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .map_err(McError::Sql)?;
            if ok != "ok" {
                return Err(McError::Other(format!("数据库完整性检查失败: {ok}")));
            }
            Ok(())
        })
    }
}

#[derive(Debug, Default)]
pub struct CacheStats {
    pub sources: i64,
    pub translations: i64,
    pub glossary: i64,
    pub runs: i64,
    pub by_provider: Vec<(String, String, i64)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mctrans-cache-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("test.db")
    }

    #[test]
    fn put_get_roundtrip() {
        let db = tmp_db("roundtrip");
        let c = Cache::open(&db).unwrap();
        c.put("en_us", "zh_cn", "Dirt", "泥土", "deepl", "deepl-v2", true)
            .unwrap();
        let hit = c.get("en_us", "zh_cn", "Dirt", None).unwrap().unwrap();
        assert_eq!(hit.translated, "泥土");
        // 大小写/内容敏感
        assert!(c.get("en_us", "zh_cn", "dirt", None).unwrap().is_none());
        assert!(c.get("en_us", "zh_tw", "Dirt", None).unwrap().is_none());
        // 覆盖写入
        c.put("en_us", "zh_cn", "Dirt", "泥土块", "gpt", "x", true).unwrap();
        let hit = c.get("en_us", "zh_cn", "Dirt", None).unwrap().unwrap();
        assert!(matches!(hit.translated.as_str(), "泥土" | "泥土块"));
        let st = c.stats().unwrap();
        assert_eq!(st.sources, 1);
        assert_eq!(st.translations, 2);
    }

    #[test]
    fn preferred_provider_first() {
        let db = tmp_db("preferred");
        let c = Cache::open(&db).unwrap();
        c.put("en_us", "zh_cn", "A", "甲-by-deepl", "deepl", "deepl-v2", true)
            .unwrap();
        c.put("en_us", "zh_cn", "A", "甲-by-gpt", "gpt", "m", true)
            .unwrap();
        let hit = c.get("en_us", "zh_cn", "A", Some(("gpt", "m"))).unwrap().unwrap();
        assert_eq!(hit.translated, "甲-by-gpt");
    }

    #[test]
    fn run_and_file_states() {
        let db = tmp_db("runs");
        let c = Cache::open(&db).unwrap();
        let rid = c.new_run("zh_cn", "zh_cn", "pack.zip").unwrap();
        c.set_file_state(rid, "mods/a.jar", "json", Some("utf-8"), "done", 10, 10, None)
            .unwrap();
        c.set_file_state(rid, "mods/b.jar", "json", None, "failed", 3, 1, Some("boom"))
            .unwrap();
        let done = c.done_files(rid).unwrap();
        assert!(done.contains("mods/a.jar"));
        assert!(!done.contains("mods/b.jar"));
        c.finish_run(rid, "done", "{}").unwrap();
    }

    #[test]
    fn glossary_crud() {
        let db = tmp_db("glossary");
        let c = Cache::open(&db).unwrap();
        c.glossary_upsert("Efficiency", "效率").unwrap();
        c.glossary_upsert("Efficiency", "效率").unwrap();
        assert_eq!(c.glossary_list().unwrap().len(), 1);
        assert!(c.glossary_delete("Efficiency", "效率").unwrap());
        assert!(c.glossary_list().unwrap().is_empty());
    }

    #[test]
    fn export_import_clear() {
        let db1 = tmp_db("exp1");
        let db2 = tmp_db("exp2");
        let c1 = Cache::open(&db1).unwrap();
        c1.put("en_us", "zh_cn", "X", "叉", "deepl", "v2", true).unwrap();
        let data = c1.export_jsonl().unwrap();
        assert_eq!(data.len(), 1);
        let c2 = Cache::open(&db2).unwrap();
        c2.import_jsonl(&data).unwrap();
        assert_eq!(c2.export_jsonl().unwrap().len(), 1);
        c2.clear("all").unwrap();
        assert!(c2.export_jsonl().unwrap().is_empty());
        assert_eq!(c2.stats().unwrap().sources, 0);
    }

    #[test]
    fn hash_stability() {
        let a = content_hash("en_us", "zh_cn", "same");
        let b = content_hash("en_us", "zh_cn", "same");
        assert_eq!(a, b);
        assert_ne!(a, content_hash("en_us", "zh_cn", "other"));
    }
}
