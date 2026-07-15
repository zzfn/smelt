import { GitHubIcon } from "./icons";

export function Footer() {
  return (
    <footer className="border-t border-border">
      <div className="mx-auto flex max-w-5xl flex-col items-center gap-4 px-6 py-10 text-sm text-dim sm:flex-row sm:justify-between">
        <div className="flex items-center gap-2">
          <span className="font-mono text-foreground/80">smelt</span>
          <span>· MIT License</span>
        </div>
        <a
          href="https://github.com/smelt-ai/smelt"
          target="_blank"
          rel="noopener noreferrer"
          className="flex items-center gap-1.5 transition-colors hover:text-foreground"
        >
          <GitHubIcon className="h-4 w-4" />
          github.com/smelt-ai/smelt
        </a>
      </div>
    </footer>
  );
}
