# 参与贡献

感谢关注 AriaHost。项目处于 M0（设计定版、尚无代码），现在最有价值的贡献是**指出设计文档中的矛盾与不可行处**，而不是提代码补丁。

## 1. 现状与可预期节奏

| 阶段 | 你可以贡献什么 |
|---|---|
| M0（当前） | 文档纠错、需求/验收标准质疑、架构风险指出 |
| M1–M2 | Rust 代码（`crates/`）、egui 界面、测试 |
| M3 起 | 安装器、真机验收、发版验证 |

## 2. 提 Issue

- 先搜已有 issue，避免重复。
- 模板要素：**现象**（实际发生什么）、**期望**（应该怎样）、**复现步骤**（含版本号、Windows 版本、安装方式）、**日志**（`%APPDATA%\AriaHost\logs\`，注意脱敏）。
- 提交前删掉：RPC secret、带 token 的下载链接、个人路径（`C:\Users\<你的名字>`）。
- 安全漏洞**不要**开公开 issue，见 [SECURITY 说明](#6-安全问题)。

## 3. 提 PR

```powershell
git switch -c feat/short-slug        # 或 fix/…、docs/…
# 改代码 / 改文档
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git commit -m "feat(aria2): 增加版本切换失败回滚"
git push -u origin feat/short-slug
```

要求：

1. **CI 的 `ci-gate` 必须绿**（`fmt`/`clippy`/`test`/`build` 全过）。M1 之前 `ci-gate` 会因缺少 `Cargo.toml` 而红，这期间以文档评审通过为准。
2. 一个 PR 一件事；顺手重构请单独开 PR。
3. 描述里写清：改了什么、为什么这么改、怎么验证的（命令 + 结果）、有没有影响安装/升级/回滚路径。
4. 改了 CLI 参数、错误码、产物路径、目录结构 → **同一个 PR 内**同步 `docs/dev/` 与 `installer/ariahost.nsi`。
5. 新增依赖要在 PR 里用一句话说明理由。
6. **每个完整的变化都要及时提交并推送**（项目规范）：做完一件完整的事就 `commit` + `push`，不要攒成一堆未提交的改动。半成品留在本地分支上，不要推 `main`；推到 `main` 的必须是「自洽、能讲清、可回退」的一步。

## 4. 提交信息

Conventional Commits 风格，中文正文：

```text
<type>(<scope>): <一句话概括>

<可选：为什么这么改、有什么副作用>
```

- `type`：`feat` / `fix` / `docs` / `refactor` / `test` / `build` / `ci` / `chore` / `perf`
- `scope`：`aria2` / `ui` / `svc` / `web` / `common` / `installer` / `ci` / `docs`
- 不要在提交信息里写「已验证构建通过」之类的未经验证的断言。

## 5. 不要提交这些

`.gitignore` 已覆盖，仍请自查：`target/`、`dist/`、`*.pdb`、`installer/*.exe`、`.vscode/`、`.idea/`、`*.pfx`、任何含 secret/token 的配置。用户配置（`%APPDATA%\AriaHost\config.toml`）永远不进仓库。

## 6. 安全问题

RPC secret、证书、签名密钥相关的问题请走私下渠道（仓库 Security 页的 private advisory），不要开公开 issue，也不要在 PR 里贴真实凭据。

## 7. 许可证状态（重要）

仓库目前**没有选定开源许可证**（`LICENSE` 是占位说明，见该文件）。在许可证确定之前：

- 提交 PR 即表示你同意你的贡献可以随本项目最终选定的许可证一起分发。
- 无法接受这一点的贡献请暂缓提交，等许可证落定。

许可证选择由项目所有者决定，PR 里请勿自行添加或替换 `LICENSE` 内容。

## 8. 语言

代码注释、文档、提交信息、界面文案统一用中文；标识符（变量/函数/类型/配置键）用英文。详见 [docs/dev/style.md](docs/dev/style.md)。
