const REPO_URL = "https://github.com/zzfn/smelt";

const LINKS = [
  {
    href: `${REPO_URL}/blob/main/docs/workspace.md`,
    title: "架构文档",
    desc: "技术栈、目录结构、已实现功能清单、关键技术决策。",
  },
  {
    href: `${REPO_URL}/blob/main/docs/roadmap.md`,
    title: "Roadmap",
    desc: "待做点子存档，看看接下来会长出什么。",
  },
];

export function DocsCTA() {
  return (
    <section className="mx-auto max-w-5xl px-6 py-20">
      <div className="rounded-2xl border border-border bg-surface/60 p-8 sm:p-10">
        <div className="flex flex-col gap-6 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <h2 className="text-xl font-semibold tracking-tight text-foreground">
              想看得更深一点？
            </h2>
            <p className="mt-2 max-w-md text-sm leading-6 text-muted">
              代码和文档都在仓库里，没有另开一个独立的文档站。
            </p>
          </div>

          <div className="flex flex-col gap-3 sm:flex-row">
            {LINKS.map((link) => (
              <a
                key={link.title}
                href={link.href}
                target="_blank"
                rel="noopener noreferrer"
                className="group w-full rounded-lg border border-border bg-background px-4 py-3 transition-colors hover:border-accent/50 sm:w-56"
              >
                <div className="flex items-center justify-between text-sm font-medium text-foreground">
                  {link.title}
                  <span className="text-dim transition-colors group-hover:text-accent">
                    →
                  </span>
                </div>
                <p className="mt-1 text-xs leading-5 text-muted">{link.desc}</p>
              </a>
            ))}
          </div>
        </div>
      </div>
    </section>
  );
}
