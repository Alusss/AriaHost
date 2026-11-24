//! 进程内存口径（EG-4）。
//!
//! 验收口径写死为：release 构建、启动后 60s 稳态、`GetProcessMemoryInfo → PrivateWorkingSetSize`
//! （≈任务管理器「内存」列）、含开窗。本文件只提供「当前进程私有工作集」这一个读数，
//! `--panel-selftest` 用它把数字打出来，真机 60s 稳态仍由验收方按同一口径量。
//!
//! 为什么不引 `windows` crate：这里只需要一个函数，`windows-sys` 是 Microsoft 官方轻量绑定，
//! 且已经在本仓库依赖树里（winit/wgpu 间接依赖），不增加新的供应链面。

/// 当前进程的私有工作集（KB）。取不到时返回 `None`（面板继续运行，不因量测失败而崩）。
#[cfg(windows)]
pub fn private_working_set_kb() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // 结构体按 EX2 尺寸填 cb，系统才会写回 PrivateWorkingSetSize（EX 结构没有这个字段）。
    let mut counters: PROCESS_MEMORY_COUNTERS_EX2 = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::addr_of_mut!(counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    (ok != 0).then(|| (counters.PrivateWorkingSetSize / 1024) as u64)
}

/// 非 Windows 目标（本项目只发 Windows，这里只为 `cargo check` 之类工具链检查不炸）。
#[cfg(not(windows))]
pub fn private_working_set_kb() -> Option<u64> {
    None
}

/// 量测只在 Windows 上有意义；非 Windows 目标没有这个测试，避免空测试模块的未用导入告警。
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// CI 的 Windows runner 上必须能量到正数；量不到说明 P/Invoke 用法错了。
    #[test]
    fn measures_own_private_working_set() {
        let kb = private_working_set_kb().expect("Windows 上应能取到私有工作集");
        assert!(kb > 0, "私有工作集应大于 0 KB，实际 {kb}");
        // 测试进程不可能只有几 KB（那是量错了）；也不该到 100GB（那是单位错了）。
        assert!(kb > 512, "读数过小，疑似单位/结构体用错：{kb} KB");
        assert!(
            kb < 100 * 1024 * 1024,
            "读数过大，疑似单位/结构体用错：{kb} KB"
        );
    }
}
