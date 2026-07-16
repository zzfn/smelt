import { useEffect, useState } from "preact/hooks";
import type { ChoiceMenu } from "../lib/parseChoiceMenu";

type Props = {
  menu: ChoiceMenu;
  busy?: boolean;
  onSelect: (index: number) => void;
  onCancel: () => void;
  onCustom?: (text: string) => void;
};

/**
 * 移动端选择弹层：大触控目标，自己渲染选项，不依赖 TUI 里的小列表。
 */
export function ChoiceSheet({ menu, busy, onSelect, onCancel, onCustom }: Props) {
  const [custom, setCustom] = useState("");
  const [showCustom, setShowCustom] = useState(false);

  // body 锁滚，避免底下终端跟着滑
  useEffect(() => {
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prev;
    };
  }, []);

  return (
    <div
      class="fixed inset-0 z-50 flex flex-col justify-end bg-black/55 backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-label={menu.title || "请选择"}
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onCancel();
      }}
    >
      <div class="mx-auto flex max-h-[min(78vh,640px)] w-full max-w-lg flex-col rounded-t-2xl border border-border bg-panel shadow-2xl">
        <div class="flex shrink-0 items-start justify-between gap-3 border-b border-border px-4 pb-3 pt-3.5">
          <div class="min-w-0">
            {menu.title ? (
              <h2 class="text-base font-semibold leading-snug text-ink">{menu.title}</h2>
            ) : (
              <h2 class="text-base font-semibold text-ink">请选择</h2>
            )}
            {menu.prompt ? (
              <p class="mt-1 text-sm leading-snug text-muted">{menu.prompt}</p>
            ) : null}
          </div>
          <button
            type="button"
            class="shrink-0 rounded-lg px-2 py-1 text-sm text-muted active:bg-card"
            disabled={busy}
            onClick={onCancel}
          >
            取消
          </button>
        </div>

        <div class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-3 py-2">
          <ul class="space-y-2">
            {menu.options.map((opt) => {
              const active = menu.activeIndex === opt.index;
              return (
                <li key={opt.index}>
                  <button
                    type="button"
                    disabled={busy}
                    class={`flex w-full flex-col items-start rounded-xl border px-3.5 py-3.5 text-left transition active:scale-[0.99] disabled:opacity-50 ${
                      active
                        ? "border-accent bg-accent/15"
                        : "border-border bg-card active:bg-[#1c1c22]"
                    }`}
                    onClick={() => onSelect(opt.index)}
                  >
                    <span class="flex w-full items-center gap-2">
                      <span
                        class={`flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-sm font-bold ${
                          active ? "bg-accent text-white" : "bg-bg text-muted"
                        }`}
                      >
                        {opt.index}
                      </span>
                      <span class="text-[16px] font-semibold leading-snug text-ink">
                        {opt.label}
                      </span>
                    </span>
                    {opt.description ? (
                      <span class="mt-1.5 pl-9 text-[13px] leading-snug text-muted">
                        {opt.description}
                      </span>
                    ) : null}
                  </button>
                </li>
              );
            })}
          </ul>

          {onCustom ? (
            <div class="mt-3 border-t border-border pt-3">
              {!showCustom ? (
                <button
                  type="button"
                  class="w-full rounded-xl border border-dashed border-border py-3 text-sm text-muted active:bg-card"
                  disabled={busy}
                  onClick={() => setShowCustom(true)}
                >
                  自己输入…
                </button>
              ) : (
                <div class="flex gap-2">
                  <input
                    class="min-h-11 flex-1 rounded-xl border border-border bg-bg px-3 text-[15px] text-ink outline-none focus:border-accent"
                    placeholder="输入自定义内容"
                    value={custom}
                    disabled={busy}
                    onInput={(e) => setCustom((e.target as HTMLInputElement).value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && custom.trim()) {
                        onCustom(custom.trim());
                      }
                    }}
                  />
                  <button
                    type="button"
                    class="rounded-xl bg-accent px-4 text-sm font-semibold text-white disabled:opacity-45"
                    disabled={busy || !custom.trim()}
                    onClick={() => custom.trim() && onCustom(custom.trim())}
                  >
                    发送
                  </button>
                </div>
              )}
            </div>
          ) : null}
        </div>

        <p class="shrink-0 px-4 pb-[max(0.75rem,env(safe-area-inset-bottom))] pt-1 text-center text-[11px] text-muted/80">
          点选项即可，无需在终端里对准小光标
        </p>
      </div>
    </div>
  );
}
