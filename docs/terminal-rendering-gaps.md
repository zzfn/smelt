# 终端：与 Zed 对照的差距清单

对着 Zed 的终端实现逐条核对（Zed 源码：`~/.cargo/git/checkouts/zed-*/1d217ee/crates/`，
我们依赖的 gpui 就是这个 commit）。本文档**只记现状与未完成项**——已落地的条目放在
「已完成」归档区，避免再被当成待办。

对照文件：

| smelt | Zed |
|-------|-----|
| `src/bin/workspace/terminal.rs` | `crates/terminal/src/{terminal,alacritty}.rs` |
| `src/bin/workspace/terminal_view.rs` | `crates/terminal_view/src/{terminal_view,terminal_element}.rs` |
| `src/bin/smeltd.rs` | （无对等物；Zed 进程内 PTY） |
| — | `crates/terminal/src/mappings/{keys,mouse}.rs` |

**渲染基础（已完成，别再动）**：正文在 canvas 里直接 paint，逐行
`shape_line(.., force_width = Some(cell_w))`（见 `paint_row` 头注）。glyph 的 x 由 gpui 的
`apply_force_width_to_layout` 强制钉到 `序号 × cell_w`，字体 advance 作废。**唯一要守住的
不变量：宽字符必须落在批尾**（force_width 按 glyph 序号钉位，中文占两格却只算一个 glyph），
由「宽字符后跟 `'\0'` 占位格 → 下个字符列号对不上而断批」保证。

---

## 总览

| 层级 | 状态 |
|------|------|
| VTE / 网格（alacritty 0.26） | 同源，够用 |
| 渲染（样式 / 光标 / 链接 / 选区 / 像素吸附） | **主链路已对齐** |
| 输入 / 协议（粘贴、修饰键、鼠标 motion） | **仍有真 gap** |
| 会话持久化（smeltd） | smelt 独有，比 Zed 强 |

---

## 已完成（归档，勿当待办）

下列项曾是本文旧版 1–7 条或零碎小项，**当前代码已落地**：

| 项 | 实现要点 | 位置（约） |
|----|----------|-----------|
| 行尾底色 | `bg_spans` 扫满行，不跟字形 `end` 截断；测试 `background_runs_to_end_of_row_not_to_last_glyph` | `terminal_view.rs` `bg_spans` / `visible_end` |
| DIM / italic / 下划线族 / 删除线 | `Flags::DIM/ITALIC/ALL_UNDERLINES/UNDERCURL/STRIKEOUT` → `Cell` → `TextRun`（dim 压 alpha 0.7，undercurl → wavy） | `terminal.rs` `snapshot`，`paint_row` `run_of` |
| 光标形状 + 失焦空心 + 宽字两格 | `CursorKind`；失焦强制 Hollow；下一格 `'\0'` 盖两格 | `snapshot` + `paint_row` |
| 零宽 / 组合字符 | `cell.zerowidth()` → `Cell.zw`；进批不占格 | `text_batches` |
| OSC 8 超链接 | `cell.hyperlink()` → `Cell.link`；常驻下划线；`link_at` 优先 OSC8 再正则 URL/路径 | `Cell.link`，`link_at` |
| 滚动时光标 | `cursor_pos = line + display_offset` | `snapshot` |
| 默认底色枚举 | `bg_default = matches!(Color::Named(Background))`，反色时跟真正当底色的那侧走；`CellStyle.bg: Option` | `Cell.bg_default`，`style_of` |
| 设备像素吸附 | 网格原点 `floor(v * scale) / scale`；底色 rect `x.floor()` / 右缘 `ceil()` | render canvas + `paint_row` |
| Frame 缓存 | `last_frame: Option<Rc<Frame>>`，渲染与 `url_at` / `link_range_at` / `char_steps_between` 共用 | `TerminalView` |
| DEC 1004 焦点上报 | `report_focus` → 开了 `FOCUS_IN_OUT` 时写 `\x1b[I` / `\x1b[O` | `Terminal::report_focus` + render 里焦点边沿 |
| 选区 | alacritty `Selection`（Simple/Word/Line）；拖边自动滚；copy-on-select；Cmd+C | `selection_*`，mouse handlers |
| Kitty keyboard（Enter 消歧） | `Config.kitty_keyboard = true`；Shift+Enter → CSI u | `term_config`，`keystroke_to_bytes` |
| App cursor / alt scroll / 滚轮 | DECCKM 方向键；MOUSE_MODE 滚轮 SGR/X10；ALT+ALTERNATE_SCROLL → 方向键 | `scroll_wheel`，`app_cursor_mode` |
| 鼠标单击转发 | 无非空选区时 mouse_up 发 press+release；Shift 旁路 | `mouse_button` |
| PtyWrite / ColorRequest | EventProxy 写回守护帧 | `EventListener` |
| damage 节流 | `take_damage` 滤掉「仅静止光标格」 | `take_damage` + 定时刷新 |
| IME | preedit、行内拼音、`bounds_for_range` | `EntityInputHandler` |

**渲染侧相对 Zed 的主清单可以收工。** 下文只列仍开放的差距。

---

## 待办（按优先级）

### ~~P0 / P1 协议与输入~~ ✅ 已落地

| 项 | 实现 |
|----|------|
| Bracketed paste | `Terminal::paste` / `encode_paste`；Cmd+V + `send_text` 共用 |
| 路径 `'\0'` | `cells_to_token` 跳过占位、带 `zw`；扫描不断开中文 |
| 修饰键 / F 键 | `keystroke_to_bytes` 对齐 Zed xterm 表；Enter 仍优先 kitty CSI u |
| `COLORTERM` | `smeltd` `spawn_session` 设 `truecolor` |
| **鼠标 click/drag** | `MOUSE_MODE && !Shift`：press / drag / release；**Shift 旁路**本地框选；双击/三击永远本地 |
| **中/右键** | button 1/2 转发给应用；中键未开鼠标时粘贴剪贴板 |
| **MOUSE_MOTION 悬停** | 无键时 button 35（仅 `MOUSE_MOTION` 全开） |
| **resize 像素** | type 1 帧 16 字节：cols/rows/cell_w/cell_h；smeltd 写 `ws_xpixel=cols*cw` |
| **OSC 52** | `ClipboardStore` → `pbcopy`；`ClipboardLoad` → `pbpaste` + format 写回 PTY（macOS） |
| **TextAreaSizeRequest** | EventProxy 用共享 `TermMetrics` 应答 |
| **take_damage 光标** | 记上一帧光标；静止滤「仅光标格」，移动不算吞 |
| **终端内搜索** | Cmd+F；字面量 RegexSearch；全部命中暗高亮 + 当前亮高亮；`3/12` 计数；Enter/Shift+Enter |
| **滚动条** | 有 scrollback 时右侧自定义条；点轨道/拖 thumb 设 `display_offset` |

### 鼠标分流（现行约定）

| 场景 | 行为 |
|------|------|
| 应用开了 `MOUSE_MODE`，左/中/右 | 转发给应用（press / drag / release） |
| 同上 + **Shift** | 本地框选 + copy-on-select（xterm 旁路） |
| 双击 / 三击 | 本地选词 / 选行 |
| 未开鼠标上报 + 中键 | 粘贴剪贴板 |
| 未开鼠标上报 + 左键 | 本地框选 |

---

### 仍开放（低优先级 / 功能项）

| 项 | 说明 | 优先级 |
|----|------|--------|
| **双线/点/虚线下划线** | 折叠为「有下划线」；仅 undercurl → wavy | 可忽略 |
| **emoji+VS16 spacer skip** | Zed 有 `previous_cell_had_extras` | 可忽略 |
| **Vi mode** | 驾驶舱低价值 | 不做 |
| **OSC 52 非 macOS** | 仅 `pbcopy`/`pbpaste` | 平台 |
| **搜索正则模式** | 目前固定字面量 | 增强 |

---

## 明确**不做**的

- **超宽字形 per-cell 裁剪 / 缩放**：Zed 也不做；靠 `force_width` 保位置。
- **`shape_line_by_hash`**：`shape_line` 本身有 cache；Zed 终端也只调普通 `shape_line`。
- **可视区裁剪**（只 layout 可见行）：Zed 要是因为终端嵌在可滚列表；我们整块可见。
- **背景矩形纵向合并**：行内同色合并已有，纵向收益极小。
- **`minimum_contrast` / APCA**：Zed 有；我们固定两套调色板自保对比度。除非用户可配前景板，否则不搬。
- **Kitty 完整键盘协议栈**（除 DISAMBIGUATE 层）：Enter 消歧已覆盖 Claude Code 主痛点；
  其余键继续 xterm 表即可（与 Zed 策略一致的那一半）。

---

## 架构备注（不是 gap）

smelt 有意多做的、Zed 没有或不同的部分：

1. **smeltd 会话持久化** — GUI 崩了 shell 还在，reattach + 重放缓冲。
2. **OSC 9/777 + 响铃 → 通知 / 侧栏状态** — 协议层感知 agent，不写死私有格式。
3. **Kitty keyboard（Enter）** — 比 Zed 更贴 Claude Code Shift+Enter。
4. **Shift 旁路应用鼠标** — 开了 `MOUSE_MODE` 仍能框选复制 agent 输出。

渲染 + 输入/协议主清单已与 Zed 对齐；剩余多为功能增强或平台边角。

### smeltd reattach（会话持久化）

- **✅ 完整**：常驻 `Term` + attach 吐 **history+可视区** ANSI 快照（软换行 / OSC 8 / 模式恢复）。
- 可选：实机验收长 detach + TUI Ctrl+C；守护崩溃落盘。