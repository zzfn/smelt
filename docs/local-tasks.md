# 本地任务列表

**先做本机任务编排，再挂远程 / 看板 / 飞书。**  
产品仍是驾驶舱「从想法到 agent」，不是 Jira / Linear 替代品。

**UI：**
- 侧栏「任务」：任务总览入口 · 新建 · 执行中快捷项  
- 主区 **任务总览**页（对齐会话总览：标题 + 状态 pill + 卡片网格）  
- 会话「总览」只做会话监控，**不**列任务

**开跑（当前唯一路径）：新开终端 + startup-arg**

```text
新开 smeltd 会话
  launch = `<base> "$(cat ~/.smelt/tasks/prompts/<id>.txt)"`
  例：claude --dangerously-skip-permissions "$(cat '…/id.txt')"
  column = Running
  agent 标题 spinner 消失 → column = Done
```

- **交互**会话，侧栏可见、可接管  
- **不是** `claude -p` 无头批跑  
- **当前终端上下文**：侧栏会话/分屏行右键，或终端区域右键（TUI 未抢鼠标时）→「新建任务」→ 开跑时键入+回车进该会话

配套：[collaboration.md](collaboration.md)、[remote-ops-roadmap.md](remote-ops-roadmap.md)、[roadmap.md](roadmap.md)。

---

## 目标

| 能力 | 本地版含义 |
|------|------------|
| **新建** | ⌘⇧N / 侧栏「新建任务…」：类型（普通/定时）、标题、首包 prompt、绑定 cwd / launch |
| **仅创建** | 状态=待办，卡片显示 **运行** |
| **定时** | 选「定时」+ 本地时间；到点由后台扫描自动 `run_task`（单次，不循环） |
| **运行** | 待办点「运行」：有绑会话则注入，否则新开终端 + 首包参数 |
| **打开** | 执行中/完成：只切到已绑终端 |
| **做完续跑** | 绑定任务 Done 后，同 cwd claim 下一条 **`auto_run` 待办** 自动 `run_task`（全局始终尝试） |
| **自动执行** | **任务级**字段：开 = 可被续跑/定时扫描取走；关 = 仅手动「运行」 |
| **自循环（方向）** | agent 写 TaskStore 塞队（`auto_run`）→ 完成边沿 drain → 同一套 launch 契约 |

**不做定位：** 完整项目管理、云端任务库、`-p` 无头批跑、总览任务看板、cron 循环。

---

## 数据模型

```text
Task {
  id,
  title,               // 给人看的侧栏名（可空→用 body 首行）
  body,                // 给 agent 的首包（开跑唯一进 CLI 的内容）
  column,              // 待办(backlog) | 执行中(running) | 完成(done)；旧 ready/waiting 读入后归并展示
  project_cwd,         // 在哪跑
  session_id?,         // smeltd 会话
  launch?,             // base 命令（不含首包拼接）
  kind,                // once | scheduled（缺省 once，兼容旧数据）
  run_at?,             // Unix 秒；kind=scheduled 时计划开跑时间
  auto_run,            // 是否允许系统自动开跑（缺省 true）；false = 仅手动
  created_at, updated_at,
}
```

落盘：`~/.smelt/tasks.json`；首包文件：`~/.smelt/tasks/prompts/<id>.txt`（内容 = body）。

**全局行为（无 config 总开关）：** 只要存在「待办 + `auto_run` + 可跑」的任务，系统会在合适时机自动执行。

**定时执行：** 每 30s 扫描 `auto_run && scheduled && 待办 && run_at<=now` → `run_task`（同 cwd 已有 Running 则跳过）。

**做完自动续跑：**

```text
spinner 落下（Running→Idle）
  → 绑了该 session 的任务 → Done
  → claim 同 project_cwd 下一条 auto_run 待办（FIFO / created_at）
  → run_task（新开终端 + startup-arg）
```

- 仅当本 session **确实收尾了任务** 才续跑
- 同 cwd **串行**；`auto_run=false` 的待办不会被 claim（仍可手动「运行」）
- 定时任务创建时强制 `auto_run=true`

---

## 分阶段

| 阶段 | 交付 | 验收 |
|------|------|------|
| **T0** ✅ | `Task` + 全局 store + 单测 | 重启不丢 |
| **T1** ✅ | 侧栏入口 + 主区任务总览页 + 新建弹窗 | 全量可管 |
| **T2** ✅ | 终端开跑（launch + startup-arg 首包） | agent 自动开干 |
| **T2.5** ✅ | 任务类型：普通 + 单次定时（`run_at` + 扫描） | 到点自动开跑 |
| **T2.6** ✅ | 完成边沿 → 同 cwd 自动 claim 下一条 | 队列串行续跑 |
| **T3** | 会话「钉成任务」、右键删/改状态 | 双向不割裂 |
| **T4+** | agent CLI 塞队 / 状态通道 / 远程 | 真自循环 |

---

## 与远程操作

远程看/控的是 **session**；任务列表负责 **何时、用什么 launch（含首包）产生并记住该 session**。
