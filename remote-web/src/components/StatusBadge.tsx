import { phaseMeta } from "../api";

export function StatusBadge({ phase }: { phase?: string }) {
  const m = phaseMeta(phase);
  return (
    <span
      class="shrink-0 rounded-full px-2 py-0.5 text-[11px] font-semibold text-white"
      style={{ background: m.color }}
    >
      {m.label}
    </span>
  );
}
