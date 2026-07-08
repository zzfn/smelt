"use client";

import Image from "next/image";
import Link from "next/link";
import { useRouter, usePathname } from "next/navigation";
import { GitHubIcon } from "./icons";

const REPO_URL = "https://github.com/zzfn/smelt";

function scrollToId(id: string) {
  document.getElementById(id)?.scrollIntoView({ behavior: "smooth" });
}

export function Nav() {
  const router = useRouter();
  const pathname = usePathname();
  const isHome = pathname === "/";

  function goHome() {
    if (isHome) {
      window.scrollTo({ top: 0, behavior: "smooth" });
      return;
    }
    router.push("/");
  }

  return (
    <header className="sticky top-0 z-50 border-b border-border/80 bg-background/80 backdrop-blur-md">
      <div className="mx-auto flex h-14 max-w-5xl items-center justify-between px-6">
        <button
          type="button"
          onClick={() => goHome()}
          className="flex items-center gap-2 cursor-pointer"
        >
          <Image src="/icon.svg" alt="" width={22} height={22} unoptimized />
          <span className="font-mono text-sm font-semibold tracking-tight text-foreground">
            smelt
          </span>
        </button>

        <nav className="hidden items-center gap-6 text-sm text-muted sm:flex">
          {isHome ? (
            <>
              <button
                type="button"
                onClick={() => scrollToId("features")}
                className="cursor-pointer transition-colors hover:text-foreground"
              >
                功能
              </button>
              <button
                type="button"
                onClick={() => scrollToId("comparison")}
                className="cursor-pointer transition-colors hover:text-foreground"
              >
                对比
              </button>
            </>
          ) : (
            <Link href="/" className="transition-colors hover:text-foreground">
              首页
            </Link>
          )}
          <Link href="/docs" className="transition-colors hover:text-foreground">
            文档
          </Link>
        </nav>

        <a
          href={REPO_URL}
          target="_blank"
          rel="noopener noreferrer"
          className="flex items-center gap-1.5 rounded-full border border-border px-3 py-1.5 text-xs font-medium text-foreground transition-colors hover:border-accent/50 hover:text-accent"
        >
          <GitHubIcon className="h-3.5 w-3.5" />
          GitHub
        </a>
      </div>
    </header>
  );
}
