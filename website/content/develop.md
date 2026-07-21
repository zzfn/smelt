# 开发

面向想从源码构建、了解内部架构、或者参与开发的人。日常使用 smelt 请看
[使用文档](/docs)。

## 从源码构建

需要 Rust 工具链和 Xcode Command Line Tools（不需要完整 Xcode）：

```sh
cargo run --bin workspace   # GUI 主程序
```

GUI 启动时会自动去可执行文件同目录找 `smeltd` 并拉起。只跑过 `cargo run --bin
workspace` 的话，`target/debug/` 下还没有 `smeltd`，自动拉起会静默落空——不影响
正常使用，只是这次跑起来的终端会话不会持久化。想要持久化能力，先单独构建一次：

```sh
cargo build --bin smeltd
```

之后 workspace 每次启动都会在同目录找到它并自动拉起，不需要另开一个终端手动跑
`cargo run --bin smeltd`（除非你想单独盯着它的日志调试）。

## 二进制与架构

smelt 只有两个二进制：

- **workspace**（`src/bin/workspace/`）—— GUI 本体，多项目 × 多标签的外壳，负责
  渲染、文件树、git diff、桌面宠物这些"看得见"的部分。
- **smeltd**（`src/bin/smeltd.rs`）—— 后台守护，管 PTY 生命周期，跟 GUI 之间靠
  Unix socket 字节流通信。就算 GUI 挂了，`smeltd` 照样把 shell 养着；重开 GUI
  按会话 id 自动 reattach。

两者解耦：`workspace` 可以单独跑（这时终端会话就跟着 GUI 生命周期走），也可以接上
`smeltd` 拿到跨重启的会话持久化。打包发布时两个二进制会被塞进同一个
`Smelt.app/Contents/MacOS/` 目录，`workspace` 靠 `current_exe().with_file_name
("smeltd")` 按同目录寻址、按需自动拉起它——这也是打包版用户完全不用关心 `smeltd`
的原因。

## 技术栈

- Rust 2021，tokio async
- GPUI + gpui-component（桌面 UI，GPU 渲染）
- portable-pty + alacritty_terminal（内嵌终端：PTY + ANSI 状态机）
- reqwest（桌面宠物 LLM 大脑等场景调用模型 API）

## 打包发布

```sh
./scripts/package-mac.sh --build
```

会先 `cargo build --release` 编译两个二进制，再组装成 `dist/Smelt.app`，最后打成
定制过拖拽安装窗口的 `dist/Smelt.dmg`（不签名，仅 Apple Silicon / arm64）。不加
`--build` 则跳过编译，直接用已有的 release 产物组装。

## 目录结构

- `src/bin/workspace/` —— GUI：多标签终端、文件树、git diff 视图、桌面宠物
- `src/bin/smeltd.rs` —— 终端持久化守护
- `scripts/package-mac.sh` —— 打包脚本
- `docs/workspace.md` —— GUI 已实现功能与架构（仓库内文档）
- `docs/roadmap.md` —— 待做点子存档

## 原则

- 每步 `cargo check` 通过再继续
- 配置放 `~/.smelt/config.toml`

## 反馈与贡献

Bug / 功能建议去 [GitHub Issues](https://github.com/smelt-ai/smelt/issues/new/choose) 提，
选对应模板即可；其他情况先看 [README](https://github.com/smelt-ai/smelt) 或直接去看源码。
