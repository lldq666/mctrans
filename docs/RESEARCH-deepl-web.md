# 调研：能否不用 API Key 接入 DeepL 网页版翻译

调研时间：2026-10-31　范围：作为 mctrans 的**非必须**后续优化项评估，本轮不实现。

---

## 一、结论

**不建议实现，不做成内置渠道。** 理由按权重排序：

1. **合规风险最高**：DeepL 的账户条款明确禁止「通过自动化程序访问和控制 DeepL Translator，若其请求数超过正常人类行为」，也禁止「创建以机器翻译服务为主要目的的类似产品/服务/API」和反向工程。把逆向出来的网页版接口做成一个会分发给用户的翻译工具的渠道，正好落在这两条上。
2. **稳定性不可控**：免费网页版端点按**出口 IP** 限流，风控一触发就是整段 IP 被临时封禁；协议是逆向出来的，DeepL 改一次前端就可能全线失效，且没有官方通知。
3. **收益并不匹配**：正规免费额度已经够用——DeepL API Free 每月 50 万字符、OpenAI 兼容端点有免费额度和本地 Ollama，两者都稳定、可预期、有文档。

如果只是想「没有 Key 也能跑」，正确解法不是逆向网页版，而是 **OpenAI 兼容渠道指向本地 Ollama**（`base_url = http://127.0.0.1:11434/v1`，不配 Key），mctrans 现在就支持。

---

## 二、方案长什么样（技术事实）

DeepL 网页版前端调用的是一个 JSON-RPC 端点，社区逆出来的用法大致是：

- 端点：`https://www2.deepl.com/jsonrpc`（另有 `www.deepl.com/jsonrpc`）
- 方法：`LMT_handle_texts`（老版本是 `LMT_handle_jobs`）
- 必需字段：`jsonrpc: "2.0"`、`method`、`params.texts[]`（每项含 `text` 与随机 `id`）、`params.target_lang`（如 `ZH`）、`params.raw_text`、`timestamp` 等
- 关键"暗号"：`id` 不是自增整数，而是一个由**客户端随机数与时间戳混合**算出来的 `iNumber`（社区实现里叫 `number` / `id` 计算），服务端会校验它的形式
- 反代层通常还要伪装浏览器或 iOS App 的请求头（`User-Agent`、`x-app-*` 等），并且在 429 之后加退避与熔断

现成实现（都在活跃维护）：

| 项目 | 语言/形态 | 说明 |
|---|---|---|
| `free-deepl-translator` | Python 包 | 逆向网页版协议，README 自己写明「Unofficial… may break… may violate DeepL's ToS」 |
| `DeepLX` | Go 服务（Docker 部署，端口 1188） | 逆向接口做成 HTTP 服务，自带访问规则配置 |
| `deeplx-serverless` | Cloudflare Workers | 面向无服务器场景，README 直接解释 429 来自**共享出口 IP** |
| DualSub 的 `deeplTranslateFree.js` | 浏览器扩展 | 2 秒一次的保守间隔，显式标注「web scraping approach」 |

也就是说：技术上完全做得出来，半天到一天的工作量。但它是"能用"，不是"能交付"。

---

## 三、稳定性评估

社区实测与开源实现的注释给出的限制：

| 维度 | 情况 | 来源 |
|---|---|---|
| 单次请求长度 | 约 5000 字符上限（网页版自身限制，非 API 限制） | 多篇实测文章、`deeplx` 社区 |
| 请求频率 | 同一 IP 每小时几十次量级；高频会触发 429 | 实测文章、`deeplx-serverless` 的 429 说明 |
| 封禁表现 | 返回 `your IP has been blocked by DeepL temporarily`，需等冷却 | `deeplx-lib` 的错误处理注释 |
| 服务器/容器场景更糟 | 出口 IP 共享，Cloudflare/VPS 上部署时几乎持续被限流 | `deeplx-serverless` PR 明确说明 |
| 协议易碎 | 端点、方法名、`iNumber` 算法、请求头任一处变化即失效；无通知、无兼容期 | 各实现 README 的免责声明 |
| 质量 | 与官方 API 同源，翻译质量本身没问题 | — |
| 附加能力缺失 | 无术语表（glossary）、无 formality、无 usage 查询、无 SLA | 接口本身不提供 |

对本项目而言还有一条现实约束：mctrans 的定位是**批量**翻译整合包，一个中等整合包几千到几万条文本。按网页版每小时几十次、每次 5000 字符的量级，跑完一个大包要几十上百小时——即便不被封禁也不可用。反过来，官方 API Free 的 50 万字符/月够大多数整合包用（一个中等整合包通常 5–20 万字符）。

---

## 四、合规与风险

DeepL 账户条款（DeepL Translator 部分）：

> "Customer shall not be allowed to access and control the DeepL Translator through automated procedures which may cause an increased number (exceeding usual human behavior) of requests to the DeepL Translator."

平台条款同样禁止自动化使用与系统性收集；Pro 许可里还有两条直接命中本项目场景：

> "f) to create a similar product, service or API whose primary purpose is to provide services based on machine learning, including but not limited to translations…"
> "g) to develop, market or train a machine translation algorithm"

另有独立的反向工程条款（ConductAtlas 记录 CA-D-000449，2026-09-26 抓取的条款版本）禁止 "de-compile, reverse compile, disassemble, reverse engineer or otherwise reduce to human-perceivable form all or any part of the Services"。

风险归属：

- 违反服务条款的后果由使用者承担（账号封禁、上诉、索赔风险），分发一个内置逆向渠道的工具会把风险转嫁给所有用户
- 逆向协议本身属于"规避技术措施"的灰色地带，在部分司法辖区有额外法律含义
- 商业分发场景下，"以机器翻译为主要目的的类似服务"这一条几乎必然被认定违反

**结论：不能内置。** 可以作为用户自担风险的个人实验脚本存在，但不应该由本项目提供、发布或引导。

---

## 五、正规的"零成本/低成本"替代（推荐做法）

| 方案 | 额度 | 接入方式 | 备注 |
|---|---|---|---|
| **DeepL API Free** | 每月 50 万字符 | `base_url = https://api-free.deepl.com`，Key 用邮箱注册即可，无需信用卡 | 官方文档明确的额度；免费版与付费版域名不同，注意切换。超额返回 456，工具会自动切下一个渠道 |
| **本地 Ollama** | 无上限（受本机算力限制） | `type = "openai"`，`base_url = http://127.0.0.1:11434/v1`，不配 Key | 零成本、零合规风险、断网可用；质量取决于模型，`qwen2.5`/`llama3.1` 一类中英模型做游戏汉化够用 |
| **DeepSeek / Moonshot 等 OpenAI 兼容服务** | 有免费额度或极低价 | `type = "openai"` | mctrans 已把 `structured_output`、宽松 JSON 解析、批量切分都做好了 |
| **Microsoft Translator / Google Cloud Translation** | 200 万 / 50 万字符每月 | 需另写 provider（接口形状与 OpenAI 不同） | 中文质量不错，可以作为 v0.2 的第三个 provider 类型 |
| **LibreTranslate / NLLB 自托管** | 不限 | 需另写 provider | 完全本地，术语一致性一般 |

mctrans 现有架构里加一个新 provider 类型的成本很低：`providers/` 下实现 `Translator` trait 即可，渠道切换、回退、熔断、缓存全部复用。

---

## 六、如果仍要做（自担风险版）

如果坚持要这个能力，建议这样隔离，**不进入正式发布物**：

1. 做成独立脚本（例如 `tools/deepl-web-proxy.py`），只在本机跑，提供一个 OpenAI 兼容形状的本地端点（`http://127.0.0.1:PORT/v1`）
2. mctrans 里**不新增 provider 类型**，用户自己用 `type = "openai"` 指到那个本地代理即可——这样逆向逻辑不进我们的代码、不进我们的分发包
3. 脚本里限速（≥2 秒/次）、带 429 熔断与退避、单条 ≤5000 字符
4. 文档里明确标注：非官方接口、随时可能失效、违反 DeepL 服务条款的风险由使用者自行承担、仅供个人学习使用
5. 绝不在 GUI 里出现"免费 DeepL"这类一键选项，避免把违规路径产品化

即便这样，第 1–3 条的现实问题依旧存在（限流、封 IP、协议变更），所以它只能是小规模尝鲜工具，不适合当主流程。

---

## 七、一句话总结

**网页版逆向接口能跑，但不该做。** 想要免费额度就用 DeepL API Free，想要完全免费就用本地 Ollama——这两条 mctrans 现在都支持，且都稳定、可交付、不会把用户拖进 ToS 风险里。
