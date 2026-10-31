# 配置说明

配置文件查找顺序（先命中先用）：

1. 命令行 `--config <路径>`
2. 当前目录的 `mctrans.toml`
3. `%APPDATA%/mctrans/mctrans.toml`（Linux/macOS 为 `$XDG_CONFIG_HOME` 或 `~/.config/mctrans/mctrans.toml`）

`mctrans config init` 会生成一份带注释的模板（内容同 `crates/mctrans-cli/src/mctrans.toml.example`）。

---

## [general]

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `src_lang` | string | `en_us` | 源语言代码（只影响语言对参数，不影响文件匹配） |
| `target_lang` | string | `zh_cn` | 目标语言；也是新增语言文件的名字（`en_us.json` → `zh_cn.json`） |
| `mode` | string | `zh_cn` | `zh_cn` = 新增中文语言文件，原文保留；`inplace` = 原地替换原语言文件 |
| `concurrency` | int | `8` | 并发批次数。渠道有严格限流时调小 |
| `db_path` | string | `auto` | SQLite 路径；`auto` = `%APPDATA%/mctrans/translations.db` |
| `cache` | bool | `true` | 是否启用缓存 |
| `active_provider` | string | 无 | **当前选中的渠道**。由 `config use` / `--provider` 自动写入，一般不用手改 |

`active_provider` 的行为：写入后立即生效，之后所有 run 默认用这个渠道；翻译时它排在回退链最前面，失败才回退。

---

## [providers.<名字>]

渠道名即 `<名字>`，用于 `--provider`、`config use`、以及 `[fallback].chain`。

通用字段：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `type` | `deepl` \| `openai` | 必填 | 协议类型 |
| `base_url` | string | 必填 | 服务地址，不带结尾斜杠 |
| `api_key_env` | string | 按类型默认 | 从哪个环境变量读 Key |
| `api_key` | string | 无 | 明文 Key。仅本地调试用，别提交到 git |
| `model` | string | `openai` 必填 | 模型名 |
| `max_batch_texts` | int | 按类型默认 | 单批最多文本条数 |

### DeepL（`type = "deepl"`）

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `base_url` | string | — | `https://api.deepl.com`（付费）或 `https://api-free.deepl.com`（免费版） |
| `formality` | string | `default` | `default` / `more` / `less` |
| `api_key_env` | string | `DEEPL_API_KEY` | Key 环境变量 |

- 语言代码映射：`en_us` → `EN`，`zh_cn` → `ZH`，`zh_tw` → `ZH-HANT`
- 单请求最多 50 条文本、payload 上限 128 KiB，工具内部会自动分批
- 免费版额度用尽会返回 456，此时直接切下一个渠道（不重试）

### OpenAI 兼容（`type = "openai"`）

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `base_url` | string | — | 如 `https://api.deepseek.com/v1`、`https://api.openai.com/v1`、`http://127.0.0.1:11434/v1` |
| `model` | string | `gpt-4o-mini` | 模型名 |
| `temperature` | float | `0.0` | 翻译建议保持 0 |
| `structured_output` | bool | `false` | 提供方支持 `response_format=json_object` 时打开；不支持就关掉，工具会用宽松解析兜底 |
| `api_key_env` | string | `OPENAI_API_KEY` | 本地 Ollama 之类不需要 Key，留空即可 |

请求体是标准 `POST {base_url}/chat/completions`，system 提示里写明了「保持数组等长同序、掩码原样保留、只输出 JSON 数组、术语表优先」。

模型返回不是纯 JSON 时的兜底顺序：直接解析 → 剥 ```json 围栏 → 截取首个 `[` 到末个 `]` → 数组元素兼容字符串/`{"text":...}` → 单条重试 → 标记失败。

---

## [fallback]

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `chain` | string[] | `[]` | 渠道回退顺序 |
| `circuit_breaker_failures` | int | `5` | 同一渠道连续失败多少批后，本 run 内不再使用它 |

回退的触发条件：网络错误、5xx、429（重试若干次仍失败）、401/403（鉴权失败，不重试直接换）、456（额度用尽）、返回内容不合法。

`chain` 里没写的已配置渠道会被自动追加到末尾，所以只配两个渠道时 `chain` 写一个也能正常回退。

---

## [placeholder]

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `sentinel` | string | `⟦{}⟧` | 掩码模板，`{}` 处放序号。可换成 `@@{}@@` 之类 |
| `amp_codes` | bool | `true` | 是否保护 `&6` 风格颜色码 |
| `strict_new` | bool | `true` | 译文引入原文没有的占位符时，是否丢弃该译文 |
| `custom_patterns` | string[] | `[]` | 追加自定义占位符正则 |

`sentinel` 里的 `{}` 是必须的；没有 `{}` 时按「前缀 + 序号」处理。

---

## [limits]

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `max_batch_texts` | int | `40` | 单批最多条数 |
| `max_batch_chars` | int | `20000` | 单批最多字符数 |
| `max_entry_chars` | int | `8000` | 单条超限就跳过（v1.1 计划支持安全拆分） |
| `request_timeout_secs` | int | `60` | 单次请求超时 |
| `max_retries` | int | `5` | 单渠道内重试次数（指数退避 + 抖动，429 优先按 `Retry-After`） |

---

## [[targets]] 目标文件规则

一条规则 = 一个 glob 列表 + 一种文件类型。

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | string | 规则名（报告里用） |
| `kind` | `json` \| `lang` \| `snbt_map` \| `snbt_fields` | 文件格式 |
| `match` | string[] | glob 模式，`**` 跨目录，`*` 单层 |
| `text_keys` | string[] | 仅 `snbt_fields` 用：可译字段白名单 |

glob 匹配时 jar 内部路径按 `mods/x.jar!/assets/...` 参与匹配，工具会把 `!/` 当成 `/` 处理，所以下面这条规则同时覆盖目录里的语言文件和 jar 里的：

```toml
[[targets]]
name = "mod-lang-json"
kind = "json"
match = ["mods/**/assets/*/lang/en_us.json"]
```

默认规则见 `crates/mctrans-core/src/config.rs` 的 `default_targets()`，涵盖：Mod 语言 JSON、legacy `.lang`、FTB Quests lang 表（snbt/json）、FTB Quests 章节文件（字段白名单）。

`kind` 的区别：

- `json`：整个 JSON 里所有字符串值都译（en_us.json 是扁平的键值表）
- `lang`：Java Properties 风格，按行处理，保留注释与空行，只译等号右侧
- `snbt_map`：整张 snbt 表的字符串值都译（FTB Quests lang 表）
- `snbt_fields`：只有白名单字段的字符串值才译（章节里的 title/subtitle/description/text）

值是 JSON 文本组件（`{"text":"...","color":"gold"}`）时，只译 `text` 字段，其它键原样保留。

---

## Key 管理

优先级：环境变量 > 配置里的明文 `api_key`。

```powershell
$env:DEEPL_API_KEY = "..."
$env:DS_API_KEY = "..."
```

`mctrans config show` 只显示 Key 的首尾各三位。

---

## 数据库

SQLite（WAL 模式），默认 `%APPDATA%/mctrans/translations.db`，6 张表：

| 表 | 用途 |
|---|---|
| `source_texts` | 原文与内容指纹（`sha256(src_lang + tgt_lang + 原文)`） |
| `translations` | 译文，按 (原文, 渠道, 模型) 唯一 |
| `runs` | 每次运行的记录 |
| `file_states` | 文件级状态，断点续跑用 |
| `glossary_terms` | 术语表 |
| `provider_stats` | 渠道用量统计 |

常用命令：

```bash
mctrans cache stats                       # 看缓存规模
mctrans cache export cache.jsonl          # 导出（可换机导入）
mctrans cache import cache.jsonl
mctrans cache clear --scope all           # 连运行记录、术语表一起清
mctrans cache check                       # 完整性自检
```

缓存键与包、文件路径无关：同一句 `"Get Wood"` 出现在 100 个 jar 里也只译一次。
