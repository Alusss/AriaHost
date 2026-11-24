//! 版本库与激活指针。
//!
//! 布局（架构 §3.3，路径由 [`ariahost_common::paths::versions_dir`] 提供，这里不硬编码）：
//!
//! ```text
//! <versions>/
//!   active.json            # {"active":"1.37.0","previous":"1.36.0"}
//!   1.37.0/aria2c.exe      # 安装完成后才出现（先写到 .staging-*，再整体 rename）
//!   1.36.0/aria2c.exe
//! ```
//!
//! 原子性：
//! - **目录**：先把文件写进 `.staging-<ver>-<pid>-<seq>/`，再 `rename` 成 `<ver>/`。
//!   中途失败只留下 staging 目录，不会出现「半装的版本」被当成可用版本。
//! - **指针**：写 `active.json.tmp` → `sync_all` → `rename` 覆盖 `active.json`。
//!   `rename` 在 Windows 上走 `MoveFileEx(REPLACE_EXISTING)`，同卷覆盖是原子的。
//!
//! 回滚语义：指针里只保留**一代** `previous`（设计定版）；回滚自身也把旧的 active 写回
//! `previous`，所以「回滚」可以再回滚一次（不会把上一代弄丢）。

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ariahost_aria2::{parse_version_number, Aria2Version};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::zip_extract::ExtractedCore;
use crate::CORE_EXE;

/// 激活指针文件名。
pub const ACTIVE_FILE: &str = "active.json";

/// 安装暂存目录前缀。
const STAGING_PREFIX: &str = ".staging-";

/// 用于生成互不相同的 staging 目录名。
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// 激活指针内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePointer {
    /// 当前激活的版本（目录名）。
    pub active: String,
    /// 上一代版本（单代回滚用）；首次激活时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

/// 取得目标 exe 写权限的探测函数。
///
/// 生产实现是「以写方式打开该文件」：Windows 上正在运行的 exe 会以
/// `ERROR_SHARING_VIOLATION (32)` 拒绝写打开，因此这里能真实反映「被占用」。
/// 测试可注入假实现来覆盖 `AH-E6005` 分支（Linux 上无法真锁住可执行文件）。
pub type ExeProbe = Arc<dyn Fn(&Path) -> Result<(), CoreError> + Send + Sync>;

/// 版本库（一个平面一份；用户平面与服务平面各自独立）。
#[derive(Clone)]
pub struct VersionStore {
    root: PathBuf,
    probe: ExeProbe,
}

impl fmt::Debug for VersionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VersionStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl VersionStore {
    /// 用真实探测函数构造。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_probe(root, Arc::new(probe_exe_writable))
    }

    /// 注入探测函数（测试用）。
    pub fn with_probe(root: impl Into<PathBuf>, probe: ExeProbe) -> Self {
        Self {
            root: root.into(),
            probe,
        }
    }

    /// 版本库根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 某个版本的目录。
    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.root.join(version)
    }

    /// 某个版本的 `aria2c.exe` 路径。
    pub fn exe_path(&self, version: &str) -> PathBuf {
        self.version_dir(version).join(CORE_EXE)
    }

    /// 该版本是否已安装（目录里确实有 exe）。
    pub fn is_installed(&self, version: &str) -> bool {
        self.exe_path(version).is_file()
    }

    /// 列出已安装版本（按版本号降序；目录名解析不出版本号的一律忽略）。
    pub fn list(&self) -> Result<Vec<String>, CoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // 版本库还没建 = 空库，不是错误。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(store_io(&self.root, "读版本库目录", error)),
        };

        let mut versions: Vec<(Aria2Version, String)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| store_io(&self.root, "读版本库条目", error))?;
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(STAGING_PREFIX) {
                continue;
            }
            if !self.exe_path(&name).is_file() {
                continue;
            }
            let Ok(version) = parse_version_number(&name) else {
                continue;
            };
            versions.push((version, name));
        }
        versions.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
        Ok(versions.into_iter().map(|(_, name)| name).collect())
    }

    /// 把解包结果安装成 `<root>/<version>/`。
    ///
    /// 已经是完整安装时直接返回既有目录（幂等）；已存在但缺 exe 时会先删掉再装。
    pub fn install_core(&self, version: &str, core: &ExtractedCore) -> Result<PathBuf, CoreError> {
        parse_version_number(version).map_err(|_| CoreError::BadVersion {
            value: version.to_owned(),
        })?;
        fs::create_dir_all(&self.root)
            .map_err(|error| store_io(&self.root, "创建版本库目录", error))?;

        let target = self.version_dir(version);
        if self.exe_path(version).is_file() {
            return Ok(target);
        }

        let sequence = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let staging = self.root.join(format!(
            "{STAGING_PREFIX}{version}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|error| store_io(&staging, "创建暂存目录", error))?;

        let written = self.write_staged(&staging, core);
        if let Err(error) = written {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        // 目标存在但不完整：先清掉，避免 rename 失败或留下混装目录。
        if target.exists() {
            fs::remove_dir_all(&target)
                .map_err(|error| store_io(&target, "清理不完整版本目录", error))?;
        }
        fs::rename(&staging, &target).map_err(|error| {
            let _ = fs::remove_dir_all(&staging);
            store_io(&target, "提交版本目录（rename）", error)
        })?;
        Ok(target)
    }

    /// 把核心与随包 DLL 写进暂存目录。
    fn write_staged(&self, staging: &Path, core: &ExtractedCore) -> Result<(), CoreError> {
        let exe = staging.join(CORE_EXE);
        write_file(&exe, &core.aria2c)?;
        for (name, bytes) in &core.dlls {
            // 只写文件名字符串（解析阶段已保证没有路径分隔符残留）。
            write_file(&staging.join(name), bytes)?;
        }
        Ok(())
    }

    /// 读激活指针；文件不存在返回 `Ok(None)`。
    pub fn active(&self) -> Result<Option<ActivePointer>, CoreError> {
        let path = self.root.join(ACTIVE_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(store_io(&path, "读取激活指针", error)),
        };
        let pointer: ActivePointer = serde_json::from_slice(&bytes).map_err(|error| {
            CoreError::store_io(format!(
                "激活指针 {} 不是合法 JSON：{error}",
                path.display()
            ))
        })?;
        Ok(Some(pointer))
    }

    /// 激活某个已安装版本。
    ///
    /// 目标 exe 取不到写权限（Windows 上=正在运行）→ [`CoreError::ExeInUse`]（`AH-E6005`），
    /// **指针保持不变**。
    pub fn activate(&self, version: &str) -> Result<ActivePointer, CoreError> {
        let exe = self.exe_path(version);
        if !exe.is_file() {
            return Err(CoreError::VersionNotFound {
                version: version.to_owned(),
            });
        }
        (self.probe)(&exe)?;

        let current = self.active()?;
        if let Some(pointer) = &current {
            // 已经是当前版本：幂等返回，且不要用自己覆盖 previous。
            if pointer.active == version {
                return Ok(pointer.clone());
            }
        }
        let pointer = ActivePointer {
            active: version.to_owned(),
            previous: current.map(|pointer| pointer.active),
        };
        self.write_pointer(&pointer)?;
        Ok(pointer)
    }

    /// 回滚到上一代。
    ///
    /// 回滚本身也把旧 active 写进 `previous`，所以「回滚」可以再回滚一次。
    pub fn rollback(&self) -> Result<ActivePointer, CoreError> {
        let current = self.active()?.ok_or_else(|| CoreError::RollbackFailed {
            detail: "没有激活指针（尚未激活任何版本）".to_owned(),
        })?;
        let previous = current
            .previous
            .clone()
            .ok_or_else(|| CoreError::RollbackFailed {
                detail: format!("{} 没有上一代可回滚", current.active),
            })?;
        let exe = self.exe_path(&previous);
        if !exe.is_file() {
            return Err(CoreError::RollbackFailed {
                detail: format!("上一代 {previous} 的目录已不存在"),
            });
        }
        (self.probe)(&exe)?;

        let pointer = ActivePointer {
            active: previous,
            previous: Some(current.active),
        };
        self.write_pointer(&pointer)?;
        Ok(pointer)
    }

    /// 原子写激活指针（`active.json.tmp` → `rename`）。
    fn write_pointer(&self, pointer: &ActivePointer) -> Result<(), CoreError> {
        fs::create_dir_all(&self.root)
            .map_err(|error| store_io(&self.root, "创建版本库目录", error))?;
        let target = self.root.join(ACTIVE_FILE);
        let temporary = self.root.join(format!("{ACTIVE_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(pointer)
            .map_err(|error| CoreError::store_io(format!("激活指针序列化失败：{error}")))?;

        let mut file = fs::File::create(&temporary)
            .map_err(|error| store_io(&temporary, "创建临时指针", error))?;
        file.write_all(&json)
            .map_err(|error| store_io(&temporary, "写入临时指针", error))?;
        file.sync_all()
            .map_err(|error| store_io(&temporary, "落盘临时指针", error))?;
        drop(file);

        fs::rename(&temporary, &target).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            store_io(&target, "提交激活指针（rename）", error)
        })
    }
}

/// 生产探测：以写方式打开目标文件（不截断）。
///
/// 任何写打开失败都按「被占用/无写权限」上报——这正是 `AH-E6005` 的语义。
pub fn probe_exe_writable(path: &Path) -> Result<(), CoreError> {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => {
            drop(file);
            Ok(())
        }
        Err(_) => Err(CoreError::ExeInUse {
            path: path.display().to_string(),
        }),
    }
}

/// 写一个文件并落盘。
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let mut file = fs::File::create(path).map_err(|error| store_io(path, "创建文件", error))?;
    file.write_all(bytes)
        .map_err(|error| store_io(path, "写入文件", error))?;
    file.sync_all()
        .map_err(|error| store_io(path, "落盘文件", error))?;
    Ok(())
}

/// 把 IO 错误包装成 [`CoreError::StoreIo`]（带操作与路径，便于诊断）。
fn store_io(path: &Path, action: &str, error: std::io::Error) -> CoreError {
    CoreError::store_io(format!("{action} {} 失败：{error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip_extract::ExtractedCore;
    use crate::CORE_EXE;
    use std::sync::atomic::AtomicU32;

    /// 每个测试一个独立临时目录，`Drop` 时清掉。
    struct TestDir(PathBuf);

    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let sequence = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ariahost-core-store-{tag}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn core(bytes: &[u8]) -> ExtractedCore {
        ExtractedCore {
            aria2c: bytes.to_vec(),
            dlls: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn store(tag: &str) -> (TestDir, VersionStore) {
        let dir = TestDir::new(tag);
        let store = VersionStore::with_probe(dir.path(), Arc::new(|_| Ok(())));
        (dir, store)
    }

    #[test]
    fn install_writes_exe_and_list_is_sorted() {
        let (_dir, store) = store("install");
        assert!(store.list().expect("空库应可列").is_empty());
        assert!(store.active().expect("无指针").is_none());

        store.install_core("1.36.0", &core(b"old")).expect("装旧版");
        store.install_core("1.37.0", &core(b"new")).expect("装新版");
        assert_eq!(store.list().expect("列版本"), vec!["1.37.0", "1.36.0"]);
        assert_eq!(fs::read(store.exe_path("1.37.0")).expect("读 exe"), b"new");
        assert!(store.is_installed("1.37.0"));
        assert!(!store.is_installed("1.38.0"));
    }

    #[test]
    fn install_is_idempotent_and_leaves_no_staging() {
        let (_dir, store) = store("idempotent");
        store.install_core("1.37.0", &core(b"first")).expect("首次");
        store
            .install_core("1.37.0", &core(b"second"))
            .expect("二次");
        // 已完整安装 → 保留第一次的内容（不重复覆盖）。
        assert_eq!(
            fs::read(store.exe_path("1.37.0")).expect("读 exe"),
            b"first"
        );
        let leftovers: Vec<_> = fs::read_dir(store.root())
            .expect("读根")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "不应留下 staging：{leftovers:?}");
    }

    #[test]
    fn install_repairs_incomplete_version_dir() {
        let (_dir, store) = store("repair");
        // 造一个「目录在但没 exe」的半装状态。
        fs::create_dir_all(store.version_dir("1.37.0")).expect("造半装目录");
        fs::write(store.version_dir("1.37.0").join("junk.bin"), b"junk").expect("写垃圾");
        store
            .install_core("1.37.0", &core(b"real"))
            .expect("应修复");
        assert_eq!(fs::read(store.exe_path("1.37.0")).expect("读 exe"), b"real");
        assert!(!store.version_dir("1.37.0").join("junk.bin").exists());
    }

    #[test]
    fn install_rejects_bad_version_string() {
        let (_dir, store) = store("badver");
        let error = store
            .install_core("v1.37.0", &core(b"x"))
            .expect_err("非法版本号应拒绝");
        assert!(matches!(error, CoreError::BadVersion { .. }), "{error}");
    }

    #[test]
    fn activate_sets_and_keeps_previous() {
        let (_dir, store) = store("activate");
        store.install_core("1.36.0", &core(b"old")).expect("装旧版");
        store.install_core("1.37.0", &core(b"new")).expect("装新版");

        let pointer = store.activate("1.36.0").expect("首次激活");
        assert_eq!(pointer.active, "1.36.0");
        assert_eq!(pointer.previous, None);

        let pointer = store.activate("1.37.0").expect("切换");
        assert_eq!(pointer.active, "1.37.0");
        assert_eq!(pointer.previous.as_deref(), Some("1.36.0"));

        // 重复激活同一版本：幂等，且不清掉 previous。
        let pointer = store.activate("1.37.0").expect("重复激活");
        assert_eq!(pointer.active, "1.37.0");
        assert_eq!(pointer.previous.as_deref(), Some("1.36.0"));
    }

    #[test]
    fn pointer_file_is_written_without_temp_leftover() {
        let (_dir, store) = store("pointer");
        store.install_core("1.37.0", &core(b"new")).expect("装");
        store.activate("1.37.0").expect("激活");
        let pointer_path = store.root().join(ACTIVE_FILE);
        assert!(pointer_path.is_file());
        assert!(
            !store.root().join(format!("{ACTIVE_FILE}.tmp")).exists(),
            "rename 之后不应留下 .tmp"
        );
        let parsed: ActivePointer =
            serde_json::from_slice(&fs::read(&pointer_path).expect("读指针")).expect("解析");
        assert_eq!(parsed.active, "1.37.0");
    }

    #[test]
    fn activate_missing_version_is_version_not_found() {
        let (_dir, store) = store("missing");
        let error = store.activate("1.37.0").expect_err("未安装不能激活");
        assert!(
            matches!(error, CoreError::VersionNotFound { .. }),
            "{error}"
        );
        assert_eq!(error.code(), None);
    }

    #[test]
    fn activate_reports_exe_in_use_and_keeps_pointer() {
        let dir = TestDir::new("inuse");
        let store = VersionStore::with_probe(dir.path(), Arc::new(|_| Ok(())));
        store.install_core("1.36.0", &core(b"old")).expect("旧版");
        store.install_core("1.37.0", &core(b"new")).expect("新版");
        store.activate("1.36.0").expect("先激活旧版");

        // 换成「目标 exe 被占用」的探测函数。
        let busy = VersionStore::with_probe(
            dir.path(),
            Arc::new(|path: &Path| {
                Err(CoreError::ExeInUse {
                    path: path.display().to_string(),
                })
            }),
        );
        let error = busy.activate("1.37.0").expect_err("占用必须失败");
        assert!(matches!(error, CoreError::ExeInUse { .. }), "{error}");
        assert_eq!(error.code(), Some(ariahost_common::ErrorCode::ExeInUse));
        // 指针必须保持在旧版。
        let pointer = busy.active().expect("读指针").expect("有指针");
        assert_eq!(pointer.active, "1.36.0");
    }

    #[test]
    fn rollback_swaps_and_is_itself_reversible() {
        let (_dir, store) = store("rollback");
        store.install_core("1.35.0", &core(b"a")).expect("装 1.35");
        store.install_core("1.36.0", &core(b"b")).expect("装 1.36");
        store.install_core("1.37.0", &core(b"c")).expect("装 1.37");
        store.activate("1.36.0").expect("激活 1.36");
        store.activate("1.37.0").expect("切到 1.37");

        let pointer = store.rollback().expect("回滚");
        assert_eq!(pointer.active, "1.36.0");
        assert_eq!(pointer.previous.as_deref(), Some("1.37.0"));

        // 再回滚一次 → 回到 1.37.0（回滚可逆）。
        let pointer = store.rollback().expect("再回滚");
        assert_eq!(pointer.active, "1.37.0");
        assert_eq!(pointer.previous.as_deref(), Some("1.36.0"));
    }

    #[test]
    fn rollback_without_previous_fails_with_code() {
        let (_dir, store) = store("rollback-none");
        store.install_core("1.37.0", &core(b"c")).expect("装");
        store.activate("1.37.0").expect("首次激活");
        let error = store.rollback().expect_err("没有上一代");
        assert!(matches!(error, CoreError::RollbackFailed { .. }), "{error}");
        assert_eq!(
            error.code(),
            Some(ariahost_common::ErrorCode::RollbackFailed)
        );
    }

    #[test]
    fn corrupted_pointer_is_store_io_error() {
        let (_dir, store) = store("corrupt");
        fs::write(store.root().join(ACTIVE_FILE), b"not-json").expect("写坏指针");
        let error = store.active().expect_err("坏指针必须报错");
        assert!(matches!(error, CoreError::StoreIo { .. }), "{error}");
        assert_eq!(error.code(), None);
    }

    #[test]
    fn list_ignores_staging_and_non_version_dirs() {
        let (_dir, store) = store("list-filter");
        store.install_core("1.37.0", &core(b"c")).expect("装");
        fs::create_dir_all(store.root().join(".staging-1.38.0-1-0")).expect("造 staging");
        fs::create_dir_all(store.root().join("not-a-version")).expect("造怪目录");
        fs::write(store.root().join("not-a-version").join(CORE_EXE), b"x").expect("写假 exe");
        assert_eq!(store.list().expect("列"), vec!["1.37.0"]);
    }
}
