import {
  LayersMark,
  TermMark,
  GitMark,
  SearchMark,
  GpuMark,
  SparkMark,
} from "./icons";

const FEATURES = [
  {
    icon: LayersMark,
    title: "多标签 · 多项目",
    desc: "每个标签一个独立 PTY，各自的历史与焦点，随手切换正在指挥的 agent。",
  },
  {
    icon: TermMark,
    title: "内嵌真终端",
    desc: "完整 ANSI/256 色/24-bit 色 + Nerd Font，能跑 claude、vim、htop 等交互式 / TUI 程序。",
  },
  {
    icon: GitMark,
    title: "Git diff 视图",
    desc: "agent 改了什么，一眼看清，不用切出去开另一个 diff 工具。",
  },
  {
    icon: SearchMark,
    title: "文件树 + 搜索",
    desc: "文件名 / 内容双模式搜索，定位改动涉及的文件更快。",
  },
  {
    icon: GpuMark,
    title: "原生 GPU 渲染",
    desc: "基于 GPUI/Metal，逐行整形上色，拖选、滚动都不卡顿。",
  },
  {
    icon: SparkMark,
    title: "会话持久化",
    desc: "smeltd 类 tmux 后台守护：GUI 退出或崩溃不影响 shell 存活，重开自动 reattach。",
  },
];

export function Features() {
  return (
    <section id="features" className="mx-auto max-w-5xl px-6 py-20">
      <h2 className="text-2xl font-semibold tracking-tight text-foreground">
        专为指挥多个 agent 设计
      </h2>
      <p className="mt-2 max-w-xl text-sm leading-6 text-muted">
        不是又一个终端模拟器，是给「同时开着好几个 Claude Code 会话」这件事做的外壳。
      </p>

      <div className="mt-10 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
        {FEATURES.map(({ icon: Icon, title, desc }) => (
          <div
            key={title}
            className="rounded-xl border border-border bg-surface/60 p-5 transition-colors hover:border-accent/40"
          >
            <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-surface-2 text-accent">
              <Icon className="h-4 w-4" />
            </div>
            <h3 className="mt-4 text-sm font-medium text-foreground">{title}</h3>
            <p className="mt-1.5 text-sm leading-6 text-muted">{desc}</p>
          </div>
        ))}
      </div>
    </section>
  );
}
