import type { ReactNode } from "react";

const COLORS = {
  keyword: "var(--syn-keyword)",
  string: "var(--syn-string)",
  function: "var(--syn-function)",
  number: "var(--syn-number)",
  comment: "var(--syn-comment)",
  punct: "var(--syn-punct)",
  property: "var(--syn-property)",
  accent: "var(--accent)",
  success: "var(--success)",
  muted: "var(--muted)",
  fg: "var(--foreground)",
} as const;

export function Tok({
  c,
  children,
}: {
  c: keyof typeof COLORS;
  children: ReactNode;
}) {
  return <span style={{ color: COLORS[c] }}>{children}</span>;
}
