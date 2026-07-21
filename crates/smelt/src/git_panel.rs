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
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_component::resizable::{h_resizable, resizable_panel, ResizableState};
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
pub(crate) enum DiffKind {
    Add,     // 增行（+）
    Del,     // 删行（-）
    Context, // 上下文行（空格）
    Hunk,    // @@ 段头
    Meta,    // diff/index/+++/--- 等元信息
}

/// 一行 diff：旧/新行号（None 表示该侧无此行）、类型、去掉 +/-/空格前缀的文本。
/// segments 为 Some 时表示做过行内 diff：每段 (文本, 是否变化)，变化段渲染时上深底。
#[derive(Clone)]
pub(crate) struct DiffLine {
    pub(crate) old_ln: Option<u32>,
    pub(crate) new_ln: Option<u32>,
    pub(crate) kind: DiffKind,
    text: String,
    segments: Option<Vec<(String, bool)>>,
}

/// diff 里的一个 `@@` 段——按块暂存 / 丢弃的最小单位。
///
/// `raw` 必须是 git 原样吐出来的文本，**不能**从 `DiffLine` 反向拼：`\ No newline at
/// end of file` 这类标记不进 DiffLine（它既不是增删也不是上下文），重建出来的 patch
/// 喂给 `git apply` 会被判成损坏。行号同理——原文的 `@@ -a,b +c,d @@` 直接沿用就
/// 一定对得上，自己算容易差一行（见 [[split-commits-patch-context]] 那次教训）。
struct DiffHunk {
    /// 在 [`GitDiff::lines`] 里的下标范围，起点是 `@@` 头那行。
    range: std::ops::Range<usize>,
    /// 本段原文（`@@` 头 + 各行，保留 `+`/`-`/空格前缀，每行以 \n 结尾）。
    raw: String,
}

/// Git 视图里当前选中查看的文件 diff：文件相对路径 + 结构化的 diff 行。
/// 用 Rc 供 uniform_list 闭包共享。
pub struct GitDiff {
    path: String,
    lines: Rc<Vec<DiffLine>>,
    /// 文件头原文（`diff --git` … `+++` 那几行），与单个 hunk 的 `raw` 拼起来
    /// 才是一份能喂给 `git apply` 的完整 patch。
    header: String,
    hunks: Rc<Vec<DiffHunk>>,
    /// 能否按块操作。两种 diff 拿不到合法 patch，只能整文件处理：
    /// - submodule（`--submodule=diff`）：里面是**子仓库**的文件路径，主仓库 apply 不了
    /// - 未跟踪文件（`--no-index`）：路径是 `/dev/null` 加工作区绝对路径，对不上索引
    patchable: bool,
    /// 这份 diff 是哪个视图拉的。
    scope: DiffScope,
    /// 这个文件有没有已暂存的改动。只在 All 视图下用来判断按块操作安不安全：
    /// 没暂存过，`diff HEAD` 就等价于 `diff`，按块操作照样对得上号（这是最常见的
    /// 情况，不该逼用户先去切视图）。
    has_staged: bool,
}

impl GitDiff {
    /// 当前视图下 hunk 该给哪些按钮。
    fn hunk_ops(&self) -> HunkOps {
        if !self.patchable {
            return HunkOps::None;
        }
        match self.scope {
            DiffScope::Unstaged => HunkOps::StageDiscard,
            DiffScope::Staged => HunkOps::Unstage,
            // 混合视图：只有当这个文件压根没暂存过，两层才是同一份差异。
            DiffScope::All if !self.has_staged => HunkOps::StageDiscard,
            DiffScope::All => HunkOps::None,
        }
    }
}

/// [`parse_diff`] 的产物：渲染要的行 + 按块操作要的 patch 素材。
pub(crate) struct ParsedDiff {
    pub(crate) lines: Vec<DiffLine>,
    header: String,
    hunks: Vec<DiffHunk>,
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
    /// 改动文件：(porcelain 两位状态码, 路径)。file_tree.rs 借它给改动文件标 M/A/D，
    /// 所以是 pub（main.rs 转手把这份列表传过去）。
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

impl GitStatusData {
    /// 当前分支名，给「日志」页标注 HEAD 用。
    pub fn branch_name(&self) -> &str {
        &self.branch
    }
}

impl BranchList {
    /// 本地分支名，供「日志」页的分支树用。
    pub fn local_names(&self) -> &[String] {
        &self.local
    }

    /// 远程分支名，同上。
    pub fn remote_names(&self) -> &[String] {
        &self.remote
    }
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

/// diff 看的是哪一层改动。决定跑哪条 git 命令，也决定按块操作给哪些按钮。
///
/// 分这三档是因为「暂存块」这个动作本质上是把**工作区相对索引**的差异写进索引，
/// 而 `git diff HEAD` 给的是工作区相对 HEAD——文件一旦部分暂存过，两者就不是一回
/// 事，拿后者的 hunk 去 apply --cached 会直接报 does not apply。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DiffScope {
    /// `git diff HEAD`：暂存 + 未暂存合起来看，默认。
    All,
    /// `git diff --staged`：只看已暂存的，能「取消暂存块」。
    Staged,
    /// `git diff`：只看未暂存的，能「暂存块」「丢弃块」。
    Unstaged,
}

impl DiffScope {
    fn args(self) -> &'static [&'static str] {
        match self {
            DiffScope::All => &["diff", "HEAD", "--submodule=diff", "--"],
            DiffScope::Staged => &["diff", "--staged", "--submodule=diff", "--"],
            DiffScope::Unstaged => &["diff", "--submodule=diff", "--"],
        }
    }

    fn label(self) -> &'static str {
        match self {
            DiffScope::All => "全部",
            DiffScope::Staged => "已暂存",
            DiffScope::Unstaged => "未暂存",
        }
    }
}

/// 当前视图下，hunk 上该出现哪些按钮。
#[derive(Clone, Copy, PartialEq)]
enum HunkOps {
    /// 未暂存的改动：可以暂存进索引，也可以直接丢弃。
    StageDiscard,
    /// 已暂存的改动：只能退回工作区（丢弃要去「未暂存」视图做，语义才清楚）。
    Unstage,
    /// 给不了按钮：不可 patch，或「全部」视图下这个文件确实混着两层改动。
    None,
}

/// 变更文件列表的一行（树形）。目录行只用来折叠，文件行才带状态码。
#[derive(Debug)]
pub struct GitTreeRow {
    /// 缩进层级。
    pub depth: usize,
    /// 显示名：文件是文件名，目录可能是压缩后的多段（如 `src/bin/workspace`）。
    pub name: String,
    /// 相对仓库根的完整路径。目录行拿它当折叠 key，文件行拿它开 diff。
    pub path: String,
    /// None = 目录行；Some = 文件行的 porcelain 两位状态码。
    pub status: Option<String>,
}

/// 构建变更文件的目录树。
///
/// 单链目录会被压缩成一行（`a` 里只有 `a/b`、`a/b` 里只有 `a/b/c` → 显示成
/// `a/b/c`）——git 的改动常常埋在很深的目录里，不压缩就是一串只有一个孩子的
/// 缩进，白占宽度。JetBrains 的 compact middle packages 就是这个行为。
pub fn build_git_tree(
    files: &[(String, String)],
    collapsed: &std::collections::HashSet<String>,
) -> Vec<GitTreeRow> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        dirs: BTreeMap<String, Node>,
        /// (文件名, 状态码)
        files: Vec<(String, String)>,
    }

    let mut root = Node::default();
    for (st, path) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        let Some(fname) = parts.pop() else { continue };
        let mut cur = &mut root;
        for p in parts {
            cur = cur.dirs.entry(p.to_string()).or_default();
        }
        cur.files.push((fname.to_string(), st.clone()));
    }

    /// 目录在前、文件在后地铺平；prefix 是当前节点的完整路径。
    fn walk(
        node: &Node,
        prefix: &str,
        depth: usize,
        collapsed: &std::collections::HashSet<String>,
        out: &mut Vec<GitTreeRow>,
    ) {
        for (name, child) in &node.dirs {
            // 压缩单链：只有一个子目录且没有直属文件时，把名字接起来继续往下钻。
            let mut label = name.clone();
            let mut path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let mut node = child;
            while node.files.is_empty() && node.dirs.len() == 1 {
                let (n, c) = node.dirs.iter().next().unwrap();
                label = format!("{label}/{n}");
                path = format!("{path}/{n}");
                node = c;
            }
            let is_collapsed = collapsed.contains(&path);
            out.push(GitTreeRow {
                depth,
                name: label,
                path: path.clone(),
                status: None,
            });
            if !is_collapsed {
                walk(node, &path, depth + 1, collapsed, out);
            }
        }
        for (fname, st) in &node.files {
            let path = if prefix.is_empty() {
                fname.clone()
            } else {
                format!("{prefix}/{fname}")
            };
            out.push(GitTreeRow {
                depth,
                name: fname.clone(),
                path,
                status: Some(st.clone()),
            });
        }
    }

    let mut out = Vec::new();
    walk(&root, "", 0, collapsed, &mut out);
    out
}

/// 并排视图的一行：Both = 左(旧侧)/右(新侧)各一行（None 为空侧占位）；
/// Full = 横跨整宽的 hunk/meta 行。存的是 GitDiff.lines 里的索引。
enum SplitRow {
    Both(Option<usize>, Option<usize>),
    Full(usize),
}

/// 文件查看的固定行高（供 diff 视图 uniform_list 虚拟滚动，需每行等高）。
const FILE_LINE_H: f32 = 20.0;

/// hunk 头行的 hover 分组名：按钮平时隐藏，鼠标进这一行才显形。
const HUNK_ROW_GROUP: &str = "hunk-row";

/// `@@ -49,7 +49,7 @@ pub struct Foo {` → `pub struct Foo {`。
///
/// 那串坐标是给 `git apply` 看的，人只关心「这块改动在哪个函数/结构体里」——后半
/// 截正是 git 附送的上下文。坐标仍留在 hunk 的 `raw` 里，拼 patch 用得着。
/// 没有上下文（文件开头那种）就返回空串，让这行只作视觉分隔和按钮容器。
fn hunk_context(line: &str) -> &str {
    line.splitn(3, "@@").nth(2).unwrap_or("").trim()
}

/// 行号列宽度：按这份 diff 里最大的行号算，别写死。
///
/// 统一视图有旧/新两列，写死 44px 就是白占 88px——大多数文件行号只有两三位，
/// 代码被硬生生推到右边去。等宽字体下一个数字约 7.5px，左右各留 4px 内边距。
pub(crate) fn gutter_width(lines: &[DiffLine]) -> f32 {
    let max = lines.iter().filter_map(|l| l.new_ln.max(l.old_ln)).max().unwrap_or(0);
    // 至少留两位，免得开头几行的窄 gutter 和后面宽的对不齐（宽度是整份 diff 统一的，
    // 这里只是给极短文件一个下限）。
    let digits = max.to_string().len().max(2);
    digits as f32 * 7.5 + 8.0
}

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

/// 跑一条从 stdin 读输入的 git 子命令（`git apply` 收 patch 用）。
///
/// 必须先写完 stdin 再 `wait_with_output`：stdin 句柄不 drop，git 读不到 EOF 会一直
/// 等，而我们又在等它退出，直接死锁。
fn run_git_stdin(root: &str, args: &[&str], input: &str) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut si = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("拿不到 git 的 stdin"))?;
        si.write_all(input.as_bytes())?;
    } // si 在这里 drop → git 收到 EOF
    child.wait_with_output()
}

/// 把单个 hunk 拼成一份完整 patch：文件头（`diff --git` … `+++`）+ 本段原文。
/// 两截都是 git 原样吐出来的，不做任何重排，`git apply` 才会认。
fn hunk_patch(header: &str, hunk: &DiffHunk) -> String {
    format!("{header}{}", hunk.raw)
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
    push_current(root, branch)
}

/// 推送当前分支。抽出来给「推送」按钮和「提交并推送」共用。
///
/// GIT_TERMINAL_PROMPT=0：没有凭据缓存时 git 默认会弹交互式用户名/密码输入，但这个
/// 子进程没有 TTY，会一直卡住而不是报错。禁掉交互提示后 git 会直接失败退出，
/// 报错信息进 stderr，能正常走错误提示，而不是无声挂起。
fn push_current(root: &str, branch: &str) -> Result<(), String> {
    let attempt = std::process::Command::new("git")
        .args(["-C", root, "push"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| e.to_string())?;
    if attempt.status.success() {
        return Ok(());
    }
    // 没有上游分支时 `git push` 会失败，退到 `push -u origin <branch>` 顺手建跟踪。
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
pub(crate) fn parse_diff(text: &str) -> ParsedDiff {
    let mk = |old_ln, new_ln, kind, text: &str| DiffLine {
        old_ln,
        new_ln,
        kind,
        text: text.to_string(),
        segments: None,
    };
    if text.trim().is_empty() {
        return ParsedDiff {
            lines: vec![mk(None, None, DiffKind::Meta, "（无差异）")],
            header: String::new(),
            hunks: Vec::new(),
        };
    }
    let mut old_ln = 0u32;
    let mut new_ln = 0u32;
    let mut out = Vec::new();
    // 按块操作的素材：第一个 @@ 之前的元信息行攒成 header，之后每段原文攒进 hunks。
    let mut header = String::new();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    for line in text.lines() {
        // 当前 hunk 未结束时，原样收本行（含前缀）——patch 的合法性全靠它。
        if let Some(h) = hunks.last_mut() {
            if h.range.end == usize::MAX {
                if line.starts_with("@@") || line.starts_with("diff ") {
                    // 段结束：补上真实终点，下面的分支再决定要不要开新段。
                    h.range.end = out.len();
                } else {
                    h.raw.push_str(line);
                    h.raw.push('\n');
                }
            }
        }
        if line.starts_with("@@") {
            let (o, n) = parse_hunk(line);
            old_ln = o;
            new_ln = n;
            hunks.push(DiffHunk {
                // end 先占位成 MAX 表示「still open」，收到下一个 @@ / diff 或
                // 遍历结束时再回填。
                range: out.len()..usize::MAX,
                // raw 必须是完整原文（含坐标），patch 才合法。
                raw: format!("{line}\n"),
            });
            // 渲染只留上下文那截，坐标不给人看。
            out.push(mk(None, None, DiffKind::Hunk, hunk_context(line)));
        } else if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
        {
            // 第一个 @@ 之前的元信息行就是文件头；之后再出现的（多文件 diff，如
            // submodule）不进 header——那种 diff 本来也标 patchable=false。
            if hunks.is_empty() {
                header.push_str(line);
                header.push('\n');
            }
            // `diff --git` / `index` / `---` / `+++` 是 patch 的机械头部：拼 patch
            // 时缺一不可，但对读代码的人零价值——文件名标题栏已经写着了，index 那
            // 串哈希更是纯噪音。所以只进 header，不进渲染行（IDEA 同样不显示）。
            // `new file` / `deleted file` / `rename` 留着，它们说明的是这次改动的
            // 性质，不是机械信息。
            let noise = line.starts_with("diff ")
                || line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line == "--- /dev/null"
                || line == "+++ /dev/null";
            if !noise {
                out.push(mk(None, None, DiffKind::Meta, line));
            }
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
    // 收尾：最后一段没有后继 @@ 来触发回填，这里补上。
    if let Some(h) = hunks.last_mut() {
        if h.range.end == usize::MAX {
            h.range.end = out.len();
        }
    }
    mark_inline(&mut out);
    ParsedDiff { lines: out, header, hunks }
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
pub(crate) fn diff_colors(kind: DiffKind) -> (Rgba, Option<Rgba>, Option<Rgba>, Rgba) {
    match kind {
        DiffKind::Add => (rgb(0xb5e08a), Some(rgb(0x16261a)), Some(rgb(0x4ba14b)), rgb(0x2f6b34)),
        DiffKind::Del => (rgb(0xf7a3ae), Some(rgb(0x2a1620)), Some(rgb(0xc75c6a)), rgb(0x7a2836)),
        DiffKind::Context => (rgb(0xc0caf5), None, None, rgb(0)),
        DiffKind::Hunk => (rgb(0x7dcfff), Some(rgb(0x16202e)), None, rgb(0)),
        DiffKind::Meta => (rgb(0x565f89), None, None, rgb(0)),
    }
}

/// 文本区（flex_1）：有 segments 就拆成多段（变化段上深底），否则整行一段。
pub(crate) fn diff_text_area(l: &DiffLine, fg: Rgba, hl: Rgba) -> Div {
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

/// 按块操作按钮渲染要的上下文：仓库根 + 该 diff 能不能拼出合法 patch。
/// 打包传递，省得每个渲染函数都多挂两个参数。
#[derive(Clone)]
struct HunkCtx {
    root: String,
    /// 当前视图下该给哪些按钮。
    ops: HunkOps,
    /// 行下标 → 该行是第几个 hunk 的头。只有 hunk 头那行才渲染按钮。
    starts: Rc<std::collections::HashMap<usize, usize>>,
    /// F7 当前停在第几块，给它的头行描边，不然跳完不知道落在哪。
    active: Option<usize>,
}

impl HunkCtx {
    /// 这行是不是某个 hunk 的头；是就返回块序号。不可 patch 的 diff 不给按钮，
    /// 但仍要返回序号——F7 导航和高亮对子模块/未跟踪文件一样有用。
    fn idx_at(&self, line: usize) -> Option<usize> {
        self.starts.get(&line).copied()
    }
}

/// hunk 头那行右侧的按钮组：暂存本块 / 丢弃本块。
///
/// 用 div 而不是 Button 组件：这些行住在 uniform_list 里，只有可见区间会被构造，
/// 行内嵌带状态的组件容易和虚拟滚动的复用打架，"查看完整文件 ↗" 也是同样的写法。
fn hunk_buttons(idx: usize, ctx: &HunkCtx, ws: &Entity<Workspace>) -> Div {
    let btn = |label: &'static str, id: &'static str, color: u32, hover: u32| {
        div()
            .id((id, idx))
            .px_2()
            .text_xs()
            .cursor_pointer()
            .text_color(rgb(color))
            .hover(|s| s.text_color(rgb(hover)))
            .child(label)
    };
    // 平时透明、鼠标移到这一行才显形（group 名见 render_diff_line）。IDEA 的按块
    // 操作也是藏在 gutter 里、hover 才明显——常驻的文字按钮太吵，每个 hunk 头顶
    // 着两颗按钮，一屏下来全是它们。
    let bar = div()
        .flex()
        .items_center()
        .gap_1()
        .opacity(0.0)
        .group_hover(HUNK_ROW_GROUP, |s| s.opacity(1.0));
    match ctx.ops {
        HunkOps::None => bar,
        HunkOps::StageDiscard => {
            let (ws_stage, root_stage) = (ws.clone(), ctx.root.clone());
            let (ws_discard, root_discard) = (ws.clone(), ctx.root.clone());
            bar.child(btn("暂存块", "hunk-stage", 0x7dcfff, 0xa9dcff).on_click(
                move |_ev, _w, cx| {
                    let root = root_stage.clone();
                    ws_stage.update(cx, |this, cx| this.stage_hunk(root, idx, cx));
                },
            ))
            .child(btn("丢弃块", "hunk-discard", 0x8b6b7a, 0xff7a93).on_click(
                move |_ev, _w, cx| {
                    let root = root_discard.clone();
                    ws_discard.update(cx, |this, cx| this.start_discard_hunk(root, idx, cx));
                },
            ))
        }
        HunkOps::Unstage => {
            let (ws_un, root_un) = (ws.clone(), ctx.root.clone());
            bar.child(btn("取消暂存块", "hunk-unstage", 0x7dcfff, 0xa9dcff).on_click(
                move |_ev, _w, cx| {
                    let root = root_un.clone();
                    ws_un.update(cx, |this, cx| this.unstage_hunk(root, idx, cx));
                },
            ))
        }
    }
}

/// 渲染一行 diff：左侧色条 + 旧/新行号槽 + 文本；整行按类型上淡背景。
/// 若有 segments（行内 diff 结果），变化片段再叠一层更深的底色。
/// 增/删行（i 为 GitDiff.lines 下标）可点选：选中态描边，点击切给 Workspace
/// 的 toggle_diff_line，配合底部评论框批量发给当前终端。
fn render_diff_line(
    i: usize,
    l: &DiffLine,
    selected: bool,
    ws: &Entity<Workspace>,
    hunks: &HunkCtx,
    gw: f32,
) -> Stateful<Div> {
    let (fg, bg, bar, hl) = diff_colors(l.kind);
    let gutter = |n: Option<u32>| {
        div()
            .w(px(gw))
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
    let hunk_idx = hunks.idx_at(i);
    // F7 停在这块就描一道边，跳完才看得出落点。
    if hunk_idx.is_some() && hunk_idx == hunks.active {
        row = row.border_1().border_color(rgb(0x7dcfff));
    }
    // hunk 头行才需要 hover 分组（按钮藏在里面）。
    if hunk_idx.is_some() {
        row = row.group(HUNK_ROW_GROUP);
    }
    let row = row
        // 左侧色条：增/删才有，其它用等宽透明占位保持对齐。
        .child(match bar {
            Some(c) => div().w(px(2.)).h_full().bg(c),
            None => div().w(px(2.)).h_full(),
        })
        .child(gutter(l.old_ln))
        .child(gutter(l.new_ln))
        .child(diff_text_area(l, fg, hl));
    match hunk_idx {
        // 不可 patch 的 diff（子模块 / 未跟踪）不给按钮，但上面的高亮照给。
        Some(idx) if hunks.ops != HunkOps::None => row.child(hunk_buttons(idx, hunks, ws)),
        _ => row,
    }
}

/// 只读的 diff 行渲染，给「日志」页看某次提交的改动用。
///
/// 与 [`render_diff_line`] 的区别是不带选行/评论那套交互——历史提交是既成事实，
/// 没有「选中几行发给 agent 去改」的语义。共用同一套配色和行内高亮，两处观感一致。
pub(crate) fn render_readonly_diff_line(l: &DiffLine, gw: f32) -> Div {
    let (fg, bg, bar, hl) = diff_colors(l.kind);
    let gutter = |n: Option<u32>| {
        div()
            .w(px(gw))
            .px_1()
            .flex()
            .justify_end()
            .text_color(rgb(0x4a5178))
            .child(n.map(|v| v.to_string()).unwrap_or_default())
    };
    let mut row = div().flex().items_center().h(px(FILE_LINE_H)).whitespace_nowrap();
    if let Some(b) = bg {
        row = row.bg(b);
    }
    row.child(match bar {
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
    gw: f32,
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
            .w(px(gw))
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
    hunks: &HunkCtx,
    gw: f32,
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
            // hunk 头在并排视图里也是整行，同样挂按钮；文本占满剩余宽度把按钮推到右边。
            let hunk_idx = hunks.idx_at(*i);
            if hunk_idx.is_some() && hunk_idx == hunks.active {
                d = d.border_1().border_color(rgb(0x7dcfff));
            }
            if hunk_idx.is_some() {
                d = d.group(HUNK_ROW_GROUP);
            }
            let d = d.child(div().flex_1().px_2().text_color(fg).child(l.text.clone()));
            match hunk_idx {
                Some(idx) if hunks.ops != HunkOps::None => d.child(hunk_buttons(idx, hunks, ws)),
                _ => d,
            }
        }
        SplitRow::Both(l, r) => div()
            .flex()
            .items_center()
            .h(px(FILE_LINE_H))
            // w_full 关键：容器占满整宽，两个 flex_1 半区才会真正各占一半；
            // 否则容器 hug content，grow 失效，空侧塌成 0 宽、内容顶到最左。
            .w_full()
            .whitespace_nowrap()
            .child(render_half(ri, *l, true, lines, selected, ws, gw))
            .child(div().w(px(1.)).h_full().bg(rgb(0x2a2e3d))) // 中缝分隔
            .child(render_half(ri, *r, false, lines, selected, ws, gw)),
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
    active_hunk: Option<usize>,
    scope: DiffScope,
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
            // 行号列宽按整份 diff 的最大行号算一次，所有行共用同一个值才对得齐。
            let gutter_w = gutter_width(&lines);
            let hunk_ctx = HunkCtx {
                root: root.to_string(),
                ops: d.hunk_ops(),
                starts: Rc::new(
                    d.hunks.iter().enumerate().map(|(n, h)| (h.range.start, n)).collect(),
                ),
                active: active_hunk,
            };
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
                let hc = hunk_ctx.clone();
                uniform_list("git-diff-split", count, move |range, _w, _cx| {
                    range
                        .map(|i| render_split_row(i, &rows[i], &lines2, &sel2, &ws2, &hc, gutter_w))
                        .collect::<Vec<_>>()
                })
            } else {
                let count = lines.len();
                let sel2 = diff_selected.clone();
                let ws2 = ws.clone();
                let hc = hunk_ctx.clone();
                uniform_list("git-diff", count, move |range, _w, _cx| {
                    range
                        .map(|i| render_diff_line(i, &lines[i], sel2.contains(&i), &ws2, &hc, gutter_w))
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

            // 「全部 / 已暂存 / 未暂存」三档：决定 diff 拉哪一层，也决定 hunk 上
            // 给什么按钮。分开看才谈得上「取消暂存这一块」——混着看时索引和工作区
            // 的差异叠在一起，按块操作对不上号。
            let scope_switch = h_flex().gap_1().children(
                [DiffScope::All, DiffScope::Staged, DiffScope::Unstaged].into_iter().map(|s| {
                    let on = s == scope;
                    div()
                        .id(match s {
                            DiffScope::All => "diff-scope-all",
                            DiffScope::Staged => "diff-scope-staged",
                            DiffScope::Unstaged => "diff-scope-unstaged",
                        })
                        .px_2()
                        .py(px(1.0))
                        .text_xs()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(if on { fg } else { muted })
                        .when(on, |d| d.bg(accent))
                        .hover(|d| d.opacity(0.8))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_diff_scope(s, cx)))
                        .child(s.label())
                }),
            );

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
                        .child(scope_switch)
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
    ahead: u32,
    pushing: bool,
    cx: &mut Context<Workspace>,
) -> Div {
    let (border, muted, fg) = {
        let t = cx.theme();
        (t.border, t.muted_foreground, t.foreground)
    };
    let has_text = input.is_some_and(|s| !s.read(cx).value().trim().is_empty());
    let ws_gen = cx.entity();
    let ws_commit = ws_gen.clone();
    let ws_commit_push = ws_gen.clone();
    let ws_push = ws_gen.clone();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(border)
        // 「AI 生成」是写 message 的辅助，不是 git 操作——跟提交/推送挤一排只会让
        // 底下看着像一堆按钮。挪到输入框上沿，做成轻量文字按钮（同「查看完整文件」）。
        .child(
            h_flex().justify_end().child(
                div()
                    .id("commit-msg-generate")
                    .px_1()
                    .text_xs()
                    .cursor_pointer()
                    .text_color(muted)
                    .hover(|s| s.text_color(fg))
                    .child(if generating { "生成中…" } else { "✨ AI 生成" })
                    .on_click(move |_ev, window, cx| {
                        ws_gen.update(cx, |this, cx| this.generate_commit_message(window, cx));
                    }),
            ),
        )
        .children(input.map(|state| Input::new(state).small()))
        .child(
            h_flex()
                .gap_2()
                .pt_1()
                // 左侧提示待推送数量：ahead 只在分支头显示过（↑3），到了按钮这边
                // 再说一次，人才知道「推送」按钮为什么亮着。
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(muted)
                        .child(if ahead > 0 {
                            format!("{ahead} 个提交待推送")
                        } else {
                            String::new()
                        }),
                )
                // 单独的「推送」：本地攒了提交但这会儿没有新改动要提交时，
                // 「提交并推送」是灰的，没有它就完全没法推。
                .child(
                    Button::new("git-push-only")
                        .small()
                        .label(if pushing { "推送中…" } else { "推送" })
                        .disabled(pushing || ahead == 0)
                        .on_click(move |_ev, _window, cx| {
                            ws_push.update(cx, |this, cx| this.push_only(cx));
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
                            ws_commit_push.update(cx, |this, cx| this.commit(true, window, cx));
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
    pushing: bool,
    files_scroll: &ScrollHandle,
    diff_scroll: &UniformListScrollHandle,
    active_hunk: Option<usize>,
    git_left_resize: &Entity<ResizableState>,
    git_left_w: f32,
    tree_collapsed: &HashSet<String>,
    scope: DiffScope,
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
            .children(build_git_tree(&files, tree_collapsed).into_iter().enumerate().map(|(i, row)| {
                // 每层缩进 14px，与文件树页同一套视觉节奏。
                let indent = px(4.0 + row.depth as f32 * 14.0);
                let path = row.path;
                // 目录行：只负责折叠，没有状态码也没有勾选框。
                let Some(st) = row.status else {
                    let collapsed = tree_collapsed.contains(&path);
                    let p_toggle = path.clone();
                    return div()
                        .id(("git-dir", i))
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl(indent)
                        .pr_2()
                        .py(px(1.0))
                        .text_sm()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|d| d.bg(accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.git_tree_collapsed.remove(&p_toggle) {
                                this.git_tree_collapsed.insert(p_toggle.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(if collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size_4()
                            .text_color(muted),
                        )
                        .child(div().min_w_0().text_color(muted).child(row.name))
                        // 目录行和文件行（挂了 context_menu，类型不同）要统一成
                        // AnyElement 才能进同一个 children 迭代器。
                        .into_any_element();
                };
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
                let name = row.name;
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
                    .pl(indent)
                    .pr_2()
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
                    // 只显示文件名——路径已经由树的层级表达了。整条路径挂 tooltip，
                    // 免得同名文件（一堆 mod.rs）分不清。tooltip 需要元素带 id。
                    .child(
                        div()
                            .id(("git-name", i))
                            .min_w_0()
                            .truncate()
                            .text_color(fg)
                            .child(name)
                            .tooltip({
                                let full = path.clone();
                                move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(full.clone())
                                        .build(window, cx)
                                }
                            }),
                    );
                // 右键菜单：丢弃改动 / 复制路径 / 在 Finder 中显示。此前 git 页完全
                // 没有右键入口，「丢弃这个文件的改动」这种最常用的操作根本没地方点。
                let ws_menu = cx.entity();
                let full_path = Path::new(&root).join(&path).to_string_lossy().to_string();
                let (root_menu, path_menu) = (root.clone(), path.clone());
                // 选中高亮要在挂菜单之前上色：context_menu 会把元素包一层，之后
                // 就不能再改样式了。
                let row = if is_sel { row.bg(accent) } else { row };
                row.context_menu(move |menu, _window, _cx| {
                    let (ws_d, r_d, p_d) = (ws_menu.clone(), root_menu.clone(), path_menu.clone());
                    let (ws_c, p_c) = (ws_menu.clone(), full_path.clone());
                    let (ws_f, p_f) = (ws_menu.clone(), full_path.clone());
                    menu.item(
                        PopupMenuItem::new(if untracked { "删除文件" } else { "丢弃改动" }).on_click(
                            move |_ev, _window, cx| {
                                ws_d.update(cx, |ws, cx| {
                                    ws.start_discard_file(r_d.clone(), p_d.clone(), untracked, cx)
                                });
                            },
                        ),
                    )
                    .separator()
                    .item(PopupMenuItem::new("复制文件路径").on_click(move |_ev, _window, cx| {
                        ws_c.update(cx, |ws, cx| ws.copy_file_path_to_clipboard(p_c.clone(), cx));
                    }))
                    .item(PopupMenuItem::new("在 Finder 中显示").on_click(move |_ev, _window, cx| {
                        ws_f.update(cx, |ws, cx| ws.reveal_path_in_finder(p_f.clone(), cx));
                    }))
                })
                .into_any_element()
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
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(border)
        .child(branch_header)
        .child(file_list)
        .child(commit_message_bar(commit_msg_input, commit_msg_generating, data.ahead, pushing, cx));

    // 拖拽不生效时的诊断口子：`SMELT_DEBUG_RESIZE=1 /Applications/Smelt.app/Contents/MacOS/smelt`
    // 从终端起，每帧打一行当前 panel 尺寸。尺寸不随拖动变化 = 事件没进来；变化了
    // 但画面不动 = 布局把它盖掉了。两种病因完全不同，别靠肉眼猜。
    if std::env::var_os("SMELT_DEBUG_RESIZE").is_some() {
        eprintln!("[resize] git-left sizes={:?}", git_left_resize.read(cx).sizes());
    }

    // 左栏宽度可拖拽（同文件树那套，拖完落盘）。以前写死 300px，路径一长就只能
    // 看见结尾几个字符。
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .child(
            h_resizable("git-left-split")
                .with_state(git_left_resize)
                .child(
                    resizable_panel()
                        .size(px(git_left_w))
                        .size_range(px(200.)..px(560.))
                        .child(left),
                )
                // 包一层 size_full 再放内容：diff 面板根节点带 flex_1，而 flex_1
                // 展开含 `flex-basis: 0%`，直接当 panel 的 child 会盖掉 panel 由
                // ResizableState 管理的 flex_basis——组件文档专门把 flex_basis 列为
                // 「调用方不许碰」的保留样式。包一层就把 flex_1 挡在里面了。
                // 这层必须是 .flex()：GPUI 的 div 默认 display: Block，flex_1 的
                // 子节点在 Block 里高度塌成 auto（列表 0 高，整个 diff 不可见）。
                .child(resizable_panel().child(div().size_full().flex().child(git_diff_pane(
                    &root,
                    git_diff,
                    split,
                    diff_selected,
                    diff_comment_input,
                    diff_scroll,
                    active_hunk,
                    scope,
                    cx,
                )))),
        )
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

    /// 文件右键「丢弃改动」：先弹确认。untracked 标记决定是删盘还是 restore。
    pub fn start_discard_file(
        &mut self,
        root: String,
        path: String,
        untracked: bool,
        cx: &mut Context<Self>,
    ) {
        self.discard_file_target = Some((root, path, untracked));
        cx.notify();
    }

    /// 取消丢弃文件。
    pub fn cancel_discard_file(&mut self, cx: &mut Context<Self>) {
        self.discard_file_target = None;
        cx.notify();
    }

    /// 确认丢弃整个文件的改动。
    ///
    /// 已跟踪走 `git restore --staged --worktree`（暂存区和工作区一起还原，省得
    /// 用户为「已 add 过」的文件再点一次）；未跟踪的 git 管不着，直接删盘。
    pub fn confirm_discard_file(&mut self, cx: &mut Context<Self>) {
        let Some((root, path, untracked)) = self.discard_file_target.take() else { return };
        cx.spawn(async move |this, cx| {
            let (r, p) = (root.clone(), path.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    if untracked {
                        let full = Path::new(&r).join(&p);
                        return std::fs::remove_file(&full)
                            .map_err(|e| format!("删除 {p} 失败：{e}"));
                    }
                    let out = run_git(&r, &["restore", "--staged", "--worktree", "--", &p])
                        .map_err(|e| e.to_string())?;
                    if out.status.success() {
                        Ok(())
                    } else {
                        Err(git_err(&out, "git restore 失败"))
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.invalidate_git_status(&root);
                        // 丢弃的正是当前打开的那个文件时，diff 已经没意义了，关掉。
                        if this.git_diff.as_ref().is_some_and(|d| d.path == path) {
                            this.git_diff = None;
                            this.active_hunk = None;
                        }
                    }
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「丢弃文件改动」确认弹窗。未跟踪文件是直接删盘，措辞要比 restore 更重。
    pub fn render_discard_file_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(true);
        let Some((_, path, untracked)) = self.discard_file_target.as_ref() else { return div() };
        let untracked = *untracked;
        let (title, body) = if untracked {
            ("确定删除这个新文件吗？", format!("{path} 从未被 git 跟踪过，删了就是彻底删除。"))
        } else {
            ("确定丢弃这个文件的改动吗？", format!("{path} 会被还原成 HEAD 的样子，已暂存的部分一并还原。"))
        };

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child(title))
            .child(div().text_sm().text_color(muted).child(body))
            .child(div().text_sm().text_color(accent_text).child("不进 reflog，找不回来。"))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-discard-file",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.cancel_discard_file(cx),
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-discard-file",
                        if untracked { "删除文件" } else { "丢弃改动" },
                        tint,
                        hover,
                        accent_text,
                        |this, _, _, cx| this.confirm_discard_file(cx),
                        cx,
                    )),
            );
        Self::modal_shell(380., true, content, cx)
    }

    /// F7 / Shift+F7：跳到下一个 / 上一个改动块并滚到视野里。
    ///
    /// 首次按跳第一块；到头就停在两端，不回绕——回绕会让人以为还有更多改动。
    pub fn jump_hunk(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(d) = self.git_diff.as_ref() else { return };
        let n = d.hunks.len();
        if n == 0 {
            return;
        }
        let next = match self.active_hunk {
            None => 0,
            Some(i) if forward => (i + 1).min(n - 1),
            Some(i) => i.saturating_sub(1),
        };
        let line = d.hunks[next].range.start;
        // 并排视图里 uniform_list 的 item 是 SplitRow，下标和 lines 的下标不是一回事，
        // 直接拿行号去滚会滚到别的位置——先翻译成 row 下标。
        let item = if self.diff_split {
            build_split_rows(&d.lines)
                .iter()
                .position(|r| matches!(r, SplitRow::Full(i) if *i == line))
                .unwrap_or(line)
        } else {
            line
        };
        self.active_hunk = Some(next);
        self.diff_scroll.scroll_to_item(item, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// 点「丢弃块」：先弹确认，不直接动文件。
    pub fn start_discard_hunk(&mut self, root: String, idx: usize, cx: &mut Context<Self>) {
        self.discard_hunk_target = Some((root, idx));
        cx.notify();
    }

    /// 确认丢弃：关弹窗并真正执行 reverse apply。
    pub fn confirm_discard_hunk(&mut self, cx: &mut Context<Self>) {
        let Some((root, idx)) = self.discard_hunk_target.take() else { return };
        self.discard_hunk(root, idx, cx);
    }

    /// 取消丢弃：什么都不发生。
    pub fn cancel_discard_hunk(&mut self, cx: &mut Context<Self>) {
        self.discard_hunk_target = None;
        cx.notify();
    }

    /// 「丢弃这一块」确认弹窗。用危险配色，文案点明不可恢复——这个操作直接覆写
    /// 工作区文件，既不进索引也不进 reflog，点完就真没了。
    pub fn render_discard_hunk_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(true);
        let Some((_, idx)) = self.discard_hunk_target.as_ref() else { return div() };
        let file = self.git_diff.as_ref().map(|d| d.path.clone()).unwrap_or_default();

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child("确定丢弃这一块改动吗？"))
            .child(div().text_sm().text_color(muted).child(format!(
                "{file} 的第 {} 块改动会被还原成改动前的样子。",
                idx + 1
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(accent_text)
                    .child("直接改工作区文件，不进暂存区也不进 reflog——丢了就找不回来。"),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-discard-hunk",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.cancel_discard_hunk(cx),
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-discard-hunk",
                        "丢弃这一块",
                        tint,
                        hover,
                        accent_text,
                        |this, _, _, cx| this.confirm_discard_hunk(cx),
                        cx,
                    )),
            );
        Self::modal_shell(380., true, content, cx)
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

    /// 右键「删除分支」：先弹确认。`remote` 为真时删的是远端分支（更危险，别人也
    /// 会受影响），文案要分开写。
    pub fn start_delete_branch(
        &mut self,
        root: String,
        branch: String,
        remote: bool,
        cx: &mut Context<Self>,
    ) {
        self.delete_branch_target = Some((root, branch, remote));
        cx.notify();
    }

    /// 取消删除分支。
    pub fn cancel_delete_branch(&mut self, cx: &mut Context<Self>) {
        self.delete_branch_target = None;
        cx.notify();
    }

    /// 确认删除分支。
    ///
    /// 本地分支先试 `-d`（安全删，未合并会被 git 拒绝），被拒了不自作主张改 `-D`，
    /// 而是把 git 的原话报出来让人自己决定——分支删了没有 reflog 之外的退路。
    /// 远端分支走 `push origin --delete`。
    pub fn confirm_delete_branch(&mut self, cx: &mut Context<Self>) {
        let Some((root, branch, remote)) = self.delete_branch_target.take() else { return };
        cx.spawn(async move |this, cx| {
            let (r, b) = (root.clone(), branch.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    let out = if remote {
                        // origin/feat/x → remote=origin, ref=feat/x
                        let (remote_name, ref_name) = b.split_once('/').unwrap_or(("origin", &b));
                        std::process::Command::new("git")
                            .args(["-C", &r, "push", remote_name, "--delete", ref_name])
                            .env("GIT_OPTIONAL_LOCKS", "0")
                            .env("GIT_TERMINAL_PROMPT", "0")
                            .output()
                            .map_err(|e| e.to_string())?
                    } else {
                        run_git(&r, &["branch", "-d", &b]).map_err(|e| e.to_string())?
                    };
                    if out.status.success() {
                        Ok(())
                    } else {
                        Err(git_err(&out, "删除分支失败"))
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.invalidate_git_status(&root);
                        // 分支没了，列表和日志都得重拉。
                        this.branches.remove(&root);
                        this.reload_git_log(root.clone(), cx);
                    }
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「删除分支」确认弹窗。
    pub fn render_delete_branch_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(true);
        let Some((_, branch, remote)) = self.delete_branch_target.as_ref() else { return div() };
        let remote = *remote;

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child(if remote {
                "确定删除这个远端分支吗？"
            } else {
                "确定删除这个分支吗？"
            }))
            .child(div().text_sm().text_color(muted).child(if remote {
                format!("{branch} 会从远端仓库删除，其他人拉取后本地也会跟着消失。")
            } else {
                format!("{branch} 只删本地引用，工作区文件不受影响。")
            }))
            .child(div().text_sm().text_color(accent_text).child(if remote {
                "影响所有协作者，删之前确认没人在用。"
            } else {
                "有未合并的提交时 git 会拒绝删除，不会强删。"
            }))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-delete-branch",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.cancel_delete_branch(cx),
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-delete-branch",
                        "删除",
                        tint,
                        hover,
                        accent_text,
                        |this, _, _, cx| this.confirm_delete_branch(cx),
                        cx,
                    )),
            );
        Self::modal_shell(380., true, content, cx)
    }

    /// 把某个分支合并进当前分支（`git merge <branch>`）。
    ///
    /// 冲突时 git 会以非 0 退出并把冲突文件写进工作区——此时**不自动 abort**，
    /// 保留现场让人去解，只把提示报出来（自动回滚会让人措手不及）。
    pub fn merge_branch(&mut self, root: String, branch: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let (r, b) = (root.clone(), branch.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    let out = run_git(&r, &["merge", "--no-ff", &b]).map_err(|e| e.to_string())?;
                    if out.status.success() {
                        Ok(())
                    } else {
                        let raw = git_err(&out, "git merge 失败");
                        Err(format!(
                            "{raw}\n（冲突文件已留在工作区，解决后 git add 再 git commit；\
                             想放弃这次合并用 git merge --abort）"
                        ))
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.invalidate_git_status(&root);
                        this.reload_git_log(root.clone(), cx);
                    }
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

    /// 切换 diff 视图（全部 / 已暂存 / 未暂存），并按新视图重拉当前文件的 diff。
    pub fn set_diff_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.diff_scope == scope {
            return;
        }
        self.diff_scope = scope;
        // 当前开着某个文件就按新视图重拉；没开就只记住选择。
        if let Some((root, path)) = self
            .git_diff
            .as_ref()
            .map(|d| d.path.clone())
            .and_then(|p| self.cur().and_then(|s| s.cwd(cx)).map(|r| (r, p)))
        {
            self.open_diff(root, path, false, cx);
        } else {
            cx.notify();
        }
    }

    /// 把第 `idx` 个 hunk 单独加入暂存区（`git apply --cached`）。
    ///
    /// 只暂存一块、其余留在工作区，是 agent 写的代码「对一半」时最需要的动作：挑出
    /// 对的先存下来，剩下的继续让它改。
    pub fn stage_hunk(&mut self, root: String, idx: usize, cx: &mut Context<Self>) {
        self.apply_hunk(root, idx, &["apply", "--cached", "-"], cx);
    }

    /// 把第 `idx` 个 hunk 撤出暂存区（`git apply --cached --reverse`）。
    ///
    /// 只在「已暂存」视图下给：那时 diff 就是索引相对 HEAD 的差异，reverse 回去
    /// 正好把这一块退回工作区，文件内容不受影响。
    pub fn unstage_hunk(&mut self, root: String, idx: usize, cx: &mut Context<Self>) {
        self.apply_hunk(root, idx, &["apply", "--cached", "--reverse", "-"], cx);
    }

    /// 丢弃第 `idx` 个 hunk（`git apply --reverse`，作用于工作区文件）。
    ///
    /// **会真的改用户的文件且不进 reflog**，调用方必须先让用户确认过。
    pub fn discard_hunk(&mut self, root: String, idx: usize, cx: &mut Context<Self>) {
        self.apply_hunk(root, idx, &["apply", "--reverse", "-"], cx);
    }

    /// stage_hunk / discard_hunk 共用：拼 patch → 后台 `git apply` → 刷新状态并重开 diff。
    ///
    /// 成功后必须重新拉一次 diff：apply 之后剩余 hunk 的行号和分段都变了，接着用旧的
    /// 下标去点第二块，改的就是别的地方（`-U0` 那次错位是同一类问题）。
    fn apply_hunk(
        &mut self,
        root: String,
        idx: usize,
        args: &'static [&'static str],
        cx: &mut Context<Self>,
    ) {
        let Some(d) = self.git_diff.as_ref() else { return };
        if !d.patchable {
            self.background_error =
                Some("这个 diff 不支持按块操作（子模块 / 未跟踪文件），请用整文件的勾选框".into());
            cx.notify();
            return;
        }
        let Some(hunk) = d.hunks.get(idx) else { return };
        let patch = hunk_patch(&d.header, hunk);
        let path = d.path.clone();
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let out = run_git_stdin(&r, args, &patch).map_err(|e| e.to_string())?;
                    if out.status.success() {
                        return Ok(());
                    }
                    // apply 失败最常见的原因是这个文件已经部分暂存过：当前 diff 是
                    // `git diff HEAD`（暂存+未暂存合起来），其中已进索引的那部分再
                    // apply --cached 就会 "already exists"/"does not apply"。把原因
                    // 说清楚，别只甩 git 的英文原文。
                    let raw = git_err(&out, "git apply 失败");
                    Err(format!(
                        "按块操作失败：{raw}\n\
                         （这个文件若已部分暂存，当前视图混着暂存与未暂存的改动，\
                         按块操作会对不上号；先用勾选框整文件取消暂存再试）"
                    ))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.invalidate_git_status(&root);
                        // 行号已变，重新解析一份，避免下一次点击打在错的位置上。
                        this.open_diff(root.clone(), path, false, cx);
                    }
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 跑 git + 着色放后台，用 file_gen 丢弃过期结果。
    pub fn open_diff(&mut self, root: String, path: String, untracked: bool, cx: &mut Context<Self>) {
        let scope = self.diff_scope;
        self.diff_gen = self.diff_gen.wrapping_add(1);
        let r#gen = self.diff_gen;
        self.git_diff = Some(GitDiff {
            path: path.clone(),
            lines: Rc::new(Vec::new()),
            header: String::new(),
            hunks: Rc::new(Vec::new()),
            patchable: false,
            scope,
            has_staged: false,
        });
        self.diff_selected.clear(); // 换文件/重开 diff：旧的行选区不再对应新内容
        self.active_hunk = None; // 块下标同理，换了文件就不指向原来那块了
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (r, p) = (root.clone(), path.clone());
            let parsed = cx
                .background_executor()
                .spawn(async move {
                    // 该文件有没有已暂存的改动——All 视图靠它判断按块操作安不安全。
                    let has_staged = !untracked
                        && run_git(&r, &["diff", "--staged", "--quiet", "--", &p])
                            .map(|o| !o.status.success()) // --quiet：有差异时退出码非 0
                            .unwrap_or(false);
                    let out = if untracked {
                        run_git(&r, &["diff", "--no-index", "--", "/dev/null", &p])
                    } else {
                        // --submodule=diff：submodule（mode 160000）默认只输出
                        // "Subproject commit <old sha>/<new sha>"，两行几乎全同的 hex
                        // 走字符级 diff 高亮，等于啥有用信息都没给。=diff 换成子仓库
                        // 内部的真实文件级 diff，对普通文件这个参数完全不生效。
                        //
                        // 别退回 =log：那个只列 old..new 之间的 commit 标题，而子模块
                        // **内有未提交改动**时（agent 改代码最常见的形态）它只吐一句
                        // "contains modified content"，改了什么完全看不见。
                        let mut args: Vec<&str> = scope.args().to_vec();
                        args.push(&p);
                        run_git(&r, &args)
                    };
                    // --no-index 有差异时退出码为 1，所以不看 status，只要拿到 stdout。
                    let text = match out {
                        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
                        Err(e) => format!("无法执行 git diff：{e}"),
                    };
                    // submodule 的 diff 里是子仓库的文件路径，主仓库 apply 不了；
                    // 未跟踪走 --no-index，路径同样对不上索引。两者都退回整文件操作。
                    let is_submodule = text.lines().any(|l| l.starts_with("Submodule "));
                    (parse_diff(&text), !untracked && !is_submodule, has_staged)
                })
                .await;
            let (parsed, patchable, has_staged) = parsed;
            let _ = this.update(cx, |this, cx| {
                if this.diff_gen == r#gen {
                    // 没解析出文件头就拼不出合法 patch（比如 diff 为空、或 git 报错
                    // 的文案），这时也不能让按块按钮亮着。
                    let patchable = patchable && !parsed.header.is_empty();
                    this.git_diff = Some(GitDiff {
                        path,
                        lines: Rc::new(parsed.lines),
                        header: parsed.header,
                        hunks: Rc::new(parsed.hunks),
                        patchable,
                        scope,
                        has_staged,
                    });
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

        let target = self.cur().and_then(|s| s.active_term().cloned());
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
    /// 只推送，不提交。本地攒了几个 commit 想推上去时用——以前只有「提交并推送」，
    /// 而它要求先写 commit message，于是「没有新改动、只想把已有提交推上去」这条
    /// 最常见的路径反而没有入口。
    pub fn push_only(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.cur().and_then(|s| s.cwd(cx)) else { return };
        let branch = self.git_status.get(&root).map(|(_, d)| d.branch.clone()).unwrap_or_default();
        self.pushing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (r, b) = (root.clone(), branch.clone());
            let result = cx
                .background_executor()
                .spawn(async move { push_current(&r, &b) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.pushing = false;
                match result {
                    Ok(()) => this.invalidate_git_status(&root),
                    Err(err) => this.background_error = Some(err),
                }
                cx.notify();
            });
        })
        .detach();
    }

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

#[cfg(test)]
mod tests {
    // 不用 `use super::*;`：本文件顶部有 gpui/gpui_component 的 glob 导入，带进测试
    // 模块会让 trait 解析图爆炸，`cargo test` 编译期能把 rustc 撑崩（hotspot.rs 的
    // 测试模块踩过，那边留了同样的注释）。只导入真正用到的名字。
    use super::{build_git_tree, hunk_patch, parse_diff, run_git, run_git_stdin, DiffKind};

    fn files(paths: &[&str]) -> Vec<(String, String)> {
        paths.iter().map(|p| (" M".to_string(), p.to_string())).collect()
    }

    /// 只有一个孩子的目录链要压成一行，否则深路径全是空缩进。
    #[test]
    fn tree_compacts_single_child_dir_chains() {
        let rows = build_git_tree(&files(&["crates/smelt/src/main.rs"]), &Default::default());
        assert_eq!(rows.len(), 2, "应是一行目录 + 一行文件，实际 {rows:?}");
        assert_eq!(rows[0].name, "crates/smelt/src");
        assert_eq!(rows[0].path, "crates/smelt/src");
        assert!(rows[0].status.is_none(), "目录行不该带状态码");
        assert_eq!(rows[1].name, "main.rs");
        assert_eq!(rows[1].path, "crates/smelt/src/main.rs");
        assert_eq!(rows[1].depth, 1);
    }

    /// 分叉处必须停止压缩，各分支自己成行。
    #[test]
    fn tree_stops_compacting_at_a_fork() {
        let rows = build_git_tree(&files(&["a/b/x.rs", "a/c/y.rs"]), &Default::default());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "x.rs", "c", "y.rs"], "实际 {names:?}");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].depth, 2);
    }

    /// 目录排在文件前面，同层内各自有序。
    #[test]
    fn tree_lists_dirs_before_files() {
        let rows = build_git_tree(&files(&["zz.txt", "aa/b.rs"]), &Default::default());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["aa", "b.rs", "zz.txt"], "目录应排在文件前，实际 {names:?}");
    }

    /// 折叠的目录不展开其子树，但目录行自己还在。
    #[test]
    fn tree_hides_children_of_collapsed_dir() {
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("a".to_string());
        let rows = build_git_tree(&files(&["a/b/x.rs", "a/c/y.rs", "top.rs"]), &collapsed);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "top.rs"], "折叠后不该露出子树，实际 {names:?}");
    }

    /// 在临时目录里造一个仓库：写 `content`、提交，再覆写成 `modified`（不提交）。
    /// 返回仓库根路径。用 pid + 标签避免并行测试互相踩。
    fn repo_with_change(tag: &str, content: &str, modified: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("smelt-git-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_str().unwrap();
        run_git(r, &["init", "-q"]).unwrap();
        run_git(r, &["config", "user.email", "t@t"]).unwrap();
        run_git(r, &["config", "user.name", "t"]).unwrap();
        std::fs::write(root.join("f.txt"), content).unwrap();
        run_git(r, &["add", "-A"]).unwrap();
        run_git(r, &["commit", "-qm", "init"]).unwrap();
        std::fs::write(root.join("f.txt"), modified).unwrap();
        root
    }

    /// `diff --git` / `index` / `---` / `+++` 不进渲染行（噪音），但必须留在
    /// header 里，否则拼出来的 patch 不合法、`git apply` 直接拒收。
    #[test]
    fn strips_mechanical_header_lines_from_view_but_keeps_them_in_patch() {
        let raw = "diff --git a/f.txt b/f.txt\n\
                   index 918fba6..0064e96 100644\n\
                   --- a/f.txt\n\
                   +++ b/f.txt\n\
                   @@ -1,2 +1,2 @@\n\
                   -old\n\
                   +new\n\
                    ctx\n";
        let parsed = parse_diff(raw);

        // 渲染行里不该出现这四类
        let texts: Vec<&str> = parsed.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.starts_with("diff ")
                || t.starts_with("index ")
                || t.starts_with("--- ")
                || t.starts_with("+++ ")),
            "机械头部不该出现在渲染行里：{texts:?}"
        );
        // 第一行应该直接是 hunk 头（文本已换成上下文，所以按类型判断）
        assert!(
            parsed.lines[0].kind == DiffKind::Hunk,
            "首行应是 hunk 头，实际文本 {:?}",
            texts[0]
        );

        // 但 patch 仍然完整：header 四行俱在
        assert!(parsed.header.contains("diff --git a/f.txt b/f.txt"));
        assert!(parsed.header.contains("index 918fba6..0064e96"));
        assert!(parsed.header.contains("--- a/f.txt"));
        assert!(parsed.header.contains("+++ b/f.txt"));
    }

    /// hunk 行只显示上下文（`pub struct Foo {`），不显示 `@@ -49,7 +49,7 @@` 坐标；
    /// 但坐标必须原样留在 raw 里，否则 patch 报废。
    #[test]
    fn hunk_row_shows_context_not_coordinates() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                   @@ -49,7 +49,7 @@ pub struct DeleteWorktreeTarget {\n\
                   -old\n+new\n ctx\n\
                   @@ -100,3 +100,3 @@\n\
                   -a\n+b\n c\n";
        let parsed = parse_diff(raw);

        let hunk_rows: Vec<&str> = parsed
            .lines
            .iter()
            .filter(|l| l.kind == DiffKind::Hunk)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            hunk_rows,
            vec!["pub struct DeleteWorktreeTarget {", ""],
            "第一块该只剩上下文，第二块没有上下文就留空"
        );
        // 坐标仍在 raw 里
        assert!(parsed.hunks[0].raw.starts_with("@@ -49,7 +49,7 @@"), "raw 丢了坐标");
        assert!(parsed.hunks[1].raw.starts_with("@@ -100,3 +100,3 @@"));
        // 行号解析不受影响：第一块的上下文行应从 49 起
        let first_ctx = parsed.lines.iter().find(|l| l.kind == DiffKind::Del).unwrap();
        assert_eq!(first_ctx.old_ln, Some(49), "hunk 起始行号解析被带偏了");
    }

    /// 隐藏元信息行之后，单块 patch 仍要能被 git apply 接受（回归）。
    #[test]
    fn hunk_patch_still_applies_after_hiding_header_lines() {
        let root = repo_with_change("hide-hdr", "alpha\nbravo\n", "alpha\nCHANGED\n");
        let r = root.to_str().unwrap();
        let out = run_git(r, &["diff", "HEAD", "--", "f.txt"]).unwrap();
        let parsed = parse_diff(&String::from_utf8_lossy(&out.stdout));
        let patch = hunk_patch(&parsed.header, &parsed.hunks[0]);
        let applied = run_git_stdin(r, &["apply", "--cached", "-"], &patch).unwrap();
        assert!(
            applied.status.success(),
            "隐藏元信息后 patch 反而不合法了：{}\n--- patch ---\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 行号列宽跟着最大行号走：小文件不该按四位数留白，大文件也不能挤成一团。
    #[test]
    fn gutter_width_tracks_the_largest_line_number() {
        use super::gutter_width;
        let parsed_small = parse_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n",
        );
        let parsed_big = parse_diff(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1200,2 +1200,2 @@\n-a\n+b\n c\n",
        );
        let small = gutter_width(&parsed_small.lines);
        let big = gutter_width(&parsed_big.lines);
        assert!(small < big, "四位数行号该比个位数宽：{small} vs {big}");
        assert!(small < 30.0, "两位数以内不该占到 30px：{small}");
        assert!(big < 50.0, "四位数也不该超过 50px：{big}");
    }

    /// 隔得够远的两处改动 → git 一定分成两个 hunk，且每段的 range 覆盖自己的行。
    #[test]
    fn parses_multiple_hunks_with_correct_ranges() {
        let orig: String = (1..=60).map(|i| format!("line{i}\n")).collect();
        let mut lines: Vec<String> = orig.lines().map(|l| l.to_string()).collect();
        lines[2] = "CHANGED-TOP".into();
        lines[55] = "CHANGED-BOTTOM".into();
        let modified: String = lines.iter().map(|l| format!("{l}\n")).collect();

        let root = repo_with_change("multi", &orig, &modified);
        let out = run_git(root.to_str().unwrap(), &["diff", "HEAD", "--", "f.txt"]).unwrap();
        let parsed = parse_diff(&String::from_utf8_lossy(&out.stdout));

        assert_eq!(parsed.hunks.len(), 2, "相距 50 行的两处改动应分成两个 hunk");
        // range 不能留占位值，且必须首尾相接不重叠
        for h in &parsed.hunks {
            assert!(h.range.end != usize::MAX, "range.end 占位值没回填");
            assert!(h.range.start < h.range.end, "range 为空: {:?}", h.range);
        }
        assert!(parsed.hunks[0].range.end <= parsed.hunks[1].range.start, "两段 range 重叠");
        // 每段原文里只该有自己那处改动
        assert!(parsed.hunks[0].raw.contains("CHANGED-TOP"));
        assert!(!parsed.hunks[0].raw.contains("CHANGED-BOTTOM"));
        assert!(parsed.hunks[1].raw.contains("CHANGED-BOTTOM"));
        assert!(!parsed.hunks[1].raw.contains("CHANGED-TOP"));
        assert!(parsed.header.starts_with("diff --git"), "header 应从 diff --git 起头");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 光比字符串不算数——生成的 patch 必须真能被 `git apply` 吃下。
    /// 只暂存第一个 hunk，索引里就该只有它那处改动。
    #[test]
    fn single_hunk_patch_applies_to_index() {
        let orig: String = (1..=60).map(|i| format!("line{i}\n")).collect();
        let mut lines: Vec<String> = orig.lines().map(|l| l.to_string()).collect();
        lines[2] = "CHANGED-TOP".into();
        lines[55] = "CHANGED-BOTTOM".into();
        let modified: String = lines.iter().map(|l| format!("{l}\n")).collect();

        let root = repo_with_change("apply", &orig, &modified);
        let r = root.to_str().unwrap();
        let out = run_git(r, &["diff", "HEAD", "--", "f.txt"]).unwrap();
        let parsed = parse_diff(&String::from_utf8_lossy(&out.stdout));

        let patch = hunk_patch(&parsed.header, &parsed.hunks[0]);
        let applied = run_git_stdin(r, &["apply", "--cached", "-"], &patch).unwrap();
        assert!(
            applied.status.success(),
            "单块 patch 被 git apply 拒绝：{}\n--- patch ---\n{patch}",
            String::from_utf8_lossy(&applied.stderr)
        );

        // 索引里只有第一处改动，第二处仍留在工作区未暂存
        let staged = run_git(r, &["diff", "--cached"]).unwrap();
        let staged = String::from_utf8_lossy(&staged.stdout);
        assert!(staged.contains("CHANGED-TOP"), "第一块没进索引");
        assert!(!staged.contains("CHANGED-BOTTOM"), "第二块不该被一起暂存");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 末行无换行符时 git 会吐 `\ No newline at end of file`。这行不进 DiffLine，
    /// 若 patch 从渲染结果反拼就会丢，apply 直接报 corrupt——必须留在 raw 里。
    #[test]
    fn patch_keeps_no_newline_marker() {
        let root = repo_with_change("nonewline", "alpha\n", "beta");
        let r = root.to_str().unwrap();
        let out = run_git(r, &["diff", "HEAD", "--", "f.txt"]).unwrap();
        let parsed = parse_diff(&String::from_utf8_lossy(&out.stdout));

        let patch = hunk_patch(&parsed.header, &parsed.hunks[0]);
        assert!(patch.contains("\\ No newline at end of file"), "patch 丢了无换行标记:\n{patch}");
        let applied = run_git_stdin(r, &["apply", "--cached", "-"], &patch).unwrap();
        assert!(
            applied.status.success(),
            "无换行结尾的 patch 被拒绝：{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 「已暂存」视图下取消暂存一块：patch 来自 `git diff --staged`，
    /// `apply --cached --reverse` 应把它退出索引，而工作区文件不受影响。
    #[test]
    fn unstage_hunk_patch_removes_it_from_index_only() {
        let root = repo_with_change("unstage", "alpha\nbravo\n", "alpha\nCHANGED\n");
        let r = root.to_str().unwrap();
        run_git(r, &["add", "-A"]).unwrap();

        // 已暂存视图的 diff，正是索引相对 HEAD 的差异
        let out = run_git(r, &["diff", "--staged", "--", "f.txt"]).unwrap();
        let parsed = parse_diff(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(parsed.hunks.len(), 1);

        let patch = hunk_patch(&parsed.header, &parsed.hunks[0]);
        let applied = run_git_stdin(r, &["apply", "--cached", "--reverse", "-"], &patch).unwrap();
        assert!(
            applied.status.success(),
            "取消暂存的 patch 被拒绝：{}",
            String::from_utf8_lossy(&applied.stderr)
        );

        // 索引已回到 HEAD，但工作区仍是改过的内容
        let staged = run_git(r, &["diff", "--staged"]).unwrap();
        assert!(String::from_utf8_lossy(&staged.stdout).trim().is_empty(), "索引没退干净");
        let text = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert_eq!(text, "alpha\nCHANGED\n", "取消暂存不该动工作区文件");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 丢弃块走 `apply --reverse`（作用于工作区）：改动应从文件里消失。
    #[test]
    fn reverse_patch_discards_change_in_worktree() {
        let root = repo_with_change("reverse", "alpha\nbravo\n", "alpha\nCHANGED\n");
        let r = root.to_str().unwrap();
        let out = run_git(r, &["diff", "HEAD", "--", "f.txt"]).unwrap();
        let parsed = parse_diff(&String::from_utf8_lossy(&out.stdout));

        let patch = hunk_patch(&parsed.header, &parsed.hunks[0]);
        let applied = run_git_stdin(r, &["apply", "--reverse", "-"], &patch).unwrap();
        assert!(
            applied.status.success(),
            "reverse apply 失败：{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        let text = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert_eq!(text, "alpha\nbravo\n", "工作区没被还原");
        let _ = std::fs::remove_dir_all(&root);
    }
}
