# TODO — 工具面与 Claude Code 对齐

设计见 `DESIGN.md` 的「工具面」一节。核心判断:**执行层的形状不该由我们发明** —— 模型对终端使用和特定 harness 的工具形状有专门后训练,自己造名字和 schema 就是主动偏离分布。

顺序按"便宜且独立"排,不按重要性排。**每一项都要有会失败的判据** —— 这是本仓库唯一的纪律。

---

## 0 先量(零成本,可能改变优先级)

- [x] 写一个 schema 里参数名是 `path` 的探测,看模型会不会自己发 `file_path`
- [x] 同时看它会不会去够你没有的工具(`Edit` / `Grep`)

**实测(2026-10-13)。** `yu exec` 只发一条 user 消息、**没有工具面**,所以探测走了执行者那条路(`yu run --tools full`,沙箱内,任务:把 `notes.txt` 里的 `alpha` 全改成 `beta`,并找出哪个文件提到 `gamma`)。模型发出的**每一次**调用:

```
list_dir  keys=['path']      read_file keys=['path'] ×3
read_file keys=['path']      list_dir  keys=['path']
exec keys=['command'] ×4
```

- **参数名:完全照 schema。** 12 次调用里 6 次带参数,`path` 用了 6 次,**`file_path` 零次**。训练里的名字没有泄漏出来。
- **没有去够不存在的工具。** `Edit` / `Grep` 一次都没被调用。
- **但它把每件事都回落到 `exec`** —— 搜索用 `grep -rniI`,就地改动用 `sed -i.bak`(任务成功完成,`notes.txt` 里两处 `alpha` 都变成了 `beta`)。

**这条读数的两面都要看。** 按判据,"老实照 schema 走"成立 → 后面 schema 那几步**降一档**。但 TODO 那句预言也成立:*"模型会回落到 `exec grep`"* —— 回落是真的,只是它**不响**:任务照样做完了,区别只在工具选择上,而那个区别正是 TODO 说"要第 5 步的 A/B 才看得见"的东西。所以第 4 步(补实现)**不因为这条探测而降级**,降级的是 schema 对齐本身。

**探测顺带看到的一件**:执行者读了自己的 journal(`.yu/run-*.jsonl`),花掉 4 次调用里的 1 次 —— 这正是决策层 `ShapeSignal` 那类"循环指标",不是调查。

---

## 1 对齐结果的呈现形态(便宜、独立、先做)

现在四种 `Payload` 里**三种产出的文本没有任何模型见过**:

```
[reduction] dropped 4096 of 8192 bytes of the log result; nothing of it is in this message.
[reduction] 99 of 99 bytes spilled to blob fnv1a64:…; only the pointer is in this message.
```

- [ ] 把截断/落盘两种换成 **Claude Code 的形态** —— 只有它是对齐目标
- [ ] `Reduced` 的尾注同样处理

**Claude Code 没有对应形态的那一格,就是我们自己定义的 —— 要标明是我们自己的。** 不要从另一个 harness 借一句读起来像的:借来的措辞不在任何一方的分布里,而且因为熟悉而更难被发现。

**但内容要求不变**:说到落盘指针时,那句话必须自己带上指令(完整输出在哪、派 reader 去处理、不要自己读整个文件)。这一条与形态来自谁无关。

**为什么这条先做**:它**不依赖** schema 那几步,几乎不花钱,而且这条指令本身就在这里要发的那句话里 —— 形态和目标是一致的。
**判据**:两种形态各一个测试,断言输出里出现**已知形态的标记**、且**不出现** `[reduction]` 这种自造前缀;把渲染改回旧的,测试失败。

---

## 2 抓真形状

- [ ] 把 Claude Code 指到这个端点(`ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` 已在环境里),用反代抓一次请求,取出 `tools` 数组
- [ ] 写一个 capture → profile 的转换(机械活)

**这一抓同时解决另一件事**:`system` 字段也在同一个包里,原先"扒 prompt"的计划一次做完。

**判据**:`yu` 实际发出去的 `tools` 数组与抓包**逐字段相同** —— 一个测试,把 profile 拿掉或改一个字段,它失败。

---

## 3 让形状变成数据

`Tool::definition()`(`crates/yu-tools/src/native.rs:135`)是**唯一**产生线上形状的地方。

- [ ] `name()` 保持不变 —— 它是**派发键**(实现身份)
- [ ] `definition()` 有 profile 就用 profile 的 name / description / schema
- [ ] registry 构造时建一张 `wire_name → impl` 映射
- [ ] 没有 profile 时回落到内置那份 —— 与 `Quirks::unknown()` **同一个形状**(没测量就用保守的默认)

**判据**:
- 不给 profile 时,发出去的请求与今天**逐字节相同**
- 给 profile 时,线上形状跟着变
- **把 profile 加载删掉,测试要失败**

---

## 4 补实现 —— 真正的工作量在这里

按价值排:

- [ ] **`Edit(file_path, old_string, new_string, replace_all)`** —— 价值最高,而且**约定最重**(精确匹配、必须唯一、`replace_all` 的语义)。没有它,每次改动都是整文件重写:既贵又严重偏离分布
- [ ] **搜索**(`Grep` / `Glob`)—— 没有它,模型会回落到 `exec grep`,而**被训练的 `Bash` 描述里明确劝阻这件事**
- [ ] **`exec` 补 `description`(必填)与 `timeout`** —— 模型习惯每次都发 `description`

**每一件都按同一道工序:把描述抄下来,再逐句问实现做不做得到。**

> **一个描述在说谎的工具,比一个形状新颖的工具更糟。** 后者只是没吃到红利;前者是主动误导。

做不到的那句:**要么实现它,要么别抄那句,要么别提供那个工具。** 三条都行,唯独不能留着一句做不到的描述。

**判据**:每个新工具的每个声明参数,都有一个驱动它的测试;`Edit` 的精确匹配与唯一性各一个反例测试。

---

## 5 A/B

- [ ] 同一任务、两套 profile(对齐的 / 原来的)、比结果

**判据**:把"形状无所谓"这个假设,变成一个**会失败的测量**。它失败了 —— 即两套没有差别 —— 是一个合法且重要的结论,要写进 `docs/MEASUREMENTS.md`。

---

## 不做

- **决策层的工具面不动。** `delegate`/`decide`/`retire`/`ask_human`/`spot_check` 按构造是新的,没有模型被训练过;而它们是简单类型化调用,后训练红利本来就最小。
- **实现不了的工具不要放进 `tools` 数组。** 抓包里会有 `TodoWrite`、`WebFetch`、`NotebookEdit`。给了又做不到是另一种偏离分布。
- **不要把形状焊进代码。** 约定在动(`Read` 加了 `offset`/`limit`),而且哪个模型被训练的是哪套取决于模型。profile 是可刷新的快照。
