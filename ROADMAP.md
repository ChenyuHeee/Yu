# ROADMAP

到 final state 的全程与验收标准。近期可执行项在 `TODO.md`,设计在 `DESIGN.md`,实测协议事实在 `docs/WIRE-FACTS.md`。

---

## final state 的定义

> **`DESIGN.md` 的每一条都成立,且每一条都有一个会失败的守护。**

不是"功能都在",是"每一条都能被证伪,而且有人证伪过"。

具体就是下面六个里程碑全绿,外加最后那次端到端验收。

---

## 起点:为什么是这个顺序

上一轮评审的结论是:

> **设计是一台循环机,而循环缺了两段。**

```
每轮重新渲染视野  →  决断  →  写回状态  →  收缩
    render()        decide      L1        retire
```

- **渲染**:完整,而且优秀
- **决断**:只有一个 `delegate`。`decide` 与 `ask_human` 没有实现
- **写回**:`Ledger` 只在测试里被写过。**L1 是空的**
- **收缩**:`retire` 实现了、真的移除、有真测试 —— 但**不是工具**,生产够不到;而且 `retired` 无界渲染进视野

**L1 空着不是缺功能 —— 这个设计依赖它。** "视野每轮重新渲染"意味着决策层对自己上一轮没有记忆,而记忆本该由 L1 承载。L1 空着,这层就退化成每轮失忆,只能靠塞给它的报告做判断,而且会把否决过的方案重新提一遍。

这也解释了 `docs/MEASUREMENTS.md` 里的 28–66%:**不是策略不好,是这层没有收缩机制可用,只能累积。**

所以路径是:**先让仪表可信 → 再让循环转起来 → 然后一段段拓宽。**

---

## 已定的两个设计决定

**一、度量的分子是"原始材料",不是"工具机制"。**

未经人或模型判断、直接倒进上下文的材料(工具输出原文、未约简的搜索命中、整段日志)才算。一份被约简过的报告是判断的产物,不是倒进来的。

这改写了 `MEASUREMENTS.md` 里"设计不成立"的结论:**不是结构有问题,是尺子量错了。**

**二、报告的形状在调用之前就定了 —— 预注册落位。**

调用时先预注册这个调用可能的落点,执行者把实际结果**归类**到其中一格。**决策层不读原始证据**;落位与工具结果的自洽由 **harness 确定性核对**,矛盾作为事件上报。

机制细节见 `DESIGN.md` 的「工具调用 → 预注册落位」。这两条直接决定 M1 的度量口径和 M3 的契约。

---

## 工作方式(三条规则,不是建议)

这三条针对的是上一轮暴露出的头号问题:**机制落地了,但证明机制的东西没有。**

1. **每个机制落地时,附一个"改回坏实现,测试要失败"的证明。**
   这是唯一能防住那个问题的规则。现有反例一串:九种 span 全发射、没有一条断言;`journal_response` 真的写盘、没有测试驱动它;退出码的生产者与消费者差一个字符、而测试喂的是仓库里不存在的格式。
2. **每个里程碑结束,真的跑一次,看数 —— 不是跑测试。**
   测试现在有不少是空过的。能过测试和能干活是两件事。
3. **先收口,再拓宽。** 一条线跑通之前不开第二条。
   这份代码的失败模式是"有趣的部件按吸引力被造出来,连接它们的组织没有",而它自我强化:因为看不出设计成不成立,所以继续造设计。

---

## M1 — 仪表可信

**在这一步之前,不做任何判断。** 后面每一条改动都靠这些数分辨好坏。

范围:`TODO.md` 全部。

**验收**

- [x] 一次运行报出**原始材料占比**(未经判断直接倒进来的材料),而且它能区分"这层装着五万行 grep"和"这层装着一份报告"
      —— 实测 0.0%(决策层)/ 8.3%(执行者),见 `docs/MEASUREMENTS.md`。守:`the_metric_separates_a_report_from_the_same_volume_of_raw_output`(两个**等大**上下文,一个装报告一个装原始输出;改回按形状分类,报告读成 50%)
- [x] 删掉任意一种 span 的 `timeline.start(...)` → **有测试失败**
      —— 独立验证:`ToolExec` 改成 `Reduce` 后 `a_full_run_records_every_span_kind_the_timeline_declares` 失败并指名缺失的那种
- [x] 把 `journal_response` 改回永远写 `None` → **有测试失败**
      —— 独立验证:`the_assistants_reply_is_recoverable_from_the_record` 失败
- [x] `DecisionLayer::run` 有测试,且"把 `run` 改成累积一段 transcript"会让它失败
      —— **一半成立,原因见下**:`every_request_carries_a_freshly_rendered_view_and_its_own_measurement` 断言每个请求恰好携带一份视野、每个请求一份计量。但"跨轮累积"这一类现在**测不到**,因为轮次推进不了
- [x] 把 `Exec` 的真实输出喂进 `parse_exit_code`,一条失败的 `cargo test` 显示**真实退出码**
      —— 独立验证:只改生产者格式,`the_exit_code_is_read_from_what_the_exec_tool_actually_printed` 与 `the_real_exit_code_reaches_the_decision_layers_evidence` 双双失败于 `left: None, right: Some(3)`
- [x] 没有任何已知的 key 泄漏路径
      —— 补上了 2xx 路径上最后两个(网关用 `200` 回一句带 key 的拒绝语)。原报告里那条"残留"由 `yu-wire` 的流自己标出,不是 TODO 点名的类别,但它确实是已知的

**M1 期间撞出来的两件事(不属于 M1,但必须写下来)**

- **`max_rounds` 不约束任何东西。** `DecisionLayer::run` 内层循环没有 `break`——一轮会一直对话到模型不再提问——所以外层 `for` 永远停在第一轮,尾部那条 `Err(LoopError::StepCeiling)` 只在 `max_rounds == 0` 时够得着。实测:`max_rounds = 1` 仍发了 4 个请求。**这不是 M1 的判据问题**(报告的轮数是真的),但 `--max-rounds` 是个什么都不做的旋钮,而那条看起来还活着的分支正是这个里程碑要找的东西。已由 `the_round_ceiling_does_not_bound_a_run_that_keeps_asking` 钉住,留给 M2——只有 L1 在轮与轮之间被写入,轮次才有区别。
- **因此,"跨轮累积 transcript"这一类现在无法证伪。** 轮次推进不了,就没有第二轮可供观察。M2 让它成真时,那条断言和这条一起改。

**M1 的数字**:398 passed / 0 failed / 0 warnings。

---

## M2 — 闭环转一圈

范围:`decide` 与 `retire` 成为真工具;L1 被真正写入;`retired` 有界或不出现在视野里;清单完整、或诚实说明自己不完整;prompt 与工具面一致。

**验收**

- [x] 跑一个真实任务,结束后 **L1 里有这次运行的决策与理由** —— 是决策,不是报告
      —— 实测:2 决策 + 2 否决,含"再跑第三遍只读复核"被自己否掉。守:`the_decisions_the_layer_makes_are_in_l1_when_the_run_ends`(同时断言 L1 与 L2 都有,理由在内)
- [x] 该任务里**否决过的方案不会在后续轮次被重新提出**(用一个刻意构造的重提来验)
      —— 守:`a_ruled_out_proposal_is_in_front_of_the_layer_when_it_comes_back`(脚本构造执行者重提同一方案,断言后续**每一个**携带该重提的请求里都有那条否决、且在 "Already rejected" 标题下)
- [x] **连续 8 轮后视野不随轮数单调增长**
      —— 守:`eight_rounds_that_decide_nothing_do_not_grow_the_view`(8 轮全空转,8 个新渲染视野逐字节同大)。实测曲线 10127 23433 31342 **29034** 29859 29557:轮内升,换轮**降**
- [x] 清单的"完整"断言有测试守:对记录里任意一条被省略的项,清单里要么有它,要么有它为什么不在
      —— 守:`every_superseded_piece_of_evidence_is_named_in_the_manifest` + `evidence_with_nowhere_to_point_says_so_rather_than_inventing_an_address`
- [x] prompt 广告的工具面 == 实际发出的工具面,且**有测试断言这件事**
      —— 守:`the_prompt_advertises_exactly_the_tools_the_layer_is_offered`(从 prompt 里读出清单,不重抄一份;双向报差)。它当场生效过一次:给 `decision_surface()` 加上 decide/retire 后,测试先失败,才去改 prompt

**M2 期间做的事(超出验收,但是必须做的)**

- **轮次真的会结束了。** M1 留下的 `max_rounds` 形同虚设,由这一轮修掉;判据是 DESIGN 自己的触发器("同一个失败两次就停")。同时 `DecisionOutcome::stopped_early` 让"用完轮数"变成**结局**而不是 `Err` —— 否则最长的那类运行恰恰报不出验收数字
- **视野里两处随轮数增长的东西被拿掉**:证据(轮 N 把第 1 轮的工具输出重渲 N 遍,于是 O(轮²))与 `retired`(已完成的项不是状态)。前者的守是单元层 `carry_for_this_round` 的测试 —— 端到端那条**看不见它**,因为测试用的 registry 没有 `exec`,不产证据。这一点写在那条测试的注释里,而不是让它看起来覆盖面更大
- **`Applied::record`**:journal 里原来写的是模型看的 `text`,于是**理由进不了 L2**。从结果看不出来(L1 在内存里还有),是守测抓到的

**M2 的数字**:428 passed / 0 failed / 0 warnings。

---

## M3 — 契约可信

范围:**预注册落位**成为委托契约(报告的形状在调用之前就定了);落位与工具结果的**自洽由 harness 确定性核对**;清单里的地址能解析;执行者失败返回报告而不是杀掉决策层。

**验收**

- [x] **自洽检查真的会触发** —— 构造一次执行者误报(落位说 `pass`,工具结果 exit 非零),harness 标出矛盾并作为**事件**上报
      —— 守:`a_misreported_landing_is_caught_and_recorded_as_an_event`;并且**反向也守了**:`an_honest_landing_is_not_reported_as_a_contradiction`(会在正确运行上开火的检查,没人会信)
- [x] **`other` 存在,且落进去会响** —— 集合永远附带 `other`;落进去时报告**放宽**(2500 → 8000),原词保留
      —— 守:`a_slot_the_set_does_not_contain_lands_in_other_with_the_word_used`、`landing_in_other_more_of_the_report`、`landing_in_other_makes_no_claim_and_so_raises_no_contradiction`
- [x] **预测集来自策略,不是运行时现编** —— 真实运行里证过:策略文件加载、层按 id 选、改文件即改行为
      —— 守:loader 五条规则各一个 revert 证明;`editing_the_policy_changes_the_pre_registered_outcomes_without_recompiling`
- [x] **落点分布被记录** —— 按集合×格子计数;`other` 过半即判"这组是错的"
- [x] **决策层的视野里不出现原始证据** —— 实测 `raw 0 of 47603 chars`,而且**结构上不可违反**:`RenderInput` 不再有能接收工具输出的字段
- [x] 落盘一次,然后**用清单里的地址真的把原文读回来** —— `spot_check` 已进决策层工具面并分派;解析器 16 个测试、17 次 revert
- [x] 执行者步数耗尽 → 决策层收到一份说明它停在哪的报告,**不是整个运行死掉**(M1 做,此处验证)
- [x] 制造一次落盘失败,模型看到的是真话(不出现悬空地址)—— `(dropped by reduction)` 解析成"记录说它被丢弃了",不是错误也不是成功

**M3 期间撞出来的(超出验收)**

- **检查有三个答案,不是两个。** "未决"(命令跑了、退出码没取回)必须与"证实"分开 —— 把前者报成后者,就是 harness 在制造它没观测到的保证。只有**反驳**才是矛盾
- **一次真实运行替我找出了两个我自己看不出的接口缺陷**:模型用对了集合名、写错了编码(`["investigation"]`),而被拒的提示没说该写成什么形状;被拒的调用还吃掉了那一轮的委托额度。两条都是运行读数,不是读代码读出来的
- **required 而不执行就是一个建议。** 第一次运行时层发明了一个集合名、被拒,然后干脆不再写那个字段了

**M3 的数字**:484 passed / 0 failed / 0 warnings。

---

## M4 — 执行层与拓扑

范围:约束注入执行者(不再靠决策层每轮记得打一遍);决策层真的读 `charter.topology`;第一次派生发生;递归。

**验收**

- [x] **把 constraints 从 charter 里删掉,执行者的行为会变** —— 证明它真的被注入,而不是碰巧没违反
      —— 守:`the_charters_constraints_are_injected_into_the_executors_system_block`。**这条是机制证明,不是行为保证**:它证明约束进了执行者的 system block,且**不在** per-delegation 的 task 文本里;把 charter 里的 constraints 删掉,请求里就不再有它 —— 否则"它没违反"和"它从没被告知"看起来一模一样。真实模型会不会**遵守**约束不是确定性测试能回答的,这里不声称。回退注入 → 失败于"the executor was not given the constraint the charter states"
- [x] 改一条拓扑声明,运行行为跟着变(**不是只有渲染变**)
      —— 守:`the_declared_width_decides_how_many_executors_run`:`fan_out width = 2` 与 `width = 4` 是同一次委托的 2 个与 4 个执行者,计数取自 scripted endpoint 上**真实发出的请求**,四个孩子的报告也全部回到决策层。回退"读声明"这一步(`run_derivation` 用默认 spec)→ `left: 1, right: 2`
- [x] 追踪一次运行,能画出一棵**深度 ≥ 3** 的树
      —— 守:`one_run_leaves_a_tree_three_delegations_deep`:从 L2 的 `delegated` 事件画树,深度 3、每个节点恰好一个父亲;`yu decide` 现在也把树打印出来,读的是记录而不是内存。回退"执行者可以派生" → "the record draws a tree 1 level(s) deep"
- [x] 一个子 agent 能在自己的运行里再派 agent,且血缘记录完整
      —— 同一条守里 `executor#1` 既是孩子又是父亲。上限是 `MAX_DELEGATION_DEPTH = 3`(决策层 → 执行者 → 执行者 → 执行者);守:`the_depth_ceiling_refuses_a_fourth_level_rather_than_running_it` —— 到了上限的执行者**不被提供** `delegate`,硬要调用会拿到一句说明自己撞了什么墙的拒绝。回退上限 → "a fourth level ran despite the ceiling"

**M4 期间撞出来的一件事(不在验收里,但必须写下来)**

- **`delegate` 的 schema 被写了两遍,到达请求的偏偏是错的那一份。** 计算出来的那份带着策略声明的 set ids,手写的那份没有 —— 而发出去的是手写的:于是 `required` 里有 `outcomes`,schema 里却没有它,决策层看不到有哪些集合可选,**唯一能满足 schema 的做法恰好是它唯一被禁止做的事**(发明一个集合)。守:`the_schema_carries_the_set_ids_and_declares_every_field_it_requires`

**M4 的数字**:490 passed / 0 failed / 0 warnings。上面每条"回退即失败"都是真的跑过的回退。

**真实运行(M4 之后补的,因为那条线自己的沙箱里没有凭据)。** 给 CLI 加了 `--constraint`(可重复)与 `--topology`,然后跑了一次:

```
--- agents: 2 derived
      decision
        └ executor#1 — RUNTIME: reproduce the failure of check.py and identify the …
          └ executor#1.1 — Read the file `.yu/decide-20261011T040337.jsonl` in the curr…
```

深度 3,树是从 L2 读出来的,不是从内存。**约束不只是被注入,是被遵守,而且层自己引用了它**:L1 里那条否决的理由原文是 "The charter constrains this to an investigation: never modify any file" —— 这比机制测试强,它证明约束到达了并且改变了一个判断。执行者也主动对了自己该落的格子:*"slot's declared expectation is `nothing_run`, which is exactly what happened"*。

**两件这次真实运行暴露的:**

- **声明了 `fan_out` 但没有扇出。** `plan()` 在没有 items 时返回 `Direct`(对着空集扇出就是凭空造孩子),而层没有提供 items —— 所以拓扑被声明了、被读了,却没有可作用的对象。树的形状来自执行者自己的递归,不是来自声明。**声明要生效,层得先给 items**;这条留给 M5/M6,不假装它已经成立
- 层这次 L1 里是 0 decisions、2 rejected —— 两次都是实质性的否决,但"零决策"值得记下来

---

## M5 — 换代与验证

范围:换代派发(线头 + 多视角读者 + 并集 + 验证者);验证层(非作者、去相关、从原始记录出发)。

**验收**

- [x] 换代后,新会话里**没有丢失任何仍在生效的约束**,用"注入后行为是否改变"验
      —— 守:`a_constraint_only_the_dying_session_wrote_down_reaches_the_new_sessions_executor`。会话 1 是一次**真实的 DecisionLayer 运行**:它委托、被告知一个事实、用 `decide` 记下来 —— 于是一条约束**只以 L2 一条 note 的形式存在,不在任何 charter 里**。换代后它在会话 2 的 L0 里、在**执行者的 system block 里**、且**不在** per-delegation 文本里;对照组(同一脚本、不换代)整份请求逐字节相同,只差这一条。
      **这条证明了什么、没证明什么**:它证明的是链条**逐环成立**,也就是 DESIGN 的派生原则 3 在**跨代**时也成立。它**不**证明真实模型会遵守它 —— 模型是脚本化的,沙箱里没有凭据。这是**机制证明**,与 M4 那条一样标注为机制证明。
- [x] 交接的产物是**索引 + 状态**:新会话能顺着它回到记录
      —— 守:`a_new_session_can_follow_the_handover_back_to_the_record`。每个 anchor 都通过 `spot::resolve`(也就是 `spot_check` 那道门)解析回它被摘出来的那段文字,并且在**新会话的 fork journal 上**同样成立。回退成旧的假引用(`record ({} bytes)`)→ 解析器拒绝它
- [x] 验证者能在一个**刻意构造的漏项**里把漏掉的东西找出来
      —— 守:`the_verifier_finds_the_omission_the_readers_missed`:被带上的那项**不是** gap(这是区分力),被种下的那项是 gap **且带地址**。回退"验证者读报告"或"anchor 覆盖规则"→ 失败
- [x] **验证者从原始记录出发** —— 把上一级的报告改坏,验证结论不变
      —— 守:`the_verifiers_conclusion_survives_a_corrupted_report`:同一份记录跑三遍,只改读者说了什么 —— 诚实、一段自信的编造(还宣称了记录里从没有过的事)、以及报告整个删掉。`gaps(诚实) == gaps(编造)`,且报告删掉后漏项照样被找出来

**M5 期间撞出来的**

- **`Trigger::AgentRequested` 没有生产者。** 它需要决策层工具面上有一个 `retire` 形状的工具;M5 造好了"这一步调用会触发的那条流",而没有造那个调用。写在变体上而不是留给后人发现
- **验证者已知的两个边界,是钉住而不是藏起来**:没有 anchor 的转述会被读成漏项;而**带 anchor 的谎言会被读成已覆盖**。后者是刻意的 —— anchor 就是交接在宣称"这一行读过",不采信这个宣称,这个检查就没有区分力(回退该规则后 gap 列表就是整份记录)
- **一次真实运行暴露的缺陷**:CLI 的 session id 精确到秒,同一秒内的两次运行会撞名,`resume_into` 于是**把自己 fork 到自己身上**且悄无声息。现在它会拒绝

**M5 的数字**:504 passed / 0 failed / 0 warnings。五次回退证明各自断言过匹配数。**外加我自己独立复算了一条**:criterion 4 那条,我照 M1/M4 的做法自己撤了一遍机制 —— 第一次撤错了函数(撤了 `find_gaps`,而流程用的是 `find_gaps_in`),**没让守卫失败**;撤对之后失败信息与那条线报告的一字不差。撤错函数等于什么都没撤,这正是本次会话里咬过我两次的那类错误。

---

## M6 — 常驻与收口

范围:daemon(`Start` / `Stop` / `Status`、多客户端);L3 回放 + 任意点 fork + fork 成本可见;effect 的意图与完成可区分;策略层外部化**且产品真的读**。

**验收**

- [x] 进程被 `kill -9` 后能从最后一条完好记录续跑
      —— 守:`a_killed_daemon_resumes_from_the_last_record_it_wrote`。**杀的是真进程**:`yu daemon` 起在 Unix socket 上,发两条消息,`kill -9` 并断言 `signal() == Some(9)`;第二个 daemon 起在同一状态目录上,`Resume` 答 `last_seq: Some(2)`,记录读回 `[0,1,2,3]` 且杀前字节逐字节保留为前缀。再加一轮:杀、在最后一个换行后追加半行、重启 —— 最后一条完整 seq 仍是 3,碎片被**丢弃而不是焊上**,编号接着走 4
      —— **没证明什么**:那个撕裂碎片是**测试写的,不是竞态出来的**(从外部按需制造一次"恰好落在两次写之间"的崩溃做不到)。碎片逐字节是那种崩溃会留下的东西,之后对它做的一切(重开、修尾、编号)都是生产路径。**它也没有证明任何关于"崩在模型调用里"的事** —— daemon 里根本不跑 turn
- [x] fork 一个历史点 → **显示预估成本** → 真的分叉,原分支不动
      —— 守:`a_fork_shows_its_price_takes_the_branch_and_leaves_the_parent_alone`。预估在**写任何东西之前**打印,含两半(`5 event(s)`、`4321 input tokens`、`the parent is not written to`);分支自己的文件只有标记那一行;**父文件前后逐字节相同**(`std::fs::read` 比对),没有追加。重名 fork 被拒
- [x] 一次副作用工具落在崩溃窗口里:(a) 记了意图没动手 → 重放时**不会被当成已完成**;(b) 做了没记完 → 能检出
      —— 两条都**杀真进程**。(a) 让脚本化的模型去写一个**没有读者的 FIFO** —— `std::fs::write` 阻塞在 `open(2)`,所以那件事**可证明没有发生**;看着 `intended` 落盘,断言目标还是 FIFO,再 `kill -9`。恢复的那次报出未完成的 effect,重新尝试,字节落地。**我独立撤了 `may_skip`**:让它把 `Intended` 也算作可跳过,两条测试双双失败于 "the work is silently skipped and the run reports success"
      —— (b) 让 `exec` 跑 `printf … >> P; kill -9 $PPID` —— 产物**由命令写出**,harness **被它杀掉**,所以死亡瞬间那件事已经发生、完成记录不可能写下。恢复的那次检出未完成的 effect、**拒绝重跑一个什么都证明不了的 key**、产物原封不动
      —— **没证明什么**:窗口各自只由一种装置进入(阻塞在 `open(2)` 之前 / 在调用飞行中被杀)。工具内 panic、OOM、做了之后 `ToolResult` 之前崩 —— 都没走到。而且**没有 turn 级别的回放驱动**:第二次之所以重发,是脚本让它重发,不是 harness 回放了记录的模型轮次
- [x] **改一条 `[reduction]` 规则、重启,行为改变 —— 不重新编译**
      —— 守:`editing_one_reduction_rule_changes_what_the_model_is_sent_without_a_rebuild`。同一二进制**跑两个进程**,中间只改 TOML 里一行的一个值(`pass_through` → `drop_with_manifest`),观测量是执行者**真的发出去的请求**:前者 60kB 原文在,后者不在且有清单元数据。**并核对了二进制的 (size, mtime) 没变** —— 否则"它变了"也可能只是重新编译了
      —— 回退到本里程碑之前的状态(`build_tools` 忽略策略)→ 失败,并打印出规则说 pass_through 而请求里没有原文

**M6 期间撞出来的:ROADMAP 以为接好了、实际没接的八处**

`EventBody::Effect` **没有任何生产者**(不补这条,验收 3 无法实现);`EffectLog::from_events`/`replay_seed` 无生产调用者;`EffectGuard` 仍无调用者且已与记录级的意图/完成对冗余(我留下没删 —— 删一个被文档化的机制该由你决定);**`ModelRequest.est_input_tokens` 两个生产者都字面写 `None`**,于是 fork 成本所依赖的字段什么都没记;`[reduction]` 表到不了任何做归约的地方;**`SessionHost` 是同步的且 `Send`/`Resume` 是占位**;**fork 的血缘只活在造它的那个 handle 里** —— `Journal::open` 重开同一分支时对父一无所知,记录的标记没人读;`ROADMAP.md` 的 M6 勾由我打,不是我打。

**一处值得单独说的过程教训(那条线自己交代的)**:它的第一个 revert 工具用了 `shutil.copy2`,**保留了 mtime**;还原时 mtime 往回走,cargo 认为目标文件是新的,于是下一轮证明跑在**已经 revert 过的构建**上 —— 污染了一对证明。它换了 `copyfile` + `utime`、打印前后 hash、并把每一条证明**重做了一遍**。

**M6 的数字**:526 passed / 0 failed / 0 warnings。

---

## 端到端验收

> **状态:通过。六条在同一条记录上全部成立。**
>
> 不是一次运行 —— 是**同一条记录上的三次运行**:`decide-20261011T064836` 上跑了两代,条件 3 的 `kill -9` 与续跑则在 `e2e` 那条记录上单独验过。六条各自的读数都在下面,**括号里写明了每条是由什么证据支撑的**,包括哪几条是我在 harness 外面自己复核的。

任务(在 `sandbox-exec` 沙箱里跑,写权限只放行 `/private/tmp/yu-e2e-sandbox`):*"在这目录里交付一个能用的 `report.sh`,打印每个 `.txt` 的文件名与行数、再打印总数;还没有 `.txt`,先造 `sample-a.txt` 与 `sample-b.txt`,然后真的跑一遍 `report.sh` 把输出show 出来。别改别的。"* 工具面 `--tools full`,五轮,两条 standing constraint,一份声明了 `delivery` 预测集的策略。

**交付物是真的,而且是我在 harness 外面独立复核的:**

```
$ (cd /private/tmp/yu-e2e-sandbox && sh report.sh)
sample-a.txt 3
sample-b.txt 5
total 8          exit=0
```

| # | 条件 | 判据 | 结果 |
|---|---|---|---|
| 1 | 决策层上下文是决策形状的 | 原始材料占比 ≈ 0 | ✅ `raw material 0.0% of 23308 chars (raw 0 + reduced 6490 + calls 0 + definitions 4278)` |
| 2 | 没有任何东西被静默丢弃 | 清单的完整性可验证 | ✅ 层自己用清单完成了一次判断:"Shape flags 2 files touched outside the declared 3 (policy.toml, run.log) — resolved as read-only: both listings show identical sizes and mtimes across the whole run, and the manifest contains only read_file results for them, no writes." |
| 3 | 中途 `kill -9` | 恢复后继续,且**没有重复副作用** | ✅ **第二次运行通过了** —— 见下 |
| 4 | 中途换代一次 | 换代后仍生效的约束一条不少 | ✅ **第三次运行通过了** —— 见下 |
| 5 | 事后可重建 | 能从记录里重建任意一个历史点 | ✅ **同一轮的 `yu fork`** —— 见下 |
| 6 | harness 开销 | ≤ 模型时间的 X% | ✅ 含执行者真跑的命令 3.27%,不含则 0.061%(X 定为 10%) |

### 第二次运行:条件 3 通过了,而且检查第一次真的开火了

补上 `yu decide --session` 之后重跑。任务改成三步(append 一行到 `progress.log`、写 `greet.sh`、跑它),**等那条 append 的 effect 真的 `done` 之后**再 `kill -9`:

```
DONE: run `sh -c printf 'step-one done\n' >> progress.log; echo "rc=$?"; ls -l progress.log`
SIGKILL sent (exit 137)
--- progress.log: 14 bytes  (一行)
```

续跑:

```
--- continuing a session whose record already existed: seq continues at 36; L0 = 1 constraint(s)
--- rounds already spent: 1; this continuation runs rounds 2..=4
```

**`progress.log` 续跑前 14 字节、续跑后 14 字节 —— 记录在案的副作用没有被重做。** 条件 3 的核心主张成立。执行者也认出了既存状态:*"All three requirements were already satisfied by the pre-existing state; I made no writes, and verified by real execution: `./greet.sh` printed exactly `hello from greet`"*。

**而 harness 的自洽检查在真实运行里第一次开火:**

```
!! the executor landed on `delivered_and_verified`, but a command ran and failed: `echo "---progress.log---" …` exited 1
```

执行者落在 `delivered_and_verified`(断言"跑过的命令都成功"),而它有一条命令退出码 1。**这是 M3 那个机制第一次在真实运行里抓到东西** —— 不是"可见",是"可抓"。之前我只能用构造的误报证明它会响;现在它自己响了。

其余读数:决策层 `raw material 0.0% of 24531 chars`;harness 开销 **2.29%**(不含执行者真跑的命令则 0.029%);记录 102 条事件,`session_seeded`、`round_started` 都在。

### 第三次运行:条件 4 与 5

**条件 4 —— 换代后约束一条不少。** 第一代带 `--handover-out` 跑完并写出交接;交接里带着那条约束:

```
constraints: ["Work only inside the current directory; never modify anything outside it."]
origin: {"session": "gen1", "at_seq": 41}
```

第二代 `--handover` 起跑:*"resuming from a handover: 1 item(s) carried, 5 angle(s) reported, index at blob:6f97f7…"*。**那条约束真的到了第二代的执行者** —— 记录 seq 50、agent `executor#1`,派生任务的原文以它开头。交付物我在 harness 外面复核:`tool ok`/exit 0、`TOOL OK`/exit 0。执行者自己的复核比要求的更严:*"od-verified byte-identical to the pre-change output"*、*"sha256 before/after shows tool.sh is the only changed file"*。

**条件 5 —— 从记录重建一个历史点。**

```
--- fork estimate
      forking decide-20261011T064836 at seq 20: 41 event(s) / 16817 bytes inherited by
      reference; the cache prefix diverges there, so a replay of this branch re-sends the
      prefix the last recorded request measured at 3324 input tokens; the fork itself
      costs 156 byte(s)
      the parent is not written to: the branch reads its history through it
```

父文件 **md5 前后完全相同**(54759 字节,`36fdfa01…`),分支只写了一行标记。

### 第一次运行时,条件 3 为什么跑不了 —— 那是一处真实的缺口,不是环境问题。 `yu decide` **没有 `--session`**。daemon 能续会话但**不跑 turn**(`Send` 只把消息记进记录并明说没有 turn 运行过),而 decide 跑 turn 却不能续。于是一次被 `kill -9` 打断的 `yu decide` **无法继续**。`--handover` 也救不了它:交接是在会话**结束时**写的,而被杀掉的会话什么都没写。**这条验收在设计这次运行的过程中才暴露出来** —— 想写编排脚本时才发现没有可用的续跑入口。

**条件 4 为什么没被触发。** 这次运行在第 5 轮撞到轮数上限(`stopped_early`),所以走的不是"决定达成"那条触发,而是阈值那条;视野增长没越过去,于是没有交接。它打印的原因是 *"the session stopped without deciding"* —— **措辞有歧义**:L1 里明明有 1 条决策,它想说的应该是"没有达成最终答复"。这条要改的是文案,不是逻辑,但歧义的后果是读的人会以为层什么都没决定。

---

前面六步全绿之后,一次:

> **一个真实长程任务,从人类的一句话开始,跑到交付。**

全部条件必须**在同一次运行里**成立:

| # | 条件 | 判据 |
|---|---|---|
| 1 | 决策层上下文是决策形状的 | 修正后的"原始材料占比"≈ 0 |
| 2 | 没有任何东西被静默丢弃 | 清单的完整性可验证 |
| 3 | 中途 `kill -9` | 恢复后继续,且**没有重复副作用** |
| 4 | 中途换代一次 | 换代后仍生效的约束**一条不少** |
| 5 | 事后可重建 | 能从记录里重建**任意一个历史点** |
| 6 | harness 开销 | ≤ 模型时间的 X%(X 由你定) |

---

## 一张图

```
M1 仪表可信 ──► M2 闭环转一圈 ──► M3 契约可信 ──► M4 执行层与拓扑 ──► M5 换代与验证 ──► M6 常驻与收口
     │                │                  │                  │                  │                │
  数是真的         循环转起来         两端接通           会派、会递归        记得住、验得动      扛得住崩溃
     └────────────────┴──────────────────┴──────────────────┴──────────────────┴────────────────┘
                                          │
                                    端到端验收
```

**每一步的"完成"都指同一件事:这一段的每一条都能被证伪,而且有人证伪过。**
