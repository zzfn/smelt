"use client";

import { useEffect, useState } from "react";

const SECTIONS = ["快速开始", "核心概念", "功能一览", "数据与配置", "常见问题"];
const TOP_OFFSET = 100;

function scrollToId(id: string) {
  document.getElementById(id)?.scrollIntoView({ behavior: "smooth", block: "start" });
}

export function DocsSidebar() {
  const [active, setActive] = useState(SECTIONS[0]);

  useEffect(() => {
    function updateActive() {
      let current = SECTIONS[0];
      for (const id of SECTIONS) {
        const el = document.getElementById(id);
        if (el && el.getBoundingClientRect().top <= TOP_OFFSET) {
          current = id;
        }
      }

      // 最后一节内容短、滚不到顶部阈值时，只有它已经进入视口上半区
      // 且页面确实到底了，才强制切到它；否则保留前面 walk 的结果。
      const lastId = SECTIONS[SECTIONS.length - 1];
      if (current !== lastId) {
        const lastEl = document.getElementById(lastId);
        const atBottom =
          window.innerHeight + window.scrollY >=
          document.documentElement.scrollHeight - 2;
        if (
          atBottom &&
          lastEl &&
          lastEl.getBoundingClientRect().top <= window.innerHeight / 2
        ) {
          current = lastId;
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

  return (
    <nav className="hidden w-44 shrink-0 lg:block">
      <div className="sticky top-20 flex flex-col gap-0.5 text-sm">
        {SECTIONS.map((id) => (
          <button
            key={id}
            type="button"
            onClick={() => {
              setActive(id);
              scrollToId(id);
            }}
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
