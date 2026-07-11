//! Git 面板：状态/分支/diff 查看、暂存/提交/推送、worktree 新建与删除。
//!
//! 从 main.rs 拆出来的 `impl Workspace` 方法 + 独立渲染/解析/git 子进程调用函数，
//! 字段仍然声明在 main.rs 的 `Workspace` struct 里（没有挪成子结构体）——这样搬
//! 纯粹是「剪切代码位置」，不改变量访问方式，风险跟改动量不成正比地小。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::Input;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tag::Tag;
use gpui_component::*;
use notify::{RecursiveMode, Watcher};

use crate::{agent, placeholder_view, terminal_view, Workspace};

// ===================== 类型 =====================

/// 「新建 Worktree」弹窗要新建在哪个仓库：repo_root 可以是主仓库、也可以是任意一个
/// 已存在的 worktree（`git worktree add` 从哪个检出发起都行，git 会自动解析到公共
/// 仓库），repo_label 纯展示用。
#[derive(Clone)]
pub struct NewWorktreeTarget {
    pub repo_root: String,
    pub repo_label: String,
}

/// 「删除 Worktree」弹窗要删的目标：path 是待删的 worktree 检出目录；main_root 是
/// 同仓库下另一个稳定存在的目录（主仓库根），`git worktree remove` 必须从别处发起，
/// 不能从待删目录自己发起。dirty = None 表示后台「有没有未提交改动」还没探测完，
/// 弹窗先显示"检查中"。
#[derive(Clone)]
pub struct DeleteWorktreeTarget {
    pub path: String,
    pub main_root: String,
    pub branch: String,
    pub dirty: Option<bool>,
}

/// diff 行的类型，决定行号显示、前景色、整行背景与左侧色条。
#[derive(Clone, Copy, PartialEq)]
enum DiffKind {
    Add,     // 增行（+）
    Del,     // 删行（-）
    Context, // 上下文行（空格）
    Hunk,    // @@ 段头
    Meta,    // diff/index/+++/--- 等元信息
}

/// 一行 diff：旧/新行号（None 表示该侧无此行）、类型、去掉 +/-/空格前缀的文本。
/// segments 为 Some 时表示做过行内 diff：每段 (文本, 是否变化)，变化段渲染时上深底。
struct DiffLine {
    old_ln: Option<u32>,
    new_ln: Option<u32>,
    kind: DiffKind,
    text: String,
    segments: Option<Vec<(String, bool)>>,
}

/// Git 视图里当前选中查看的文件 diff：文件相对路径 + 结构化的 diff 行。
/// 用 Rc 供 uniform_list 闭包共享。
pub struct GitDiff {
    path: String,
    lines: Rc<Vec<DiffLine>>,
}

/// 一次 `git status` 的缓存结果（后台跑、render 只读，绝不在 render 同步跑 git）。
#[derive(Clone, Default)]
pub struct GitStatusData {
    /// git 命令是否成功（false = 不是 git 仓库 / git 不可用）。
    ok: bool,
    /// 当前分支名。
    branch: String,
    /// 跟踪的上游分支名（如 `origin/main`），没配上游就是 None。
    upstream: Option<String>,
    /// 领先上游的提交数（本地有、上游没有）。
    ahead: u32,
    /// 落后上游的提交数（上游有、本地没有）。
    behind: u32,
    /// 改动文件：(porcelain 两位状态码, 路径)。file_tree.rs 借它给改动文件标红点，
    /// 所以是 pub（main.rs 转手把这份列表传过去，file_tree.rs 不需要认识
    /// GitStatusData 本身，只拿这个字段）。
    pub files: Vec<(String, String)>,
}

/// 一次 `git for-each-ref` 探测的分支列表，给 Git 页头部的分支切换下拉用。
#[derive(Clone, Default)]
pub struct BranchList {
    /// 本地分支名（不含 `refs/heads/` 前缀）。
    local: Vec<String>,
    /// 远程分支名（含 remote 前缀，如 `origin/feature-x`；`<remote>/HEAD` 这种
    /// 符号引用已经过滤掉，不是真分支）。
    remote: Vec<String>,
}

/// 一次 `git rev-parse --git-dir --git-common-dir --abbrev-ref HEAD` 探测的结果。
/// git-dir 和 common-dir 不同就说明这个 cwd 是 worktree 检出（不是主仓库）；
/// common-dir 是它和主仓库共享的公共 `.git` 路径，拿来判断"同一个仓库"、侧栏聚簇
/// 排序，以及反推主仓库根目录（其父目录）。branch 拼进 worktree 分组的显示名。
#[derive(Clone)]
pub struct RepoInfo {
    pub git_dir: String,
    pub common_dir: String,
    pub branch: String,
}

impl RepoInfo {
    pub fn is_worktree(&self) -> bool {
        self.git_dir != self.common_dir
    }
}

/// 并排视图的一行：Both = 左(旧侧)/右(新侧)各一行（None 为空侧占位）；
/// Full = 横跨整宽的 hunk/meta 行。存的是 GitDiff.lines 里的索引。
enum SplitRow {
    Both(Option<usize>, Option<usize>),
    Full(usize),
}

/// 文件查看的固定行高（供 diff 视图 uniform_list 虚拟滚动，需每行等高）。
const FILE_LINE_H: f32 = 20.0;

// ===================== git 子进程调用 =====================

/// 跑一条 git 子命令：固定 `-C root` + `GIT_OPTIONAL_LOCKS=0`（避免刷新索引 stat 缓存
/// 抢 .git/index.lock——之前吃过这个亏，见 ensure_git_status 的注释）。
pub fn run_git(root: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
}

/// `out` 失败时把 stderr 整理成错误文案；stderr 为空（有些失败模式不写 stderr）就用
/// fallback 兜底，不让用户看见空字符串报错。
fn git_err(out: &std::process::Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() { fallback.to_string() } else { stderr }
}

/// Git 视图「AI 生成」commit message 的数据来源：优先 `git diff --staged`（已 add
/// 的改动，正常提交前的状态），staged 为空就退回 `git diff`（工作区未暂存改动，
/// 方便还没 `git add` 就想先看看 AI 怎么总结）。都没有 = 没改动，返回 None。
/// diff 太长会吃掉大量 token，截到前 8000 字符——commit message 只需要看个大概，
/// 不需要逐行精读。绝不能在调用方所在的主线程/render 里跑，走后台执行器。
fn collect_commit_diff(root: &str) -> Option<String> {
    let run = |args: &[&str]| {
        run_git(root, args)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let diff = run(&["diff", "--staged"]).filter(|d| !d.trim().is_empty());
    let diff = diff.or_else(|| run(&["diff"]).filter(|d| !d.trim().is_empty()))?;
    const MAX_LEN: usize = 8000;
    if diff.len() > MAX_LEN {
        let cut = diff.char_indices().map(|(i, _)| i).take_while(|&i| i <= MAX_LEN).last().unwrap_or(0);
        Some(format!("{}\n…（diff 过长，已截断）", &diff[..cut]))
    } else {
        Some(diff)
    }
}

/// 后台执行 `git worktree add`：分支已存在就直接检出，不存在就 `-b` 新建（从当前
/// HEAD 出来）。先把目标目录的上级目录建好——老版本 git 的 `worktree add` 不会自动
/// 建多层目录。绝不能在调用方所在的主线程/render 里跑，git 在大仓可能要几百 ms。
fn create_worktree(repo_root: &str, branch: &str, path: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 先按「检出已有分支」尝试：本地已有这个分支、或者只有唯一一个 remote 有同名
    // 分支（git worktree add 内建的 DWIM，跟 `git checkout <branch>` 同一套逻辑，
    // 会自动建好跟踪分支）都会在这一步直接成功。不预先自己去查 refs/heads——那样
    // 会漏掉"只在 remote 上存在、本地还没跟踪分支"这种情况，误判成"不存在"然后
    // 新建出一个从当前 HEAD 分叉的同名分支。真的哪儿都找不到这个名字，这步才会
    // 失败，退到下面 -b 新建。
    let checkout =
        run_git(repo_root, &["worktree", "add", path, branch]).map_err(|e| e.to_string())?;
    if checkout.status.success() {
        return Ok(());
    }
    let first_err = String::from_utf8_lossy(&checkout.stderr).trim().to_string();
    let created = run_git(repo_root, &["worktree", "add", "-b", branch, path])
        .map_err(|e| e.to_string())?;
    if created.status.success() {
        Ok(())
    } else if !first_err.is_empty() {
        // 两步都失败：优先把第一步（检出已有分支）的错误报出去——像"这个分支已经
        // 在别的 worktree 检出了"这种根因，第一步的报错比第二步"分支已存在"更有用。
        Err(first_err)
    } else {
        Err(git_err(&created, "git worktree add 失败"))
    }
}

/// 后台执行 `git worktree remove`：必须从 main_root（同仓库下另一个稳定目录）发起，
/// 不能从待删的 path 自己发起。force 由调用方根据「有没有未提交改动」+ 用户确认决定。
fn remove_worktree(main_root: &str, path: &str, force: bool) -> Result<(), String> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    let out = run_git(main_root, &args).map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(git_err(&out, "git worktree remove 失败"))
    }
}

/// 后台执行 `git commit -m <message>`，push=true 再接一次 `git push`。commit message
/// 是当 `-m` 的参数值直接传给 Command（不经过 shell），不存在拼接/转义问题。push
/// 没配上游分支时（新分支第一次推最常见）plain push 会失败，退到显式
/// `push -u origin <branch>`——跟 create_worktree／checkout_branch 判断分支存不存在
/// 同一个"先试常规操作、不行再退到兜底方案"的路子。
fn commit_and_maybe_push(root: &str, message: &str, push: bool, branch: &str) -> Result<(), String> {
    let commit = run_git(root, &["commit", "-m", message]).map_err(|e| e.to_string())?;
    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&commit.stdout).trim().to_string();
        return Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "git commit 失败".to_string()
        });
    }
    if !push {
        return Ok(());
    }
    // GIT_TERMINAL_PROMPT=0：没有凭据缓存时 git 默认会弹交互式用户名/密码输入，但这个
    // 子进程没有 TTY，会一直卡住而不是报错。禁掉交互提示后 git 会直接失败退出，
    // 报错信息进 stderr，能正常走下面的错误提示，而不是无声挂起。
    let attempt = std::process::Command::new("git")
        .args(["-C", root, "push"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| e.to_string())?;
    if attempt.status.success() {
        return Ok(());
    }
    if !branch.is_empty() {
        let fallback = std::process::Command::new("git")
            .args(["-C", root, "push", "-u", "origin", branch])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| e.to_string())?;
        if fallback.status.success() {
            return Ok(());
        }
        return Err(git_err(&fallback, "git push 失败"));
    }
    Err(git_err(&attempt, "git push 失败"))
}

/// 解析 `git status --porcelain=v1 -b` 的 `## ` 行：branch 名 + 上游分支名（有的话）+
/// ahead/behind 计数。这行的格式是 `<branch>...<upstream> [ahead N, behind M]`——
/// 没配上游就只有 `<branch>`（或 detached HEAD 时是 `HEAD (no branch)`），没有
/// ahead/behind 差异时方括号那截也不出现。
fn parse_branch_status_line(b: &str) -> (String, Option<String>, u32, u32) {
    let Some((head, rest)) = b.split_once("...") else {
        return (b.trim().to_string(), None, 0, 0);
    };
    let (upstream, bracket) = match rest.split_once(" [") {
        Some((u, tail)) => (u.trim().to_string(), Some(tail.trim_end_matches(']'))),
        None => (rest.trim().to_string(), None),
    };
    let mut ahead = 0u32;
    let mut behind = 0u32;
    if let Some(bracket) = bracket {
        for part in bracket.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
    (head.trim().to_string(), Some(upstream), ahead, behind)
}

/// common-dir（形如 `/path/to/repo/.git`）反推主仓库目录名，纯展示用（worktree 分组
/// 标签「仓库名 · 分支名」的前半截）。
pub fn repo_label_from_common_dir(common_dir: &str) -> Option<String> {
    Path::new(common_dir).parent()?.file_name()?.to_str().map(String::from)
}

/// common-dir 反推主仓库根目录的完整路径——`git worktree remove` 不能从待删的
/// worktree 自己发起，得从同仓库下别的稳定目录（主仓库根）跑。
pub fn main_repo_root_from_common_dir(common_dir: &str) -> Option<String> {
    Path::new(common_dir).parent()?.to_str().map(String::from)
}

/// worktree 落脚目录：`~/.smelt/worktrees/<仓库名>/<分支名>`——集中放在 smelt 自己的
/// 地盘（跟 workspace.json/config.toml 同一惯例），而不是仓库旁边的 sibling 目录，
/// 这样"删除 Worktree"能放心整个目录一起删，不用去猜这个目录是不是 smelt 建的。
fn worktrees_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("worktrees"))
}

/// 把仓库名 / 分支名转成安全的文件名片段：只留字母数字和 `-_./`，其余（含空格）
/// 折成 `-`，连续的 `-` 合并、掐头去尾。分支名允许保留 `/`（`feature/foo` 这种很
/// 常见，落到路径里就是嵌套目录，`git worktree add` 会自己建好中间目录）。
fn slugify_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '/' {
            out.push(c);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ===================== diff 解析 =====================

/// 把 git diff 文本解析成结构化的行：从 @@ 段头取起始行号，逐行推进旧/新行号，
/// 并按前缀判定类型、剥掉 +/-/空格前缀。空 diff 给一句提示。
fn parse_diff(text: &str) -> Vec<DiffLine> {
    let mk = |old_ln, new_ln, kind, text: &str| DiffLine {
        old_ln,
        new_ln,
        kind,
        text: text.to_string(),
        segments: None,
    };
    if text.trim().is_empty() {
        return vec![mk(None, None, DiffKind::Meta, "（无差异）")];
    }
    let mut old_ln = 0u32;
    let mut new_ln = 0u32;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("@@") {
            let (o, n) = parse_hunk(line);
            old_ln = o;
            new_ln = n;
            out.push(mk(None, None, DiffKind::Hunk, line));
        } else if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
        {
            out.push(mk(None, None, DiffKind::Meta, line));
        } else if let Some(t) = line.strip_prefix('+') {
            out.push(mk(None, Some(new_ln), DiffKind::Add, t));
            new_ln += 1;
        } else if let Some(t) = line.strip_prefix('-') {
            out.push(mk(Some(old_ln), None, DiffKind::Del, t));
            old_ln += 1;
        } else {
            // 上下文行（以空格开头，或 diff 末尾的空行）。
            let t = line.strip_prefix(' ').unwrap_or(line);
            out.push(mk(Some(old_ln), Some(new_ln), DiffKind::Context, t));
            old_ln += 1;
            new_ln += 1;
        }
    }
    mark_inline(&mut out);
    out
}

/// 后处理：对每组「连续删行紧跟连续增行」按顺序逐行配对，做字符级 inline diff，
/// 把两侧变化的具体片段标出来（存进各自的 segments）。行太长则跳过（避免 O(n·m)）。
fn mark_inline(lines: &mut [DiffLine]) {
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if lines[i].kind != DiffKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < n && lines[i].kind == DiffKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < n && lines[i].kind == DiffKind::Add {
            i += 1;
        }
        let pairs = (add_start - del_start).min(i - add_start);
        for k in 0..pairs {
            let (di, ai) = (del_start + k, add_start + k);
            let (dt, at) = (lines[di].text.clone(), lines[ai].text.clone());
            if dt.len() + at.len() > 4000 {
                continue;
            }
            let (dseg, aseg) = inline_segments(&dt, &at);
            lines[di].segments = Some(dseg);
            lines[ai].segments = Some(aseg);
        }
    }
}

/// 对一对 (旧行, 新行) 做字符级 diff，分别产出两侧的 (片段, 是否变化) 列表。
/// 旧行里被删除的字符标变化，新行里新增的字符标变化，相等部分两侧都不标。
fn inline_segments(old: &str, new: &str) -> (Vec<(String, bool)>, Vec<(String, bool)>) {
    let diff = similar::TextDiff::from_chars(old, new);
    let mut olds: Vec<(String, bool)> = Vec::new();
    let mut news: Vec<(String, bool)> = Vec::new();
    // 把相邻同状态的字符合并成段，减少 span 数量。
    let push = |v: &mut Vec<(String, bool)>, ch: &str, changed: bool| {
        if let Some(last) = v.last_mut() {
            if last.1 == changed {
                last.0.push_str(ch);
                return;
            }
        }
        v.push((ch.to_string(), changed));
    };
    for change in diff.iter_all_changes() {
        let val = change.value();
        match change.tag() {
            similar::ChangeTag::Equal => {
                push(&mut olds, val, false);
                push(&mut news, val, false);
            }
            similar::ChangeTag::Delete => push(&mut olds, val, true),
            similar::ChangeTag::Insert => push(&mut news, val, true),
        }
    }
    (olds, news)
}

/// 从 hunk 头 `@@ -a,b +c,d @@` 解析出旧/新起始行号（a、c）。
fn parse_hunk(line: &str) -> (u32, u32) {
    let (mut old, mut new) = (0u32, 0u32);
    for tok in line.split_whitespace() {
        if let Some(s) = tok.strip_prefix('-') {
            old = s.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        } else if let Some(s) = tok.strip_prefix('+') {
            new = s.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        }
    }
    (old, new)
}

// ===================== diff 渲染 =====================

/// diff 行类型 → (前景, 整行背景, 左色条, 行内变化片段深底)。
fn diff_colors(kind: DiffKind) -> (Rgba, Option<Rgba>, Option<Rgba>, Rgba) {
    match kind {
        DiffKind::Add => (rgb(0xb5e08a), Some(rgb(0x16261a)), Some(rgb(0x4ba14b)), rgb(0x2f6b34)),
        DiffKind::Del => (rgb(0xf7a3ae), Some(rgb(0x2a1620)), Some(rgb(0xc75c6a)), rgb(0x7a2836)),
        DiffKind::Context => (rgb(0xc0caf5), None, None, rgb(0)),
        DiffKind::Hunk => (rgb(0x7dcfff), Some(rgb(0x16202e)), None, rgb(0)),
        DiffKind::Meta => (rgb(0x565f89), None, None, rgb(0)),
    }
}

/// 文本区（flex_1）：有 segments 就拆成多段（变化段上深底），否则整行一段。
fn diff_text_area(l: &DiffLine, fg: Rgba, hl: Rgba) -> Div {
    match &l.segments {
        Some(segs) => div().flex_1().px_2().text_color(fg).flex().children(segs.iter().map(
            |(s, changed)| {
                let span = div().child(s.clone());
                if *changed {
                    span.bg(hl).rounded_sm()
                } else {
                    span
                }
            },
        )),
        None => div()
            .flex_1()
            .px_2()
            .text_color(fg)
            .child(if l.text.is_empty() { "\u{00a0}".to_string() } else { l.text.clone() }),
    }
}

/// 渲染一行 diff：左侧色条 + 旧/新行号槽 + 文本；整行按类型上淡背景。
/// 若有 segments（行内 diff 结果），变化片段再叠一层更深的底色。
/// 增/删行（i 为 GitDiff.lines 下标）可点选：选中态描边，点击切给 Workspace
/// 的 toggle_diff_line，配合底部评论框批量发给当前终端。
fn render_diff_line(i: usize, l: &DiffLine, selected: bool, ws: &Entity<Workspace>) -> Stateful<Div> {
    let (fg, bg, bar, hl) = diff_colors(l.kind);
    let gutter = |n: Option<u32>| {
        div()
            .w(px(44.))
            .px_1()
            .flex()
            .justify_end()
            .text_color(rgb(0x4a5178))
            .child(n.map(|v| v.to_string()).unwrap_or_default())
    };

    let mut row = div()
        .id(("diff-line", i))
        .flex()
        .items_center()
        .h(px(FILE_LINE_H))
        .whitespace_nowrap();
    if let Some(b) = bg {
        row = row.bg(b);
    }
    if matches!(l.kind, DiffKind::Add | DiffKind::Del) {
        row = row.cursor_pointer();
        if selected {
            row = row.border_2().border_color(rgb(0x4a9eff));
        }
        let ws = ws.clone();
        row = row.on_click(move |_ev, _window, cx| {
            ws.update(cx, |this, cx| this.toggle_diff_line(i, cx));
        });
    }
    row
        // 左侧色条：增/删才有，其它用等宽透明占位保持对齐。
        .child(match bar {
            Some(c) => div().w(px(2.)).h_full().bg(c),
            None => div().w(px(2.)).h_full(),
        })
        .child(gutter(l.old_ln))
        .child(gutter(l.new_ln))
        .child(diff_text_area(l, fg, hl))
}

/// 把线性的 diff 行重排成并排的行对：上下文左右对齐；一组删/增按顺序配对，
/// 数量不等时多出的一侧留空；纯新增（无对应删行）左侧空。
fn build_split_rows(lines: &[DiffLine]) -> Vec<SplitRow> {
    let n = lines.len();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < n {
        match lines[i].kind {
            DiffKind::Hunk | DiffKind::Meta => {
                rows.push(SplitRow::Full(i));
                i += 1;
            }
            DiffKind::Context => {
                rows.push(SplitRow::Both(Some(i), Some(i)));
                i += 1;
            }
            DiffKind::Del => {
                let ds = i;
                while i < n && lines[i].kind == DiffKind::Del {
                    i += 1;
                }
                let de = i;
                let as_ = i;
                while i < n && lines[i].kind == DiffKind::Add {
                    i += 1;
                }
                let ae = i;
                let (dn, an) = (de - ds, ae - as_);
                for k in 0..dn.max(an) {
                    let l = (k < dn).then_some(ds + k);
                    let r = (k < an).then_some(as_ + k);
                    rows.push(SplitRow::Both(l, r));
                }
            }
            DiffKind::Add => {
                // 纯新增块（前面没有删行）：左侧空、右侧逐行。
                while i < n && lines[i].kind == DiffKind::Add {
                    rows.push(SplitRow::Both(None, Some(i)));
                    i += 1;
                }
            }
        }
    }
    rows
}

/// 渲染并排的半行（左或右，flex_1）。idx 为 None 时是空侧占位（暗底）。
/// left=true 用旧行号，否则用新行号。ri 是并排行在 rows 里的下标，只用来拼 id
/// （idx 本身在 Both(None, Some(i)) 这类情况下左右可能撞号，ri+left 才唯一）。
/// 增/删行同 render_diff_line 一样可点选，选中态描边。
fn render_half(
    ri: usize,
    idx: Option<usize>,
    left: bool,
    lines: &[DiffLine],
    selected: &HashSet<usize>,
    ws: &Entity<Workspace>,
) -> Stateful<Div> {
    // overflow_hidden：长行必须裁剪在本半区内，否则会溢出盖住另一半，并排就糊了。
    let base = div()
        .id(("diff-half", ri * 2 + left as usize))
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .items_center()
        .h_full();
    let Some(i) = idx else {
        // 空侧：略暗的底表示「此侧无对应行」。
        return base.bg(rgb(0x101218));
    };
    let l = &lines[i];
    let (fg, bg, bar, hl) = diff_colors(l.kind);
    let ln = if left { l.old_ln } else { l.new_ln };
    let mut row = base;
    if let Some(b) = bg {
        row = row.bg(b);
    }
    if matches!(l.kind, DiffKind::Add | DiffKind::Del) {
        row = row.cursor_pointer();
        if selected.contains(&i) {
            row = row.border_2().border_color(rgb(0x4a9eff));
        }
        let ws = ws.clone();
        row = row.on_click(move |_ev, _window, cx| {
            ws.update(cx, |this, cx| this.toggle_diff_line(i, cx));
        });
    }
    row.child(match bar {
        Some(c) => div().w(px(2.)).h_full().bg(c),
        None => div().w(px(2.)).h_full(),
    })
    .child(
        div()
            .w(px(44.))
            .px_1()
            .flex()
            .justify_end()
            .text_color(rgb(0x4a5178))
            .child(ln.map(|v| v.to_string()).unwrap_or_default()),
    )
    .child(diff_text_area(l, fg, hl))
}

/// 渲染并排视图的一行。ri 是该行在 rows 里的下标，透传给 render_half 拼 id。
fn render_split_row(
    ri: usize,
    row: &SplitRow,
    lines: &[DiffLine],
    selected: &HashSet<usize>,
    ws: &Entity<Workspace>,
) -> Div {
    match row {
        SplitRow::Full(i) => {
            let l = &lines[*i];
            let (fg, bg, _, _) = diff_colors(l.kind);
            let mut d = div()
                .flex()
                .items_center()
                .h(px(FILE_LINE_H))
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap();
            if let Some(b) = bg {
                d = d.bg(b);
            }
            d.child(div().px_2().text_color(fg).child(l.text.clone()))
        }
        SplitRow::Both(l, r) => div()
            .flex()
            .items_center()
            .h(px(FILE_LINE_H))
            // w_full 关键：容器占满整宽，两个 flex_1 半区才会真正各占一半；
            // 否则容器 hug content，grow 失效，空侧塌成 0 宽、内容顶到最左。
            .w_full()
            .whitespace_nowrap()
            .child(render_half(ri, *l, true, lines, selected, ws))
            .child(div().w(px(1.)).h_full().bg(rgb(0x2a2e3d))) // 中缝分隔
            .child(render_half(ri, *r, false, lines, selected, ws)),
    }
}

/// Git diff 查看面板：uniform_list 虚拟滚动。split 为 true 时并排（左旧右新），
/// 否则统一视图。顶部文件名右侧有「统一/并排」切换按钮。改动行（+/-）可点选，
/// 选中后配合底部评论框「发送到终端」，把反馈批量写进当前激活终端的 PTY。
fn git_diff_pane(
    root: &str,
    git_diff: &Option<GitDiff>,
    split: bool,
    diff_selected: &HashSet<usize>,
    diff_comment_input: Option<&Entity<gpui_component::input::InputState>>,
    diff_scroll: &UniformListScrollHandle,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.accent)
    };
    match git_diff {
        None => placeholder_view("← 选择改动文件查看 diff", muted),
        Some(d) => {
            let name = d.path.rsplit('/').next().unwrap_or(d.path.as_str()).to_string();
            let lines = d.lines.clone();
            let ws = cx.entity();
            // 完整文件路径：diff 里的 path 是相对仓库根的，拼上 root 才是 view_file
            // 要的绝对路径。
            let full_path = Path::new(root).join(&d.path).to_string_lossy().to_string();
            let ws_open = ws.clone();
            let open_full_file = div()
                .id("diff-view-full-file")
                .px_2()
                .py(px(1.0))
                .text_xs()
                .cursor_pointer()
                .text_color(muted)
                .hover(|s| s.text_color(fg))
                .child("查看完整文件 ↗")
                .on_click(move |_ev, window, cx| {
                    let path = full_path.clone();
                    ws_open.update(cx, |wsx, cx| {
                        wsx.view = crate::MainView::Files;
                        wsx.view_file(path, window, cx);
                    });
                });

            let list = if split {
                let rows = Rc::new(build_split_rows(&lines));
                let count = rows.len();
                let lines2 = lines.clone();
                let sel2 = diff_selected.clone();
                let ws2 = ws.clone();
                uniform_list("git-diff-split", count, move |range, _w, _cx| {
                    range
                        .map(|i| render_split_row(i, &rows[i], &lines2, &sel2, &ws2))
                        .collect::<Vec<_>>()
                })
            } else {
                let count = lines.len();
                let sel2 = diff_selected.clone();
                let ws2 = ws.clone();
                uniform_list("git-diff", count, move |range, _w, _cx| {
                    range
                        .map(|i| render_diff_line(i, &lines[i], sel2.contains(&i), &ws2))
                        .collect::<Vec<_>>()
                })
            }
            .flex_1()
            .min_h_0()
            .w_full()
            .py_1()
            .font_family(terminal_view::font_family())
            .text_sm()
            .track_scroll(diff_scroll);

            // 「统一 / 并排」切换按钮。
            let toggle = div()
                .id("diff-split-toggle")
                .px_2()
                .py(px(1.0))
                .text_xs()
                .rounded_sm()
                .cursor_pointer()
                .text_color(fg)
                .bg(accent)
                .hover(|d| d.opacity(0.8))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.diff_split = !this.diff_split;
                    cx.notify();
                }))
                .child(if split { "并排 ⇄" } else { "统一 ☰" }.to_string());

            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(muted)
                        .border_b_1()
                        .border_color(border)
                        .child(div().flex_1().min_w_0().child(name))
                        .child(open_full_file)
                        .child(toggle),
                )
                // 包一层 relative 容器承载 gpui-component 竖向滚动条（覆盖在 diff 上）。
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(list)
                        .vertical_scrollbar(diff_scroll),
                )
                .child(diff_comment_bar(diff_selected, diff_comment_input, cx))
        }
    }
}

/// 交互式 diff 底部工具条：已选行数提示 + 评论输入框 + 「发送到终端」按钮，
/// 发送目标固定是当前激活的终端标签（不跨项目匹配，简单直接）。
fn diff_comment_bar(
    selected: &HashSet<usize>,
    input: Option<&Entity<gpui_component::input::InputState>>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, border) = {
        let t = cx.theme();
        (t.muted_foreground, t.border)
    };
    let n = selected.len();
    let can_send = n > 0;
    let hint =
        if n == 0 { "点选中改动行（+/-），可选写评论，发给当前终端".to_string() } else { format!("已选 {n} 行") };
    let ws = cx.entity();

    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(border)
        .child(div().flex_none().text_xs().text_color(muted).child(hint))
        .children(input.map(|state| div().flex_1().min_w_0().child(Input::new(state).small())))
        .child(
            Button::new("diff-send")
                .small()
                .label("发送到终端")
                .disabled(!can_send)
                .on_click(move |_ev, window, cx| {
                    ws.update(cx, |this, cx| this.send_diff_comments(window, cx));
                }),
        )
}

/// Git 视图左栏底部的 commit message 条：输入框 +「生成」（AI 起草，见
/// Workspace::generate_commit_message，读整个仓库的 diff 而非只是选中的行）+
/// 「发送到终端」（拼成 `git commit -m '...'` 写进当前激活终端，不自动回车，等
/// 用户自己看一眼、needed 的话改两个字再确认执行——跟 diff_comment_bar 一个哲学）。
fn commit_message_bar(
    input: Option<&Entity<gpui_component::input::InputState>>,
    generating: bool,
    cx: &mut Context<Workspace>,
) -> Div {
    let border = cx.theme().border;
    let has_text = input.is_some_and(|s| !s.read(cx).value().trim().is_empty());
    let ws_gen = cx.entity();
    let ws_commit = ws_gen.clone();
    let ws_push = ws_gen.clone();

    div()
        .flex()
        .flex_col()
        .gap_2()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(border)
        .children(input.map(|state| Input::new(state).small()))
        .child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("commit-msg-generate")
                        .small()
                        .label(if generating { "生成中…" } else { "AI 生成" })
                        .disabled(generating)
                        .on_click(move |_ev, window, cx| {
                            ws_gen.update(cx, |this, cx| this.generate_commit_message(window, cx));
                        }),
                )
                .child(
                    Button::new("commit-msg-commit")
                        .small()
                        .label("提交")
                        .disabled(!has_text)
                        .on_click(move |_ev, window, cx| {
                            ws_commit.update(cx, |this, cx| this.commit(false, window, cx));
                        }),
                )
                .child(
                    Button::new("commit-msg-commit-push")
                        .small()
                        .primary()
                        .label("提交并推送")
                        .disabled(!has_text)
                        .on_click(move |_ev, window, cx| {
                            ws_push.update(cx, |this, cx| this.commit(true, window, cx));
                        }),
                ),
        )
}

/// Git 视图：左侧分支 + 改动文件列表（可点击），右侧显示选中文件的 diff。
pub fn git_view(
    cwd: Option<String>,
    status: Option<&GitStatusData>,
    branches: Option<&BranchList>,
    git_diff: &Option<GitDiff>,
    split: bool,
    diff_selected: &HashSet<usize>,
    diff_comment_input: Option<&Entity<gpui_component::input::InputState>>,
    commit_msg_input: Option<&Entity<gpui_component::input::InputState>>,
    commit_msg_generating: bool,
    files_scroll: &ScrollHandle,
    diff_scroll: &UniformListScrollHandle,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.accent)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted);
    };
    // 只读后台缓存（ensure_git_status 负责刷新）：缺失=首次加载中，ok=false=非 git 仓库。
    let Some(data) = status else {
        return placeholder_view("加载改动中…", muted);
    };
    if !data.ok {
        return placeholder_view("不是 git 仓库，或 git 不可用", muted);
    }
    let branch = data.branch.clone();
    let files = data.files.clone();

    let selected = git_diff.as_ref().map(|d| d.path.clone());
    let file_list = if files.is_empty() {
        placeholder_view("工作区干净，无改动 ✓", muted).into_any_element()
    } else {
        div()
            .id("git-files")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(files_scroll)
            .vertical_scrollbar(files_scroll)
            .flex()
            .flex_col()
            .p_1()
            .children(files.into_iter().enumerate().map(|(i, (st, path))| {
                let st_trim = st.trim();
                // 状态标记用 Tag 彩色胶囊：新增=绿 删除=红 修改=黄 未跟踪=灰 其余=蓝。
                let label = if st_trim.is_empty() {
                    "•".to_string()
                } else {
                    st_trim.to_string()
                };
                let status_tag = if st_trim.contains('?') {
                    Tag::secondary()
                } else if st_trim.contains('A') {
                    Tag::success()
                } else if st_trim.contains('D') {
                    Tag::danger()
                } else if st_trim.contains('M') {
                    Tag::warning()
                } else {
                    Tag::info()
                }
                .small()
                .child(label);
                let untracked = st.contains('?');
                let is_sel = selected.as_deref() == Some(path.as_str());
                let (r, p) = (root.clone(), path.clone());
                // 暂存勾选框：索引状态（porcelain 第一位）不是空格/`?` 就算已暂存
                // （`??` 是 untracked，两位都不算暂存；`MM` 这种"暂存过又改"第一位
                // 仍是暂存态，勾着）。纯本地索引操作，直接执行不用发终端确认。
                let staged = st.as_bytes().first().is_some_and(|&b| b != b' ' && b != b'?');
                let ws_stage = cx.entity();
                let (r_stage, p_stage) = (root.clone(), path.clone());
                let stage_checkbox = Checkbox::new(("git-stage", i))
                    .checked(staged)
                    .on_click(move |checked, _window, cx| {
                        cx.stop_propagation();
                        let checked = *checked;
                        let root = r_stage.clone();
                        let path = p_stage.clone();
                        ws_stage.update(cx, |wsx, cx| {
                            if checked {
                                wsx.stage_file(root, path, cx);
                            } else {
                                wsx.unstage_file(root, path, cx);
                            }
                        });
                    });
                let row = div()
                    .id(("git", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(1.0))
                    .text_sm()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_diff(r.clone(), p.clone(), untracked, cx)
                    }))
                    .child(stage_checkbox)
                    .child(status_tag)
                    .child(div().min_w_0().text_color(fg).child(path));
                // 选中项高亮背景（无 .when，用普通条件分支）。
                if is_sel {
                    row.bg(accent)
                } else {
                    row
                }
            }))
            .into_any_element()
    };

    let branch_header = {
        let ahead_behind = match (data.ahead, data.behind) {
            (0, 0) => String::new(),
            (a, 0) => format!("  ↑{a}"),
            (0, b) => format!("  ↓{b}"),
            (a, b) => format!("  ↑{a} ↓{b}"),
        };
        let current = branch.clone();
        let local: Vec<String> = branches.map(|b| b.local.clone()).unwrap_or_default();
        let remote: Vec<String> = branches.map(|b| b.remote.clone()).unwrap_or_default();
        let ws = cx.entity();
        let root_for_menu = root.clone();
        div()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("branch-switch")
                    .ghost()
                    .small()
                    .label(format!("⎇ {branch}{ahead_behind}"))
                    .dropdown_menu(move |menu, _window, _cx| {
                        let mut menu = menu;
                        if local.is_empty() && remote.is_empty() {
                            menu = menu.item(PopupMenuItem::new("（没有其他分支）"));
                        }
                        for name in &local {
                            let item = PopupMenuItem::new(name.clone());
                            menu = menu.item(if *name == current {
                                item.icon(IconName::Check)
                            } else {
                                let ws = ws.clone();
                                let root = root_for_menu.clone();
                                let target = name.clone();
                                item.on_click(move |_ev, _window, cx| {
                                    ws.update(cx, |wsx, cx| {
                                        wsx.checkout_branch(root.clone(), target.clone(), cx)
                                    });
                                })
                            });
                        }
                        if !remote.is_empty() {
                            menu = menu.separator();
                            for name in &remote {
                                let ws = ws.clone();
                                let root = root_for_menu.clone();
                                // 远程分支切换用短名（去掉 `<remote>/` 前缀），走 git 内建
                                // DWIM 自动建好跟踪分支——直接传 `origin/xxx` 全名会变成
                                // detached HEAD，不是我们想要的（同 create_worktree 的判断）。
                                let short =
                                    name.split_once('/').map(|(_, s)| s.to_string()).unwrap_or_else(|| name.clone());
                                menu = menu.item(PopupMenuItem::new(name.clone()).on_click(
                                    move |_ev, _window, cx| {
                                        ws.update(cx, |wsx, cx| {
                                            wsx.checkout_branch(root.clone(), short.clone(), cx)
                                        });
                                    },
                                ));
                            }
                        }
                        menu
                    }),
            )
    };

    let left = div()
        .w(px(300.))
        .min_h_0()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(border)
        .child(branch_header)
        .child(file_list)
        .child(commit_message_bar(commit_msg_input, commit_msg_generating, cx));

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .child(left)
        .child(git_diff_pane(&root, git_diff, split, diff_selected, diff_comment_input, diff_scroll, cx))
}

// ===================== Workspace 方法 =====================

impl Workspace {
    /// 项目行「+ → 新建 Worktree…」：弹文本框填分支名。repo_root 可以是主仓库、
    /// 也可以是任意一个已存在的 worktree（git 自己会解析到公共仓库），repo_label
    /// 纯展示（弹窗标题里说清是在哪个仓库下新建）。
    pub fn start_new_worktree(
        &mut self,
        repo_root: String,
        repo_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::input::{InputEvent, InputState};
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("分支名，例如 feature/foo"));
        input.update(cx, |s, cx| s.focus(window, cx));
        self._new_worktree_sub = Some(cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.confirm_new_worktree(window, cx);
                }
            },
        ));
        self.new_worktree_target = Some(NewWorktreeTarget { repo_root, repo_label });
        self.new_worktree_input = Some(input);
        cx.notify();
    }

    /// 提交新建 worktree：分支名为空就什么都不做；否则后台跑 `git worktree add`
    /// （见 create_worktree：分支已存在就直接检出，不存在就从当前 HEAD 新建），成功
    /// 后在新目录里开一个会话并切过去；失败写 background_error，交给 render 顶部弹
    /// 通知（后台任务里没有 Window，弹不了）。
    pub fn confirm_new_worktree(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.new_worktree_target.take() else { return };
        let Some(input) = self.new_worktree_input.take() else { return };
        self._new_worktree_sub = None;
        cx.notify();
        let branch = input.read(cx).value().trim().to_string();
        if branch.is_empty() {
            return;
        }
        let Some(worktrees_root) = worktrees_root() else { return };
        let repo_slug = slugify_path_segment(&target.repo_label);
        let branch_slug = slugify_path_segment(&branch);
        if repo_slug.is_empty() || branch_slug.is_empty() {
            self.background_error = Some("分支名不能是空的或全是特殊字符".to_string());
            return;
        }
        let path = worktrees_root.join(repo_slug).join(branch_slug).to_string_lossy().to_string();
        let repo_root = target.repo_root;
        cx.spawn(async move |this, cx| {
            let path_for_git = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { create_worktree(&repo_root, &branch, &path_for_git) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.add_session(Some(path), cx),
                Err(err) => {
                    this.background_error = Some(err);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 取消新建 worktree：不落地任何改动。
    pub fn cancel_new_worktree(&mut self, cx: &mut Context<Self>) {
        self.new_worktree_target = None;
        self.new_worktree_input = None;
        self._new_worktree_sub = None;
        cx.notify();
    }

    /// 项目行右键「删除 Worktree」（仅 worktree 分组显示，见 render 里 is_worktree_group）：
    /// 先弹窗（dirty=None，显示"检查中…"），后台探测有没有未提交改动，探测完再把
    /// dirty 写回去驱动弹窗文案/是否要红色警告。main_root 是同仓库下的主仓库根目录，
    /// 真正执行删除时 `git worktree remove` 要从那跑（不能从待删目录自己发起）。
    pub fn start_delete_worktree(
        &mut self,
        path: String,
        main_root: String,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        self.delete_worktree_target =
            Some(DeleteWorktreeTarget { path: path.clone(), main_root, branch, dirty: None });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let dirty = cx
                .background_executor()
                .spawn(async move {
                    run_git(&p, &["status", "--porcelain"])
                        .ok()
                        .is_some_and(|o| o.status.success() && !o.stdout.is_empty())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // 弹窗期间用户可能已经取消/又点了别的 worktree，只在还是同一个目标时写回。
                if this.delete_worktree_target.as_ref().is_some_and(|t| t.path == path) {
                    if let Some(t) = this.delete_worktree_target.as_mut() {
                        t.dirty = Some(dirty);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 确认删除：先关掉这个 worktree 下的所有会话，再后台跑 `git worktree remove`
    /// （探测出有未提交改动就带 --force——用户已经在弹窗里看到红色警告并主动点了
    /// 确定）。失败写 background_error，交给 render 顶部弹通知。
    pub fn confirm_delete_worktree(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.delete_worktree_target.take() else { return };
        cx.notify();
        self.close_sessions_under(&target.path, cx);
        let force = target.dirty.unwrap_or(false);
        cx.spawn(async move |this, cx| {
            let path = target.path.clone();
            let main_root = target.main_root.clone();
            let result = cx
                .background_executor()
                .spawn(async move { remove_worktree(&main_root, &path, force) })
                .await;
            if let Err(err) = result {
                let _ = this.update(cx, |this, cx| {
                    this.background_error = Some(err);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 取消删除 worktree：不落地任何改动。
    pub fn cancel_delete_worktree(&mut self, cx: &mut Context<Self>) {
        self.delete_worktree_target = None;
        cx.notify();
    }

    /// 「新建 Worktree」弹窗：填分支名，回车 / 点「新建」提交（confirm_new_worktree），
    /// 点「取消」什么都不发生。视觉同 render_rename_session 一套（居中卡片 + 半透明
    /// 遮罩）。
    pub fn render_new_worktree_dialog(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(false);
        let Some(input) = self.new_worktree_input.as_ref() else { return div() };
        let Some(target) = self.new_worktree_target.as_ref() else { return div() };

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child("新建 Worktree"))
            .child(div().text_sm().text_color(muted).child(format!(
                "在「{}」下新建一个 worktree。分支已存在就直接检出，不存在就从当前 HEAD 新建分支。",
                target.repo_label
            )))
            .child(Input::new(input))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-new-worktree",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.cancel_new_worktree(cx),
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-new-worktree",
                        "新建",
                        tint,
                        hover,
                        accent_text,
                        |this, _, window, cx| this.confirm_new_worktree(window, cx),
                        cx,
                    )),
            );
        Self::modal_shell(360., true, content, cx)
    }

    /// 「删除 Worktree」确认弹窗：dirty 探测完之前（None）按钮禁用显示"检查中…"；
    /// 探测出有未提交改动就红字警告 + 按钮仍可点（--force 由确认后的调用方处理）。
    /// 视觉同 render_daemon_restart_confirm 一套（红色危险操作配色）。
    pub fn render_delete_worktree_confirm(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(true);
        let Some(target) = self.delete_worktree_target.as_ref() else { return div() };

        let (body_text, warn) = match target.dirty {
            None => ("正在检查有没有未提交的改动…".to_string(), false),
            Some(true) => (
                format!(
                    "分支「{}」的这个 worktree 还有未提交的改动，删除后会永久丢失，且其下所有终端会话都会被关闭。",
                    target.branch
                ),
                true,
            ),
            Some(false) => (
                format!("删除分支「{}」的这个 worktree，其下所有终端会话都会被关闭。", target.branch),
                false,
            ),
        };
        let ready = target.dirty.is_some();
        let fg = cx.theme().foreground;

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child("确定删除这个 Worktree 吗？"))
            .child(
                div()
                    .text_sm()
                    .text_color(if warn { accent_text } else { muted })
                    .child(body_text),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-delete-worktree",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.cancel_delete_worktree(cx),
                        cx,
                    ))
                    .child(
                        Self::modal_button_base(
                            "confirm-delete-worktree",
                            if ready { "确定删除" } else { "检查中…" },
                            tint,
                            accent_text,
                        )
                        .when(ready, |el| {
                            el.cursor_pointer().hover(move |s| s.bg(hover)).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.confirm_delete_worktree(cx);
                                },
                            ))
                        }),
                    ),
            );
        Self::modal_shell(360., true, content, cx)
    }

    /// 给某个 root 建一次性的文件监听（notify crate，macOS 走 FSEvents）：仓库目录树
    /// 里任何东西变了就在 git_dirty 里标脏，配合下面 250ms 一次的检查循环，git 页
    /// 基本能做到「文件一变就刷新」而不是干等 1.5s 轮询窗口。
    ///
    /// 递归监听整棵目录树（含 .git/ 内部），没有按 .gitignore 过滤噪音——多余的唤醒
    /// 顶多让 ensure_git_status 多跑一次（已用 GIT_OPTIONAL_LOCKS=0 保证很轻），
    /// 250ms 的检查节流已经把最坏情况锁定在每秒最多 4 次，不会失控。
    /// 只在这个 root 第一次进 Git 页时建一次；watcher 存进 git_watchers 常驻到应用退出
    /// （必须持有 watcher 不被 drop，否则会停止收事件）。
    pub fn ensure_git_watch(&mut self, root: String, cx: &mut Context<Self>) {
        if self.git_watchers.contains_key(&root) {
            return;
        }
        let dirty = self.git_dirty.clone();
        let root_for_cb = root.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                if let Ok(mut set) = dirty.lock() {
                    set.insert(root_for_cb.clone());
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(Path::new(&root), RecursiveMode::Recursive).is_err() {
            return;
        }
        self.git_watchers.insert(root.clone(), watcher);

        let dirty = self.git_dirty.clone();
        cx.spawn(async move |this, cx| loop {
            smol::Timer::after(std::time::Duration::from_millis(250)).await;
            let hit = dirty.lock().is_ok_and(|mut set| set.remove(&root));
            if !hit {
                continue;
            }
            // 只标脏 + 唤醒重绘：真正的重新拉取仍交给 ensure_git_status（render 里
            // 每帧都会调），这里不用重复实现一遍 git status 调用。
            let r = this.update(cx, |this, cx| {
                this.invalidate_git_status(&root);
                cx.notify();
            });
            if r.is_err() {
                break; // Workspace 已销毁
            }
        })
        .detach();
    }

    /// 确保某 cwd 的仓库身份缓存新鲜（>5s 或缺失就后台刷新）：探测它是不是 worktree
    /// 检出、当前分支是什么，供侧栏分组用（见 group_info_for_cwd）。跟 git_status
    /// 不同，这个要对**所有**会话的 cwd 探测（侧栏一直显示全部项目分组，不止当前
    /// 打开的那个），所以单独走一套缓存，TTL 也放宽到 5s——身份和分支不常变，没必要
    /// 跟 git status 一样 1.5s 就重跑。非 git 目录（比如临时终端的 $HOME）缓存
    /// None，同样不重复重试。
    pub fn ensure_repo_info(&mut self, cwd: String, cx: &mut Context<Self>) {
        if cwd.is_empty() {
            return;
        }
        let fresh = self
            .repo_info
            .get(&cwd)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(5));
        if fresh || self.repo_info_inflight.contains(&cwd) {
            return;
        }
        self.repo_info_inflight.insert(cwd.clone());
        cx.spawn(async move |this, cx| {
            let c = cwd.clone();
            let info = cx
                .background_executor()
                .spawn(async move {
                    let out = run_git(
                        &c,
                        &[
                            "rev-parse",
                            "--path-format=absolute",
                            "--git-dir",
                            "--git-common-dir",
                            "--abbrev-ref",
                            "HEAD",
                        ],
                    );
                    let o = out.ok()?;
                    if !o.status.success() {
                        return None;
                    }
                    let text = String::from_utf8_lossy(&o.stdout);
                    let mut lines = text.lines();
                    let git_dir = lines.next()?.to_string();
                    let common_dir = lines.next()?.to_string();
                    let branch = lines.next().unwrap_or("HEAD").to_string();
                    Some(RepoInfo { git_dir, common_dir, branch })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.repo_info_inflight.remove(&cwd);
                this.repo_info.insert(cwd, (Instant::now(), info));
                cx.notify();
            });
        })
        .detach();
    }

    /// cwd → 侧栏分组显示名 + 聚簇 key。是 worktree 检出（git-dir ≠ common-dir）就
    /// 显示「仓库名 · 分支名」，聚簇 key 用 common-dir（worktree 和主仓库共享同一个
    /// 值，project_groups 靠它把同仓库的组排在一起）；非 git 目录 / 身份缓存还没到位
    /// 就退回旧的纯目录末段名（跟改动前完全一致，不会闪烁成别的样子）。
    pub fn group_info_for_cwd(&self, cwd: &str) -> (String, Option<String>) {
        let base_name = crate::project_name_for_cwd(cwd);
        match self.repo_info.get(cwd).and_then(|(_, info)| info.as_ref()) {
            Some(info) if info.is_worktree() => {
                let repo_label = repo_label_from_common_dir(&info.common_dir).unwrap_or(base_name);
                (format!("{repo_label} · {}", info.branch), Some(info.common_dir.clone()))
            }
            Some(info) => (base_name, Some(info.common_dir.clone())),
            None => (base_name, None),
        }
    }

    /// 标记某 root 的 git status 缓存过期，逼下一帧 ensure_git_status 重新拉取——
    /// 但不是直接 `.remove()`：整个删掉的话，git_view 在新数据回来之前那几帧会掉进
    /// "加载改动中…"整页占位，肉眼看就是勾一下框、切一下分支，整个 Git 页闪一下。
    /// 把时间戳往回拨到新鲜窗口之外，旧数据留着继续显示，新数据一到无缝替换，中间
    /// 没有空档可闪。
    pub fn invalidate_git_status(&mut self, root: &str) {
        if let Some((t, _)) = self.git_status.get_mut(root) {
            *t = Instant::now() - std::time::Duration::from_secs(3600);
        }
    }

    /// Git 视图：查看某个改动文件的 diff。已跟踪文件用 `git diff HEAD`，
    /// 未跟踪文件（??）用 `git diff --no-index` 展示全文（整体当作新增）。
    /// 确保某 root 的 git status 缓存新鲜（>1.5s 或缺失就后台刷新；ensure_git_watch
    /// 建的监听命中时会主动标脏缓存，比 1.5s 轮询更快触发这里重新拉取）。
    /// 绝不阻塞 render：git status 在大仓要 ~90ms，同步跑就是掉帧元凶。
    pub fn ensure_git_status(&mut self, root: String, cx: &mut Context<Self>) {
        let fresh = self
            .git_status
            .get(&root)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_millis(1500));
        if fresh || self.git_status_inflight.contains(&root) {
            return;
        }
        self.git_status_inflight.insert(root.clone());
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let data = cx
                .background_executor()
                .spawn(async move {
                    // GIT_OPTIONAL_LOCKS=0（run_git 里固定带）：避免刷新索引 stat 缓存去抢
                    // .git/index.lock——之前吃过这个亏（见 smeltd/GUI 并发跑 git 命令时的
                    // index.lock 争用问题）；顺带也防止我们自己的 status 调用触发上面那个
                    // 文件监听自扰。
                    let out = run_git(&r, &["status", "--porcelain=v1", "-b"]);
                    let mut d = GitStatusData::default();
                    if let Ok(o) = out {
                        if o.status.success() {
                            d.ok = true;
                            let text = String::from_utf8_lossy(&o.stdout);
                            for line in text.lines() {
                                if let Some(b) = line.strip_prefix("## ") {
                                    let (branch, upstream, ahead, behind) = parse_branch_status_line(b);
                                    d.branch = branch;
                                    d.upstream = upstream;
                                    d.ahead = ahead;
                                    d.behind = behind;
                                } else if line.len() >= 3 {
                                    d.files.push((line[..2].to_string(), line[3..].to_string()));
                                }
                            }
                        }
                    }
                    d
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git_status_inflight.remove(&root);
                this.git_status.insert(root, (Instant::now(), data));
                cx.notify();
            });
        })
        .detach();
    }

    /// 确保某 root 的分支列表缓存新鲜（>1.5s 或缺失就后台刷新），Git 页头部分支切换
    /// 下拉用。`for-each-ref` 一次传两个 pattern 拿全 `refs/heads` + `refs/remotes`，
    /// 靠 refname 前缀区分本地/远程，不用起两次 git 进程。
    pub fn ensure_branches(&mut self, root: String, cx: &mut Context<Self>) {
        let fresh = self
            .branches
            .get(&root)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_millis(1500));
        if fresh || self.branches_inflight.contains(&root) {
            return;
        }
        self.branches_inflight.insert(root.clone());
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let list = cx
                .background_executor()
                .spawn(async move {
                    let out = run_git(
                        &r,
                        &["for-each-ref", "refs/heads", "refs/remotes", "--format=%(refname)"],
                    );
                    let mut list = BranchList::default();
                    if let Ok(o) = out {
                        if o.status.success() {
                            let text = String::from_utf8_lossy(&o.stdout);
                            for line in text.lines() {
                                if let Some(name) = line.strip_prefix("refs/heads/") {
                                    list.local.push(name.to_string());
                                } else if let Some(name) = line.strip_prefix("refs/remotes/") {
                                    // <remote>/HEAD 是指向默认分支的符号引用，不是真分支。
                                    if !name.ends_with("/HEAD") {
                                        list.remote.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                    list
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.branches_inflight.remove(&root);
                this.branches.insert(root, (Instant::now(), list));
                cx.notify();
            });
        })
        .detach();
    }

    /// Git 页分支切换下拉：checkout 目标分支（本地分支直接切；远程分支传短名，靠 git
    /// 内建 DWIM 自动建好跟踪分支——跟 create_worktree 判断分支存不存在同一个逻辑）。
    /// 成功后清掉 status/分支缓存强制下一帧重新拉（文件、ahead/behind 全变了）。
    pub fn checkout_branch(&mut self, root: String, branch: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let b = branch.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let out = run_git(&r, &["checkout", &b]).map_err(|e| e.to_string())?;
                    if out.status.success() {
                        Ok(())
                    } else {
                        Err(git_err(&out, "git checkout 失败"))
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    // 切分支后文件列表、ahead/behind 都变了，标脏逼下一帧重新拉取；
                    // 分支列表本身（有哪些分支）不受切换影响，不用跟着失效。
                    Ok(()) => this.invalidate_git_status(&root),
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Git 页文件列表勾选框：把某个改动文件加入暂存区（`git add --`，untracked/修改/
    /// 删除都适用）。纯本地索引改动、可逆，不像 commit/push 那样需要走"发到终端让人
    /// 确认"那一套，直接执行。成功后清 git_status 缓存强制下一帧重新拉状态。
    pub fn stage_file(&mut self, root: String, path: String, cx: &mut Context<Self>) {
        self.run_git_index_op(root, vec!["add", "--"], path, cx);
    }

    /// 文件列表取消勾选：把已暂存的改动移出暂存区（`git reset --`），不影响工作区
    /// 内容本身，随时能重新勾选加回去。
    pub fn unstage_file(&mut self, root: String, path: String, cx: &mut Context<Self>) {
        self.run_git_index_op(root, vec!["reset", "--"], path, cx);
    }

    /// stage_file/unstage_file 共用的后台执行 + 缓存失效逻辑：`git <args.. > -- <path>`。
    fn run_git_index_op(
        &mut self,
        root: String,
        args: Vec<&'static str>,
        path: String,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let p = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut full_args: Vec<&str> = args;
                    full_args.push(&p);
                    let out = run_git(&r, &full_args).map_err(|e| e.to_string())?;
                    if out.status.success() {
                        Ok(())
                    } else {
                        Err(git_err(&out, "git 操作失败"))
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.invalidate_git_status(&root),
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 跑 git + 着色放后台，用 file_gen 丢弃过期结果。
    pub fn open_diff(&mut self, root: String, path: String, untracked: bool, cx: &mut Context<Self>) {
        self.diff_gen = self.diff_gen.wrapping_add(1);
        let gen = self.diff_gen;
        self.git_diff = Some(GitDiff { path: path.clone(), lines: Rc::new(Vec::new()) });
        self.diff_selected.clear(); // 换文件/重开 diff：旧的行选区不再对应新内容
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (r, p) = (root.clone(), path.clone());
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let out = if untracked {
                        run_git(&r, &["diff", "--no-index", "--", "/dev/null", &p])
                    } else {
                        // --submodule=log：submodule 指针变化（mode 160000）默认只输出
                        // "Subproject commit <old sha>/<new sha>"，两行几乎全同的 hex
                        // 走字符级 diff 高亮，等于啥有用信息都没给。=log 换成子仓库里
                        // old..new 之间的实际 commit 列表，对普通文件这个参数完全不生效。
                        run_git(&r, &["diff", "HEAD", "--submodule=log", "--", &p])
                    };
                    // --no-index 有差异时退出码为 1，所以不看 status，只要拿到 stdout。
                    let text = match out {
                        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
                        Err(e) => format!("无法执行 git diff：{e}"),
                    };
                    parse_diff(&text)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.diff_gen == gen {
                    this.git_diff = Some(GitDiff { path, lines: Rc::new(lines) });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 点击 diff 行：切换该行（按 GitDiff.lines 下标）是否被选中待评论。
    fn toggle_diff_line(&mut self, i: usize, cx: &mut Context<Self>) {
        if !self.diff_selected.remove(&i) {
            self.diff_selected.insert(i);
        }
        cx.notify();
    }

    /// 把选中的 diff 行 + 评论输入框内容拼成一段文本，写进当前激活终端的 PTY
    /// （不带回车，留给用户自己看一眼再发送）。
    fn send_diff_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.diff_selected.is_empty() {
            return;
        }
        let Some(diff) = &self.git_diff else { return };
        let comment = self
            .diff_comment_input
            .as_ref()
            .map(|s| s.read(cx).value().trim().to_string())
            .unwrap_or_default();

        let mut selected: Vec<usize> = self.diff_selected.iter().copied().collect();
        selected.sort_unstable();
        let mut msg = format!("对 {} 的这几行有反馈：\n", diff.path);
        for i in selected {
            if let Some(l) = diff.lines.get(i) {
                let ln = l.new_ln.or(l.old_ln).map(|n| n.to_string()).unwrap_or_else(|| "?".into());
                let marker = match l.kind {
                    DiffKind::Add => "+",
                    DiffKind::Del => "-",
                    _ => " ",
                };
                msg.push_str(&format!("  L{ln} {marker} {}\n", l.text));
            }
        }
        if !comment.is_empty() {
            msg.push_str(&format!("\n{comment}\n"));
        }

        let target = self.cur().map(|s| s.active.clone());
        if let Some(view) = target {
            view.update(cx, |tv, cx| tv.send_text(&msg, cx));
        }
        self.diff_selected.clear();
        if let Some(state) = self.diff_comment_input.clone() {
            state.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Git 视图「AI 生成」：读当前项目的 diff（优先 `git diff --staged`，没有暂存改动
    /// 就退回 `git diff` 工作区改动），喂给 LLM 生成一条 Conventional Commits 风格的
    /// commit message，写回 commit_msg_input（只是填框，不自动发送/提交）。
    fn generate_commit_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.commit_msg_generating {
            return;
        }
        let Some(root) = self.cur().and_then(|s| s.cwd(cx)) else { return };
        let Some(cfg) = cx.try_global::<agent::LlmConfig>().cloned() else { return };
        if !agent::has_credentials(&cfg) {
            self.background_error =
                Some("还没配置 LLM API key（设置 → 宠物大脑），没法生成 commit message".to_string());
            cx.notify();
            return;
        }
        self.commit_msg_generating = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let r = root.clone();
            let diff = cx
                .background_executor()
                .spawn(async move { collect_commit_diff(&r) })
                .await;
            let Some(diff) = diff else {
                let _ = this.update(cx, |this, cx| {
                    this.commit_msg_generating = false;
                    this.background_error = Some("没有改动可生成 commit message".to_string());
                    cx.notify();
                });
                return;
            };
            let system = "你是一个 git commit message 生成器。根据用户给出的 git diff，\
                生成一条遵循 Conventional Commits 规范（feat/fix/docs/refactor/chore/test/style \
                等前缀）的提交信息，用简洁的中文描述改动内容和目的。只输出这一行 commit message \
                本身，不要加引号、不要加解释、不要换行。"
                .to_string();
            let result = cx
                .background_executor()
                .spawn(async move {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(anyhow::Error::from)?
                        .block_on(agent::complete_with_system(cfg, system, diff, 100))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.commit_msg_generating = false;
                match result {
                    Ok(msg) => {
                        if let Some(input) = this.commit_msg_input.clone() {
                            input.update(cx, |s, cx| s.set_value(msg, window, cx));
                        }
                    }
                    Err(err) => this.background_error = Some(format!("生成 commit message 失败：{err}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Git 页「提交」/「提交并推送」共用入口：直接执行（不再走"发到终端"那套）——
    /// 要提交的内容已经在暂存区里明明白白摆着（部分暂存那套勾选框），跟 stage/
    /// checkout 一样属于本地可控操作，不需要再让人去终端里确认一遍回车；真正没法
    /// 回头的风险点在 push 影响远程共享状态，但 WebStorm 等主流 git 客户端也是
    /// 「提交并推送」一键做的，这里跟随这个惯例。
    fn commit(&mut self, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.cur().and_then(|s| s.cwd(cx)) else { return };
        let Some(message) = self.commit_msg_input.as_ref().map(|s| s.read(cx).value().trim().to_string())
        else {
            return;
        };
        if message.is_empty() {
            return;
        }
        let branch = self.git_status.get(&root).map(|(_, d)| d.branch.clone()).unwrap_or_default();
        cx.spawn_in(window, async move |this, cx| {
            let r = root.clone();
            let msg = message.clone();
            let b = branch.clone();
            let result = cx
                .background_executor()
                .spawn(async move { commit_and_maybe_push(&r, &msg, push, &b) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.invalidate_git_status(&root);
                        if let Some(input) = this.commit_msg_input.clone() {
                            input.update(cx, |s, cx| s.set_value("", window, cx));
                        }
                    }
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
