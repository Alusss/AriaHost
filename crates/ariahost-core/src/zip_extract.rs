//! aria2 官方 Windows zip 的解包：只取 `aria2c.exe`（以及随包的 DLL，如果有）。
//!
//! 安全边界：
//! - **不按 zip 内的路径写盘**：只认文件名（basename），写入位置由 [`crate::store`] 决定，
//!   因此路径穿越（zip slip）在结构上不可能发生；带 `..` 的条目仍然显式跳过并记录。
//! - 单体上限 [`MAX_ENTRY_BYTES`]：拒绝异常大的条目（防解压炸弹）。
//! - 必须恰好有一个 `aria2c.exe`；缺失或重复都算包损坏。
//! - CRC32 由 `zip` crate 在读到条目末尾时校验（`InvalidData` → 包损坏）。

use std::io::{Cursor, Read};

use crate::error::CoreError;
use crate::http::ZIP_MAGIC;

/// 单个条目解压后的上限（aria2c.exe 约 5–7 MB；64 MiB 已是异常）。
pub const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// 解包结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedCore {
    /// `aria2c.exe` 内容。
    pub aria2c: Vec<u8>,
    /// 随包 DLL（文件名 + 内容）。官方 win 包通常没有 DLL，有则一并带走，避免「解压少文件」。
    pub dlls: Vec<(String, Vec<u8>)>,
    /// 被跳过的条目（目录、带 `..` 的可疑路径等），只用于诊断。
    pub skipped: Vec<String>,
}

impl ExtractedCore {
    /// 总字节数（安装前的磁盘预估用）。
    pub fn total_bytes(&self) -> u64 {
        let dlls: u64 = self.dlls.iter().map(|(_, bytes)| bytes.len() as u64).sum();
        self.aria2c.len() as u64 + dlls
    }
}

/// 从 zip 字节里取出核心文件。
pub fn extract_core(zip_bytes: &[u8]) -> Result<ExtractedCore, CoreError> {
    if !zip_bytes.starts_with(ZIP_MAGIC) {
        return Err(CoreError::package("不是 zip：开头不是 PK\\x03\\x04"));
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|error| CoreError::package(format!("zip 解析失败：{error}")))?;

    let mut core: Option<Vec<u8>> = None;
    let mut dlls: Vec<(String, Vec<u8>)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| CoreError::package(format!("读取第 {index} 个条目失败：{error}")))?;

        if entry.is_dir() {
            continue;
        }
        // zip 9.x 的 name() 返回 Result（条目名可能不是合法 UTF-8/CP437）。
        let name = entry
            .name()
            .map_err(|error| CoreError::package(format!("读取第 {index} 个条目名失败：{error}")))?
            .into_owned();
        let base = basename(&name);
        if base.is_empty() || name.contains("..") {
            skipped.push(name);
            continue;
        }
        let size = entry.size();
        if size > MAX_ENTRY_BYTES {
            return Err(CoreError::package(format!(
                "条目 {base} 解压后 {size} 字节，超过上限 {MAX_ENTRY_BYTES}"
            )));
        }

        if base.eq_ignore_ascii_case(crate::CORE_EXE) {
            if core.is_some() {
                return Err(CoreError::package("zip 内有多个 aria2c.exe"));
            }
            core = Some(read_entry(&mut entry, base)?);
        } else if base.to_ascii_lowercase().ends_with(crate::DLL_SUFFIX) {
            dlls.push((base.to_owned(), read_entry(&mut entry, base)?));
        } else {
            skipped.push(name);
        }
    }

    let aria2c = core.ok_or_else(|| CoreError::package("zip 内没有 aria2c.exe"))?;
    Ok(ExtractedCore {
        aria2c,
        dlls,
        skipped,
    })
}

/// 读一个条目到内存（带上限二次校验，防 zip 头里的大小作假）。
fn read_entry<R: Read>(entry: &mut R, name: &str) -> Result<Vec<u8>, CoreError> {
    let mut buffer: Vec<u8> = Vec::new();
    let read = entry
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut buffer)
        .map_err(|error| {
            CoreError::package(format!("解压 {name} 失败（可能 CRC 不符）：{error}"))
        })?;
    if read as u64 > MAX_ENTRY_BYTES {
        return Err(CoreError::package(format!("{name} 解压后超过上限")));
    }
    Ok(buffer)
}

/// 取路径的文件名部分（同时接受 `/` 与 `\`）。
fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    /// 在内存里造一个 zip（条目：(名, 内容, 是否存储不压缩)）。
    fn build_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content, stored) in entries {
            let method = if *stored {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            let options = SimpleFileOptions::default().compression_method(method);
            writer.start_file(*name, options).expect("start_file");
            writer.write_all(content).expect("write entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    #[test]
    fn extracts_core_from_deflated_zip() {
        let zip = build_zip(&[
            ("aria2c.exe", b"fake-aria2c-binary", false),
            ("README.txt", b"readme", false),
        ]);
        let extracted = extract_core(&zip).expect("应能解出 aria2c.exe");
        assert_eq!(extracted.aria2c, b"fake-aria2c-binary");
        assert!(extracted.dlls.is_empty());
        assert!(extracted.total_bytes() > 0);
    }

    #[test]
    fn extracts_stored_zip_and_ignores_case() {
        let zip = build_zip(&[("ARIA2C.EXE", b"upper", true)]);
        let extracted = extract_core(&zip).expect("大小写不敏感");
        assert_eq!(extracted.aria2c, b"upper");
    }

    #[test]
    fn finds_core_in_subdirectory_by_basename() {
        let zip = build_zip(&[
            ("aria2-1.37.0/aria2c.exe", b"nested", false),
            ("aria2-1.37.0/doc/README.txt", b"x", false),
        ]);
        let extracted = extract_core(&zip).expect("子目录里的 exe 也要认");
        assert_eq!(extracted.aria2c, b"nested");
    }

    #[test]
    fn carries_sidecar_dlls() {
        let zip = build_zip(&[
            ("aria2c.exe", b"core", false),
            ("libssl-3-x64.dll", b"dll-bytes", false),
        ]);
        let extracted = extract_core(&zip).expect("应一并取出 DLL");
        assert_eq!(extracted.dlls.len(), 1);
        assert_eq!(extracted.dlls[0].0, "libssl-3-x64.dll");
        assert_eq!(extracted.dlls[0].1, b"dll-bytes");
    }

    #[test]
    fn rejects_missing_core() {
        let zip = build_zip(&[("README.txt", b"only readme", false)]);
        let error = extract_core(&zip).expect_err("缺 aria2c.exe 必须报错");
        assert!(matches!(error, CoreError::PackageInvalid { .. }), "{error}");
    }

    #[test]
    fn rejects_duplicate_core() {
        let zip = build_zip(&[
            ("aria2c.exe", b"one", false),
            ("sub/aria2c.exe", b"two", false),
        ]);
        let error = extract_core(&zip).expect_err("两个 aria2c.exe 必须报错");
        assert!(matches!(error, CoreError::PackageInvalid { .. }), "{error}");
    }

    #[test]
    fn rejects_non_zip_payload() {
        for payload in [&b"<html>404</html>"[..], b"", b"PK"] {
            let error = extract_core(payload).expect_err("非 zip 必须报错");
            assert!(matches!(error, CoreError::PackageInvalid { .. }), "{error}");
        }
    }

    #[test]
    fn skips_suspicious_traversal_names() {
        let zip = build_zip(&[
            ("../evil.dll", b"evil", false),
            ("aria2c.exe", b"core", false),
        ]);
        let extracted = extract_core(&zip).expect("核心仍应解出");
        assert_eq!(extracted.aria2c, b"core");
        assert!(extracted.dlls.is_empty(), "带 .. 的条目必须跳过");
        assert!(extracted.skipped.iter().any(|name| name.contains("..")));
    }
}
