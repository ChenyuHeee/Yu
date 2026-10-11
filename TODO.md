# TODO

这一轮的目标只有一个:**让仪表可信**。全程见 `ROADMAP.md`。

理由:这份代码里的机制大多是真的,但**证明机制的东西大半没有**。在仪表可信之前,后面每一条改动都无法判断是好是坏 —— 而这正是上一轮"机制落地、证明缺席"会自我强化的原因。

---

## 立刻(小,但属于"某天会失去一切")

- [ ] **`crates/yu-wire/src/sse.rs:256` 加 `redact_key_material`** —— 流内 SSE `error` 事件的 message 原样包进 `WireError::new`,而那个 match **显式映射 `authentication_error`**,正是消息回显 key 的那一类。这是最后一条已知的 key 泄漏路径,一行
- [ ] **`exit: ` vs `exit `** —— 生产者写 `exit: {code}`(`yu-tools/src/native.rs:362`),消费者在最后三行找 `"exit "`(**空格**,`yu-core/src/delegate.rs:357-363`)。真实输出下**每次返回 `None`**,所以视野里全是 `exit unknown`。而且它只看尾部 —— 一条失败命令若 stdout 尾部恰好有 `exit 0` 会被报成**通过**。证据块是设计里的安全带,现在它不通电
- [ ] **删掉旧的 `measure_composition`,或让它委托给新 meter** —— `render.rs:735-764` 仍硬编码 `tool_call_chars: 0`,喂给 `View.composition`;它只是"没人读"才死的,而那个字段的文档**仍宣称自己是验收指标**
- [ ] **修 `docs/MEASUREMENTS.md` 那段示例输出** —— 它的 span 行没有 `journal=`,而 `cmd_run` 把 timeline 接进了 journal,每次运行至少追加两条记录。**当前构建产生不出那段输出**,而紧挨着的话正是用它来论证补上三个 span 之后这个数才有意义

---

## 主题一:把度量改成判据真正问的那个数

- [ ] **度量不要按轮累加** —— `meter` 把每一轮的 system + tools + view 相加,所以 system prompt 和工具定义被数了四遍。"28.8% of 33766 chars" 是四个独立上下文的并集,不是判据问的那个数,而且**偏差朝好看的方向**
- [ ] **charter 不要在 L0 里写两遍** —— `main.rs:492-503` 同时设 `Charter.text` 和 `human_words[0].text`,两个都被渲染。最该省的那层每轮付两次钱,顺带撑大分母
- [ ] **补上增长场景** —— 视野是 O(轮² × 步):`outcome_from_run` 每次读整个 journal(无过滤),`build_view` 再把所有 outcome 的证据铺平,第 N 轮把第 1 轮的证据渲染 N 遍;证据本身无界渲染。**而只读的跑法恰好不产生 exec 证据 —— 上一轮的测量选了一个藏住最坏增长的场景**
- [ ] **决定分子的定义**(见 `ROADMAP.md` 的开放设计决定)—— 一份被约简过的报告,算不算"工具机制"?设计真正要排除的是**未经判断、直接倒进来的原始材料**,不是结论

**验收**:一次运行能报出一个数,并且能区分"这层装着五万行 grep"和"这层装着一份报告"。

---

## 主题二:给没有测试的机制补"会失败的"测试

标准是项目自己的:**删掉/改回坏实现,这个测试会失败吗?**

- [ ] **span 发射** —— 九种 span 都有活调用点,**没有任何测试断言过其中一种**。删掉任意一条 `timeline.start(...)`,现在什么都不会失败
- [ ] **`journal_response`** —— assistant 正文真的作为 blob 引用写盘了,**没有任何测试驱动它**。改回永远写 `None`,整个套件照样通过
- [ ] **`DecisionLayer::run`** —— **一个测试都没有**。验收关键的循环(每轮重渲染、委托上限、工具分派、composition 计量)只由对真实端点的 `yu decide` 覆盖。最接近的 `the_view_is_rebuilt_each_round_rather_than_appended_to` 直接调了两次 `render`,**`run` 改成累积一段 transcript 它照样通过**
- [ ] **`exit` 解析** —— 现有测试喂的是手写的 `"some output\nexit 0\n"`,**仓库里没有任何工具产出这种格式**。改成驱动 `Exec` 的真实输出
- [ ] **`cache_control` 能力探针** —— `body(with_breakpoint)` 定义了却只调过 `body(false)`,**`with_breakpoint` 是死代码**。在真实 Anthropic 风格端点上会报 `No` 并覆盖掉正确的默认值
- [ ] **`unobservable_features_are_not_reported_as_plain_yes`** —— 手搓一个 `Capability` 然后断言它等于自己,**从不调用 `probes()`**。把 `observable` 翻回 `true` 它不会失败
- [ ] **`YU_QUIRKS_JSON` 往返** —— 往返测试用 `json!` 字面量而不是 `serde_json::to_string(&Capabilities)`,改个字段名就能悄悄断掉管线;而且没有任何测试碰过这个环境变量
- [ ] **`EventBody` 对 `Forked` 的相等性** —— 手写 `PartialEq` 唯独漏了这个变体,落进 `_ => false`,所以 `Forked == Forked` 是假。那段 impl 的注释说新字段"必须在这里逼出一个决定" —— **兜底臂恰好废掉了这个意图**

---

## 主题三:修掉已知的行为错误

- [ ] **执行者撞上限不能杀掉决策层** —— `Agent::run` 只在 `Complete` 时返回 `Ok`,所以 `completed` 对任何存在的结局**恒为真**,那个 `DELEGATION STOPPED WITHOUT COMPLETING` 头描述的是生产到不了的状态;而步数耗尽时 `?` 一路上去,**整个 `DecisionLayer::run` 返回 `Err`**:没有 final_text、没有 composition 报告、没有验收数字。这是这个 harness 最常见的失败
- [ ] **委托循环检查工具名** —— 现在每个 `tool_use` 都按 `DelegateArgs` 反序列化,没有 `if tu.name == "delegate"`。一个照着 prompt 发的 `retire`/`decide`,只要 JSON 里有 `task`,就会被当成委托执行
- [ ] **`pause_turn` 统一处理** —— `decision.rs` 直接拒绝(理由:"这层没有服务端工具,当成 continue 会空转");`agent.rs:339-343` **正是当成 continue**,空转到 40 步。**错的是花钱的那个**
- [ ] **daemon 的目录扫描** —— 状态目录里每个条目都变成一个 `Crashed` 会话,包括 `blobs/`;而 CLI 正把 journal 和 blobs 写在那里。另外 `serve` 一次只服务一个连接,而且一个写错误(如 EPIPE)会顺着 `?` 把 accept 循环整个带下去
- [ ] **`read_range` 在 fork 分支上的上界** —— `until` 不在本分支索引里时回退成 `committed_len`,于是可能返回上界之外的事件

---

## 这一轮不做

闭环(`decide`/`retire`/L1)、拓扑、换代、验证层、effect 状态、策略接线、daemon 收口 —— 全部属于 `ROADMAP.md` 的 M2 及以后。

**但有一个例外要现在决定**:`ROADMAP.md` 里那个开放设计决定(度量的分子是什么)。它决定了上面"主题一"的验收怎么写。
