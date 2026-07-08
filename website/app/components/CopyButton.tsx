"use client";

import { useState } from "react";

export function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);

  return (
    <button
      type="button"
      onClick={async () => {
        await navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      }}
      className="shrink-0 rounded-md border border-border px-2 py-1 text-xs text-muted transition-colors hover:border-accent/50 hover:text-accent cursor-pointer"
      aria-label="复制命令"
    >
      {copied ? "已复制" : "复制"}
    </button>
  );
}
