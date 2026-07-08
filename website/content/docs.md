# 文档

smelt 是一个跑在 Mac 上的桌面工作台：内嵌真终端，专为「同时开着好几个 Claude Code
会话」这件事设计。这页文档只讲怎么用，架构细节不在这——smelt 现在还是 working
prototype，接口和默认行为可能会变。

## 快速开始

从 [GitHub Releases](https://github.com/zzfn/smelt/releases) 下载 `Smelt.dmg`，拖进
应用程序文件夹即可，只支持 Apple Silicon Mac。

**从源码构建**（需要 Rust 工具链和 Xcode Command Line Tools，不需要完整 Xcode）：

```sh
cargo run --bin workspace   # GUI 主程序
```

后台还有一个可选的持久化守护进程，建议一起跑：

```sh
cargo run --bin smeltd
```

`smeltd` 是类 tmux 的终端持久化守护：GUI 退出或崩溃不影响里面的 shell 存活，重开
GUI 会按会话 id 自动 reattach，不用重新 `cd` 进项目、重新跑 `claude`。

## 核心概念

smelt 只有两个二进制：

- **workspace** —— GUI 本体，多项目 × 多标签的外壳，负责渲染、文件树、git diff、
  桌面宠物这些"看得见"的部分。
- **smeltd** —— 后台守护，管 PTY 生命周期，跟 GUI 之间靠字节流通信。就算 GUI 挂了，
  `smeltd` 照样把 shell 养着。

两者是解耦的：`workspace` 可以单独跑（这时终端会话就跟着 GUI 生命周期走），也可以
接上 `smeltd` 拿到跨重启的会话持久化。

## 功能一览

**终端**
- 完整 ANSI / 256 色 / 24-bit 真彩色，Nerd Font 图标正常显示
- 能跑交互式程序和全屏 TUI：`claude`、`vim`、`htop` 都是真跑起来，不是阉割版伪终端
- 10000 行滚动回看，鼠标滚轮 / `Shift+PageUp` / `Shift+PageDown` 翻页
- 中文输入法（IME）原生支持
- 拖拽框选复制，双击选词，三击选行
- `Cmd+C` 复制选区，`Cmd+V` 粘贴剪贴板

**工作台外壳**
- 多标签，每个标签独立 PTY / 历史 / 焦点，`+` 新建、`×` 关闭
- 分屏布局，窗口结构会自动存档，下次打开原样恢复
- 文件树浏览，支持文件名 / 内容双模式搜索
- Git diff 视图，改动一眼看清

**桌面宠物（可选）**
- 悬浮小窗，可选接一个 OpenAI 兼容协议的 LLM 当"大脑"
- 能感知鼠标位置和当前前台 app，做出对应反应
- 纯只读感知，不做任何输入模拟或屏幕截图

## 数据与配置

smelt 不建数据库，状态都是本地小文件：

| 文件 | 内容 |
|---|---|
| `~/.smelt/workspace.json` | 分屏布局存档（结构 / 嵌套 / 方向），启动时据此重建 |
| `~/.smelt/appearance.json` | 终端外观设置（配色、字体等），所有终端共享一份 |
| `~/.smelt/smeltd.sock` | `smeltd` 的 Unix socket，`workspace` 靠它跟守护通信 |

都是本地文件，没有任何数据会离开这台机器。

## 常见问题

**必须先跑 `smeltd` 才能用吗？**
不需要。单独跑 `workspace` 完全可用，只是这种情况下关掉 GUI 相当于关掉里面所有终端。
想要"GUI 崩了 shell 还在"这种持久化能力，才需要额外起 `smeltd`。

**支持 Linux / Windows 吗？**
目前只做 Mac。GPU 渲染依赖 Metal，暂时没有跨平台计划。

**在哪反馈问题？**
仓库还没有开放稳定的 issue 流程，先看 [README](https://github.com/zzfn/smelt) 或直接
去看源码。
