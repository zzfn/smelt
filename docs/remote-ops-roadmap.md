# 远程操作 Roadmap

**产品概念：远程操作**——对 smeltd 里仍存活的 agent 会话，在**任意客户端**上查看、知情、操控与协作。

客户端可以是桌面浏览器、手机 H5、飞书机器人卡片等。  
「完整终端画面」「状态元数据」「可写输入」是远程操作的**能力分层**；**xterm.js 只是其中一种呈现**，不是远程操作本身。

配套：[collaboration.md](collaboration.md)、[state-channel-plan.md](state-channel-plan.md)。  
其它 backlog 见 [roadmap.md](roadmap.md)。

---

## 原则

1. **对象**是 smeltd 活会话，不是再开一个假 shell。
2. **API 优先，渲染可选**：网关暴露 stream / state / input / action；各端自己决定怎么画。
3. **不绑死 xterm.js**：完整 ANSI 终端只是 L1 的一种皮肤；手机与 IM 默认走「操作台 / 卡片」。
4. **可写是完整形态**；只读是安全默认与冷启动第一步。
5. **smeltd 会话脱离 GUI 存活**；远程不得顶掉主 GUI attach（fan-out 或只读旁路）。
6. 团队向：**接收方尽量零安装**（浏览器链接或已有飞书）；不做 WebRTC 全家桶（视频/多人会议那一整套）、内嵌 WebView、第二套 Jira——但 Phase 3 用 WebRTC 的 data channel 解决「两台设备不在同一个网络」这一件事，范围窄，不算破例。

---

## 能力分层（都叫远程操作）

| 层 | 用户感知 | 技术要点 | 现状 |
|----|----------|----------|------|
| **L1 远程查看** | 看到会话在输出什么 | stream API；客户端可选 xterm / 纯文本尾部 / 不看画面 | 协议有，产品未做 |
| **L2 远程知情** | 在跑 / 等你 / 问什么 | state + subscribe（+ hook） | 仅有方案 |
| **L3 远程操控** | 键入、粘贴、或「允许/拒绝/短回复」 | input / **action** API；phase 门闩 | 未做 |
| **L4 远程协作** | 链接给同事、飞书推送、认领移交 | token、机器人、收件箱 | 未做 |

---

## 多端呈现：三种形态（可并存）

**不要**假定所有远程操作都等于「网页里嵌一个终端」。

| 形态 | 是什么 | 适合 | 不依赖 |
|------|--------|------|--------|
| **A. 完整终端** | 吃 ANSI 流，画网格/TUI | 桌面浏览器深看 Claude 全屏 | — |
| **B. 操作台** | 大状态 + 问题文案 + 主按钮（允许/拒绝/回复）+ 可选最近输出摘要 | **手机 H5** | xterm |
| **C. IM 卡片** | 飞书/其它机器人推送状态变化；按钮回调 action | **飞书、以后企业微信等** | xterm、甚至不需常开网页 |

```
                    smeltd 活会话
                          │
          ┌───────────────┼───────────────┐
          ▼               ▼               ▼
     A 完整终端画面    B 操作台 H5      C 飞书卡片
     (xterm 等可选)   (state+action)   (state+action)
          │               │               │
          └───────────────┴───────────────┘
                          │
                    远程操作网关 API
```

### 关于 xterm.js

| 问题 | 结论 |
|------|------|
| 是前端终端组件吗？ | 是，跑在浏览器里，负责把 ANSI 画成终端 UI。 |
| 远程操作必须用吗？ | **否。** 仅当某客户端需要「完整终端」时采用。 |
| 手机表现？ | 小屏 + 软键盘 + 复杂 TUI 体验往往差；手机**默认 B 操作台**，「终端模式」作二级入口。 |
| 飞书？ | **不嵌终端**；走 C：卡片 + 按钮 → action API。 |

第一个参考客户端可以仍是「桌面页 + xterm」验证 stream，但架构上 stream 与 UI 解耦。

---

## 网关 API 草图（契约中心）

实现语言不限；**契约先于皮肤**。

| 能力 | 示意 | 用途 |
|------|------|------|
| 画面流 | `WS/GET …/sessions/:id/stream` | A：xterm 或其它终端组件 |
| 状态 | `GET/WS …/sessions/:id/state` 或全局 subscribe | B/C 列表与角标 |
| 原始输入 | `POST …/sessions/:id/input` body 字节/文本 | L3 完整键盘 |
| **业务动作** | `POST …/sessions/:id/action` `{ type: approve\|deny\|reply, text? }` | **手机/飞书主路径**；服务端翻译成写入 PTY 的序列 |
| 列表 | `GET …/sessions` | 选会话、发卡片 |

鉴权：token / 链接密钥；MVP 可限本机或局域网。

**多连接：** 网关对 smeltd 单路 attach（或只读旁路）+ 向多个 WS 客户端 fan-out，避免踢 GUI。

```
桌面 xterm / 手机 H5 / 飞书回调
         │
         ▼
   远程操作网关
    ├─ stream  ──▶ smeltd open/流
    ├─ state   ──▶ SessionState
    ├─ input   ──▶ type0 帧
    └─ action  ──▶ 映射为 input（或专用协议）
```

---

## Phase 0 — 前提：多连接行为

| 项 | 内容 |
|----|------|
| 验证 | 同一 `session id` 第二路 `open` 是否顶掉 GUI |
| 结论 | 会踢 → 网关单 attach + fan-out，或 smeltd 旁路 |
| 验收 | 远程打开不踢主界面；行为写入协议注释 |

**粗量级：** ~0.5 天。

---

## Phase 1 — 远程操作网关 MVP ⭐ 当前优先

**目标：** 有稳定 API；至少一个客户端能远程**查看**活会话。

| 交付 | 说明 |
|------|------|
| 网关 | 连 `smeltd.sock`；**stream**（快照+实时字节）；fan-out |
| 鉴权雏形 | token；默认绑定本机/局域网 |
| **参考客户端 A** | 桌面浏览器 + xterm.js **只读**订 stream（验证管道，可替换） |
| 文档 | 标明：xterm 非必须；手机/IM 走后续 state/action |

**明确不做：** 公网裸奔、飞书接入、可写、大改 smeltd 主协议。

**验收：** GUI 可关；浏览器打开链接能看到同一会话画面；API 形状固定，不绑死前端库。

**粗量级：** ~1–3 天。

---

## Phase 2 — 查看可用化 + 列表

| 交付 | 说明 |
|------|------|
| `GET /sessions` | 选会话（先 id，再标题等） |
| 断线 / 重连 | stream 断开提示与再 attach |
| GUI 入口 | 「远程打开 / 复制链接」 |
| （可选）纯文本 tail | 无 xterm 时也能看最近输出（为手机铺路） |

**验收：** 不用手拼 id；日常自己远程盯会话够用。

**粗量级：** ~2–4 天。

---

## Phase 3 — 跨网络访问（Cloudflare Tunnel）⭐ 提前

**为什么插在这里、且提前：** 最戳的真实场景是「在电脑上干活，有事出门，手机上接着
看/接着弄」。Phase 1/2 的网关默认绑回环，跨机器访问要求同局域网或已有 Tailscale——
手机切到蜂窝网络那一刻，这条链路就断了，"出门继续"这个承诺兑现不了。这条不依赖
Phase 4/5/6（状态/手机 UI/可写），纯粹是传输层，可以插在这里独立做，不用等前面的
知情/操控功能齐全。

**为什么改用 Cloudflare Tunnel、不再自己搭信令服务器 + WebRTC**（原方案见
[collaboration.md](collaboration.md)「点对点连接」一节，作为考虑过的备选留档）：

1. 自己实现 WebRTC 信令，需要一个双方都能连出去的公网信令服务器——这意味着**必须
   有人运营一个公网服务**，不是纯客户端方案，跟"个人项目不做账号体系/运维"这条
   原则正面冲突。
2. Rust 的 WebRTC 库现状不理想：`webrtc-rs`（crates.io 上的 `webrtc`）有已知的
   内存/socket 泄漏（约 111 KiB/连接，根源是回调架构，v0.20 重构还没稳定）；
   `str0m` 更干净但是 sans-I/O，等于要自己写一整套 UDP + 定时器的事件循环，
   工程量和调试成本都不小。smeltd 是要长期跑的常驻进程，不该背一个有已知泄漏
   的组件。
3. **Cloudflare Tunnel（`cloudflared`）直接绕开了整个"两端怎么找到对方"的
   问题**：本地网关开一条到 Cloudflare 边缘的出站连接，Cloudflare 分配一个
   公网地址转发回来，手机端就是普通 HTTPS 访问——不用信令、不用 ICE、不用自己
   运营任何公网服务，`cloudflared tunnel --url` 这条"quick tunnel"连账号都不用
   注册。代价是数据经过 Cloudflare 中转，不是真正点对点；但 WebRTC 在很多真实
   NAT 场景下最终也是走 TURN 中继，实际体验差别不大，工程量却小一个数量级。

| 交付 | 说明 |
|------|------|
| 检测 `cloudflared` | 本机有没有装；没装则引导用户安装（`brew install cloudflared`），不vendor 这个二进制 |
| 按需拉起 Quick Tunnel | GUI 侧 spawn `cloudflared tunnel --url http://127.0.0.1:<网关端口>` 子进程，解析 stdout 拿到生成的 `https://xxx.trycloudflare.com` |
| GUI 展示公网链接 | 跟本机链接一起显示在"远程"设置页；明确标注"临时链接，进程重启会变" |
| （可选，进阶） Named Tunnel | 给有自己 Cloudflare 账号 + 域名的用户一条稳定链接的路径，写文档即可，不强求做 UI |

**明确不做：**
- 自己实现 WebRTC 信令服务器 / ICE 状态机（上面第 1、2 点已经说明原因）
- 自己运营任何公网中转/协调服务（不管信令还是 TURN）
- Named Tunnel 的完整 UI 集成（先文档，MVP 只做 Quick Tunnel）

**验收：** 手机关掉 Wi-Fi、纯用蜂窝数据，能打开 Cloudflare 生成的公网链接看到实时画面——不需要手机和电脑在同一个网络，不需要用户有 Cloudflare 账号。

**粗量级：** ~1–2 天（比原 WebRTC 方案省下信令协议 + ICE 状态机这一大块）。

---

## Phase 4 — 远程知情（状态）

函数级改动见 [state-channel-plan.md](state-channel-plan.md)。  
状态是远程操作的**知情层**，服务 B/C 与侧栏灯。

| 交付 | 说明 |
|------|------|
| 内存 `SessionState` + `phase` | 见 state-channel-plan |
| `state` op + `smelt-notify` + spawn 注入 env | hook 事实 |
| `subscribe` / 网关转发 state | GUI 与远程端 |

**验收：** 列表能显示「等审批」；不必盯完整终端。

**粗量级：** ~3–7 天。

---

## Phase 5 — 操作台（手机友好）+ 远程端整合

| 交付 | 说明 |
|------|------|
| **客户端 B** | 手机优先 H5：phase、问题文案、主按钮区；完整终端入口可选且降级预期 |
| 列表筛选 | 「等你处理」 |
| 通知（可选） | waiting / approval |

**验收：** 手机上主要用操作台完成「看懂要我干嘛」，而不是挤 xterm。

**粗量级：** ~2–4 天。

---

## Phase 6 — 远程操控（input + action）

远程端是 PC 工作的**延续**：能力上要能往 PTY 写任意字节；交互上用操作台按钮减负。
`action` 是高频快捷方式，**不是**能力上限。

| 交付 | 说明 |
|------|------|
| **`input`** | 原始键盘/粘贴 → 写 PTY；**无 phase 门闩**（随时 Ctrl+C / TUI / 补一句） |
| **`action`** | approve / deny / reply → 映射为固定按键；**有** phase 门闩防误点 |
| 权限 | 链接只读 vs 可写（`write_enabled` 同时管 input + action） |
| 终端页 | 可写时 xterm stdin + 手机快捷键条 |
| 操作台 | Composer 始终可发（走 input）；waiting 时再显示批准/拒绝 |

**验收：** 手机操作台能批权限 + 任意时刻输入；完整终端页能像本机一样持续键入。

**粗量级：** ~数天。

---

## Phase 7 — IM 卡片（飞书等）

| 交付 | 说明 |
|------|------|
| **客户端 C** | 状态变化 → 飞书卡片（标题、phase、问题摘要） |
| 按钮回调 | 打网关 `action`（允许/拒绝/打开操作台链接） |
| 不嵌终端 | 详情跳 H5 操作台或可选终端页 |

**验收：** 不装 smelt、不看 xterm，在飞书里能处理一条「等审批」。

**粗量级：** 视飞书应用与部署，单独排期。

**依赖：** Phase 4（state）+ Phase 6（action）；可与 Phase 5 交错。

---

## Phase 8 — 板 / 认领 / 协作加深

| 项 | 说明 |
|----|------|
| 板 = Kanban 壳 × 认领池心 | 见 collaboration.md；条目绑 `session_id`，远程链接同一会话 |
| Provider / Profile / Worktree | 远程「开跑」更稳 |
| 同事链接 | 浏览器或飞书零安装；复用 Phase 3 的 Cloudflare Tunnel |
| 移交 | 活会话交接 |

---

## 明确后置 / 现阶段不做

| 项 | 原因 |
|----|------|
| 全端强制 xterm | 手机/IM 体验差；与 API 优先相反 |
| 飞书内嵌完整终端 | 无必要且难维护 |
| 内嵌 WebView + DOM inspect | 大；先外开 URL + 复制 selector |
| 自己实现 WebRTC 信令/ICE、自建 WireGuard | Phase 3 改用 Cloudflare Tunnel，不自己运营任何公网中转/协调服务（见 Phase 3 详述） |
| 主 GUI 换 Electron | 与 GPUI + smeltd 路线相反 |
| 完整项目管理 | 糊掉驾驶舱定位 |

---

## 建议推进焦点

```
现在  → Phase 0 多连接
      → Phase 1 网关 API + 参考客户端（xterm 只读）
      → Phase 2 列表与可用化
提前  → Phase 3 跨网络访问（Cloudflare Tunnel）—— 解决"出门后手机继续"的连接问题
然后  → Phase 4 状态（知情）
      → Phase 5 手机操作台
      → Phase 6 input/action（可写）—— 到这里"出门后手机继续"才算真正闭环
      → Phase 7 飞书卡片
有余力 → Phase 8 板 / 协作
```

---

## 粗量级汇总（一人）

| Phase | 粗量级 |
|-------|--------|
| 0 | ~0.5 天 |
| 1 | ~1–3 天 |
| 2 | ~2–4 天 |
| 3 | ~3–5 天 |
| 4 | ~3–7 天 |
| 5 | ~2–4 天 |
| 6 | ~数天 |
| 7 | 单独排（含飞书侧） |
| 8 | 按条拆 |

---

## 一句话

**远程操作 = 对活会话的 API（stream / state / input / action）+ 多端皮肤。**  
xterm 只是桌面完整终端的参考实现；手机默认操作台；飞书默认卡片与按钮——三者共用网关，不共用一个前端组件。
