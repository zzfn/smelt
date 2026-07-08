import { TerminalWindow } from "./TerminalWindow";
import { Tok } from "./Syntax";

export function Hero() {
  return (
    <section className="mx-auto grid max-w-5xl gap-12 px-6 pt-16 pb-20 sm:pt-24 sm:pb-28 lg:grid-cols-2 lg:items-center">
      <div>
        <div className="mb-5 inline-flex items-center gap-1.5 rounded-full border border-border bg-surface px-3 py-1 text-xs text-muted">
          <span className="h-1.5 w-1.5 rounded-full bg-success" />
          working prototype · 持续迭代中
        </div>

        <h1 className="text-4xl font-semibold leading-tight tracking-tight text-foreground sm:text-5xl">
          Mac 上的
          <br />
          <span className="bg-gradient-to-r from-accent to-accent-2 bg-clip-text text-transparent">
            AI Coding 驾驶舱
          </span>
        </h1>

        <p className="mt-5 max-w-md text-base leading-7 text-muted">
          基于 GPUI 的桌面工作台，内嵌真终端，专为
          <span className="text-foreground">同时指挥多个 Claude Code agent</span>
          设计——多项目 × 多标签，会话状态一目了然。
        </p>

        <div className="mt-8 flex flex-wrap items-center gap-3">
          <a
            href="https://github.com/zzfn/smelt/releases"
            target="_blank"
            rel="noopener noreferrer"
            className="rounded-lg bg-gradient-to-r from-accent to-accent-2 px-5 py-2.5 text-sm font-medium text-[#08090c] transition-opacity hover:opacity-90"
          >
            下载 Smelt.dmg（macOS）
          </a>
          <a
            href="https://github.com/zzfn/smelt"
            target="_blank"
            rel="noopener noreferrer"
            className="rounded-lg px-4 py-2.5 text-sm font-medium text-muted transition-colors hover:text-foreground"
          >
            或从源码构建 →
          </a>
        </div>
      </div>

      <TerminalWindow title="workspace — zsh">
        <div>
          <span className="text-dim">$ </span>
          <Tok c="function">cargo</Tok> <Tok c="fg">run --bin workspace</Tok>
        </div>
        <div style={{ color: "var(--syn-comment)" }}>
          &nbsp;&nbsp; Compiling <Tok c="string">smelt v0.1.0</Tok> (~/dev/smelt)
        </div>
        <div>
          &nbsp;&nbsp;&nbsp;&nbsp;
          <Tok c="success">Finished</Tok>{" "}
          <span style={{ color: "var(--syn-comment)" }}>
            `dev` profile [unoptimized] target(s) in 4.82s
          </span>
        </div>
        <div>
          &nbsp;&nbsp;&nbsp;&nbsp;&nbsp;
          <Tok c="accent">Running</Tok> <Tok c="string">`target/debug/workspace`</Tok>
        </div>
        <div>&nbsp;</div>
        <div>
          <Tok c="accent">▸ workspace</Tok>{" "}
          <span className="text-dim">▸ smeltd  ▸ docs</span>
          <span className="text-dim">   +</span>
        </div>
        <div className="text-dim">
          ────────────────────────────────────
        </div>
        <div>
          <span className="text-dim">$ </span>
          <Tok c="keyword">claude</Tok>
        </div>
        <div>
          <Tok c="success">✓</Tok>{" "}
          <span style={{ color: "var(--foreground)" }}>session attached</span>
          <span className="text-dim"> · </span>
          <Tok c="property">tokyo-night</Tok>
          <span className="text-dim"> · </span>
          <Tok c="number">256-color</Tok>
          <span className="text-dim"> · </span>
          <Tok c="function">nerd-font</Tok>
        </div>
      </TerminalWindow>
    </section>
  );
}
