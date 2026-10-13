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

- [x] 把截断/落盘两种换成 **Claude Code 的形态** —— 只有它是对齐目标
- [x] `Reduced` 的尾注同样处理

**实测。** 形态从装好的 bundle 里取的,带字节偏移:
- **截断** —— `... [N truncated] ...` 是 Claude Code 的脚手架(`h8t` / `Gut` 两处都在),但**单位是我们的**:它数行或字符,而被丢掉的 payload 没有文本可数,manifest 量的是字节。这一点写在 `render` 的文档注释里。
- **落盘** —— `<persisted-output>` 块**逐字**采纳(`Lae`,它自己的 `Output too large (…). Full output saved to: …` 和它自己的字节渲染)。两处差异都标明是我们的:没有 `Preview (first 2KB):` 段(一个 `Spilled` 手里没有字节,不能声称有预览),以及那句指令是**我们的**(Claude Code 里地址是模型能直接打开的路径,块本身站得住;而 blob digest 不是执行者工具能读的东西,所以那句指令是回去的路)。
- **`Reduced`** —— **Claude Code 没有这一格**:它从不把工具结果换成对它的摘要,所以没得抄。括号是它的注释风格,字是我们的,**标明是我们的**。

顺带同一个函数里另外两个自造标签(`[delegated]` / `[sandboxed]`)也去掉了 —— 同一类缺陷。两条测试各一个,**回退渲染即失败**(失败信息是真实的:回退后打印出 `[reduction] dropped 40000 of 40000 bytes…`)。

**两处诚实与形态相抵的地方,都判给了诚实**:没有预览段(有就是声称一个不存在的预览)、单位用字节(抄 `N lines truncated` 就是一个没人量过的数)。

**Claude Code 没有对应形态的那一格,就是我们自己定义的 —— 要标明是我们自己的。** 不要从另一个 harness 借一句读起来像的:借来的措辞不在任何一方的分布里,而且因为熟悉而更难被发现。

**但内容要求不变**:说到落盘指针时,那句话必须自己带上指令(完整输出在哪、派 reader 去处理、不要自己读整个文件)。这一条与形态来自谁无关。

**为什么这条先做**:它**不依赖** schema 那几步,几乎不花钱,而且这条指令本身就在这里要发的那句话里 —— 形态和目标是一致的。
**判据**:两种形态各一个测试,断言输出里出现**已知形态的标记**、且**不出现** `[reduction]` 这种自造前缀;把渲染改回旧的,测试失败。

---

## 2 抓真形状

- [x] 把 Claude Code 指到这个端点,用反代抓一次请求,取出 `tools` 数组
- [x] 写一个 capture → profile 的转换

**实测(2026-10-13)。** Claude Code 2.1.248,一个真实会话,3 个请求全 200,`Read` 与 `Bash` 真的被调用过。**26 个工具**。抓包在**转发前**记录,`body_sha256_as_received == body_sha256_recorded` 三个请求全等;线上数组与 Claude Code 自己的 `system:init` 事件两个来源对工具列表一致(26/26)。

**抓包推翻了这个 TODO 的三条前提 —— 见下面第 4 步的批注。** 另外两条实测:凭据走 `Authorization: Bearer` 也可以(WIRE-FACTS 之前只测过 `x-api-key`);`system` 是 3 个 block(计费头 / 62 字符 SDK 前言 / ~7.5KB 系统提示,前两块带 `cache_control`),所以"扒 prompt"那份材料也在同一个包里。

**判据**:`yu` 实际发出去的 `tools` 数组与抓包**逐字段相同** —— 一个测试,把 profile 拿掉或改一个字段,它失败。

---

## 3 让形状变成数据

`Tool::definition()`(`crates/yu-tools/src/native.rs:135`)是**唯一**产生线上形状的地方。

- [x] `name()` 保持不变 —— 它是**派发键**(实现身份)
- [x] 有 profile 就用 profile 的 name / description / schema
- [x] registry 构造时建一张 `wire_name → impl` 映射
- [x] 没有 profile 时回落到内置那份 —— 与 `Quirks::unknown()` **同一个形状**(没测量就用保守的默认)

**实测(2026-10-13)。** 形状来自两处,而且**分开**(`crates/yu-tools/src/profile.rs` 与 `native.rs` 的脸表):

- 快照:`{"tools": […], "system": […]}`。`system` **不读** —— 执行层的 prompt 是另一个决定,把同一个包里的东西顺手拿进来不是"对齐形状"该做的事。装载器不修剪、不重排、不"整理":它一解释,就不再是"发出去过的那份"。
- 脸表(`FACES`,4 条)只说快照**说不出**的事:哪个实现站在哪张脸后面、它读哪些参数、哪些句子它不肯重复。26 个名字因此收成 4 张能兑现的脸,其余 22 个不出现(约束 4)。
- `ToolRegistry::profiled(cwd, profile)` / `profiled_read_only(...)` 按**快照的顺序**注册(缓存前缀);dispatch 两个名字都认(线上名 `Read` 与实现名 `read_file` —— 后者是记录里的身份,重跑指令靠它继续有效);输入在 `run_one` 里**翻译一次**,因此 effect / effect_in_place / execute 看到的是同一个对象。
- 参数是**承诺**:快照里有、实现不读的参数(`pages`、`run_in_background`、`dangerouslyDisableSandbox`)从 schema 里去掉;反过来,实现要读而快照里没有的参数 → **装载失败**。快照改了措辞、把我们拒抄的那句换掉 → 也**装载失败**(否则那句谎话会安安静静地活下来)。
- `--tool-profile`(`run` 与 `decide`):装载失败就**不启动**,绝不悄悄退回内置脸。

**判据**:
- 不给 profile 时,发出去的请求与今天**逐字节相同** —— `wire_shape_baseline` 未动、未编辑,仍绿(把内置 schema 加一个参数,它立刻红,已实测)
- 给 profile 时,线上形状跟着变 —— `crates/yu-tools/tests/profile_shapes.rs`(形状、句子、参数、翻译、结果形状)与 `crates/yu-cli/tests/tool_profile.rs`(真进程、真请求)
- **把 profile 加载删掉,测试要失败** —— 已实测:让 `definitions()` 不看 profile,`every_face_says_what_was_kept…` 与 `the_schema_is_the_captures…` 两个测试红

**两条读数的注脚,写下来免得下次重新发现:**
- `input_schema` 内部的**键序**与抓包不同(serde_json 默认 map 排序),字段集合与取值相同 —— 要逐字节同就得开 `preserve_order`,而那会**反过来**移动内置那四份(它们是按排序写的),撞上 `WIRE_SHAPE`。逐字段相同,已经是这条判据的全部。
- 没被 profile 覆盖的工具(`list_dir`)不进脸表,所以对齐面上没有它 —— 列目录在 Claude Code 的面上是 `Bash`/`Read` 的事。

---

## 4 补实现 —— 真正的工作量在这里

按价值排:

- [x] **`Edit(file_path, old_string, new_string, replace_all)`** —— 价值最高,而且**约定最重**(精确匹配、必须唯一、`replace_all` 的语义)。没有它,每次改动都是整文件重写:既贵又严重偏离分布
- [x] ~~**搜索**(`Grep` / `Glob`)~~ —— **不做,前提被证伪**(见下;划线是结论,不是待办)
- [x] **`exec` 补 `description` 与 `timeout`** —— 模型习惯每次都发 `description`

**实测(2026-10-13)。** 三件都做了,但**没有一件是照抄** —— 逐句核对之后有三处必须偏离原文:

1. **`description` 不是必填。** 抓包的 `required` 只有 `["command"]`,而"schema 说可选、工具却报错"是反方向的说谎(模型会收到它没被警告过的失败)。它照常收下,并进**记录**(`EffectIntent.description`):那是事后最想知道、而记录别处补不回来的东西。改的是"脸后面的事",不是脸。
2. **`timeout` 的 `default 120000 / max 600000` 只在脸的下面成立。** 对齐脸自带这个默认与上限(照着描述实现,句句为真);没有脸的 `exec` 没有 deadline —— 一个跑了十分钟的 `cargo build` 不是挂住,为了一个没人发过的句子去杀它是**改动了未测量的那条路**。显式的 `timeout` 两条路都认。
3. **读的结果形状也照抄了。** 对齐脸下 `Read` 输出 `{n}\t{line}`(取自抓包自己的 `tool_result`:`"1\tThe blue crumb…\n2\t"`,注意连文件末尾那个空片段都编号 —— 真 `cat -n` 不编,抓包编),`offset`/`limit` 是**行号与行数**(`offset` 从 1 数,0 等价于 1:抓包 schema 的 `minimum: 0` 不允许它是个错误)。没有脸时按原样返回文件字节 —— 同一实现、两张脸的第一处,也是 `Read` 的 `cat -n` 句与 `Edit` 的 strip-the-prefix 句得以保留的原因。

**逐句核对的结果(清单在各工具的 refusal 表里,判据在各测试里):**
- `Edit` 的 "You must Read the file in this conversation…" **去掉**(本 harness 不记录会话读过哪些文件),`Write` 的 "Overwriting an existing file you haven't Read will fail" 是同一句谎话,同理;`Read` 去掉 "must be an absolute path"、"Reads up to 2000 lines by default"(截断是归约机制的事,带 manifest;阅读器里再来一个静默上限就会藏掉 manifest 没记账的字节)、images/PDF/notebook 那句(连 `pages` 参数一起)、"Reading a directory…system reminder" 那句、"the harness tracks file state for you" 那句。
- `Read` 的 offset/limit 句与 `cat -n` 句**保留**(都做到了);`Edit` 的精确匹配、唯一性、strip-the-prefix 三句**保留**(前两句是被强制的,第三句正因为我们按抓包编号输出才成立);`Write` 的 "For partial changes, use Edit instead." 保留。
- `Bash`:avoid-list 句保留(这里确实有 `cat/head/tail/sed/awk/echo` 的专用工具),`timeout` 句保留,# Git 段保留两条(`gh`、只在被要求时提交/推送),**去掉** "Executes a bash command"(这里跑 `sh -c`)、工作目录持久化那段、输出给用户看、`run_in_background`、交互式 flag,以及 Claude Code 自己的署名行与 🤖 标记 —— 后者不是这条工具的行为,是**别的产品的名字**。

**三条前提被实测推翻:**

1. **`Grep` / `Glob` 在 2.1.248 的工具面里根本不存在** —— 全 body 零次出现。**没有形状可抄。** 而这条的理由也是错的:TODO 原写"被训练的 `Bash` 描述里明确劝阻这件事",实测 Bash 描述的 avoid-list 是 `cat/head/tail/sed/awk/echo`,**不含 grep**。模型确实去够 `Grep` 了(第 0 步实测),没够到,回落到 `Bash` grep —— 那是**它自己也在摸索**,不是偏离一个明确约定。
2. **`TodoWrite` 不在默认工具面里。** 被 `Task*` 家族(`TaskCreate`/`TaskGet`/`TaskList`/`TaskUpdate`)取代,只在 `CLAUDE_CODE_ENABLE_TASKS=0` 时出现(两种都测了,那个 flag 用 4 个工具换 1 个,别的没变)。原写"抓包里会有 `TodoWrite`"只在那个 flag 下成立。
3. **`Task` 是客户端内部名,线上名是 `Agent`。**

**还有一件改变了工序的:描述在与实现不符时会说谎,而已经有实例。** `Read`/`Write`/`Edit` 的描述断言了**这个 harness 不保持的客户端状态** —— *"You must Read the file in this conversation before editing, or the call will fail"*。逐字抄下来就是 DESIGN 说的那件事。所以"把描述抄下来"这一步**必须**接上"逐句问实现做不做得到",而做不到的那句要去掉 —— 这不是保险起见,是已知至少有一句做不到。

**每一件都按同一道工序:把描述抄下来,再逐句问实现做不做得到。**

> **一个描述在说谎的工具,比一个形状新颖的工具更糟。** 后者只是没吃到红利;前者是主动误导。

做不到的那句:**要么实现它,要么别抄那句,要么别提供那个工具。** 三条都行,唯独不能留着一句做不到的描述。

**判据**:每个新工具的每个声明参数,都有一个驱动它的测试;`Edit` 的精确匹配与唯一性各一个反例测试。

---

## 5 A/B

- [x] 同一任务、两套 profile(对齐的 / 原来的)、比结果

**实测(2026-10-13),结果写进 `docs/MEASUREMENTS.md`。** 同一任务、同一模型、同一沙箱,唯一变量是 `--tool-profile`:

| 臂 | 工具调用 | tokens in | out | cache_read | 步数 |
|---|---|---|---|---|---|
| **A 对齐** | `Read`×6 `Edit`×3 `Bash`×4 = 13 | **3742** | 1728 | **19840** | 8 |
| **B 原来** | `read_file`×4 `list_dir`×2 `exec`×3 = 9 | **1725** | 1464 | 9984 | 7 |

**两条臂都成功**(3/3、104 字节一致、零 `pending` 残留,在 harness 外复核),而**对齐的那条更贵**:输入 token 约 2×、调用约 1.4×。原因是抓来的描述长得多,而描述是**每次请求**都要付的。

**顺带推翻了本步骤给 `Edit` 的理由。** 原写"没有它,每次改动都是整文件重写:既贵又严重偏离分布"。臂 B 没有 `Edit`,它**没有重写任何文件**,它用了 `exec` + `sed`。执行者永远有 `exec` —— 缺一个专用工具不会退化到重写,而是退化到 shell。这与 `Grep` 那条是同一个模式。

**这条测量没说的事**:一次任务、每臂一次。任务太容易,两条臂都有一行 shell 答案,所以它分不开"对齐的工具面在编辑语义上更好"和"这个任务不需要编辑语义"。要分开它,需要一个 `Edit` 的精确性真的吃紧的任务(目标串出现多次、或空白是承重的)。**在那之前,诚实的读法是:对齐有实测的代价,没有实测的收益。**

---

## 不做

- **决策层的工具面不动。** `delegate`/`decide`/`retire`/`ask_human`/`spot_check` 按构造是新的,没有模型被训练过;而它们是简单类型化调用,后训练红利本来就最小。
- **实现不了的工具不要放进 `tools` 数组。** 抓包里会有 `TodoWrite`、`WebFetch`、`NotebookEdit`。给了又做不到是另一种偏离分布。
- **不要把形状焊进代码。** 约定在动(`Read` 加了 `offset`/`limit`),而且哪个模型被训练的是哪套取决于模型。profile 是可刷新的快照。
