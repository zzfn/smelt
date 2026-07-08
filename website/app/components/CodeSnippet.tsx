import { TerminalWindow } from "./TerminalWindow";
import { Tok } from "./Syntax";

export function CodeSnippet() {
  return (
    <section className="mx-auto max-w-5xl px-6 py-20">
      <div className="grid gap-10 lg:grid-cols-2 lg:items-center">
        <div>
          <h2 className="text-2xl font-semibold tracking-tight text-foreground">
            终端后端是真的终端
          </h2>
          <p className="mt-3 text-sm leading-7 text-muted">
            PTY 起子进程，<Tok c="function">alacritty_terminal</Tok>
            解析 ANSI 状态机，维护完整的网格与滚动缓冲。后台线程读输出、
            UI 线程按 30ms 节奏对网格做快照重绘——所以整行只整形一次，
            拖选不抖动，宽度不截断。
          </p>
          <p className="mt-3 text-sm leading-7 text-muted">
            这意味着 <Tok c="keyword">claude</Tok>、<Tok c="keyword">vim</Tok>、
            <Tok c="keyword">htop</Tok> 这类交互式 / 全屏 TUI 程序都能正常跑，
            不是阉割版的伪终端。
          </p>
        </div>

        <TerminalWindow title="terminal.rs">
          <div>
            <Tok c="comment">{"// PTY + alacritty 状态机"}</Tok>
          </div>
          <div>
            <Tok c="keyword">pub struct</Tok> <Tok c="function">Terminal</Tok> {"{"}
          </div>
          <div>
            &nbsp;&nbsp;<Tok c="property">term</Tok>:{" "}
            <Tok c="function">Arc</Tok>
            {"<"}
            <Tok c="function">Mutex</Tok>
            {"<"}
            <Tok c="function">Term</Tok>
            {"<EventProxy>>>,"}
          </div>
          <div>
            &nbsp;&nbsp;<Tok c="property">pty</Tok>:{" "}
            <Tok c="function">Box</Tok>
            {"<dyn "}
            <Tok c="function">MasterPty</Tok>
            {">,"}
          </div>
          <div>{"}"}</div>
          <div>&nbsp;</div>
          <div>
            <Tok c="keyword">impl</Tok> <Tok c="function">Terminal</Tok> {"{"}
          </div>
          <div>
            &nbsp;&nbsp;<Tok c="keyword">pub fn</Tok>{" "}
            <Tok c="function">spawn</Tok>(shell: &<Tok c="function">str</Tok>){" "}
            {"->"} <Tok c="function">Result</Tok>
            {"<Self> {"}
          </div>
          <div>
            &nbsp;&nbsp;&nbsp;&nbsp;<Tok c="keyword">let</Tok> pty ={" "}
            <Tok c="function">native_pty_system</Tok>().
            <Tok c="function">openpty</Tok>(size)?;
          </div>
          <div>
            &nbsp;&nbsp;&nbsp;&nbsp;<Tok c="function">Ok</Tok>(
            <Tok c="function">Self</Tok> {"{ term, pty: pty.master }"})
          </div>
          <div>&nbsp;&nbsp;{"}"}</div>
          <div>{"}"}</div>
        </TerminalWindow>
      </div>
    </section>
  );
}
