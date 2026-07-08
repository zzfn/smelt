import { CheckIcon, CrossIcon, DashIcon } from "./icons";

type Cell = "yes" | "no" | "partial";

const ROWS: { feature: string; smelt: Cell; tmux: Cell; cloud: Cell }[] = [
  { feature: "多 Agent 并行指挥（多项目 × 多标签）", smelt: "yes", tmux: "partial", cloud: "yes" },
  { feature: "会话崩溃不丢（GUI 退出自动 reattach）", smelt: "yes", tmux: "yes", cloud: "yes" },
  { feature: "原生 GPU 渲染，无网络延迟", smelt: "yes", tmux: "yes", cloud: "no" },
  { feature: "Git diff 可视化视图", smelt: "yes", tmux: "no", cloud: "yes" },
  { feature: "文件树 + 文件名/内容搜索", smelt: "yes", tmux: "no", cloud: "yes" },
  { feature: "本地优先，代码不出设备", smelt: "yes", tmux: "yes", cloud: "no" },
  { feature: "完整 ANSI/24-bit 真终端（可跑 vim/htop）", smelt: "yes", tmux: "yes", cloud: "partial" },
];

const COLUMNS = [
  { key: "smelt" as const, label: "smelt" },
  { key: "tmux" as const, label: "裸终端 + tmux" },
  { key: "cloud" as const, label: "云端 AI IDE" },
];

function Mark({ v }: { v: Cell }) {
  if (v === "yes") return <CheckIcon className="mx-auto h-4 w-4" />;
  if (v === "no") return <CrossIcon className="mx-auto h-4 w-4" />;
  return <DashIcon className="mx-auto h-4 w-4" />;
}

export function FeatureComparison() {
  return (
    <section id="comparison" className="mx-auto max-w-5xl px-6 py-20">
      <h2 className="text-2xl font-semibold tracking-tight text-foreground">
        为什么不直接用 tmux 或云端 IDE
      </h2>
      <p className="mt-2 max-w-xl text-sm leading-6 text-muted">
        smelt 想做的是两者之间的折中：既要真终端的掌控力和本地隐私，也要一个「看得见状态」的外壳。
      </p>

      <div className="mt-8 overflow-x-auto rounded-xl border border-border">
        <table className="w-full min-w-[560px] border-collapse text-sm">
          <thead>
            <tr className="border-b border-border bg-surface">
              <th className="px-4 py-3 text-left font-medium text-muted">功能</th>
              {COLUMNS.map((col) => (
                <th
                  key={col.key}
                  className={`px-4 py-3 text-center font-medium ${
                    col.key === "smelt" ? "text-accent" : "text-muted"
                  }`}
                >
                  {col.label}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {ROWS.map((row, i) => (
              <tr
                key={row.feature}
                className={i % 2 === 0 ? "bg-background" : "bg-surface/40"}
              >
                <td className="px-4 py-3 text-foreground/90">{row.feature}</td>
                <td className="px-4 py-3">
                  <Mark v={row.smelt} />
                </td>
                <td className="px-4 py-3">
                  <Mark v={row.tmux} />
                </td>
                <td className="px-4 py-3">
                  <Mark v={row.cloud} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}
