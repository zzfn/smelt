import type { Components } from "react-markdown";

export const mdComponents: Components = {
  h1: ({ id, children }) => (
    <h1 id={id} className="text-3xl font-semibold tracking-tight text-foreground">
      {children}
    </h1>
  ),
  h2: ({ id, children }) => (
    <h2
      id={id}
      className="mt-12 scroll-mt-20 border-t border-border pt-8 text-xl font-semibold tracking-tight text-foreground first:mt-8 first:border-0 first:pt-0"
    >
      {children}
    </h2>
  ),
  h3: ({ id, children }) => (
    <h3 id={id} className="mt-6 scroll-mt-20 text-base font-semibold text-foreground">
      {children}
    </h3>
  ),
  p: ({ children }) => (
    <p className="mt-4 text-sm leading-7 text-muted">{children}</p>
  ),
  ul: ({ children }) => (
    <ul className="mt-4 list-disc space-y-2 pl-5 text-sm leading-7 text-muted marker:text-dim">
      {children}
    </ul>
  ),
  li: ({ children }) => <li>{children}</li>,
  a: ({ href, children }) => (
    <a
      href={href}
      target={href?.startsWith("http") ? "_blank" : undefined}
      rel={href?.startsWith("http") ? "noopener noreferrer" : undefined}
      className="text-accent underline decoration-accent/30 underline-offset-2 hover:decoration-accent"
    >
      {children}
    </a>
  ),
  strong: ({ children }) => (
    <strong className="font-semibold text-foreground">{children}</strong>
  ),
  code: ({ children, className }) => {
    const isBlock = Boolean(className);
    if (isBlock) {
      return <code className="text-[13px] text-foreground">{children}</code>;
    }
    return (
      <code className="rounded border border-border bg-surface px-1.5 py-0.5 font-mono text-[13px] text-accent">
        {children}
      </code>
    );
  },
  pre: ({ children }) => (
    <pre className="mt-4 overflow-x-auto rounded-xl border border-border bg-surface p-4 font-mono leading-6">
      {children}
    </pre>
  ),
  table: ({ children }) => (
    <div className="mt-4 overflow-x-auto rounded-lg border border-border">
      <table className="w-full min-w-[420px] border-collapse text-sm">
        {children}
      </table>
    </div>
  ),
  thead: ({ children }) => (
    <thead className="border-b border-border bg-surface">{children}</thead>
  ),
  th: ({ children }) => (
    <th className="px-4 py-2.5 text-left font-medium text-muted">{children}</th>
  ),
  td: ({ children }) => (
    <td className="border-t border-border px-4 py-2.5 text-foreground/90">
      {children}
    </td>
  ),
  hr: () => <hr className="mt-8 border-border" />,
};
