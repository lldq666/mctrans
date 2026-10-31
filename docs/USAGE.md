# 使用说明

## 一、翻译一个整合包

```bash
mctrans translate MyPack.zip -o MyPack-zh_cn.zip
```

输出目录/文件名规则：

- 输入 `MyPack.zip` → 默认输出 `MyPack-zh_cn.zip`
- 输入 `MyPack/`（目录）→ 默认输出 `MyPack-zh_cn/`
- 输出已存在时报错退出，要覆盖加 `--force`

产出包的结构：

| 输入条目 | zh_cn 模式产出 | inplace 模式产出 |
|---|---|---|
| `mods/x.jar!/assets/ns/lang/en_us.json` | 原文保留 + 新增 `zh_cn.json` | `en_us.json` 内容被替换 |
| `mods/x.jar!/assets/ns/lang/en_us.lang` | 原文保留 + 新增 `zh_cn.lang` | 同上 |
| `config/ftbquests/quests/lang/en_us.snbt` | 原文保留 + 新增 `zh_cn.snbt` | 同上 |
| `config/ftbquests/quests/chapters/*.snbt` | 原地替换（章节文件没有「第二份语言文件」的机制） | 同左 |
| 其它条目（pack.mcmeta、贴图、配置…） | 字节不变 | 同左 |

「原文保留 + 新增」是默认行为：游戏语言切到简体中文时读 `zh_cn.*`，切回英文读 `en_us.*`，互不影响。

如果包里已经有 `zh_cn.*`，该文件会被跳过（不算失败），报告里计入 `files_skipped`；加 `--force` 可以强制重译并覆盖。

---

## 二、先扫描再翻译

```bash
mctrans scan MyPack.zip
```

输出：

```
整合包：MyPack.zip
命中目标文件：128
待译单元：4210（共 96000 字符）
缓存命中：3800
预计翻译：410
```

`--json` 输出机器可读格式，方便脚本判断值不值得跑。

`--dry-run` 同理：走完整套文件识别，但不调用任何 API，只统计将要翻译的量。

---

## 三、切换翻译渠道

```bash
mctrans config use deepl     # 切换，立即写入配置
mctrans config use gpt
mctrans config show          # 看当前渠道、回退链、Key 是否就位
mctrans config test gpt      # 发一次 Ping 验证连通
```

或者在翻译时顺手切：

```bash
mctrans translate MyPack.zip -o out.zip --provider gpt
```

切换后 `general.active_provider` 被改写，后续 run 默认用新渠道。翻译过程中如果它出问题（限流、5xx、超时、额度用尽、返回内容不合法），会自动按 `fallback.chain` 回退：

```toml
[fallback]
chain = ["deepl", "gpt"]
circuit_breaker_failures = 5
```

同一渠道连续失败 5 批后本 run 内跳过它，避免在一个坏渠道上耗时间。

用 `--no-cache` 可以强制绕过缓存重译，这时切换渠道才真正打到新渠道的接口。

---

## 四、术语表

术语表在两处生效：

1. 送译时注入 system prompt（最多 30 条），让模型按你的译法走
2. 译后做一次精确替换（只替换占位符之外的片段，不会误伤颜色码）

```bash
mctrans glossary add "Efficiency" "效率"
mctrans glossary list
mctrans glossary export terms.csv      # src,tgt 两列
mctrans glossary import terms.csv
mctrans glossary remove "Efficiency" "效率"
```

术语表存在 SQLite 里，跟着数据库走。

---

## 五、断点续跑

```bash
mctrans translate MyPack.zip -o out.zip --resume
```

`--resume` 会读取该整合包上一次运行的 `file_states`，跳过状态为 done 的文件。中断后重跑同一个输出路径时加上它即可；没加的话所有文件都会重新处理，但缓存命中让实际 API 开销接近零。

---

## 六、运行报告

每次翻译产出两个文件：

- `<out>.report.json`：结构化数据，给脚本用
- `<out>.report.md`：给人看的汇总

报告里的关键字段：

| 字段 | 含义 |
|---|---|
| `active_provider` / `provider_order` | 实际用的渠道与回退顺序 |
| `files_total` / `files_translated` / `files_skipped` / `files_failed` | 文件级统计 |
| `units_total` / `translated` / `cache_hits` | 单元级统计 |
| `placeholder_failures` | 占位符校验不通过、已保留原文的条数 |
| `too_long` | 超过 `max_entry_chars` 被跳过的条数 |
| `provider_usage` | 各渠道的批次数、输入/输出字符数（DeepL 按字符计费，用它对账） |
| `failures` | 失败明细：文件、单元、原文摘要、原因 |

---

## 七、常见问题

**Q：翻译后的包进游戏不生效？**
A：确认游戏语言设置成了简体中文（`zh_cn`）。`zh_cn` 模式生成的是标准命名，客户端按语言包名加载。另外检查 `mods/xxx.jar` 里是不是有 `en_us.json` 之外的其它命名（如 `en_uk.json`），那种要单独加规则。

**Q：报告里 `placeholder_failures` 不为 0？**
A：说明模型把占位符改坏了，工具保留了原文。这些条目不会被写坏，但需要人工处理，报告里能看到是哪个文件的哪一条。常见原因是模型把 `%1$s` 写成了 `%s`，换个模型或加术语表提示通常能改善。

**Q：429 太多了？**
A：调小 `limits.max_batch_texts`（换更小批次）、调小 `general.concurrency`、加长退避。DeepL 免费版本身按字符限额，超额返回 456。

**Q：想看没翻译前有多少字符要花钱？**
A：`mctrans scan` 看「待译单元 / 共 N 字符」，或翻译后看报告里 `provider_usage.chars_in`。

**Q：GBK 的 .lang 文件能不能处理？**
A：能。编码按 BOM → UTF-8 → GBK/Big5 顺序探测，写回时保持原编码。纯 ASCII 文件会按 UTF-8 处理（对游戏没有区别）。

**Q：能不能只翻译某些 mod？**
A：改 `[[targets]]` 的 `match` 规则，把不想翻的排除掉即可。规则按顺序匹配，第一个命中的生效。

**Q：会改动我的原包吗？**
A：不会。输出永远是另一个文件/目录；输入路径与输出路径相同时会被拒绝（除非 `inplace` 模式或 `--force`）。
