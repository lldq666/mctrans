-- mctrans SQLite schema v1
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS runs (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at    TEXT NOT NULL DEFAULT (datetime('now')),
  finished_at   TEXT,
  src_lang      TEXT NOT NULL DEFAULT 'en_us',
  tgt_lang      TEXT NOT NULL,
  mode          TEXT NOT NULL,
  pack_name     TEXT,
  status        TEXT NOT NULL DEFAULT 'running',
  stats_json    TEXT
);

CREATE TABLE IF NOT EXISTS file_states (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  container     TEXT NOT NULL,
  kind          TEXT NOT NULL,
  encoding      TEXT,
  status        TEXT NOT NULL DEFAULT 'pending',
  units_total   INTEGER NOT NULL DEFAULT 0,
  units_done    INTEGER NOT NULL DEFAULT 0,
  error         TEXT,
  updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(run_id, container)
);
CREATE INDEX IF NOT EXISTS idx_file_states_status ON file_states(run_id, status);

CREATE TABLE IF NOT EXISTS source_texts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  src_lang      TEXT NOT NULL,
  tgt_lang      TEXT NOT NULL,
  content       TEXT NOT NULL,
  content_sha   TEXT NOT NULL,
  content_len   INTEGER NOT NULL,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(src_lang, tgt_lang, content_sha)
);
CREATE INDEX IF NOT EXISTS idx_source_texts_sha ON source_texts(content_sha);

CREATE TABLE IF NOT EXISTS translations (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id     INTEGER NOT NULL REFERENCES source_texts(id) ON DELETE CASCADE,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  translated    TEXT NOT NULL,
  placeholder_ok INTEGER NOT NULL DEFAULT 1,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE(source_id, provider, model)
);
CREATE INDEX IF NOT EXISTS idx_translations_source ON translations(source_id);

CREATE TABLE IF NOT EXISTS glossary_terms (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  term_src      TEXT NOT NULL,
  term_tgt      TEXT NOT NULL,
  note          TEXT,
  case_sensitive INTEGER NOT NULL DEFAULT 0,
  enabled       INTEGER NOT NULL DEFAULT 1,
  UNIQUE(term_src, term_tgt)
);

CREATE TABLE IF NOT EXISTS provider_stats (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id        INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  requests      INTEGER NOT NULL DEFAULT 0,
  failures      INTEGER NOT NULL DEFAULT 0,
  retries       INTEGER NOT NULL DEFAULT 0,
  input_chars   INTEGER NOT NULL DEFAULT 0,
  output_chars  INTEGER NOT NULL DEFAULT 0,
  total_ms      INTEGER NOT NULL DEFAULT 0,
  UNIQUE(run_id, provider, model)
);
