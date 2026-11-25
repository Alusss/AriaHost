//! 通知通道自检（M3-c）：**人工目视 + 可脚本判读**。
//!
//! 干什么：建托盘 → 投递一条「清理」通知（应放行）→ 立刻再投一次（应被冷却拦下）
//! → 把放行的那条以托盘气泡发出去 → 停 12 秒便于目视/截图 → 打印日志。
//!
//! 为什么单独做成 example 而不是单测：
//! - 托盘与 `Shell_NotifyIconW` 只在真 Windows 上存在，`cargo test` 里做不了「气泡出现」；
//! - `app.rs` 此刻归 builder-ui（task-46），触发点接线还没轮到本批，所以先用这条自检入口取证。
//!
//! 跑法（Windows）：`cargo run --example notify_selftest`
//! 预期输出含：
//! `SELFTEST first=Queued` / `SELFTEST second=Suppressed { ... Cooldown }` /
//! `notify suppressed kind=clean reason=cooldown` / `SELFTEST balloon ... ok=true`
use std::process::ExitCode;
use std::time::Duration;

use ariahost_ui::{LogBuffer, Notification, Notifier, NotifyKind, NotifyLevel, Tray};

fn main() -> ExitCode {
    let log = LogBuffer::new();

    let Some(mut tray) = Tray::new(&log) else {
        // 没有托盘（非 Windows 或图标资源缺失）：不算失败，明确报告即可。
        println!("SELFTEST tray=absent");
        for line in log.snapshot() {
            println!("LOG {line}");
        }
        return ExitCode::SUCCESS;
    };
    println!("SELFTEST tray=ready can_notify={}", tray.can_notify());

    let mut notifier = Notifier::new();
    // 托盘层把「气泡到底能不能发」写进通知中心，关闭逻辑将来据此决定能不能「藏进托盘」。
    notifier.set_channel(tray.channel());
    println!(
        "SELFTEST channel={} reason={:?}（none 时关闭逻辑必须降级，不许藏进托盘）",
        notifier.channel().summary(),
        notifier.channel().reason()
    );
    let notice = Notification::new(
        NotifyKind::Clean,
        NotifyLevel::Info,
        "可回收 820 MB 缓存",
        "点托盘图标查看明细（自检通知，未真的清理任何文件）",
    );

    let first = notifier.offer(notice.clone());
    println!("SELFTEST first={first:?}");
    let second = notifier.offer(notice);
    println!("SELFTEST second={second:?}");
    if let Some(line) = second.log_line() {
        println!("SELFTEST log={line}");
        log.push(line);
    }

    let mut sent = 0u32;
    for notice in notifier.drain() {
        let ok = tray.balloon(notice.level, &notice.title, &notice.body);
        println!(
            "SELFTEST balloon kind={} level={} ok={ok}",
            notice.kind.as_str(),
            notice.level.as_str()
        );
        log.push(format!(
            "notify sent via=balloon kind={} ok={ok}",
            notice.kind.as_str()
        ));
        if ok {
            sent += 1;
        }
    }

    println!("SELFTEST holding 12s（气泡此时应可见）...");
    std::thread::sleep(Duration::from_secs(12));

    println!("SELFTEST done sent={sent}");
    println!("=== LOG ===");
    for line in log.snapshot() {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
