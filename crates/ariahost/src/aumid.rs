//! `--write-aumid <lnk>`：给快捷方式写 `System.AppUserModel.ID`（架构 R9，Toast 的前提）。
//!
//! ## 为什么必须自己写
//!
//! Toast 只认「带 `System.AppUserModel.ID` 的开始菜单 `.lnk`」。而 NSIS 的 `CreateShortCut`
//! （底层 `IShellLink`）**写不了属性存储**，所以安装器 `ariahost.nsi` 每建一个快捷方式就回调
//! `ariahost.exe --write-aumid <lnk>`；这里失败**不阻断安装**（脚本只记警告，Toast 运行期降级）。
//!
//! ## 结构
//!
//! - 纯逻辑（[`validate_target`]、[`AUMID`]、[`AumidError`] 的码表）在**所有平台**编译并测试；
//! - 真正的属性存储写入（COM + `IPropertyStore`）只在 `cfg(windows)` 下编译。
//!
//! 本机没有 MSVC 链接器，构建不出 Windows 可执行，所以「写进去是否真的生效」由 CI 产物与真机
//! 验证承担 —— 这是本批报告里明确列为**未验证**的一项，不要当成已验。
//!
//! ## 为什么用裸 vtable 而不是 `windows` crate
//!
//! `windows-sys` 只导出 `SHGetPropertyStoreFromParsingName`（返回 `*mut c_void`），不生成
//! COM 接口包装。为了这一个调用引入 `windows` crate（编译面大、审计面也大）不值得，
//! 所以这里按 ABI 手写 8 个槽位的 vtable —— 槽位顺序是 `IUnknown`(3) + `IPropertyStore`(5)，
//! 与 `propsys.h` 一致，写错一个槽位就会调错函数，因此有 [`tests::vtable_layout_is_pinned`]
//! 钉住顺序与大小。

use std::path::{Path, PathBuf};

use ariahost_common::ErrorCode;

/// 本产品的 AUMID（与 `ariahost.nsi`、Toast 代码里的字面量必须一致）。
pub const AUMID: &str = "AriaHost.Desktop";

/// 写入失败的两类原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AumidError {
    /// 目标路径不可用（空、不是 `.lnk`、不存在、不是文件、含 NUL）。
    InvalidTarget {
        /// 调用方给的路径（原样，便于排障）。
        path: String,
        /// 中文原因（面向用户）。
        reason: &'static str,
    },
    /// 属性存储写入失败（COM/属性系统）。`hresult` 是原始 HRESULT，用于日志与报障。
    WriteFailed {
        /// 目标路径。
        path: String,
        /// 原始 `HRESULT`（十六进制打印时按 `0x` 处理）。
        hresult: i64,
        /// 一张人话的说明。
        detail: String,
    },
}

impl AumidError {
    /// 映射到的既有错误码（本批**不新增码**）。
    ///
    /// - 目标路径问题 → `AH-E1001`（命令行用法错误，用户可直接改参数重试）；
    /// - 写入失败 → `AH-E1003`（既有「配置/存储 IO 失败」码段：属性存储写入就是文件级写入），
    ///   详情里带原始 HRESULT，别指望用户自己翻译。
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidTarget { .. } => ErrorCode::Usage,
            Self::WriteFailed { .. } => ErrorCode::ConfigIoFailed,
        }
    }

    /// 一行中文说明（第一行给用户看）。
    pub fn message(&self) -> String {
        match self {
            Self::InvalidTarget { path, reason } => {
                format!("AUMID 未写入：{reason}（{path}）")
            }
            Self::WriteFailed {
                path,
                hresult,
                detail,
            } => format!("AUMID 写入失败：{detail}（{path}，HRESULT 0x{hresult:08X}）"),
        }
    }

    /// 可执行建议（第二行）。
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::InvalidTarget { .. } => {
                "检查路径：它必须是已存在的快捷方式文件（扩展名 .lnk）；重跑安装器会自动重建。"
            }
            Self::WriteFailed { .. } => {
                "快捷方式可能被占用或属性存储只读；重新运行安装器可修复。Toast 会降级为托盘气泡，不影响下载。"
            }
        }
    }
}

/// 校验目标路径并返回绝对路径（Windows 上不带 `\\?\` 前缀，属性系统不认扩展长度前缀）。
///
/// 这一步**与平台无关**，所以非法路径在本机（Linux）也能测出正确的中文原因。
pub fn validate_target(lnk: &Path) -> Result<PathBuf, AumidError> {
    let shown = lnk.display().to_string();
    let invalid = |reason: &'static str| AumidError::InvalidTarget {
        path: shown.clone(),
        reason,
    };

    if lnk.as_os_str().is_empty() {
        return Err(invalid("路径为空"));
    }
    // 宽字符路径里出现 NUL 会被截断，等于悄悄写错文件。
    if shown.contains('\0') {
        return Err(invalid("路径含 NUL 字符"));
    }
    let is_lnk = lnk
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"));
    if !is_lnk {
        return Err(invalid("不是 .lnk 快捷方式"));
    }
    if !lnk.exists() {
        return Err(invalid("文件不存在"));
    }
    if !lnk.is_file() {
        return Err(invalid("不是普通文件"));
    }
    // `std::path::absolute` 只做「补当前目录 + 规范化 `.`/`..`」，不加 `\\?\` 前缀。
    std::path::absolute(lnk).map_err(|error| AumidError::InvalidTarget {
        path: shown,
        reason: if error.kind() == std::io::ErrorKind::NotFound {
            "文件不存在"
        } else {
            "路径无法解析为绝对路径"
        },
    })
}

/// 给 `lnk` 写入 [`AUMID`]。成功返回 `Ok(())`。
pub fn write_aumid(lnk: &Path) -> Result<(), AumidError> {
    let absolute = validate_target(lnk)?;
    platform::write(&absolute)
}

#[cfg(windows)]
mod platform {
    //! 真正的 COM 调用（仅 Windows 编译）。

    use std::ffi::c_void;
    use std::path::Path;

    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::Foundation::{PROPERTYKEY, S_OK};
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::Shell::PropertiesSystem::{
        SHGetPropertyStoreFromParsingName, GPS_READWRITE,
    };

    use super::{AumidError, AUMID};

    /// `IID_IPropertyStore`（`propsys.h`：886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99）。
    const IID_IPROPERTY_STORE: GUID = GUID {
        data1: 0x886D_8EEB,
        data2: 0x8CF2,
        data3: 0x4446,
        data4: [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99],
    };

    /// `PKEY_AppUserModel_ID`（`propkey.h`：9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3 PID 5）。
    pub(super) const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID {
            data1: 0x9F4C_2855,
            data2: 0x9F79,
            data3: 0x4B39,
            data4: [0xA8, 0xD0, 0xE1, 0xD4, 0x2D, 0xE1, 0xD5, 0xF3],
        },
        pid: 5,
    };

    /// `VT_LPWSTR`（`wtypes.h`）。本文件只用这一种 PROPVARIANT 形态。
    pub(super) const VT_LPWSTR: u16 = 31;

    /// `PROPVARIANT` 的**只读入参视图**：8 字节头（vt + 3×reserved）+ 偏移 8 处的一个指针。
    ///
    /// **这不是完整的 `PROPVARIANT`，只是一个「读得对」的视图。** 官方定义 = 8 字节头 +
    /// 16 字节联合（联合里含 `DECIMAL`，见 `windows-sys` 的
    /// `Win32::System::Variant::PROPVARIANT`）→ **两种架构都是 24 字节**；
    /// 本结构体只有 16（x64）/ 12（x86）字节。
    ///
    /// 之所以安全：只用于 `IPropertyStore::SetValue` 的 `*const` 入参，Windows 只按 `vt`
    /// 读 `vt@0` 与 `data@8`，**不会写回来**。反之——
    /// **绝不能**把它交给会写入的实现（`GetValue` / `PropVariantCopy` / 任何 `*mut` 出参），
    /// 32 位下 12 字节被写就会越界。要支持那些接口，别扩这个结构体，改用官方定义。
    #[repr(C)]
    pub(super) struct PropVariant {
        pub vt: u16,
        pub reserved1: u16,
        pub reserved2: u16,
        pub reserved3: u16,
        /// `VT_LPWSTR` 时是指向以 NUL 结尾的宽字符串的指针。
        pub data: *mut u16,
    }

    impl PropVariant {
        /// 构造 `VT_LPWSTR`（`text` 必须已经以 NUL 结尾且活得比属性存储 `SetValue` 更久）。
        fn lpwstr(text: *mut u16) -> Self {
            Self {
                vt: VT_LPWSTR,
                reserved1: 0,
                reserved2: 0,
                reserved3: 0,
                data: text,
            }
        }
    }

    /// `IPropertyStore` 的 vtable：`IUnknown` 3 槽 + `IPropertyStore` 5 槽（顺序即 ABI）。
    #[repr(C)]
    pub(super) struct PropertyStoreVtbl {
        pub query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        pub release: unsafe extern "system" fn(*mut c_void) -> u32,
        pub get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        pub get_at: unsafe extern "system" fn(*mut c_void, u32, *mut PROPERTYKEY) -> HRESULT,
        pub get_value:
            unsafe extern "system" fn(*mut c_void, *const PROPERTYKEY, *mut PropVariant) -> HRESULT,
        pub set_value: unsafe extern "system" fn(
            *mut c_void,
            *const PROPERTYKEY,
            *const PropVariant,
        ) -> HRESULT,
        pub commit: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    }

    /// 裸接口指针（首字段是 vtable 指针，这是 COM 的 ABI）。
    #[repr(C)]
    pub(super) struct PropertyStore {
        pub vtbl: *const PropertyStoreVtbl,
    }

    /// 及时 `Release`，任何提前返回都不泄漏引用计数。
    struct StoreGuard(*mut PropertyStore);

    impl Drop for StoreGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 指针来自 SHGetPropertyStoreFromParsingName 成功返回，只释放一次。
                unsafe {
                    let vtbl = &*(*self.0).vtbl;
                    (vtbl.release)(self.0.cast());
                }
            }
        }
    }

    /// 把 `AUMID` 写进 `.lnk` 的属性存储。
    pub fn write(lnk: &Path) -> Result<(), AumidError> {
        use std::os::windows::ffi::OsStrExt as _;

        // 1) COM：这是 UI 线程之外的一次性助手，用 STA 更稳（属性系统常与 shell 扩展同屋）。
        // SAFETY: CoInitializeEx 是进程级初始化；失败码见下（RPC_E_CHANGED_MODE 可忽略）。
        unsafe {
            let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            // 0x80010106 = RPC_E_CHANGED_MODE：本线程已被初始化成 MTA，继续用即可。
            if hr != S_OK && hr != 0x8001_0106_u32 as HRESULT {
                return Err(failed(lnk, hr, "COM 初始化失败"));
            }
        }

        // 2) 路径与值转宽字符（都以 NUL 结尾）。
        let mut wide_path: Vec<u16> = lnk.as_os_str().encode_wide().collect();
        wide_path.push(0);
        let mut wide_value: Vec<u16> = AUMID.encode_utf16().collect();
        wide_value.push(0);

        // 3) 打开属性存储（读写）。
        let mut raw: *mut PropertyStore = std::ptr::null_mut();
        // SAFETY: 入参都是本函数持有的有效缓冲区；`ppv` 指向的指针由系统写。
        let hr = unsafe {
            SHGetPropertyStoreFromParsingName(
                wide_path.as_ptr(),
                std::ptr::null_mut(),
                GPS_READWRITE,
                &IID_IPROPERTY_STORE,
                (&raw mut raw).cast::<*mut c_void>(),
            )
        };
        if hr != S_OK {
            // SAFETY: CoInitializeEx 成功过，对称反初始化。
            unsafe { CoUninitialize() };
            return Err(failed(lnk, hr, "打开快捷方式属性存储失败"));
        }
        // 从这里开始由 StoreGuard 保证 Release（任何提前返回都不泄漏引用）。
        let store = StoreGuard(raw);

        // 4) SetValue(PKEY_AppUserModel_ID, VT_LPWSTR(AUMID))，然后 Commit。
        // SAFETY: store 由 StoreGuard 保证在 Commit 之后才 Release；两个缓冲区活得比调用久，
        // 且属性存储在 SetValue 时**复制**值（不会留指针），所以临时 PropVariant 可以丢弃。
        let hr = unsafe {
            let vtbl = &*(*store.0).vtbl;
            let value = PropVariant::lpwstr(wide_value.as_mut_ptr());
            (vtbl.set_value)(store.0.cast(), &PKEY_APP_USER_MODEL_ID, &value)
        };
        if hr != S_OK {
            unsafe { CoUninitialize() };
            return Err(failed(lnk, hr, "写入 AUMID 属性失败"));
        }
        // SAFETY: 同上，store 仍有效。
        let hr = unsafe {
            let vtbl = &*(*store.0).vtbl;
            (vtbl.commit)(store.0.cast())
        };
        if hr != S_OK {
            unsafe { CoUninitialize() };
            return Err(failed(lnk, hr, "提交属性存储失败"));
        }

        // SAFETY: 与开头的 CoInitializeEx 配对。
        unsafe { CoUninitialize() };
        Ok(())
    }

    fn failed(lnk: &Path, hresult: HRESULT, detail: &str) -> AumidError {
        AumidError::WriteFailed {
            path: lnk.display().to_string(),
            // HRESULT 是 i32；按 u32 位模式扩成 i64，日志里才不会出现负号。
            hresult: i64::from(hresult as u32),
            detail: detail.to_owned(),
        }
    }
}

#[cfg(not(windows))]
mod platform {
    //! 非 Windows：如实报「写不了」，不伪造成功。

    use std::path::Path;

    use super::AumidError;

    /// 缩略后的说明：Windows 属性存储不在非 Windows 上存在。
    pub fn write(lnk: &Path) -> Result<(), AumidError> {
        Err(AumidError::WriteFailed {
            path: lnk.display().to_string(),
            hresult: 0,
            detail: "非 Windows 平台没有 Shell 属性存储".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUMID 是跨文件契约（安装器与 Toast 代码都写死这个字面量），改动必须同步三处。
    #[test]
    fn aumid_is_pinned() {
        assert_eq!(AUMID, "AriaHost.Desktop");
    }

    #[test]
    fn empty_path_is_invalid() {
        let error = validate_target(Path::new("")).expect_err("空路径必须失败");
        assert_eq!(error.code(), ErrorCode::Usage);
        assert_eq!(error.code().code(), "AH-E1001");
        assert!(error.message().contains("路径为空"), "{error:?}");
    }

    #[test]
    fn non_lnk_extension_is_invalid() {
        let error = validate_target(Path::new("C:/temp/x.exe")).expect_err("非 .lnk 必须失败");
        assert!(error.message().contains("不是 .lnk"), "{error:?}");
    }

    /// 扩展名大小写不敏感（安装器在不同机器上给出 `.LNK`/`.lnk` 都可能）。
    #[test]
    fn lnk_extension_is_case_insensitive() {
        // 先用不存在的路径验证「扩展名判断通过、但文件不存在」这条分支。
        let error =
            validate_target(Path::new("C:/temp/不存在.LNK")).expect_err("文件不存在也要失败");
        assert!(error.message().contains("文件不存在"), "{error:?}");
    }

    #[test]
    fn missing_file_is_invalid() {
        let path = std::env::temp_dir().join("ariahost-aumid-missing.lnk");
        let _ = std::fs::remove_file(&path);
        let error = validate_target(&path).expect_err("不存在的文件必须失败");
        assert!(error.message().contains("文件不存在"), "{error:?}");
        assert_eq!(error.code(), ErrorCode::Usage);
    }

    #[test]
    fn directory_is_invalid() {
        let dir = std::env::temp_dir();
        let error = validate_target(&dir).expect_err("目录不是快捷方式");
        assert!(error.message().contains(".lnk"), "{error:?}");
    }

    #[test]
    fn nul_in_path_is_invalid() {
        let error = validate_target(Path::new("a\0b.lnk")).expect_err("NUL 必须失败");
        assert!(error.message().contains("NUL"), "{error:?}");
    }

    /// 真实文件（空的 `.lnk` 也能过校验——校验只看「是文件」，内容由属性系统负责）。
    #[test]
    fn existing_lnk_file_passes_validation() {
        let path =
            std::env::temp_dir().join(format!("ariahost-aumid-ok-{}.lnk", std::process::id()));
        std::fs::write(&path, b"pk").expect("建临时文件");
        let absolute = validate_target(&path).expect("应通过校验");
        assert!(absolute.is_absolute(), "{absolute:?}");
        // 非 Windows 上写不进去，但必须是「写入失败」而不是「路径无效」。
        let error = write_aumid(&path).expect_err("非 Windows 写不了");
        assert_eq!(error.code(), ErrorCode::ConfigIoFailed);
        assert_eq!(error.code().code(), "AH-E1003");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn vtable_slot_order_is_pinned() {
        use std::mem::size_of;
        // IUnknown(3) + IPropertyStore(5) = 8 个函数指针槽位；槽位顺序即 ABI，
        // 结构被改动（增删槽位、换类型）会在这里先炸，而不是在真机上调用错函数。
        assert_eq!(
            size_of::<platform::PropertyStoreVtbl>(),
            usize::BITS as usize
        );
    }

    /// `PROPVARIANT` 入参视图的布局钉死：`vt@0`、`data@8`、总长 = 8 + 指针宽度。
    ///
    /// 32 位首轮真跑就红在这里：原断言写死 `size_of == 16`，而 x86 上是 **12**（8 + 4）。
    /// 这不是产品缺陷——入参路径只被读，不被写；但断言必须按指针宽度表达，
    /// 否则等于用 x64 的数字去约束 x86。
    #[cfg(windows)]
    #[test]
    fn propvariant_layout_is_pinned() {
        use std::mem::{offset_of, size_of};
        let expected = 8 + size_of::<*mut u16>();
        assert_eq!(size_of::<platform::PropVariant>(), expected);
        assert!(
            expected == 16 || expected == 12,
            "未知的目标指针宽度：{expected}"
        );
        assert_eq!(offset_of!(platform::PropVariant, vt), 0);
        // 偏移 8 = 官方联合区起点，两架构一致（头部固定 8 字节）。
        assert_eq!(offset_of!(platform::PropVariant, data), 8);
    }
}
