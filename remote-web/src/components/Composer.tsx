import { useEffect, useRef } from "preact/hooks";

type Props = {
  disabled?: boolean;
  pending?: boolean;
  onSend: (text: string) => void;
  placeholder?: string;
};

/** CLI 输入条：提示符 + 多行框 + 发送 */
export function Composer({ disabled, pending, onSend, placeholder }: Props) {
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!disabled) ref.current?.focus();
  }, [disabled]);

  function autoResize() {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 96)}px`;
  }

  function submit() {
    const el = ref.current;
    if (!el || disabled || pending) return;
    const v = el.value;
    if (!v.trim()) return;
    onSend(v);
    el.value = "";
    autoResize();
  }

  return (
    <div class="shrink-0 border-t border-border bg-panel px-2.5 pb-[max(0.45rem,env(safe-area-inset-bottom))] pt-2">
      <div class="flex items-end gap-1.5">
        <span class="mb-2 select-none px-0.5 font-mono text-sm font-semibold text-accent">
          ›
        </span>
        <textarea
          ref={ref}
          rows={1}
          disabled={disabled || pending}
          placeholder={placeholder ?? "输入命令或回复…"}
          enterKeyHint="send"
          autocomplete="off"
          autocorrect="off"
          autocapitalize="off"
          spellcheck={false}
          class="min-h-[2.35rem] max-h-24 flex-1 resize-none rounded-[10px] border border-border bg-bg px-3 py-2 font-mono text-[15px] leading-snug text-ink outline-none focus:border-[#3a5570] disabled:opacity-50"
          onInput={autoResize}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
        />
        <button
          type="button"
          disabled={disabled || pending}
          class="shrink-0 rounded-[10px] bg-accent px-3.5 py-2 text-sm font-semibold text-white active:scale-[0.98] disabled:opacity-45"
          onClick={submit}
        >
          {pending ? "…" : "发送"}
        </button>
      </div>
      <p class="mt-1 pl-4 text-[10px] text-muted/70">回车发送 · Shift+回车换行</p>
    </div>
  );
}
