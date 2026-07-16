# smelt remote-web

远程操作 H5：Preact + Tailwind + **xterm** 自研 CLI 面板。

- 列表：项目 / 会话名（读网关 `/sessions`）
- 会话页：`CliPanel` = 状态 + 审批 + **xterm 渲染面** + Composer  
  （不再用 `<pre>` 剥 ANSI，才能正确显示 Claude Code TUI）

## 开发

```bash
# 终端 1：API 网关（连现有 smeltd）
cargo run --bin gateway -- --port 18765 --write

# 终端 2：前端热更新（代理到 18765）
cd remote-web && npm run dev
# 打开 http://127.0.0.1:5173/?token=...（token 用网关打印的）
```

## 构建（给 gateway / smeltd 托管）

```bash
cd remote-web && npm run build
# 产物 remote-web/dist；网关启动时若存在 dist 则自动用 SPA
cargo run --bin gateway -- --port 18765 --write
```

未构建 dist 时，网关回退到内嵌 HTML 模板。
