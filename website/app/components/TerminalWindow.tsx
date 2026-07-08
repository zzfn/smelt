import type { ReactNode } from "react";

export function TerminalWindow({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <div className="w-full overflow-hidden rounded-xl border border-border bg-surface shadow-2xl shadow-black/40">
      <div className="flex items-center gap-2 border-b border-border bg-surface-2 px-4 py-2.5">
        <span className="h-3 w-3 rounded-full bg-[#ff5f57]" />
        <span className="h-3 w-3 rounded-full bg-[#febc2e]" />
        <span className="h-3 w-3 rounded-full bg-[#28c840]" />
        <span className="ml-2 text-xs text-dim font-mono">{title}</span>
      </div>
      <pre className="overflow-x-auto p-5 font-mono text-[13px] leading-6 whitespace-pre">
        {children}
      </pre>
    </div>
  );
}
