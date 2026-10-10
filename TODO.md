# TODO

源自 2026-10-10 对骨架的三路代码评审（`yu-wire` / `yu-journal` / 其余全部）。设计见 `DESIGN.md`，实测协议事实见 `docs/WIRE-FACTS.md`。

**顺序的原则**：先止血 → 再把仪表修对 → 然后立刻做一条最薄的竖切。本项目的失败模式不是代码质量，是「永远没有竖着打通一次」。在竖切跑通之前，不要开始任何新模块。

**评审的总判断**：对设计的理解远好于对设计的实现。每个 crate 都把自己的那部分想对了（`gate.rs` 的准入锁、`GrowthBudget` 把坑编码进类型、`alloc.rs` 的归因、`StopReason` 的穷尽匹配），问题全在缝上。这种状态最典型的恶化方式，是继续写新的、更漂亮的模块。

---

## 0 止血（今天，几小时）

四条都是小改动，但都属于「某一天会失去一切」那一类。

- [x] **`redirect::Policy::none()`** —— `yu-wire/src/client.rs:675-680` 没设重定向策略，走 reqwest 默认 10 跳。reqwest 只在跨主机时剥 `Authorization`/`Cookie`，**不剥自定义头**，而这里的凭证是 `x-api-key`。一个 307 到别处的网关就把 key 送出去。
- [x] **错误体脱敏** —— 401 的响应体里带 key 后四位，而它进了日志和持久化 JSON：`yu-wire/src/error.rs:109-119`、`yu-wire/src/capability.rs:359`、`yu-cli/src/main.rs:138`。目前没有任何脱敏。
- [x] **写失败时 `set_len` 截回** —— `yu-journal/src/log.rs:126-137`。文件是 `append(true)`（O_APPEND），实际写位置由 OS 定，与内存里的 `writer.offset` 无关。写失败时 `?` 先返回，offset/next_seq **不推进**，于是下次 append 的整行紧贴在残字节后面 → `{残}{整}\n` 变成一行 → 重开时解析失败 → `Corrupt` → **`open` 从此永久失败，且没有修复入口**。ENOSPC 就是现实触发条件。顺带：同一个 crate 的 `repair_torn_tail`（`log.rs:190-209`）已经把「为什么焊接是毁灭性的」讲清楚了（*destroys two records to preserve a fragment*），只是没用到 append 路径上。
- [x] **落盘失败必须说出来** —— `yu-core/src/agent.rs:509-514` 的 spill 闭包失败时返回空串并只记 `tracing::error!`；`yu-tools/src/reduce.rs:266-271` 于是退回预计算的 `fnv1a64:` digest，并把它写进给模型看的指针文本（`reduce.rs:484-493` 声称「字节已经在盘上」）。而 blob 存储只认 64 位 hex sha256（`yu-journal/src/blob.rs:186`）。**模型拿到一个解析不了的地址，日志记下一个不存在的 digest。** 违反「清单不谎称」。

---

## 1 把仪表修对（一天）

**不能跳。** 后面每一步都靠这些数验证，而它们现在是错的。

- [x] **补上没发射的 span** —— `yu-core/src/span.rs:43-64` 声明了九种，实际只有 `Assemble` / `WireWait` / `Stream` 被发射。缺 **`ToolExec` / `Reduce` / `Journal` / `Spawn` / `Init` / `FirstToken`**。
  - 为什么要紧：`yu-observe/src/metrics.rs:107-113` 的 `harness_time_ns()` 是「把所有非模型的 span 加起来」，于是**结构性地漏掉了工具执行、归约、日志 IO** —— 恰好就是这个项目说要暴露的那一项，而输出上没有任何提示。
  - `FirstToken` 现在只是 `StreamTiming` 里的一个字段，不是 span。
- [x] **让「无效信息比」能失败** —— `yu-core/src/render.rs:453-477` 的 `measure_composition` 把 `tool_call_chars` 硬编码为 0，`tool_result_chars` 取的是**清单自己的散文长度**，而且只在决策层视图上测量（按构造不含工具输出）→ **只能返回 0**。一个不可能失败的指标抓不住任何东西。
  - 要测的是**执行者上下文**里的工具机制占比（那里的 `messages` 是无界增长的）。
- [x] **修 `request_digest`** —— `yu-core/src/agent.rs:605-614` 只哈希 `model` / `max_tokens` / 消息正文，漏掉 `system` / `tools` / `thinking` / `output_config`。**两个 system prompt 完全不同的请求 digest 相同。** 而 `yu-journal/src/event.rs:69-71` 说它让「重发的相同视图可被检测」。
  - 为什么在这个端点上格外要紧：WIRE-FACTS Quirk 3 测出 `cache_control` 惰性、缓存**完全靠前缀字节**。看不见前缀 = 看不见缓存。

**验收**：一次真实运行能同时报出「决策层的工具机制占比」和「执行层的工具机制占比」两个真数。

---

## 2 最薄的一条竖切（**决定项目成败**）

不做 daemon、不做拓扑、不做 L3、不做策略接入。只做一条能跑通的路径：

```
charter (L0) → Ledger (L1) → render() → 决策层 agent
                                            ↓ delegate
                                         执行者（已存在）
                                            ↓ 报告
                                         回到决策层视野
```

- [x] **把 `render()` 接到真实调用** —— `yu-core/src/render.rs:306` 现在**只有它自己的测试在调**（`render.rs:506,675,683,700,708,733`）。`Charter` / `Ledger` 在测试之外从未被构造过。设计的心脏目前是死代码。
- [x] **决策层 agent + 它的 prompt** —— 全项目**最需要原创、最没参照**的一块。规格在 `DESIGN.md` 的「决策层」一节：只读四样（计划 / 每轮最终答复 / 证据块 / 检查点上的 diff）、看形状不看内容、五条干预触发、一句话下钻检验、权限边界、工具面是 `delegate/ask_human/retire/decide/spot_check`（**不是** Read/Write/Bash）。
- [x] **`delegate` 这一个工具**（其余四个先不做）。

**「做完了」的判据 —— 一个可证伪的数：**

> 决策层跑完一个真实任务后，它上下文里**工具机制的占比**是多少？
>
> - 接近零 → **设计成立**，后面所有工作有依据
> - 超过一两成 → **前提有问题**，现在就要知道，而不是六个月后

**这一条跑通之前，不要碰任何别的东西。**

---

## 3 让记录诚实（竖切跑通之后）

- [x] **assistant 的正文写进 L2** —— `yu-core/src/agent.rs:395` 写的是 `b.type_name()`，即 `["thinking","text","tool_use"]`，**只有类型名，没有内容**。一轮里最重要的产物不在磁盘上。
  - 这一条同时打断三处：L2 的完整性承诺；决策层规格要读的「每轮最终答复」；换代验证者的「从原始记录出发」。
  - 而 `yu-journal/tests/constraints.rs:169` 用 `format!("block-{i}")` 这种字面串盖过去了，测试对现实是空的。
- [x] **thinking 改成有序的多个** —— `yu-core/src/agent.rs:366-374` 把所有 thinking 正文用 `""` 拼起来、取**第一个**签名 → 两个 thinking 块塌缩成一个，且**签名 N 焊在正文 M 上**。而 WIRE-FACTS 明确测到过 `thinking → text → server_tool_use → … → thinking` 的交错。Quirk 1 要求 thinking 逐字节回传，配错签名回放就是坏的。
- [x] **signature 与正文的存储位置装反了** —— `yu-journal/src/event.rs:90-91`：签名**按值存在 JSONL 行里**，thinking 正文反而进 blob 仓库。真实 Anthropic 的签名是 KB 级的，那会撑大**正是要保小的那行**。代码给的理由（`lib.rs:20-23`「这个端点上签名是 36 字节 UUID」）是关于这个 shim 的事实，不是关于这个格式要活过多久的事实。
- [x] **`read_range(from, until)`** —— `yu-journal/src/log.rs:151-159` 的 `read_from` seek 之后 `read_to_end` **整个尾巴**，没有上界。读第 3 条事件会 materialize 后面 999,997 条。索引里两个 offset 都有，几行的事。这是 `delegate_read` 能不能便宜的前提。
- [x] **让 `yu probe` 真能复现 WIRE-FACTS 的六条 quirk** —— `yu-wire/src/capability.rs:297-328` 的 `infer_quirks` 从不设置 `cache_breakpoints` / `automatic_prefix_cache` / `unsupported_is_422` / `beta_header_matters` / `thinking_signature_validated`，返回的 profile 有**四个字段与实测相反**。Quirk 1/3/4/6 根本没有探针（住在 `.probe/probe*.sh`，Quirk 1 有 16 个用例），而 **WIRE-FACTS 第 4 行写着「Re-run with `yu probe`」**。
  - 后果：哪天 shim 开始校验 signature，没有任何东西会注意到。
  - 附带：`quirks.rs:203-210` 的 `Quirks::from_name` 无法往返 `infer_quirks` 生成的名字，存下来的 Capabilities JSON 喂不回去 → 约束 3（「约束靠注入」）恰好在它唯一承重的地方断了。
- [x] **`cache_control` 那个假 `yes`** —— `capability.rs:132-142` 把它标成 `observable: true`，而 `capability.rs:49-56` 把 `observable: true` 定义为「一个 200 就是结论」。缓存断点的效果是**后续请求命中**，一个 200 什么都证明不了。探测表现在打出 `yes cache_control 200 … cr=0` —— 正是这个模块的文档第一句话说要防的失败。

---

## 4 把反增长做成真的

- [x] **`Ledger::retire` 要真的移除** —— `yu-core/src/render.rs:137-143` 只往 `retired` 里 push，不移除任何东西；`decisions` / `open_questions` / `rejected` 完全不收缩。于是视图**单调增长** —— 正是这个设计存在的理由。而声称覆盖有界性的测试 `view_size_does_not_grow_with_the_record`（`render.rs:627-650`）只变了 `omitted`，**真正会长的 L1 完全没测**。
- [x] **字节前缀稳定要做出来** —— `render.rs:307` 永远只发**一个** system block，于是 L0/L1 边界没法放缓存断点，加一条 decision 就把后面每个字节挪位。而**全仓库没有任何地方设过断点**（`agent.rs:216` 检查的那个计数结构性恒为 0）。`SystemBlock::with_cache()` 除测试外无调用者。
- [x] **`View` 要有 `tools` 字段** —— DESIGN 的渲染顺序是 `tools → system → messages`，现在 tools 在 `agent.rs:354-357` 单独拼装，**前缀在两个地方拼**，这正是漂移的成因。

---

## 5 之后（现在都别动）

- [x] **注入 system** —— `AgentConfig::system()`（`agent.rs:74-77`）没有非测试调用者，`cmd_run`（`main.rs:292`）不调它 → 执行者**根本没拿到 system prompt**。而 `yu-policy/src/resource.rs:143-150` 的注释写的就是这个失败。
- [ ] **策略接入行为** —— `Policy`（`resource.rs:104-122`）没实现 `PolicySource`（`yu-core/src/traits.rs:16`）；Rhai/Wasmtime 恒返回 `Unsupported`；归约表从不被真正的 reducer 查询（`reduce.rs` 用自己的常量）。约束 0 的验收标准目前只对「解析一个文件」成立。
- [ ] **`delegate_read`** —— 只在注释和清单文本里出现过（`yu-core/src/lib.rs:7`、`yu-workflow/src/handover.rs:103`）。
- [ ] **拓扑解释器（第一次派生）** —— `TopologyRule`（`resource.rs:58-67`）存在且无人读；`SpanKind::Spawn` 从未发射。没有第一次派生，谈不上递归。
- [ ] **换代派发** —— 类型、并集、验证者都在且有测试，但**没有东西派发 `ReaderJob`**（`handover.rs:105-111`），`k` 无处可寻。同一个概念实现了两遍、接线零遍（`GrowthBudget` 与 `Trigger::Threshold`）。
- [ ] **daemon** —— `Host::{Embedded, LocalDaemon, Remote}` 和 JSONL 分帧都写好了且有测试，但**没有任何 `impl SessionHost`**，没有 UnixListener。
- [ ] **L3 重放与 fork** —— 全仓库只有注释；没有血缘字段、没有 API。`log.rs:27-29` 说共享 blob 把 fork 成本降到 O(1)，**那只对 payload 成立** —— 事件本身仍是一条一行。
- [x] **effect 纪律** —— `EventBody::Effect`（`event.rs:127-128`）没有状态、不指向它产生的 `ToolResult`；`EffectGuard::mark_done`（`effect.rs:81-85`）只写内存，**完成从不入日志**。于是崩溃在「记意图」与「动手」之间（**正是这条纪律存在的窗口**）留下的 Effect 无法与「做完了」区分，而测试里的回放种子会把它当作已完成并静默跳过。

---

## 别碰

`daemon` · `拓扑` · `L3` · `多 provider` —— 四个都很诱人，但在竖切跑通之前做它们，就是**继续造漂亮的模块然后不接线**，也就是现在的状态。

---

## 工作方式

每一步做完，**真的跑一次，然后看那个数**。不是跑测试 —— 测试现在有不少是空过的（比如用 `"block-0"` 喂日志那个、`from_spill` 只在测试里被设置所以「回读封闭」由一个没人设的标志守着、约束 1 的守卫只查一个结构体的两个字段而真实重复是跨字段的）。

**先跑通一条线，再让它变宽。**
