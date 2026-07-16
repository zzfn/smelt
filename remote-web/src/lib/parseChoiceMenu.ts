/** 从终端文本里提取 Claude Code 等「编号选择菜单」 */

export type ChoiceOption = {
  /** 1-based 序号（与 TUI 一致） */
  index: number;
  /** 主标题，如「猫」 */
  label: string;
  /** 副文案 / 描述 */
  description?: string;
};

export type ChoiceMenu = {
  title?: string;
  prompt?: string;
  options: ChoiceOption[];
  /** 当前高亮项（若解析到 `>` 前缀） */
  activeIndex?: number;
};

/**
 * 选项行：允许 TUI 装饰与多种高亮前缀。
 * 例：`> 1. 丰田` / `❯ 1. 丰田` / `  2. 保时捷` / `│ 3. 沃尔沃`
 *
 * 注意：高亮项常用 ❯ / › / ▶ 等，旧正则只认 `>`，会把第 1 项漏掉，
 * 结果第 1 项掉进「标题」槽位（用户看到的「第一项跑到标题上」）。
 */
const OPTION_RE =
  /^[\s│|┃║]*([>›❯▶►→➜•●◆*✦➢➤])?\s*(\d{1,2})(?:[\.．、:)\]])\s*(.+?)\s*$/u;

const FOOTER_RE = /enter to select|↑|↓|esc to cancel|jump to bottom|type a number/i;

function isOptionLine(line: string): boolean {
  return OPTION_RE.test(line);
}

function parseOptionLine(
  line: string,
): { index: number; label: string; highlighted: boolean } | null {
  const m = line.match(OPTION_RE);
  if (!m) return null;
  const label = m[3].trim();
  if (!label || label.length > 80) return null;
  if (/^(enter|esc|type|jump)/i.test(label)) return null;
  return {
    index: parseInt(m[2], 10),
    label,
    highlighted: !!m[1],
  };
}

/**
 * 解析形如：
 *   汽车选择
 *   你更喜欢哪家？
 *   > 1. 丰田
 *     日系…
 *     2. 保时捷
 *   …
 *   Enter to select · ↑/↓ …
 */
export function parseChoiceMenu(text: string): ChoiceMenu | null {
  const rawLines = text.split(/\r?\n/);
  // 只看末尾一段，避免整段 transcript 误匹配
  const lines = rawLines.slice(-60).map((l) => l.replace(/\s+$/, ""));

  type Hit = { lineIdx: number; index: number; label: string; highlighted: boolean };
  const hits: Hit[] = [];
  const seenIdx = new Set<number>();

  for (let i = 0; i < lines.length; i++) {
    const parsed = parseOptionLine(lines[i]);
    if (!parsed) continue;
    // 同一屏可能重复出现历史菜单；保留最后一次该序号
    if (seenIdx.has(parsed.index)) {
      const prev = hits.findIndex((h) => h.index === parsed.index);
      if (prev >= 0) hits.splice(prev, 1);
    }
    seenIdx.add(parsed.index);
    hits.push({
      lineIdx: i,
      index: parsed.index,
      label: parsed.label,
      highlighted: parsed.highlighted,
    });
  }

  if (hits.length < 2) return null;

  // 按行序，再校验序号合理
  hits.sort((a, b) => a.lineIdx - b.lineIdx);
  const indices = hits.map((h) => h.index);
  const min = Math.min(...indices);
  const max = Math.max(...indices);
  if (max - min + 1 > hits.length + 3) return null;
  if (hits.length < 2 || hits.length > 12) return null;

  // 合并描述：选项行之间的非选项行
  const options: ChoiceOption[] = hits.map((h, hi) => {
    const nextIdx =
      hi + 1 < hits.length ? hits[hi + 1].lineIdx : Math.min(lines.length, h.lineIdx + 4);
    const descLines: string[] = [];
    for (let j = h.lineIdx + 1; j < nextIdx; j++) {
      const t = lines[j].trim();
      if (!t) continue;
      if (isOptionLine(lines[j])) break;
      if (FOOTER_RE.test(t)) break;
      const cleaned = t.replace(/^[\s│|>›❯▶►·•*✦➢➤]+/u, "").trim();
      // 描述行不应再像「N. xxx」
      if (isOptionLine(cleaned) || isOptionLine(t)) break;
      if (cleaned) descLines.push(cleaned);
    }
    return {
      index: h.index,
      label: h.label,
      description: descLines.length ? descLines.join(" ") : undefined,
    };
  });

  // 按 index 排序展示（TUI 顺序）
  options.sort((a, b) => a.index - b.index);

  // 标题 / 提问：选项块上方，且**绝不能**是选项行（否则第 1 项会变成标题）
  const firstHit = Math.min(...hits.map((h) => h.lineIdx));
  const head = lines
    .slice(Math.max(0, firstHit - 6), firstHit)
    .map((l) => l.trim())
    .filter(Boolean)
    .filter((l) => !FOOTER_RE.test(l))
    .filter((l) => !isOptionLine(l))
    // 去掉「1. 丰田」这类漏网选项形
    .filter((l) => !/^\d{1,2}[\.．、:)\]]\s*\S/.test(l))
    .map((l) => l.replace(/^[\s│|>›❯▶►·•*]+/u, "").trim())
    .filter(Boolean);

  let title: string | undefined;
  let prompt: string | undefined;
  if (head.length >= 2) {
    title = head[head.length - 2];
    prompt = head[head.length - 1];
  } else if (head.length === 1) {
    prompt = head[0];
  }

  const clean = (s?: string) =>
    s
      ?.replace(/[─━═|\u2500-\u257F]+/g, " ")
      .replace(/\s+/g, " ")
      .trim();

  const cleanedTitle = clean(title);
  const cleanedPrompt = clean(prompt);

  // 双保险：若标题/提问仍像选项，丢弃（不当标题）
  const safeTitle =
    cleanedTitle && !isOptionLine(cleanedTitle) && !/^\d{1,2}[\.．]/.test(cleanedTitle)
      ? cleanedTitle
      : undefined;
  const safePrompt =
    cleanedPrompt && !isOptionLine(cleanedPrompt) && !/^\d{1,2}[\.．]/.test(cleanedPrompt)
      ? cleanedPrompt
      : undefined;

  const active = hits.find((h) => h.highlighted)?.index;

  return {
    title: safeTitle,
    prompt: safePrompt,
    options,
    activeIndex: active,
  };
}

/**
 * 把选项转成写入 PTY 的按键序列：↑ 顶到头再 ↓ 到目标 + Enter。
 * @param targetIndex 选项的 1-based 序号（与 TUI 一致，不是数组下标）
 * @param maxIndex 菜单最大序号（用于决定↑次数；有缺口时不能用 options.length）
 */
export function choiceKeySequence(targetIndex: number, maxIndex: number): string {
  const max = Math.max(1, maxIndex);
  const n = Math.max(1, Math.min(targetIndex, max));
  const up = "\x1b[A".repeat(Math.max(max + 3, 8));
  const down = "\x1b[B".repeat(Math.max(0, n - 1));
  return up + down + "\r";
}
