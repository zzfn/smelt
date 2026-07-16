import type { IBufferCell, Terminal } from "@xterm/xterm";

/**
 * 从 xterm 缓冲单元还原 SGR，保留数据流里已经解析好的颜色/样式。
 * （历史层若只用 translateToString，会丢掉这些属性。）
 */
function sgrOf(cell: IBufferCell): string {
  const p: number[] = [];
  if (cell.isBold()) p.push(1);
  if (cell.isDim()) p.push(2);
  if (cell.isItalic()) p.push(3);
  if (cell.isUnderline()) p.push(4);
  if (cell.isBlink()) p.push(5);
  if (cell.isInverse()) p.push(7);
  if (cell.isInvisible()) p.push(8);
  if (cell.isStrikethrough()) p.push(9);
  if (cell.isOverline()) p.push(53);

  if (cell.isFgRGB()) {
    const c = cell.getFgColor();
    p.push(38, 2, (c >> 16) & 255, (c >> 8) & 255, c & 255);
  } else if (cell.isFgPalette()) {
    const c = cell.getFgColor();
    if (c < 8) p.push(30 + c);
    else if (c < 16) p.push(90 + c - 8);
    else p.push(38, 5, c);
  }

  if (cell.isBgRGB()) {
    const c = cell.getBgColor();
    p.push(48, 2, (c >> 16) & 255, (c >> 8) & 255, c & 255);
  } else if (cell.isBgPalette()) {
    const c = cell.getBgColor();
    if (c < 8) p.push(40 + c);
    else if (c < 16) p.push(100 + c - 8);
    else p.push(48, 5, c);
  }

  if (p.length === 0) return "";
  return `\x1b[${p.join(";")}m`;
}

function attrKey(cell: IBufferCell): string {
  return [
    cell.getFgColorMode(),
    cell.getFgColor(),
    cell.getBgColorMode(),
    cell.getBgColor(),
    cell.isBold(),
    cell.isDim(),
    cell.isItalic(),
    cell.isUnderline(),
    cell.isBlink(),
    cell.isInverse(),
    cell.isInvisible(),
    cell.isStrikethrough(),
    cell.isOverline(),
  ].join(",");
}

export type ScreenCapture = {
  /** 带完整 SGR 的 ANSI，可写回 xterm 还原配色 */
  ansi: string;
  /** 纯文本，仅用于去重比较 */
  text: string;
};

/** 序列化当前活动缓冲的可视区（含备用屏），颜色来自已解析的 cell。 */
export function serializeVisibleScreen(term: Terminal): ScreenCapture {
  const buf = term.buffer.active;
  const cols = term.cols;
  const rows = term.rows;
  let ansi = "";
  let text = "";
  let lastKey = "";

  for (let y = 0; y < rows; y++) {
    const line = buf.getLine(buf.baseY + y);
    if (!line) {
      ansi += "\r\n";
      text += "\n";
      lastKey = "";
      continue;
    }
    for (let x = 0; x < cols; x++) {
      const cell = line.getCell(x);
      if (!cell || cell.getWidth() === 0) continue;
      let ch = cell.getChars();
      if (!ch) ch = " ";
      // 内容里偶发 ESC 时避免破坏后续解析
      if (ch.includes("\x1b")) ch = ch.replace(/\x1b/g, " ");
      text += ch;
      const key = attrKey(cell);
      if (key !== lastKey) {
        ansi += "\x1b[0m";
        ansi += sgrOf(cell);
        lastKey = key;
      }
      ansi += ch;
    }
    ansi += "\x1b[0m\r\n";
    text += "\n";
    lastKey = "";
  }
  return { ansi, text };
}
