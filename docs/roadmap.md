# smelt Roadmap / Backlog

待做点子的存档，做完了就从这里挪走或标 ✅。

## 系统感知能力（OS perception）

宠物 / 会话监控可以更「懂」用户当下在干嘛。按「获取难度 + 权限成本」分档：

### ✅ 已做：只读、零权限
直接用 `objc` 发消息，不需任何系统权限：
- `[NSEvent mouseLocation]` —— 全屏鼠标位置（宠物眼睛 / 身体跟随、AFK 检测）
- `[NSWorkspace frontmostApplication].localizedName` —— 当前前台 app 名（宠物切 app 评论 + 喂给大脑当上下文）

**未用任何 `enigo` / `tfc`（输入模拟）或 `xcap` / `scrap`（截屏）库**；`cocoa` / `core-graphics` 只是 gpui 的间接依赖，自有代码没碰。

### 🔲 输入模拟（enigo / tfc）
让 smelt / 宠物能**替用户操作**：自动点按钮、宠物真的用爪子拖窗口、快捷宏、一键把某段文本敲进当前输入框。
- 成本：加一个 crate（enigo 跨平台），无需特殊权限（但受辅助功能限制的某些操作除外）
- 风险：误触、抢焦点；要非常克制，最好用户显式触发
- 适合：给宠物加「叼着东西放到你光标处」这类**用户主动发起**的趣味动作

### 🔲 屏幕捕获（xcap / scrap）
让宠物 / agent **看见屏幕**：
- 会话卡片实时缩略图（现在总览是文字预览，可换成真·画面缩略）
- 宠物「瞄一眼」你在干嘛（截当前窗口喂多模态模型）
- ⚠️ 需要**屏幕录制权限**（首次弹窗授权），比读 app 名重得多；耗性能，要节流
- 建议：真要做先做「会话缩略图」这个明确有用的，别泛泛截全屏

### 🔲 更深的上下文（需辅助功能权限）
拿「浏览器具体在看哪个网址」「文档标题」——要开**辅助功能 (Accessibility)** 权限读 AX 树。收益不确定、隐私敏感，暂不碰。

---

## 会话持久化增强（smeltd）

现状：smeltd 用**内存 2MB 环形缓冲**存输出，守护重启即失忆。可借鉴 codex 的做法落盘：
- 会话历史写 JSONL（`~/.smelt/sessions/<date>/<uuid>.jsonl`，每行带时间戳的 typed 事件）
- SQLite（项目已有 rusqlite）做索引，支持会话列表分页 / 全文搜索
- 好处：机器 / 守护重启后可恢复完整历史、总览页能列历史会话
- 注：**不需要** PostgreSQL/TimescaleDB —— 本项目栈是 SQLite，本地单机 IPC 场景 SQLite 足够

## 其他 codex 借鉴项（详见调研）
- smeltd 加 JSON-RPC 结构化控制通道：会话状态从「解析字节流猜」升级为「协议事实」
- Claude Code hook → `smelt-notify` 小工具直写 smeltd socket（比解析 OSC 更可靠的第二信源）
- 会话卡片「运行了多久」计时、token 用量 / 上下文余量展示
- macOS Seatbelt 沙箱（`sandbox-exec` + SBPL 模板）跑 agent 命令

## 宠物
- 鼠标很近时凑上来 / 划过身体害羞挤压（距离 / 接触反应）
- Stage 3：打字跟宠物多轮对话（输入框 + 对话历史）
