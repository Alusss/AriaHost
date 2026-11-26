# 第三方组件与许可（THIRD-PARTY NOTICES）

本文件随 AriaHost 发布，并在安装包里落到 `<安装目录>\THIRD-PARTY-NOTICES.md`（与 `licenses\aria2-COPYING.txt` 同目录）。
它只在**安装包内自带 `aria2c.exe`** 时随包安装（与 `aria2c.exe` 同进同退），因此不会出现「没有二进制却有一份声明」的组合。

---

## 1. aria2（`aria2c.exe`）

| 项 | 值 |
|---|---|
| 组件 | aria2 |
| 版本 | **1.37.0** |
| 上游项目 | <https://github.com/aria2/aria2> |
| 上游发布页 | <https://github.com/aria2/aria2/releases/tag/release-1.37.0> |
| 本次分发的资产 | `aria2-1.37.0-win-64bit-build1.zip`（上游官方 Windows 64-bit build1） |
| zip SHA-256 | `67d015301eef0b612191212d564c5bb0a14b5b9c4796b76454276a4d28d9b288` |
| `aria2c.exe` SHA-256 | `be2099c214f63a3cb4954b09a0becd6e2e34660b886d4c898d260febfe9d70c2` |
| `aria2c.exe` 字节数 | 5 649 408 |
| 许可 | **GNU General Public License v2.0**（GPLv2） |
| 许可全文位置 | 安装目录内 `licenses\aria2-COPYING.txt`（取自上述 zip 内的 `COPYING`，未删节、未改写） |
| 修改情况 | **未修改**。AriaHost 原样分发上游构建产物，不打补丁、不重打包、不嵌入资源 |
| 使用方式 | 以**独立进程**调用（`aria2c.exe --conf-path=...`），AriaHost 自身不与 aria2 静态或动态链接 |

### 对应源码的获取方式（GPLv2 §3 要求）

- 上游为每个 release 同时提供源码包：`aria2-1.37.0.tar.xz`，见上面的发布页；
- 也可从上游仓库检出对应 tag：`git clone https://github.com/aria2/aria2 && git checkout release-1.37.0`；
- 本安装包分发的二进制即为该版本的上游构建产物（SHA-256 见上表），因此**上游同版本源码即为对应源码**。

### 为什么不是「链接」也仍要附许可

AriaHost 与 aria2 是两个独立进程，通过命令行与 JSON-RPC 交互，不构成衍生作品；但**分发二进制本身**就触发 GPLv2 的分发义务——必须让接收者拿到许可全文与源码获取途径。上面两条已覆盖，安装目录里的 `licenses\aria2-COPYING.txt` 就是履行该义务的载体。

---

## 2. Inter（字体，SIL Open Font License 1.1）

`installer\fonts\Inter-Regular.ttf` 与 `installer\fonts\Inter-SemiBold.ttf` 随安装包分发，安装到 `<安装目录>\fonts\`（见 `installer/ariahost.nsi`），由面板启动时读入并注册为比例族（`FontPriority::Highest`），用于拉丁字母与数字。

| 项 | 值 |
|---|---|
| 上游 | Inter（Rasmus Andersson），https://github.com/rsms/inter |
| 取件来源 | npm 包 `inter-font@3.19.0` 的 `ttf/Inter-Regular.ttf`、`ttf/Inter-SemiBold.ttf`（与上游 release 同一字体文件） |
| 下载地址 | `https://cdn.jsdelivr.net/npm/inter-font@3.19.0/ttf/Inter-Regular.ttf` / `.../Inter-SemiBold.ttf` |
| 许可 | SIL Open Font License 1.1（允许嵌入与再分发；不得单独售卖字体本身） |
| 许可全文 | 仓库 `installer/fonts/OFL.txt`，安装后为 `<安装目录>\licenses\OFL.txt`（卸载时随 `licenses\` 整目录删除） |
| SHA-256 | Regular `41AB0F707A2BFAB8133CCDFCDAB52282F5F79E5751F43A264805451C7BB95FB8`（309,828 B）<br>SemiBold `E8CBC2B88BC4268237FF5E251776D3C54EDCB14E015A9E66E4883BDE4B55F13F`（315,756 B） |

复核命令：`Get-FileHash installer\fonts\Inter-*.ttf -Algorithm SHA256`（应与上表逐字一致；不一致就是取件被换过，不许合入）。

---

## 3. Rust 依赖

AriaHost 自身以 `MIT OR Apache-2.0` 双许可发布。编译进 `ariahost.exe` 的第三方 crate 全部为宽松许可（MIT / Apache-2.0 / BSD / ISC / Zlib / MPL-2.0 等）或同等宽松的双许可，**不含 GPL/AGPL 系列代码**。

- 完整依赖树与锁定版本见仓库根 `Cargo.lock`（`cargo tree` 可复现）；
- 需要逐 crate 的许可清单时，在仓库内执行 `cargo metadata` / `cargo deny list` 获得权威结果——本文件不复制那份可自动生成的长表，避免与锁文件脱节。

若后续引入任何 copyleft 依赖，必须在合并前更新本文件，并同步 `docs/dev/release.md` §7 的分发口径。
