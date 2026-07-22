//! GPUI 的 background executor 不是 tokio：`reqwest` 等依赖 tokio reactor 的 future
//! 不能直接在里面 `.await`，否则会 panic「no reactor running」（表现为
//! `Option::expect_failed`，且 GPUI 的执行器不会替任务捕获这个 panic，会直接带崩
//! 整个 GUI 进程）。
//!
//! 这里现造一个临时 current-thread 运行时把 future 跑完，并用 `catch_unwind` 兜底：
//! 运行时内部任何 panic 都转成 `Err`，而不是让调用方所在的进程整个消失。

use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// 在临时 tokio current-thread 运行时里跑一个 future，捕获其中的 panic。
///
/// 调用方仍需自己 `cx.background_executor().spawn(...)`，这里只解决"给 reqwest
/// 一个 reactor"和"panic 不带崩 GUI"两件事，不负责调度到后台线程。
pub fn block_on_tokio<F, T>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = T>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    catch_unwind(AssertUnwindSafe(|| rt.block_on(fut))).map_err(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "后台任务内部错误".to_string());
        anyhow::anyhow!("后台任务崩溃（已拦截）：{msg}")
    })
}
