//! aria2 核心「同步到服务平面」（M2-7，R9-b/§4.9）。
//!
//! ## 为什么要有这个模块
//!
//! 服务模式下 SCM 拉起的 `ariahost.exe` 以 `LocalSystem` 运行，读不到用户的
//! `%LOCALAPPDATA%`；而 `%ProgramData%\AriaHost\aria2\versions\` 之前**没有任何代码往里放东西**，
//! 所以服务注册成功也找不到 `aria2c.exe`。
//!
//! R9-b 定的规矩是：**必须显式同步，不许悄悄复制**。所以这里只提供显式函数，
//! 上层（面板「同步到服务」按钮 / `--sync-service-aria2`）调用一次、用户看见一次。
//! **本模块没有任何自启动、升级时自动调用、定时器**——想自动同步只能改调用方。
//!
//! ## 落位协议（状态机）
//!
//! | 步 | 动作 | 失败后果 |
//! |---|---|---|
//! | 1 | 读用户平面 `active.json` 拿 active 版本 | `AH-E2001` 未找到核心 |
//! | 2 | 枚举源目录文件并逐文件算 sha256 | `AH-E6003` 源不完整/超大 |
//! | 3 | 目标已有同版本 → 逐文件比 sha | 全等 → 幂等成功（不复制） |
//! | 4 | 写 `<ver>.tmp/`（先删残留 `<ver>.tmp`） | 尽力清理 `.tmp`；`AH-E5002`/`AH-E5003`/`AH-E4001` |
//! | 5 | **在 tmp 内**回读逐文件校验 sha | 校验失败 → 删 tmp，`AH-E3002` |
//! | 6 | 需替换时先删旧 `<ver>/`，再 `rename(.tmp → <ver>)` | 最坏只剩 `.tmp`，**绝不留半装 `<ver>/`** |
//! | 7 | 终态目录再校验一次（rename 不改内容，防磁盘异常） | 删掉坏目录，`AH-E3002` |
//! | 8 | 写服务平面 `active.json`（原子，`previous` 保留一代） | 目录已就位；重试同步即可补指针 |
//! | 9 | 清掉悬空 `previous`（复用 `Store::repair_dangling_previous`） | 同上 |
//!
//! 第 6 步的次序是刻意的：**先建好并校验完 tmp，再删旧目录**。反过来（先删后建）会在
//! 中途失败时把一份可用的核心弄没。
//!
//! ## 权限
//!
//! `%ProgramData%` 写入要管理员。`ERROR_ACCESS_DENIED` 统一映射成 `AH-E5002` 并给一句
//! 可执行建议，不把裸 Win32 错误码抛给用户。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ariahost_common::error::ErrorCode;
use ariahost_common::paths::{self, Plane};
use sha2::{Digest, Sha256};

use crate::error::CoreError;
use crate::store::{ActivePointer, VersionStore};
use crate::CORE_EXE;

/// 暂存目录后缀：`<ver>.tmp`。
pub const TMP_SUFFIX: &str = ".tmp";

/// 单个核心文件的大小上限（防误把巨物读进内存）。
const MAX_CORE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// 权限被拒时给用户的建议（可执行，不是「请联系管理员」）。
const ADMIN_HINT: &str =
    "以管理员身份运行 AriaHost（或用面板里的「以管理员身份同步」），再点一次「同步到服务」";

// ---------------------------------------------------------------------------
// 文件系统接缝
// ---------------------------------------------------------------------------

/// 同步过程用到的全部文件操作。
///
/// 抽成 trait 有两条理由：① 单测能注入「第 N 次写必失败」「权限拒绝」这类分支，
/// 不必真的造一个 `%ProgramData%`；② 让所有写盘动作集中在一处，审阅时一眼看完。
pub trait SyncIo: Send + Sync {
    /// 递归建目录。
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// 写一个文件（覆盖）。
    fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// 读整个文件。
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// 改名/移动。
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// 递归删目录（不存在视为成功）。
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// 目录是否存在。
    fn is_dir(&self, path: &Path) -> bool;
    /// 文件是否存在。
    fn is_file(&self, path: &Path) -> bool;
    /// 列出目录里的**文件名**（只要普通文件，排序后返回）。
    fn list_files(&self, dir: &Path) -> io::Result<Vec<String>>;
}

/// 真实实现（`std::fs`）。
pub struct RealSyncIo;

impl SyncIo for RealSyncIo {
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        fs::write(path, bytes)
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn list_files(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 同步失败的原因（每种都有对应的既有错误码，不新造码）。
#[derive(Debug)]
pub enum SyncError {
    /// 用户平面没有激活版本（`AH-E2001`）。
    NoActiveCore {
        /// 用户平面版本库根。
        user_root: PathBuf,
        /// 进一步的细节（例如指针文件缺失）。
        detail: String,
    },
    /// 源版本目录不完整或异常（`AH-E6003`）。
    SourceIncomplete {
        /// 版本。
        version: String,
        /// 出问题的路径。
        path: PathBuf,
        /// 细节。
        detail: String,
    },
    /// 权限不足，需要管理员（`AH-E5002`）。
    NeedsAdmin {
        /// 被拒的路径。
        path: PathBuf,
        /// 在哪一步被拒。
        action: &'static str,
    },
    /// 磁盘满（`AH-E4001`）。
    DiskFull {
        /// 写入失败的路径。
        path: PathBuf,
    },
    /// 其它 IO 失败（`AH-E5003`）。
    Io {
        /// 哪一步。
        action: &'static str,
        /// 出问题的路径。
        path: PathBuf,
        /// 底层错误。
        error: io::Error,
    },
    /// 复制后校验不一致（`AH-E3002`）。
    ShaMismatch {
        /// 哪个文件。
        path: PathBuf,
        /// 期望值。
        expected: String,
        /// 实际值。
        actual: String,
    },
    /// 版本库自身操作失败（激活指针等）。
    Store {
        /// 底层错误。
        error: CoreError,
    },
}

impl SyncError {
    /// 对应的既有错误码（不新造码）。
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::NoActiveCore { .. } => ErrorCode::Aria2NotFound,
            Self::SourceIncomplete { .. } => ErrorCode::UpdatePackageInvalid,
            Self::NeedsAdmin { .. } => ErrorCode::ServiceNeedsAdmin,
            Self::DiskFull { .. } => ErrorCode::DiskFull,
            Self::Io { .. } => ErrorCode::ServiceRegisterPrereq,
            Self::ShaMismatch { .. } => ErrorCode::HashMismatch,
            Self::Store { error } => error.code().unwrap_or(ErrorCode::ServiceRegisterPrereq),
        }
    }

    /// 给用户看的完整一句话（码 + 发生了什么 + 该做什么）。
    pub fn message(&self) -> String {
        match self {
            Self::NoActiveCore { user_root, detail } => format!(
                "{} 还没有可用的 aria2 核心，无法同步到服务：{detail}。请先在面板里下载核心（{}）",
                self.code().code(),
                user_root.display()
            ),
            Self::SourceIncomplete {
                version,
                path,
                detail,
            } => format!(
                "{} 用户平面的 aria2 {version} 不完整：{detail}（{}）。请重新下载该版本后再同步",
                self.code().code(),
                path.display()
            ),
            Self::NeedsAdmin { path, action } => format!(
                "{} {action}被系统拒绝（需要管理员权限）：{}。{ADMIN_HINT}",
                self.code().code(),
                path.display()
            ),
            Self::DiskFull { path } => format!(
                "{} 磁盘空间不足，写入 {} 失败。请清理磁盘后重试",
                self.code().code(),
                path.display()
            ),
            Self::Io {
                action,
                path,
                error,
            } => format!(
                "{} {action}失败：{}（{error}）",
                self.code().code(),
                path.display()
            ),
            Self::ShaMismatch {
                path,
                expected,
                actual,
            } => format!(
                "{} 复制后校验不一致：{}（期望 {expected}，实际 {actual}）。目标未落位，请重试",
                self.code().code(),
                path.display()
            ),
            Self::Store { error } => {
                format!("{} 更新服务平面激活指针失败：{error}", self.code().code())
            }
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            Self::Store { error } => Some(error),
            _ => None,
        }
    }
}

impl From<CoreError> for SyncError {
    fn from(error: CoreError) -> Self {
        Self::Store { error }
    }
}

/// 把 `io::Error` 分类成「要管理员 / 磁盘满 / 其它」。
fn io_error(action: &'static str, path: &Path, error: io::Error) -> SyncError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => SyncError::NeedsAdmin {
            path: path.to_path_buf(),
            action,
        },
        io::ErrorKind::StorageFull => SyncError::DiskFull {
            path: path.to_path_buf(),
        },
        _ => SyncError::Io {
            action,
            path: path.to_path_buf(),
            error,
        },
    }
}

// ---------------------------------------------------------------------------
// 结果
// ---------------------------------------------------------------------------

/// 同步的三种结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// 目标已有同版本且逐文件 sha 一致，什么都没复制。
    AlreadyInSync,
    /// 目标原本没有这个版本，新建。
    Created,
    /// 目标有同版本但内容不一致，已删旧建新。
    Replaced,
}

impl SyncOutcome {
    /// 中文一句话（UI 直接用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::AlreadyInSync => "服务平面已是该版本，无需复制",
            Self::Created => "已复制到服务平面",
            Self::Replaced => "服务平面原有内容不一致，已替换",
        }
    }
}

/// 一次同步的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// 同步的版本（目录名）。
    pub version: String,
    /// 结局。
    pub outcome: SyncOutcome,
    /// 文件数。
    pub files: usize,
    /// 总字节。
    pub bytes: u64,
    /// 同步后的服务平面激活指针。
    pub pointer: ActivePointer,
}

/// 只读探测：服务平面当前状态（给面板显示「服务用的版本与面板是否一致」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCoreStatus {
    /// 服务平面激活的版本；没装就是 `None`。
    pub installed_version: Option<String>,
    /// 用户平面激活的版本；没下载过就是 `None`。
    pub user_active: Option<String>,
    /// 服务平面那份文件的哈希是否与用户平面同版本一致。
    ///
    /// 语义：目标目录里每个文件都能读到，且——**如果**用户平面也有同版本——哈希逐个相等。
    /// 用户平面没有同版本可比时，只检查「文件齐全可读」，此时 `sha_ok=true` 不代表内容正确
    /// （没有参考值），面板文案应避免说「校验通过」。
    pub sha_ok: bool,
    /// 两个平面是否指向同一版本（面板提示「一致/建议同步」的依据）。
    pub matches_user_active: bool,
    /// 文件数（`installed_version` 为 `None` 时是 0）。
    pub files: usize,
    /// 目标总字节。
    pub bytes: u64,
}

impl ServiceCoreStatus {
    /// 需要用户点「同步到服务」吗。
    pub fn needs_sync(&self) -> bool {
        if self.installed_version.is_none() {
            return true;
        }
        !self.matches_user_active || !self.sha_ok
    }

    /// 一句中文状态（UI 直接用）。
    pub fn label(&self) -> String {
        match (&self.installed_version, &self.user_active) {
            (None, None) => "服务平面还没有 aria2 核心，面板也还没下载".to_owned(),
            (None, Some(version)) => {
                format!("服务平面还没有 aria2 核心（面板是 {version}），请点「同步到服务」")
            }
            (Some(installed), None) => {
                format!("服务平面用的是 {installed}，面板还没有下载核心")
            }
            (Some(installed), Some(user)) => {
                if !self.matches_user_active {
                    format!("服务用的是 {installed}，与面板（{user}）不一致，建议同步")
                } else if !self.sha_ok {
                    format!("服务用的是 {installed}，但文件校验不一致，建议重新同步")
                } else {
                    format!("服务用的是 {installed}，与面板一致")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 同步
// ---------------------------------------------------------------------------

/// 用真实文件系统把用户平面当前激活的核心同步到服务平面。
pub fn sync_core_to_service(
    user_root: &Path,
    service_root: &Path,
) -> Result<SyncReport, SyncError> {
    sync_core_to_service_with(user_root, service_root, &RealSyncIo)
}

/// 按当前环境变量取两平面的标准路径做同步。
///
/// 只提供函数，**不自动调用**（R9-b：不许悄悄复制）。
pub fn sync_core_to_service_default() -> Result<SyncReport, SyncError> {
    let user_root = paths::versions_dir(Plane::User).ok_or_else(|| SyncError::NoActiveCore {
        user_root: PathBuf::new(),
        detail: "拿不到 %LOCALAPPDATA%（用户平面目录不可用）".to_owned(),
    })?;
    let service_root =
        paths::versions_dir(Plane::Service).ok_or_else(|| SyncError::NoActiveCore {
            user_root: user_root.clone(),
            detail: "拿不到 %ProgramData%（服务平面目录不可用）".to_owned(),
        })?;
    sync_core_to_service(&user_root, &service_root)
}

/// 可注入文件系统的同步实现（单测入口）。
pub fn sync_core_to_service_with(
    user_root: &Path,
    service_root: &Path,
    io: &dyn SyncIo,
) -> Result<SyncReport, SyncError> {
    // 1. 源：用户平面激活版本
    let user_store = VersionStore::new(user_root);
    let pointer = user_store.active()?;
    let Some(pointer) = pointer else {
        return Err(SyncError::NoActiveCore {
            user_root: user_root.to_path_buf(),
            detail: format!("{} 不存在或没有 active 指针", user_root.display()),
        });
    };
    let version = pointer.active.clone();
    let source_dir = user_root.join(&version);
    if !io.is_dir(&source_dir) {
        return Err(SyncError::SourceIncomplete {
            version: version.clone(),
            path: source_dir,
            detail: "激活指针指向的版本目录不存在".to_owned(),
        });
    }

    // 2. 枚举源文件 + 逐个算 sha
    let names = io
        .list_files(&source_dir)
        .map_err(|error| io_error("读取用户平面版本目录", &source_dir, error))?;
    if names.is_empty() {
        return Err(SyncError::SourceIncomplete {
            version: version.clone(),
            path: source_dir,
            detail: "版本目录里没有任何文件".to_owned(),
        });
    }
    if !names.iter().any(|name| name == CORE_EXE) {
        return Err(SyncError::SourceIncomplete {
            version: version.clone(),
            path: source_dir.join(CORE_EXE),
            detail: format!("缺少 {CORE_EXE}，不是可用的核心目录"),
        });
    }

    let mut expected: BTreeMap<String, (Vec<u8>, String)> = BTreeMap::new();
    let mut total_bytes: u64 = 0;
    for name in &names {
        let path = source_dir.join(name);
        let bytes = io
            .read_file(&path)
            .map_err(|error| io_error("读取用户平面核心文件", &path, error))?;
        if bytes.len() as u64 > MAX_CORE_FILE_BYTES {
            return Err(SyncError::SourceIncomplete {
                version: version.clone(),
                path,
                detail: format!("单个文件超过 {MAX_CORE_FILE_BYTES} 字节，明显不是核心文件"),
            });
        }
        total_bytes += bytes.len() as u64;
        let sha = sha256_hex(&bytes);
        expected.insert(name.clone(), (bytes, sha));
    }

    let target_dir = service_root.join(&version);
    let tmp_dir = service_root.join(format!("{version}{TMP_SUFFIX}"));

    // 3. 目标已有同版本：内容全等就幂等成功（不复制）
    let mut outcome = SyncOutcome::Created;
    if io.is_dir(&target_dir) {
        if dir_matches(io, &target_dir, &expected)? {
            let pointer = finish_pointer(service_root, &version)?;
            return Ok(SyncReport {
                version,
                outcome: SyncOutcome::AlreadyInSync,
                files: expected.len(),
                bytes: total_bytes,
                pointer,
            });
        }
        outcome = SyncOutcome::Replaced;
    }

    // 4. 清掉上次中断留下的 .tmp，再重新建
    io.create_dir_all(service_root)
        .map_err(|error| io_error("创建服务平面版本库目录", service_root, error))?;
    if tmp_dir.exists() || io.is_dir(&tmp_dir) {
        io.remove_dir_all(&tmp_dir).map_err(|error| {
            // 清不掉旧 tmp 不致命：下面写文件会覆盖；但目录无法创建时会报出来。
            io_error("清理上次残留的暂存目录", &tmp_dir, error)
        })?;
    }
    let write_result = (|| -> Result<(), SyncError> {
        io.create_dir_all(&tmp_dir)
            .map_err(|error| io_error("创建暂存目录", &tmp_dir, error))?;
        for (name, (bytes, _)) in &expected {
            let path = tmp_dir.join(name);
            io.write_file(&path, bytes)
                .map_err(|error| io_error("写入核心文件", &path, error))?;
        }
        // 5. 在 tmp 内回读校验（rename 不改内容，这一步才是真正的「复制后校验」）
        let checked = dir_matches(io, &tmp_dir, &expected)?;
        if !checked {
            return Err(SyncError::ShaMismatch {
                path: tmp_dir.clone(),
                expected: expected
                    .values()
                    .map(|(_, sha)| sha.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                actual: "（复读后不一致，详见 tmp 目录）".to_owned(),
            });
        }
        Ok(())
    })();

    if let Err(error) = write_result {
        // 失败：尽力清掉 tmp，**绝不留半装的 <ver>/**
        let _ = io.remove_dir_all(&tmp_dir);
        return Err(error);
    }

    // 6. 需替换时先删旧目录，再原子改名（反过来会把可用核心弄没）
    if io.is_dir(&target_dir) {
        io.remove_dir_all(&target_dir)
            .map_err(|error| io_error("删除旧的服务平面版本目录", &target_dir, error))?;
    }
    io.rename(&tmp_dir, &target_dir)
        .map_err(|error| io_error("暂存目录改名为正式目录", &target_dir, error))?;

    // 7. 终态再核一次（rename 本身不改内容，这一步防磁盘异常）
    if !dir_matches(io, &target_dir, &expected)? {
        let _ = io.remove_dir_all(&target_dir);
        return Err(SyncError::ShaMismatch {
            path: target_dir,
            expected: "（与源目录逐文件比对）".to_owned(),
            actual: "（终态校验失败，已删除该目录）".to_owned(),
        });
    }

    // 8-9. 指针 + 悬空 previous
    let pointer = finish_pointer(service_root, &version)?;

    Ok(SyncReport {
        version,
        outcome,
        files: expected.len(),
        bytes: total_bytes,
        pointer,
    })
}

/// 目录是否与期望的「文件名 → sha」完全一致（数目、名字、内容都要对上）。
fn dir_matches(
    io: &dyn SyncIo,
    dir: &Path,
    expected: &BTreeMap<String, (Vec<u8>, String)>,
) -> Result<bool, SyncError> {
    let names = io
        .list_files(dir)
        .map_err(|error| io_error("读取目标目录", dir, error))?;
    if names.len() != expected.len() {
        return Ok(false);
    }
    for name in &names {
        let Some((_, want)) = expected.get(name) else {
            return Ok(false);
        };
        let path = dir.join(name);
        let bytes = io
            .read_file(&path)
            .map_err(|error| io_error("读取目标文件", &path, error))?;
        if sha256_hex(&bytes) != *want {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 写服务平面激活指针，并清掉悬空 `previous`。
fn finish_pointer(service_root: &Path, version: &str) -> Result<ActivePointer, SyncError> {
    let store = VersionStore::new(service_root);
    store.activate(version)?;
    // 上一代目录可能已被清理掉；复用 M1-3 的既有修复逻辑，不让 previous 指向空气。
    let pointer = store.repair_dangling_previous()?.unwrap_or_else(|| {
        store.active().ok().flatten().unwrap_or(ActivePointer {
            active: version.to_owned(),
            previous: None,
        })
    });
    Ok(pointer)
}

// ---------------------------------------------------------------------------
// 只读探测
// ---------------------------------------------------------------------------

/// 读服务平面状态（真实文件系统）。
pub fn service_core_status(
    user_root: &Path,
    service_root: &Path,
) -> Result<ServiceCoreStatus, SyncError> {
    service_core_status_with(user_root, service_root, &RealSyncIo)
}

/// 按当前环境变量读两平面状态。**只读**：不会建目录、不会写指针。
pub fn service_core_status_default() -> Result<ServiceCoreStatus, SyncError> {
    let user_root = paths::versions_dir(Plane::User).unwrap_or_default();
    let service_root = paths::versions_dir(Plane::Service).unwrap_or_default();
    service_core_status(&user_root, &service_root)
}

/// 可注入文件系统的只读探测。
pub fn service_core_status_with(
    user_root: &Path,
    service_root: &Path,
    io: &dyn SyncIo,
) -> Result<ServiceCoreStatus, SyncError> {
    let user_active = VersionStore::new(user_root)
        .active()?
        .map(|pointer| pointer.active);
    let installed_version = VersionStore::new(service_root)
        .active()?
        .map(|pointer| pointer.active);

    let Some(version) = installed_version.clone() else {
        return Ok(ServiceCoreStatus {
            installed_version: None,
            matches_user_active: false,
            user_active,
            sha_ok: false,
            files: 0,
            bytes: 0,
        });
    };

    let target_dir = service_root.join(&version);
    let mut files = 0;
    let mut bytes = 0;
    let mut sha_ok = io.is_dir(&target_dir);
    if sha_ok {
        match io.list_files(&target_dir) {
            Ok(names) => {
                files = names.len();
                for name in &names {
                    let path = target_dir.join(name);
                    match io.read_file(&path) {
                        Ok(content) => {
                            bytes += content.len() as u64;
                            // 有同版本的用户平面副本时，以它为参考值比哈希。
                            if let Some(user_version) = user_active.as_deref() {
                                if user_version == version {
                                    let reference = user_root.join(&version).join(name);
                                    if let Ok(want) = io.read_file(&reference) {
                                        if sha256_hex(&want) != sha256_hex(&content) {
                                            sha_ok = false;
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => sha_ok = false,
                    }
                }
            }
            Err(_) => sha_ok = false,
        }
    }

    let matches_user_active = user_active.as_deref() == Some(version.as_str());
    Ok(ServiceCoreStatus {
        installed_version: Some(version),
        user_active,
        sha_ok,
        matches_user_active,
        files,
        bytes,
    })
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// sha256 十六进制小写（与 `fetch` 里的同名函数同一算法、同一输出形态）。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    /// 一个临时「两平面」目录：`root/user/versions` 与 `root/service/versions`。
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("ariahost-sync-{tag}-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("建根目录");
            Self { root }
        }

        fn user(&self) -> PathBuf {
            self.root.join("user")
        }

        fn service(&self) -> PathBuf {
            self.root.join("service")
        }

        /// 在用户平面装一个版本（exe + 可选 DLL）。
        fn install_user(&self, version: &str, exe: &[u8], dll: Option<&[u8]>) -> PathBuf {
            let dir = self.user().join(version);
            fs::create_dir_all(&dir).expect("建版本目录");
            fs::write(dir.join(CORE_EXE), exe).expect("写 exe");
            if let Some(bytes) = dll {
                fs::write(dir.join("libaria2.dll"), bytes).expect("写 dll");
            }
            let store = VersionStore::new(self.user());
            store.activate(version).expect("写用户平面指针");
            dir
        }

        fn tmp(&self, version: &str) -> PathBuf {
            self.service().join(format!("{version}{TMP_SUFFIX}"))
        }

        fn target(&self, version: &str) -> PathBuf {
            self.service().join(version)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// 记账的真实 IO：数写了几次文件，可在第 N 次注入失败。
    struct CountingIo {
        writes: AtomicUsize,
        fail_at_write: Option<usize>,
        fail_kind: io::ErrorKind,
        seen: Mutex<Vec<PathBuf>>,
    }

    impl CountingIo {
        fn new() -> Self {
            Self {
                writes: AtomicUsize::new(0),
                fail_at_write: None,
                fail_kind: io::ErrorKind::PermissionDenied,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn failing_at(index: usize, kind: io::ErrorKind) -> Self {
            Self {
                writes: AtomicUsize::new(0),
                fail_at_write: Some(index),
                fail_kind: kind,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn write_count(&self) -> usize {
            self.writes.load(Ordering::Relaxed)
        }
    }

    impl SyncIo for CountingIo {
        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            RealSyncIo.create_dir_all(path)
        }

        fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            let index = self.writes.fetch_add(1, Ordering::Relaxed);
            if self.fail_at_write == Some(index) {
                return Err(io::Error::new(self.fail_kind, "注入的写失败"));
            }
            self.seen.lock().expect("锁").push(path.to_path_buf());
            let mut file = fs::File::create(path)?;
            file.write_all(bytes)
        }

        fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
            RealSyncIo.read_file(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            RealSyncIo.rename(from, to)
        }

        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            RealSyncIo.remove_dir_all(path)
        }

        fn is_dir(&self, path: &Path) -> bool {
            RealSyncIo.is_dir(path)
        }

        fn is_file(&self, path: &Path) -> bool {
            RealSyncIo.is_file(path)
        }

        fn list_files(&self, dir: &Path) -> io::Result<Vec<String>> {
            RealSyncIo.list_files(dir)
        }
    }

    // ---- 正常路径 ----

    #[test]
    fn sync_copies_core_and_writes_pointer() {
        let fixture = Fixture::new("basic");
        fixture.install_user("1.37.0", b"exe-bytes", Some(b"dll-bytes"));

        let report = sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步成功");
        assert_eq!(report.version, "1.37.0");
        assert_eq!(report.outcome, SyncOutcome::Created);
        assert_eq!(report.files, 2);
        assert_eq!(report.bytes, 9 + 9);

        // 内容一致
        assert_eq!(
            fs::read(fixture.target("1.37.0").join(CORE_EXE)).expect("读 exe"),
            b"exe-bytes"
        );
        assert_eq!(
            fs::read(fixture.target("1.37.0").join("libaria2.dll")).expect("读 dll"),
            b"dll-bytes"
        );
        // 指针
        let pointer = VersionStore::new(fixture.service())
            .active()
            .expect("读指针")
            .expect("有指针");
        assert_eq!(pointer.active, "1.37.0");
        // 没有 tmp 残留
        assert!(!fixture.tmp("1.37.0").exists(), "不该留下暂存目录");
        // 服务平面根下只有「版本目录 + 指针文件」
        assert!(fixture.target("1.37.0").is_dir());
        assert!(fixture.service().join(crate::store::ACTIVE_FILE).is_file());
    }

    #[test]
    fn second_sync_is_idempotent_and_copies_nothing() {
        let fixture = Fixture::new("idempotent");
        fixture.install_user("1.37.0", b"exe-bytes", None);
        sync_core_to_service(&fixture.user(), &fixture.service()).expect("首次同步");

        let io = CountingIo::new();
        let report =
            sync_core_to_service_with(&fixture.user(), &fixture.service(), &io).expect("二次同步");
        assert_eq!(report.outcome, SyncOutcome::AlreadyInSync);
        assert_eq!(io.write_count(), 0, "幂等路径一次写都不该发生");
    }

    #[test]
    fn mismatched_target_is_replaced() {
        let fixture = Fixture::new("replace");
        fixture.install_user("1.37.0", b"new-exe", Some(b"new-dll"));
        // 服务平面已有一份内容不同的同版本
        let target = fixture.target("1.37.0");
        fs::create_dir_all(&target).expect("建旧目录");
        fs::write(target.join(CORE_EXE), b"old-exe").expect("写旧 exe");
        fs::write(target.join("stale.dll"), b"stale").expect("写多余 dll");

        let report = sync_core_to_service(&fixture.user(), &fixture.service()).expect("替换成功");
        assert_eq!(report.outcome, SyncOutcome::Replaced);
        assert_eq!(fs::read(target.join(CORE_EXE)).expect("读 exe"), b"new-exe");
        assert!(!target.join("stale.dll").exists(), "旧目录必须整体替换");
        assert_eq!(report.files, 2);
    }

    // ---- 失败路径 ----

    #[test]
    fn write_failure_leaves_no_half_installed_version() {
        let fixture = Fixture::new("midfail");
        fixture.install_user("1.37.0", b"exe-bytes", Some(b"dll-bytes"));
        let io = CountingIo::failing_at(1, io::ErrorKind::Other);

        let error = sync_core_to_service_with(&fixture.user(), &fixture.service(), &io)
            .expect_err("必须失败");
        assert_eq!(error.code(), ErrorCode::ServiceRegisterPrereq);
        assert!(
            !fixture.target("1.37.0").exists(),
            "失败绝不许留下半装的 <ver>/"
        );
        assert!(!fixture.tmp("1.37.0").exists(), "尽力清掉暂存目录");
        assert!(
            VersionStore::new(fixture.service())
                .active()
                .expect("读指针")
                .is_none(),
            "失败不该写指针"
        );
    }

    #[test]
    fn stale_tmp_from_crash_is_cleaned_by_next_sync() {
        let fixture = Fixture::new("staletmp");
        fixture.install_user("1.37.0", b"exe-bytes", None);
        let tmp = fixture.tmp("1.37.0");
        fs::create_dir_all(&tmp).expect("造残留 tmp");
        fs::write(tmp.join("garbage.bin"), b"junk").expect("写垃圾");

        let report = sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步成功");
        assert_eq!(report.outcome, SyncOutcome::Created);
        assert!(!tmp.exists(), "残留 tmp 必须被清掉");
        assert_eq!(
            fs::read(fixture.target("1.37.0").join(CORE_EXE)).expect("读 exe"),
            b"exe-bytes"
        );
    }

    #[test]
    fn permission_denied_maps_to_e5002() {
        let fixture = Fixture::new("denied");
        fixture.install_user("1.37.0", b"exe-bytes", None);
        let io = CountingIo::failing_at(0, io::ErrorKind::PermissionDenied);

        let error = sync_core_to_service_with(&fixture.user(), &fixture.service(), &io)
            .expect_err("要拒绝");
        assert_eq!(error.code(), ErrorCode::ServiceNeedsAdmin);
        assert_eq!(error.code().code(), "AH-E5002");
        let message = error.to_string();
        assert!(message.contains("管理员"), "{message}");
        assert!(message.contains("同步到服务"), "要给可执行建议：{message}");
        assert!(!fixture.target("1.37.0").exists());
    }

    #[test]
    fn no_active_core_is_e2001() {
        let fixture = Fixture::new("noactive");
        let error = sync_core_to_service(&fixture.user(), &fixture.service()).expect_err("要报错");
        assert_eq!(error.code(), ErrorCode::Aria2NotFound);
        assert!(error.to_string().contains("请先在面板里下载核心"));
    }

    #[test]
    fn active_pointing_to_missing_dir_is_e6003() {
        let fixture = Fixture::new("dangling-active");
        // 指针指向一个不存在的版本目录
        let store = VersionStore::new(fixture.user());
        fs::create_dir_all(fixture.user()).expect("建用户平面目录");
        fs::write(
            fixture.user().join(crate::store::ACTIVE_FILE),
            br#"{"active":"1.37.0"}"#,
        )
        .expect("写指针");
        assert_eq!(store.active().expect("读").expect("有").active, "1.37.0");

        let error = sync_core_to_service(&fixture.user(), &fixture.service()).expect_err("要报错");
        assert_eq!(error.code(), ErrorCode::UpdatePackageInvalid);
    }

    #[test]
    fn missing_exe_in_source_is_e6003() {
        let fixture = Fixture::new("noexe");
        let dir = fixture.user().join("1.37.0");
        fs::create_dir_all(&dir).expect("建目录");
        fs::write(dir.join("notes.txt"), b"no exe here").expect("写文件");
        // 手写指针：`VersionStore::activate` 会校验 exe 存在，这里要造「指针指向坏目录」。
        fs::write(
            fixture.user().join(crate::store::ACTIVE_FILE),
            br#"{"active":"1.37.0"}"#,
        )
        .expect("写指针");

        let error = sync_core_to_service(&fixture.user(), &fixture.service()).expect_err("要报错");
        assert_eq!(error.code(), ErrorCode::UpdatePackageInvalid);
        assert!(error.to_string().contains(CORE_EXE));
    }

    #[test]
    fn dangling_previous_is_cleared_after_sync() {
        let fixture = Fixture::new("previous");
        fixture.install_user("1.37.0", b"exe-37", None);
        sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步 1.37.0");

        // 用户换到 1.38.0 再同步：服务平面 previous 应指向 1.37.0
        fixture.install_user("1.38.0", b"exe-38", None);
        let report =
            sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步 1.38.0");
        assert_eq!(report.version, "1.38.0");
        assert_eq!(report.pointer.previous.as_deref(), Some("1.37.0"));

        // 手工删掉 1.37.0 目录 → 指针里的 previous 悬空，再同步时被清掉
        fs::remove_dir_all(fixture.target("1.37.0")).expect("删旧版本");
        let report = sync_core_to_service(&fixture.user(), &fixture.service()).expect("再次同步");
        assert_eq!(report.outcome, SyncOutcome::AlreadyInSync);
        assert_eq!(
            report.pointer.previous, None,
            "悬空的 previous 必须被清掉，不能指向空气"
        );
    }

    // ---- 只读探测 ----

    #[test]
    fn status_reflects_both_planes() {
        let fixture = Fixture::new("status");
        fixture.install_user("1.37.0", b"exe-bytes", None);

        let before = service_core_status(&fixture.user(), &fixture.service()).expect("读状态");
        assert_eq!(before.installed_version, None);
        assert_eq!(before.user_active.as_deref(), Some("1.37.0"));
        assert!(before.needs_sync());
        assert!(before.label().contains("请点「同步到服务」"));

        sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步");

        let after = service_core_status(&fixture.user(), &fixture.service()).expect("读状态");
        assert_eq!(after.installed_version.as_deref(), Some("1.37.0"));
        assert_eq!(after.user_active.as_deref(), Some("1.37.0"));
        assert!(after.matches_user_active);
        assert!(after.sha_ok);
        assert_eq!(after.files, 1);
        assert!(!after.needs_sync());
        assert!(after.label().contains("与面板一致"));
    }

    #[test]
    fn status_detects_divergence_and_corruption() {
        let fixture = Fixture::new("diverged");
        fixture.install_user("1.37.0", b"exe-37", None);
        sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步 1.37.0");

        // 用户换版本 → 不一致
        fixture.install_user("1.38.0", b"exe-38", None);
        let status = service_core_status(&fixture.user(), &fixture.service()).expect("读状态");
        assert!(!status.matches_user_active);
        assert!(status.needs_sync());
        assert!(status.label().contains("建议同步"));

        // 把服务平面的 exe 改坏 → sha 不一致（先同步 1.38.0 让两平面同版本）
        sync_core_to_service(&fixture.user(), &fixture.service()).expect("同步 1.38.0");
        fs::write(fixture.target("1.38.0").join(CORE_EXE), b"corrupted").expect("改坏");
        let status = service_core_status(&fixture.user(), &fixture.service()).expect("读状态");
        assert!(status.matches_user_active);
        assert!(!status.sha_ok, "内容不一致必须被发现");
        assert!(status.needs_sync());
        assert!(status.label().contains("校验不一致"));
    }

    #[test]
    fn status_is_read_only_and_creates_nothing() {
        let fixture = Fixture::new("readonly");
        let _ = service_core_status(&fixture.user(), &fixture.service()).expect("读状态");
        assert!(!fixture.user().exists(), "只读探测不该建用户平面目录");
        assert!(!fixture.service().exists(), "只读探测不该建服务平面目录");
    }

    // ---- 小工具 ----

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn outcome_labels_are_pinned() {
        assert!(SyncOutcome::AlreadyInSync.label().contains("无需复制"));
        assert!(SyncOutcome::Created.label().contains("已复制"));
        assert!(SyncOutcome::Replaced.label().contains("已替换"));
    }
}
