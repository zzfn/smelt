//! Git 提交历史（「日志」页）：提交列表 + 分支图 + 单个提交的 diff。
//!
//! 对标 JetBrains 的 Git Log 窗口。本文件只放**与渲染无关**的部分——拉取解析
//! `git log` 和分支图的 lane 布局算法，都是纯函数，好测也好改；GPUI 那半在
//! `git_log_view.rs`。
//!
//! 分支图为什么要自己算：`git log --graph` 吐的是 ASCII 画好的图，拿来做 GUI 得
//! 反过来解析字符画，既脆又对不准行高。直接读 parent 关系自己分配列，画多宽多高
//! 都由我们说了算。

/// 一条提交记录。
#[derive(Clone, Debug, PartialEq)]
pub struct CommitNode {
    /// 完整 sha，父子关系全靠它对齐。
    pub hash: String,
    /// 短 sha，显示用。
    pub short: String,
    pub author: String,
    /// author date，Unix 秒。
    pub time: i64,
    /// 提交标题（第一行）。
    pub subject: String,
    /// 父提交完整 sha。第一个是主线父，其余是被合并进来的。
    pub parents: Vec<String>,
    /// 指向本提交的引用名（分支 / tag），已剥掉 `HEAD -> ` 之类前缀。
    pub refs: Vec<String>,
}

/// 一行在分支图里要画的东西。列号从 0 起，0 是最左边那列。
#[derive(Clone, Debug, PartialEq)]
pub struct GraphRow {
    /// 本行提交的圆点落在第几列。
    pub node: usize,
    /// 本行要画的线段。
    pub edges: Vec<Edge>,
    /// 本行需要多少列（画布宽度按整张图的最大值算，这里用于调试/测试）。
    pub lanes: usize,
}

/// 分支图里的一段线。行的上边缘 → 下边缘，中间可能经过本行的节点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Edge {
    /// 与本行提交无关的线，从上边缘第 `from` 列穿到下边缘第 `to` 列。
    /// `from != to` 时是斜线——旁边的分支因为有列被腾空而横向挪了位。
    Through { from: usize, to: usize },
    /// 上边缘第 `from` 列的线汇入本行节点（本提交是那条线的父）。
    In { from: usize },
    /// 从本行节点出发到下边缘第 `to` 列（本提交的父）。
    Out { to: usize },
}

/// `git log` 的输出格式：用 \x00 分隔字段、\x1e 分隔记录，免得提交标题里的
/// 空格换行把解析搞乱。
pub const LOG_FORMAT: &str = "--pretty=format:%H%x00%h%x00%an%x00%at%x00%s%x00%P%x00%D%x1e";

/// 解析 `git log` 的输出（格式见 [`LOG_FORMAT`]）。
pub fn parse_log(text: &str) -> Vec<CommitNode> {
    text.split('\x1e')
        .map(str::trim_start) // 记录间的换行
        .filter(|r| !r.is_empty())
        .filter_map(|rec| {
            let mut f = rec.split('\x00');
            let hash = f.next()?.to_string();
            let short = f.next()?.to_string();
            let author = f.next()?.to_string();
            let time = f.next()?.parse().unwrap_or(0);
            let subject = f.next()?.to_string();
            let parents = f
                .next()?
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            // %D 形如 "HEAD -> main, origin/main, tag: v1.0"。
            // 两个前缀都剥掉：`HEAD -> ` 只是说明当前检出的是谁，`tag: ` 在列表里
            // 是纯噪音（CI 生成的 tag 名本来就长，再顶个前缀更挤）。
            let refs = f
                .next()
                .unwrap_or("")
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.rsplit(" -> ").next().unwrap_or(s))
                .map(|s| s.strip_prefix("tag: ").unwrap_or(s).to_string())
                .collect();
            Some(CommitNode { hash, short, author, time, subject, parents, refs })
        })
        .collect()
}

/// 给提交序列分配分支图的列。
///
/// 核心是一张「活跃列」表：每列记着**下一个**该出现在这列的提交 sha。逐行处理时——
/// 认领所有等着本提交的列（可能多列：好几个子提交都指向它，此处就是分支汇合），
/// 把最左的那列留给自己当节点，再把它交给第一个父；其余父各占一列，于是有了分叉。
///
/// 只依赖 parent 关系，因此调用方给什么顺序就按什么顺序画（默认的时间序、
/// `--topo-order` 都行）。
pub fn layout_graph(commits: &[CommitNode]) -> Vec<GraphRow> {
    // 每列等待的 sha；None = 该列空闲，可被后来的分支复用。
    // 多列等同一个 sha 是正常的——那正是「几条分支都将汇到这里」，等它出现时一起
    // 汇入画成合流。
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());

    for c in commits {
        // 处理前的快照，末尾算穿行线要用。
        let before = lanes.clone();
        // 1) 认领所有在等本提交的列。
        let claimed: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_deref() == Some(c.hash.as_str()))
            .map(|(i, _)| i)
            .collect();
        let node = match claimed.first() {
            Some(&i) => i,
            // 没人等它：是某条线的头（HEAD、或时间序里刚冒出来的分支）。
            None => {
                let i = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                if i == lanes.len() {
                    lanes.push(None);
                }
                i
            }
        };
        // 被认领的其余列在本行汇入节点后就空了。
        for &i in claimed.iter().skip(1) {
            lanes[i] = None;
        }
        lanes[node] = None;

        // 2) 父提交占列。
        //    第一个父**无条件**继承节点这列，主线才会是一条直的竖线；即便别处已有
        //    列在等同一个父也不合并——两条线各画各的，到那个父出现的行再汇合，
        //    这样中间隔着的提交不会被错误地画到同一条线上。
        let mut out_lanes: Vec<usize> = Vec::new();
        for (n, p) in c.parents.iter().enumerate() {
            let i = if n == 0 {
                node
            } else if let Some(i) = lanes.iter().position(|l| l.as_deref() == Some(p.as_str())) {
                // 被合并进来的这条线已经有列在等了，直接指过去，不再多占一列。
                out_lanes.push(i);
                continue;
            } else {
                let i = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                if i == lanes.len() {
                    lanes.push(None);
                }
                i
            };
            lanes[i] = Some(p.clone());
            out_lanes.push(i);
        }

        // 3) 尾部空列收掉，免得图右边拖一串空白。
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }

        // 4) 连线：汇入 / 发出 / 与本行无关但要穿过去的。
        let mut edges = Vec::new();
        for &i in &claimed {
            edges.push(Edge::In { from: i });
        }
        for &i in &out_lanes {
            edges.push(Edge::Out { to: i });
        }
        // 处理前活着、处理后同一个 sha 仍在等，且没参与本行汇入 → 穿行线。
        // 列号可能因为回收/复用而变，所以按 sha 找它现在落在哪列。
        for (i, slot) in before.iter().enumerate() {
            let Some(sha) = slot else { continue };
            if claimed.contains(&i) {
                continue;
            }
            if let Some(j) = lanes.iter().position(|l| l.as_deref() == Some(sha.as_str())) {
                edges.push(Edge::Through { from: i, to: j });
            }
        }
        let width = lanes.len().max(before.len()).max(node + 1);
        rows.push(GraphRow { node, edges, lanes: width });
    }
    rows
}

/// 日志看哪一段历史。
///
/// 默认是 [`LogScope::Head`]——**当前分支**，跟 JetBrains 一致。一进来就 `--all`
/// 会把所有分支的提交混在一起按时间排，跟「我这条线上都干了啥」完全是两回事。
#[derive(Clone, PartialEq, Debug)]
pub enum LogScope {
    /// 当前检出的分支（`git log` 不带 ref，默认就是 HEAD）。
    Head,
    /// 所有分支（`--all`），看完整拓扑用。
    All,
    /// 指定分支。
    Branch(String),
}

impl Default for LogScope {
    fn default() -> Self {
        Self::Head
    }
}

/// 「日志」页的全部状态。
#[derive(Default)]
pub struct GitLogState {
    /// 当前载入的提交（已按 git log 给的顺序）。
    pub commits: Vec<CommitNode>,
    /// 与 `commits` 一一对应的分支图布局。
    pub graph: Vec<GraphRow>,
    /// 正在拉取提交列表。
    pub loading: bool,
    /// 选中的提交 sha。
    pub selected: Option<String>,
    /// 看哪一段历史，默认当前分支。
    pub scope: LogScope,
    /// 选中提交的改动文件：(状态字母, 路径)。
    pub detail_files: Vec<(String, String)>,
    /// 正在拉取选中提交的详情。
    pub detail_loading: bool,
    /// 详情里选中的文件。
    pub detail_selected_file: Option<String>,
    /// 选中文件在这次提交里的 diff 行（复用 Git 页那套解析与配色）。
    pub detail_diff: Vec<crate::git_panel::DiffLine>,
    /// diff 列表的滚动位置。
    pub detail_scroll: gpui::ScrollHandle,
    /// 已加载的仓库根，切项目时用来判断要不要重拉。
    pub loaded_root: Option<String>,
    pub scroll: gpui::ScrollHandle,
}

/// 一次拉多少条。历史动辄上万条，全量拉既慢又没人翻得到底；先给够用的量，
/// 需要更多再说（滚动到底自动续拉记在 roadmap）。
pub const LOG_LIMIT: usize = 500;

/// 拼 `git log` 的参数。
pub fn log_args(scope: &LogScope, limit: usize) -> Vec<String> {
    let mut a = vec!["log".to_string(), LOG_FORMAT.to_string(), format!("--max-count={limit}")];
    match scope {
        // 不带 ref = HEAD，也就是当前分支。
        LogScope::Head => {}
        LogScope::All => a.push("--all".to_string()),
        LogScope::Branch(b) => a.push(b.clone()),
    }
    a
}

/// 解析 `git show --name-status` 的文件部分：每行 `M\tpath` / `A\tpath` /
/// `R100\told\tnew`（重命名取新名）。
pub fn parse_name_status(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let st = f.next()?.trim();
            if st.is_empty() {
                return None;
            }
            // 重命名/复制有两个路径，取最后一个（新路径）。
            let path = f.last()?.trim();
            if path.is_empty() {
                return None;
            }
            // 只取首字母：R100 → R
            Some((st.chars().next()?.to_string(), path.to_string()))
        })
        .collect()
}

// ===================== Workspace 方法 =====================

use crate::git_panel::run_git;
use crate::Workspace;
use gpui::Context;

impl Workspace {
    /// 进「日志」页时确保数据在（换了仓库也重拉）。render 顶部调用，必须便宜：
    /// 已经是这个 root 就直接返回，真正的 git 调用都在后台。
    pub fn ensure_git_log(&mut self, root: String, cx: &mut Context<Self>) {
        if self.git_log.loading || self.git_log.loaded_root.as_deref() == Some(root.as_str()) {
            return;
        }
        self.reload_git_log(root, cx);
    }

    /// 强制重拉提交列表（「刷新」按钮、切分支）。
    pub fn reload_git_log(&mut self, root: String, cx: &mut Context<Self>) {
        self.git_log.loading = true;
        self.git_log.loaded_root = Some(root.clone());
        cx.notify();
        let scope = self.git_log.scope.clone();
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let commits = cx
                .background_executor()
                .spawn(async move {
                    let args = log_args(&scope, LOG_LIMIT);
                    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    let out = run_git(&r, &argv).ok()?;
                    if !out.status.success() {
                        return None;
                    }
                    Some(parse_log(&String::from_utf8_lossy(&out.stdout)))
                })
                .await
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.git_log.graph = layout_graph(&commits);
                this.git_log.commits = commits;
                this.git_log.loading = false;
                // 列表换了，旧的选中项多半已经不在里面。
                this.git_log.selected = None;
                this.git_log.detail_files.clear();
                this.git_log.detail_selected_file = None;
                cx.notify();
            });
        })
        .detach();
    }

    /// 切换日志范围（当前分支 / 全部分支 / 指定分支）。
    pub fn set_log_scope(&mut self, root: String, scope: LogScope, cx: &mut Context<Self>) {
        if self.git_log.scope == scope {
            return;
        }
        self.git_log.scope = scope;
        self.reload_git_log(root, cx);
    }

    /// 点某条提交：拉它改了哪些文件。
    pub fn select_commit(&mut self, root: String, hash: String, cx: &mut Context<Self>) {
        self.git_log.selected = Some(hash.clone());
        self.git_log.detail_loading = true;
        self.git_log.detail_files.clear();
        self.git_log.detail_selected_file = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (r, h) = (root.clone(), hash.clone());
            let files = cx
                .background_executor()
                .spawn(async move {
                    // --name-status 只要文件清单，diff 内容等点了具体文件再拉——
                    // 一个大提交的完整 diff 可能上万行，全量拉会卡住这一屏。
                    let out = run_git(
                        &r,
                        &["show", "--name-status", "--pretty=format:", "--no-color", &h],
                    )
                    .ok()?;
                    Some(parse_name_status(&String::from_utf8_lossy(&out.stdout)))
                })
                .await
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                // 期间可能又点了别的提交，别把旧结果盖上去。
                if this.git_log.selected.as_deref() == Some(hash.as_str()) {
                    this.git_log.detail_files = files;
                    this.git_log.detail_loading = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 点提交详情里的某个文件：拉它在这次提交里的 diff。
    pub fn select_commit_file(&mut self, root: String, path: String, cx: &mut Context<Self>) {
        let Some(hash) = self.git_log.selected.clone() else { return };
        self.git_log.detail_selected_file = Some(path.clone());
        self.git_log.detail_diff.clear();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (r, h, p) = (root.clone(), hash.clone(), path.clone());
            let lines = cx
                .background_executor()
                .spawn(async move {
                    // 单个文件在这次提交里的改动。--format= 去掉提交头，只留 diff 本体。
                    let out = run_git(
                        &r,
                        &["show", "--format=", "--no-color", &h, "--", &p],
                    )
                    .ok()?;
                    Some(crate::git_panel::parse_diff(&String::from_utf8_lossy(&out.stdout)).lines)
                })
                .await
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                // 期间可能又点了别的文件/提交，别把旧结果盖上去。
                if this.git_log.selected.as_deref() == Some(hash.as_str())
                    && this.git_log.detail_selected_file.as_deref() == Some(path.as_str())
                {
                    this.git_log.detail_diff = lines;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{layout_graph, parse_log, CommitNode, Edge};

    fn c(hash: &str, parents: &[&str]) -> CommitNode {
        CommitNode {
            hash: hash.into(),
            short: hash.into(),
            author: "t".into(),
            time: 0,
            subject: format!("commit {hash}"),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            refs: vec![],
        }
    }

    #[test]
    fn parses_log_records_including_refs_and_parents() {
        let text = "aaa\u{0}aa\u{0}Zoey\u{0}1700000000\u{0}feat: 加个东西\u{0}bbb ccc\u{0}HEAD -> main, origin/main, tag: v1.0\u{1e}\
                    bbb\u{0}bb\u{0}Matt\u{0}1699999999\u{0}fix: 修一下\u{0}\u{0}\u{1e}";
        let got = parse_log(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].author, "Zoey");
        assert_eq!(got[0].subject, "feat: 加个东西");
        assert_eq!(got[0].parents, vec!["bbb", "ccc"], "两个父说明是 merge");
        assert_eq!(
            got[0].refs,
            vec!["main", "origin/main", "v1.0"],
            "`HEAD -> ` 和 `tag: ` 两个前缀都该被剥掉"
        );
        assert_eq!(got[1].time, 1699999999);
        assert!(got[1].parents.is_empty(), "根提交没有父");
    }

    /// 一条直线的历史：所有节点都在第 0 列，中间不该冒出别的列。
    #[test]
    fn linear_history_stays_in_one_lane() {
        let commits = vec![c("a", &["b"]), c("b", &["c"]), c("c", &[])];
        let rows = layout_graph(&commits);
        assert!(rows.iter().all(|r| r.node == 0), "直线历史不该分列：{rows:?}");
        assert!(rows.iter().all(|r| r.lanes <= 1), "不该有多余的列：{rows:?}");
        // 中间那行：上面接 a、下面接 c
        assert!(rows[1].edges.contains(&Edge::In { from: 0 }));
        assert!(rows[1].edges.contains(&Edge::Out { to: 0 }));
        // 根提交没有父，不该再往下发线
        assert!(!rows[2].edges.iter().any(|e| matches!(e, Edge::Out { .. })));
    }

    /// merge 提交有两个父：主线留在原列，被合并的那条另占一列。
    #[test]
    fn merge_commit_forks_a_second_lane() {
        // m 合并了 a（主线）和 b（分支），两者最后都汇到 base
        let commits =
            vec![c("m", &["a", "b"]), c("a", &["base"]), c("b", &["base"]), c("base", &[])];
        let rows = layout_graph(&commits);

        assert_eq!(rows[0].node, 0, "merge 提交自己在第 0 列");
        let outs: Vec<usize> = rows[0]
            .edges
            .iter()
            .filter_map(|e| match e {
                Edge::Out { to } => Some(*to),
                _ => None,
            })
            .collect();
        assert_eq!(outs.len(), 2, "merge 应向下发出两条线：{:?}", rows[0].edges);
        assert!(outs.contains(&0), "第一个父应继承本列，让主线保持直的");
        assert!(outs.iter().any(|&l| l != 0), "第二个父应另占一列");

        // a 和 b 分别落在两列上
        assert_ne!(rows[1].node, rows[2].node, "两个分支不该挤在同一列");
        // base 是汇合点：两条线都汇进来
        let ins = rows[3].edges.iter().filter(|e| matches!(e, Edge::In { .. })).count();
        assert_eq!(ins, 2, "base 应收到两条汇入线：{:?}", rows[3].edges);
    }

    #[test]
    fn parses_name_status_including_renames() {
        let text = "M\tsrc/main.rs\nA\tsrc/new.rs\nD\tsrc/old.rs\nR100\tsrc/a.rs\tsrc/b.rs\n";
        let got = super::parse_name_status(text);
        assert_eq!(
            got,
            vec![
                ("M".to_string(), "src/main.rs".to_string()),
                ("A".to_string(), "src/new.rs".to_string()),
                ("D".to_string(), "src/old.rs".to_string()),
                // 重命名取新路径，状态取首字母
                ("R".to_string(), "src/b.rs".to_string()),
            ]
        );
    }

    /// 拿真仓库跑一遍：手搓的用例可能和 git 的真实拓扑有出入，这里造一个带真实
    /// merge 的仓库，确认解析和布局都不崩、merge 行确实分出两条线。
    #[test]
    fn layouts_a_real_repo_with_a_merge() {
        use super::LOG_FORMAT;
        use crate::git_panel::run_git;

        let root = std::env::temp_dir().join(format!("smelt-gitlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let r = root.to_str().unwrap();
        let git = |args: &[&str]| run_git(r, args).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("f.txt"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        // 分支上改一笔
        git(&["checkout", "-q", "-b", "side"]);
        std::fs::write(root.join("g.txt"), "side\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "side work"]);
        // 主线上也改一笔，制造真正的分叉
        git(&["checkout", "-q", "main"]);
        std::fs::write(root.join("f.txt"), "base\nmain\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "main work"]);
        // 合并（--no-ff 保证生成 merge 提交）
        git(&["merge", "--no-ff", "-q", "side", "-m", "merge side"]);

        let out = run_git(r, &["log", LOG_FORMAT]).unwrap();
        let commits = parse_log(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(commits.len(), 4, "应有 4 个提交：{commits:?}");

        let merge = &commits[0];
        assert_eq!(merge.parents.len(), 2, "第一条应是 merge 提交");
        let rows = layout_graph(&commits);
        assert_eq!(rows.len(), 4);
        let outs = rows[0].edges.iter().filter(|e| matches!(e, Edge::Out { .. })).count();
        assert_eq!(outs, 2, "merge 行应向下发两条线：{:?}", rows[0].edges);
        assert!(rows.iter().any(|r| r.lanes >= 2), "分叉期间应该出现第二列");
        // 最后一个（根）提交回到第 0 列
        assert_eq!(rows[3].node, 0, "根提交应在第 0 列");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 分支合并回来之后，腾出的列要能被回收，不能一直往右长。
    #[test]
    fn lanes_are_reused_after_branches_merge() {
        let commits = vec![
            c("m", &["a", "b"]),
            c("a", &["base"]),
            c("b", &["base"]),
            c("base", &["old"]),
            c("old", &[]),
        ];
        let rows = layout_graph(&commits);
        assert_eq!(rows[4].node, 0, "合并完之后应回到第 0 列，实际 {}", rows[4].node);
        assert!(rows.iter().all(|r| r.lanes <= 2), "两条分支不该撑出第三列：{rows:?}");
    }
}
