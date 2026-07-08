import { RustMark, GpuMark, SparkMark, GitMark, TermMark, AppleMark } from "./icons";

const STACK = [
  { icon: RustMark, label: "Rust" },
  { icon: GpuMark, label: "GPUI" },
  { icon: SparkMark, label: "Claude Code" },
  { icon: TermMark, label: "alacritty_terminal" },
  { icon: GitMark, label: "Git" },
  { icon: AppleMark, label: "macOS" },
];

export function Integrations() {
  return (
    <section className="border-y border-border bg-surface/30">
      <div className="mx-auto max-w-5xl px-6 py-12">
        <p className="text-center text-xs uppercase tracking-widest text-dim">
          构建于
        </p>
        <div className="mt-6 grid grid-cols-3 gap-6 sm:grid-cols-6">
          {STACK.map(({ icon: Icon, label }) => (
            <div
              key={label}
              className="group flex flex-col items-center gap-2 text-dim transition-colors hover:text-foreground"
            >
              <Icon className="h-6 w-6 opacity-60 grayscale transition-all group-hover:opacity-100 group-hover:grayscale-0 group-hover:text-accent" />
              <span className="text-center text-xs">{label}</span>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}
