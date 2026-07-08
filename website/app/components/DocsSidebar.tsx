"use client";

import { useEffect, useRef, useState } from "react";

const SECTIONS = ["快速开始", "核心概念", "功能一览", "数据与配置", "常见问题"];

function scrollToId(id: string) {
  document.getElementById(id)?.scrollIntoView({ behavior: "smooth", block: "start" });
}

export function DocsSidebar() {
  const [active, setActive] = useState(SECTIONS[0]);
  // 点击跳转时立刻信任用户的选择，抑制窗口内忽略滚动监听的重新判定——
  // 页面尾部几节内容短，滚动落点的像素误差经常让阈值判定选错相邻章节。
  const suppressed = useRef(false);
  const suppressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    function updateActive() {
      if (suppressed.current) return;

      const threshold = window.innerHeight * 0.35;
      const atBottom =
        window.innerHeight + window.scrollY >=
        document.documentElement.scrollHeight - 2;
      if (atBottom) {
        setActive(SECTIONS[SECTIONS.length - 1]);
        return;
      }

      let current = SECTIONS[0];
      for (const id of SECTIONS) {
        const el = document.getElementById(id);
        if (el && el.getBoundingClientRect().top <= threshold) {
          current = id;
        }
      }
      setActive(current);
    }

    updateActive();
    window.addEventListener("scroll", updateActive, { passive: true });
    window.addEventListener("resize", updateActive);
    return () => {
      window.removeEventListener("scroll", updateActive);
      window.removeEventListener("resize", updateActive);
    };
  }, []);

  function handleClick(id: string) {
    setActive(id);
    suppressed.current = true;
    if (suppressTimer.current) clearTimeout(suppressTimer.current);
    suppressTimer.current = setTimeout(() => {
      suppressed.current = false;
    }, 1000);
    scrollToId(id);
  }

  return (
    <nav className="hidden w-44 shrink-0 lg:block">
      <div className="sticky top-20 flex flex-col gap-0.5 text-sm">
        {SECTIONS.map((id) => (
          <button
            key={id}
            type="button"
            onClick={() => handleClick(id)}
            className={`cursor-pointer rounded-md px-3 py-1.5 text-left transition-colors ${
              active === id
                ? "bg-surface text-accent"
                : "text-muted hover:text-foreground"
            }`}
          >
            {id}
          </button>
        ))}
      </div>
    </nav>
  );
}
