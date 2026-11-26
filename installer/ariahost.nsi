; ============================================================================
;  AriaHost 安装脚本（NSIS 3.x / Unicode）
;
;  编译（**只在 Windows 上编译**；CI 与本地一致）：
;      makensis /DVERSION=0.1.0 /DVI_VERSION=0.1.0.0 /DARCH=x64 ^
;               /DSRCDIR=<repo>\dist\x64 /DOUTFILE=<repo>\installer\AriaHost-0.1.0-x64-setup.exe ^
;               installer\ariahost.nsi
;  产物（per-user，首版唯一发布形态）：installer\AriaHost-<ver>-x64-setup.exe
;
;  ---- 编译期路径规则（改动前必读；2026-11-25 实测结论）---------------------
;  Windows 版 makensis 的 `File` **只接受全反斜杠的绝对路径**。实测探针
;  （windows-latest，run 45452855606）：
;    File "D:\a\w\dist\x64\ariahost.exe"   → ✅ 出包
;    File "D:/a/w/dist/x64/ariahost.exe"   → ❌ File: ... -> no files found
;    File "dist\x64\ariahost.exe"（相对）   → ❌ 找不到（相对路径按**脚本所在目录**解析）
;  因此：
;    * **编译期路径（File、!if /FileExists）一律用 "\"，且优先用绝对路径**（来自 ${SRCDIR}）。
;    * 早期版本写的「用 / 分隔以便 Linux 也能编」是**已被证伪的错误结论**：Linux 版 makensis
;      接受 "/"，Windows 版不接受 → 造成「Linux stub 编译绿、Windows 真实打包红」的假绿。
;      本脚本现在只在 Windows 上编译（ci.yml 的 windows job、release.yml 的 build job），
;      ubuntu 的 nsi-syntax stub job 已于 2026-11-25 删除。
;    * 运行期字符串（$INSTDIR\…、$APPDATA\…）保持 "\"，它们只在 Windows 上执行。
;
;  !! 验证状态：**已在 Windows 上真实编译通过**（windows-latest + 预装 NSIS 3.x，
;     见 ci.yml 的 windows job「安装脚本编译门禁」与 release.yml 的 build job；
;     Release run 45454312274 用它出过包）。
;     已验证：语法/宏/MUI2 页面、Section ID 前向引用、UTF-8 无 BOM + /INPUTCHARSET UTF8
;     的中文文案、${SRCDIR} 载荷路径、编译通过即可出 setup.exe。
;     **未验证（M3 批次全部在此列）**：真机安装/卸载、UAC 提权路径（runas 的
;     回执文件能否读到）、服务注册/注销的真实结果、AUMID 是否真的让 Toast 显示、
;     /S 静默安装、per-user 与 /DPER_MACHINE 的运行时差异。
;     人工自检清单见 docs/dev/release.md §5。
;
;  ---- M3 批次（2026-11-25）改了什么 --------------------------------------
;     A. 组件页新增可选「同时注册为 Windows 服务」：默认不勾；勾选后 per-user 包
;        用 runas 调 --install-service（单次 UAC），**成败以 SCM 为准**
;        （`sc query AriaHost` 查到即成功；%TEMP% 回执只用来区分"取消 UAC"与
;        "确定失败"，两者都没有时只能说"未确认"），失败只提示不中止安装。
;     B. 每个快捷方式建完都写 AUMID（!macro WriteAumidFor），失败只记日志。
;     C. 卸载顺序固定为：注销服务 → 停 ariahost（等 2s 收 aria2c）→ 删快捷方式/
;        注册表 → 删 $INSTDIR → 用户数据默认保留（问过才删）。
;        残留的 aria2c 只在其映像路径位于 AriaHost 数据目录下时才 taskkill，
;        否则只告警（可能是用户自己的 aria2 实例，杀了是数据损失）。
;     D. 升级停进程改成「--quit + 每 250ms 真实复查，墙钟上限 20s」，不再 Sleep 假同步。
;     另：删除 A6 的 HidePerUserServiceSection（per-user 现在可以注册服务）。
;
;     历史修复（2026-11-23 独立审计 .scratch/wasm-design/review-exec-v2.md，静态）：
;     ① ${SEC_SERVICE} 曾在 .onInit（Section 之前）被引用；② 卸载段曾 Call 无 un.
;     前缀的普通函数；③ WinVer 下限从 ${AtLeastWin10}（10.0.10240）改为
;     ${AtLeastBuild} 17763（Server 2016 必须拒装，AH-E1001）。
;     2026-11-25 实测修复：File 的正斜杠路径在 Windows makensis 下必然失败 → 改回 "\"
;     （首发 tag v0.1.0 的 release run 45451494558 就是死在这里）。
;
;  ---- 两种编译变体（架构 §4.10）------------------------------------------
;  默认 per-user：RequestExecutionLevel user、装到 %LOCALAPPDATA%\Programs\AriaHost、
;               全程免 UAC；服务组件在组件页**隐藏**（R9：per-user 建不了服务）。
;  加 /DPER_MACHINE：admin、装到 %PROGRAMFILES64%\AriaHost、HKLM，
;               服务组件可勾选并直接注册（已是管理员，不再额外 UAC）。
;               首版不发布此变体，M4 进 release matrix；产物名带 -machine- 后缀。
;
;  ---- CLI 契约（架构 §7.1 R9，由 M1 的 ariahost-cli 实现）----------------
;    ariahost.exe --install-service          一次 UAC 提权助手：建 ProgramData、
;                                            复制版本库、CreateService（幂等）
;    ariahost.exe --uninstall-service        一次 UAC：StopService→DeleteService→清 ProgramData
;    ariahost.exe --write-aumid <lnk>        自写 IPropertyStore 写 System.AppUserModel.ID
;                                            （AUMID 值 AriaHost.Desktop，实现在 exe 内）
;    ariahost.exe --quit                     通知已运行实例退出（停止链内完成）
;  本脚本假设的 --quit 退出码（M3 定稿；实测见 docs/dev/release.md §11）：
;    0=目标实例已退出 **或** 本来就没有运行中的实例（安装器/升级流程视为成功）
;    1=管道在但请求被拒 / 2=握手或等退出超时（其余非 0 同样按失败处理）
;
;  注意：本脚本**不弹** aria2 来源页（v2.0 决定）。**自 task-54（M3-j）起发布包随包分发
;  aria2c.exe**（在线双线路在真实用户网络不可用）：release.yml 下载官方 zip、硬校验
;  SHA-256、把 aria2c.exe 放进 dist\<arch>\；本脚本见 ${SRCDIR}\aria2c.exe 即一并安装，
;  并把 GPLv2 许可文本与 THIRD-PARTY-NOTICES.md 装进 $INSTDIR\licenses\（同进同退）。
;  **默认必带核心（fail-closed）**：${SRCDIR}\aria2c.exe 缺失即编译失败；确实要出
;  无核心包必须显式 /DNO_BUNDLED_ARIA2=1。口径见 docs/dev/release.md §7。
;
;  图标管线（2026-11-25 落地）：installer\ariahost.ico 是 7 尺寸
;  （16/24/32/48/64/128/256）、32bpp、**非 PNG 压缩**（BMP 条目）的 ICO，由
;  assets/branding/ariahost-icon-256.png 经 Pillow 生成（见 docs/dev/build.md）。编译期一律以
;  ${SRCDIR}\ariahost.ico 引用（CI 会先把 ico 拷进 SRCDIR）；运行期落盘到 $INSTDIR，
;  开始菜单/桌面快捷方式与 setup.exe 自身图标都指向它。
; ============================================================================

Unicode true
ManifestDPIAware true
ManifestSupportedOS all
SetCompressor /SOLID lzma
SetCompressorDictSize 32

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
; Sections.nsh 提供 SectionSetText / SectionSetFlags 等（M3 起本脚本已不再隐藏
; 任何组件，per-user 也能注册服务，故这里**没有** SectionSetText 调用；include
; 保留是因为后续若要做「组件置灰」仍会用到，且该文件有 include guard、成本为 0）。
!include "Sections.nsh"

; ---- 版本注入（全部由 makensis /D 传入，脚本内不硬编码版本）---------------
!ifndef VERSION
  !define VERSION "0.0.0-dev"
!endif
!ifndef VI_VERSION
  !define VI_VERSION "0.0.0.0"
!endif
!ifndef ARCH
  !define ARCH "x64"
!endif
!ifndef SRCDIR
  ; 默认值仅供本地手工编译：相对路径按**本脚本所在目录**解析，故用 "..\dist\x64"。
  ; CI 一律用 /DSRCDIR 传绝对路径（见文件头「编译期路径规则」）。
  !define SRCDIR "..\dist\x64"
!endif

; ---- 产品常量（改名/改路径只需改这一段）----------------------------------
!define APP_NAME       "AriaHost"
!define APP_EXE        "ariahost.exe"
!define APP_PUBLISHER  "AriaHost"
!define SVC_NAME       "AriaHost"
!define REG_APP_KEY    "Software\AriaHost"
!define REG_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AriaHost"
!define SVC_REG_KEY    "SYSTEM\CurrentControlSet\Services\${SVC_NAME}"

!ifdef PER_MACHINE
  RequestExecutionLevel admin
  !define INSTALL_ROOT "$PROGRAMFILES64"          ; x86 全机变体（M4）应改 $PROGRAMFILES32
  !define REG_ROOT     HKLM
!else
  RequestExecutionLevel user
  !define INSTALL_ROOT "$LOCALAPPDATA\Programs"
  !define REG_ROOT     HKCU
!endif

; A7：两个变体同为 x64 时不能同名，否则发布的包互相覆盖（架构 §3.3 安装包命名）
!ifndef OUTFILE
  !ifdef PER_MACHINE
    !define OUTFILE "AriaHost-${VERSION}-${ARCH}-machine-setup.exe"
  !else
    !define OUTFILE "AriaHost-${VERSION}-${ARCH}-setup.exe"
  !endif
!endif

Name "${APP_NAME} ${VERSION}"
OutFile "${OUTFILE}"
; 默认目录只给编译期默认值；**不要**再加回 `InstallDirRegKey`。
; 原因：NSIS 会在 .onInit 之前就把注册表值塞进 $INSTDIR，等于零校验地把它当成默认目录；
; 而那个键完全可能是调试期或失败安装留下的残留（v0.1.2 真机踩过，见 .onInit 的 D1 段）。
InstallDir "${INSTALL_ROOT}\${APP_NAME}"
ShowInstDetails show
ShowUninstDetails show
BrandingText "${APP_NAME} ${VERSION} (${ARCH})"

; 版本信息资源块。
; - LegalCopyright 已按 MIT OR Apache-2.0 双许可填写（架构 §13 S1 已定：2026-11）。
; - 首版安装包**不含许可页 / EULA 页**：双许可不需要用户先接受条款，属 Q13/S3 决定，
;   勿自行新增 MUI_LICENSEPAGE（加了会改变安装流程与验收 #1 口径）。
; - 版权行只放所有者署名，**不**把 aria2 的 GPLv2 声明拼进来：本产品的许可是
;   MIT OR Apache-2.0，aria2 是**并列的第三方组件**（自带时随附其 GPL-2.0 全文与
;   THIRD-PARTY-NOTICES.md），两者的许可声明必须分开写，混在一起会让人误以为
;   本产品整体是 GPL。口径见 release.md §7。
VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName"     "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "CompanyName"     "${APP_PUBLISHER}"
VIAddVersionKey "LegalCopyright"  "Copyright (c) 2026 Alusss"

; ---- 界面 -----------------------------------------------------------------
!define MUI_ABORTWARNING

; 安装器/卸载器图标：与 ariahost.exe 用的是**同一份**多尺寸 ICO
; （16/24/32/48/64/128/256，32bpp，由 assets/branding/ariahost-icon-256.png 生成）。
; 路径走 ${SRCDIR}（编译期绝对路径，CI 会先把 installer\ariahost.ico 拷进 SRCDIR）：
; 与下面 File 的载荷路径同一套规则，见文件头「编译期路径规则」。
!define MUI_ICON   "${SRCDIR}\ariahost.ico"
!define MUI_UNICON "${SRCDIR}\ariahost.ico"

!define MUI_WELCOMEPAGE_TITLE "${APP_NAME} 安装向导"
!define MUI_WELCOMEPAGE_TEXT "将安装 ${APP_NAME} ${VERSION}（${ARCH}）。$\r$\n$\r$\n${APP_NAME} 是 aria2 的 Windows 服务管理器：只通过子进程与 RPC 调度 aria2，不修改其源码、不链接其二进制。$\r$\n$\r$\n下一步可确认安装目录与可选组件。"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES

!ifndef PER_MACHINE
  !define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
  !define MUI_FINISHPAGE_RUN_TEXT "立即启动 ${APP_NAME}"
!endif

; Finish 页补一句第三方组件说明（task-54 B4）。
; 为什么放 Finish 页而不是新增 MUI_LICENSEPAGE：许可页会改变安装流程与验收 #1 口径
; （见上面 Q13/S3 的说明），而这里只是**告知**，不构成接受条款。
; 两个分支必须都说实话：带核心的包说「含 aria2c 与许可见安装目录」；用
; /DNO_BUNDLED_ARIA2=1 编出来的包必须明说「没带核心、首次启动要联网下」——否则
; 用户装完看到「找不到 aria2c」会以为是坏包（那正是 L6 事故的用户视角）。
!ifdef NO_BUNDLED_ARIA2
  !define MUI_FINISHPAGE_TEXT "本安装包**未内置** aria2 核心：首次启动需要联网下载 aria2c，或在设置里指定本地的 aria2c.exe。$\r$\n$\r$\n${APP_NAME} 仅通过子进程与 RPC 调度 aria2，不修改其源码、不链接其二进制。"
!else
  !define MUI_FINISHPAGE_TEXT "本安装包内置第三方组件 aria2c 1.37.0（GPL-2.0）：许可全文与来源说明见安装目录下的 THIRD-PARTY-NOTICES.md 与 licenses\aria2-COPYING.txt。$\r$\n$\r$\n${APP_NAME} 仅通过子进程与 RPC 调度 aria2，不修改其源码、不链接其二进制。"
!endif
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; setup.exe / uninst.exe 自身的图标。放在所有 MUI 宏**之后**：NSIS 的 Icon 是「最后一次生效」，
; 而 MUI2 会按 MUI_ICON 自己发一次 Icon；两处指向同一文件，这里放最后只是为了让结果确定。
Icon          "${SRCDIR}\ariahost.ico"
UninstallIcon "${SRCDIR}\ariahost.ico"

; ---- 通用：进程探测（tasklist，不依赖第三方插件）---------------------------
; 为什么用 tasklist 而不是 FindWindow：托盘实例没有可靠窗口类名/标题，
; 而"是否在跑"这件事只需要映像名。tasklist 找不到进程时**退出码仍为 0**，
; 所以只能看输出文本：/FO TABLE /NH 命中时首行就是 "<image>  <pid> ..."。
;
; **2026-11-25 修（v0.1.1 真机事故的根因）：非控制台上下文里 tasklist 输出以 CRLF 开头。**
; 实测前 16 字节 = 0D 0A 61 72 69 61 68 6F 73 74 2E 65 78 65 20 20，即
; "\r\nariahost.exe  "。修之前这里直接 `StrCpy $R4 $R5 ${LEN}`，得到
; "\r\nariahost.e"，**永不等于**映像名 → 探测恒判"没在跑" → 安装/覆盖/卸载
; 全都跳过关闭步骤：安装不 --quit 直接 File 覆盖被占用的 exe；卸载/升级
; 既不 --quit 也不复查 aria2c，删文件失败还当成功。
; 修法：先 TrimEol 再取前 LEN 个字符比较；**不做列解析**（列宽与本地化都会变）。
;
; 入参：!macro 参数 IMAGE（映像名）、LEN（映像名字符数，用于截取比较）
;       **传参不要带引号**（引号一律写在宏体内），避免"引号是否算参数一部分"的歧义：
;       调用写 `!insertmacro ProcessIsRunning ariahost.exe 12`。
; 出参：$R7 = "1" 在跑 / "0" 不在跑；$R4 首段文本、$R5 原始输出、$R6 退出码
; 约束：宏体不写 label（宏会被多处置入）；除上述出参外只借用 $R2（TrimEol 的临时字符）。
!macro ProcessIsRunning IMAGE LEN
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ${IMAGE}" /FO TABLE /NH'
  Pop $R6
  Pop $R5
  !insertmacro TrimEol $R5
  StrCpy $R4 $R5 ${LEN}
  ${If} $R4 == "${IMAGE}"
    StrCpy $R7 1
  ${Else}
    StrCpy $R7 0
  ${EndIf}
!macroend

; ---- 可选：运行期取证 TRACE（编译期 /DTRACE=<绝对路径>）-------------------
; 为什么需要：/S 静默安装看不到 DetailPrint，而"到底有没有探测到实例、--quit
; 返回几、复查了几轮、最后为什么没删掉"只有安装器自己知道。这是安装器唯一
; 能自证黑盒行为的通道（task-55 的验收要求）。
; 规则（刻意做到最小、可审计）：
;   · **只写不读、不改任何判定**：任何分支都不以 TRACE 是否成功为条件；
;   · 不给 /DTRACE 时宏体为空（无文件、无变量、零开销），CI 只编译不运行；
;   · 路径写死在编译期，**不读环境变量**（安装器/助手可能被提权到别的账户）。
; 用法：makensis /DTRACE="F:\tmp\ariahost-trace.txt" ...
; 取证人：$EXEDIR（当前 exe 所在目录——卸载器通常先自我复制到 %TEMP% 再运行，
;         这一项直接回答"卸载器到底从哪儿跑的"）、$TEMP、$INSTDIR。
; 实现细节（2026-11-25 实测修）：① 追加前显式 `FileSeek 0 END`——同一进程里
; 多次 FileOpen/FileClose 交替时，仅靠 "a" 模式的指针语义曾出现**两次写入互相
; 覆盖**（真机跑出「后一行盖掉前一行前缀」的残缺文件）；② 每行写完立即
; FileClose，不留悬挂句柄；③ 路径必须用**反斜杠**形态（正斜杠形态 FileOpen
; 静默失败，一个字节都不会落盘——实测 F:/… 与 F:\… 的对照）。
!ifdef TRACE
  !macro TraceWrite TEXT
    FileOpen $9 "${TRACE}" a
    FileSeek $9 0 END
    FileWrite $9 "${TEXT}$\r$\n"
    FileClose $9
  !macroend
!else
  !macro TraceWrite TEXT
  !macroend
!endif

; task-61 取证：TRACE 构建下额外记录「整个停进程步」的墙钟（含 --quit 阻塞的那几秒）。
; 为什么需要：`$R1` 只夹住 `--quit` **之后**的复查等待（面板正常退出时它就是 0），
; 看不出用户实际等多久；而验收要的正是「面板在下载 → 覆盖升级」这一档的端到端读数。
; 实现约束：只新增一个具名变量（不占用 $R0–$R9/$0–$9 已有的借用约定），并且整段
; 包在 `!ifdef TRACE` 里——不给 /DTRACE 时既不声明变量，也不多一次系统调用。
!ifdef TRACE
  Var StopStepStartTick
!endif

; ---- 通用：去掉字符串首尾的 CR/LF/空格 ------------------------------------
; cmd 的 echo、sc/tasklist/wmic 的输出都带行尾换行（wmic 还会在前面空一行），
; 比较前必须先裁掉。首部用 `StrCpy … 1024 1` 逐字符前移（NSIS 不支持"取剩余全部"）。
; 入参：!macro 参数 VAR = **变量名**（例如 `$R7`，文本替换，不是值）
; 副作用：使用 $R2 作临时字符（调用方不要同时依赖 $R2）
!macro TrimEol VAR
  ; 尾部
  ${Do}
    StrCpy $R2 ${VAR} 1 -1
    ${If} $R2 == "$\r"
    ${OrIf} $R2 == "$\n"
    ${OrIf} $R2 == " "
      StrCpy ${VAR} ${VAR} -1
    ${Else}
      ${ExitDo}
    ${EndIf}
  ${Loop}
  ; 首部
  ${Do}
    StrCpy $R2 ${VAR} 1
    ${If} $R2 == "$\r"
    ${OrIf} $R2 == "$\n"
    ${OrIf} $R2 == " "
      StrCpy ${VAR} ${VAR} 1024 1
    ${Else}
      ${ExitDo}
    ${EndIf}
  ${Loop}
!macroend

; ---- 通用：停掉正在运行的实例（不 Sleep 假同步）---------------------------
; MODE = "install"：安装/覆盖前调用。检测到在跑 → 提示 → --quit → **每 250ms
;        真实复查、墙钟上限 20s**；仍活着才 Abort（否则 File 必然失败）。
; MODE = "uninstall"：卸载时调用。不提示、不 Abort；停完等 2s 让 Job Object
;        收掉 aria2c 子进程，再复查 aria2c 是否残留。
;
; 为什么必须是「复查」而不是「Sleep 若干秒后当它已退出」：run 45451494558 与
; 早期 A4 实现都吃过这个亏——睡眠时长与真实退出时间无关，睡完照样占用文件。
; 这里的 Sleep 只用作两次探测之间的间隔（250ms），判定依据始终是 tasklist 结果。
;
; 为什么等待预算是 20s（task-61 从 15s 抬上来；再往前是 40×250ms=10s）：
; 三个预算必须形成 **CLI < 安装器 < 面板** 的阶梯，否则最内层还没走完，外层先
; 判失败——用户看到的是「安装器说失败、其实装成了」这种最难解释的误判。
;
;   | 谁 | 预算 | 出处 |
;   |----|------|------|
;   | `--quit` 客户端 | 10s | cli.rs 的 QUIT_TIMEOUT（握手 + 等面板退出） |
;   | 安装器等待 | **20s** | 本文件 STOP_APP_BUDGET_MS |
;   | 面板退出档 | 30s | ariahost-ui 的 QUIT_TIMEOUT（ctl 线程自己的上限） |
;
; 为什么 15s 不够（实测依据，不是推算）：`4a` 场景（**下载进行中**、面板不即时
; 信令）最坏一档实测到 **~19s** —— 2s 等 UI 线程消费 quit 请求 + 15s 优雅停止
; （saveSession → aria2.shutdown）+ 2s 宽限（见 ariahost-ui/ctl.rs 与 app.rs 的
; 退出档）。15s 预算会比面板真实退出**早到 2s**：安装器弹出「仍在运行」并 Abort，
; 而面板 2s 后就退了——差 2 秒的误判。20s 覆盖 19s 最坏档，且仍在面板 30s 之内，
; 阶梯不倒挂。
;
; 代价（知情接受）：所有「面板真的退不掉」的失败路径，静默等待从 15s 变成 20s。
; 这是把「假失败」换成「晚 5 秒报真失败」，方向对；若将来要缩，必须同时缩 CLI 与
; 面板侧的预算，别只动这一个数（否则阶梯又倒挂）。
;
; **2026-11-25 实测修：预算按"墙钟"算，不按轮次算。** 每轮都要 spawn 一次
; tasklist（非控制台上下文一次约 100–150ms），所以「N 轮 × 250ms」必然低估真实
; 等待时长，且随机器快慢漂移——静默安装的失败反馈时间会被无声放大。改法：用
; kernel32!GetTickCount 夹住墙钟，轮次只作为取证计数。
;
; 覆盖面核算（task-61 复核，Sleep 与 GetTickCount 是否仍够 20s）：墙钟差值
; `$R1 = tick_now - tick_start`，与轮次**完全无关**，所以上限必然被覆盖；每轮
; ≈ tasklist(100–150ms) + Sleep 250ms ≈ **350–400ms**，20s 预算约 **50–57 轮**，
; 轮次远未到任何计数器上限（IntOp 是 64 位）。判定在每轮末尾采样，故实际中止
; 时刻是 20.0s + 末轮残留时间（≤ ~400ms），不会提前。
; 已知边界（不改数，仅登记）：GetTickCount 是 32 位无符号、按有符号读会在开机
; 24.8 天后变成负数；但两次采样**同时**平移，差值仍正确——只有「这 20s 恰好跨过
; 那一个翻转瞬间」才会失效（概率 ~1e-5，且 15s 时代就已存在，非本批引入）。
;
; 历史实测（15s 时代留档，用来证明墙钟夹子是有效的）：面板始终不让出时
; 15204ms 触发中止、39 轮、退出码 2。
;
; 入参：$INSTDIR 必须已就位（--quit 要用 $INSTDIR\ariahost.exe）
; 出参：$R7 最后一次探测结果（"1"：还在跑）；$R9 实际轮次；$R1 实际等待毫秒
; 借用：$R0（起始 tick）、$R1（已等毫秒）——调用方不要依赖这两个寄存器
; 复用：安装器（Function StopAppForInstall）、卸载器（Function un.StopAppForUninstall）
!define STOP_APP_BUDGET_MS 20000
!macro StopAppBody MODE
  !insertmacro TraceWrite "[${MODE}] StopAppBody 进入；exedir=$EXEDIR temp=$TEMP instdir=$INSTDIR"
  !ifdef TRACE
    System::Call 'kernel32::GetTickCount()i .R2'
    StrCpy $StopStepStartTick $R2
  !endif
  !insertmacro ProcessIsRunning ${APP_EXE} 12
  !insertmacro TraceWrite "[${MODE}] 探测 ${APP_EXE} 结果=$R7"
  ${If} $R7 == "0"
    DetailPrint "没有检测到运行中的 ${APP_EXE}，跳过关闭步骤。"
  ${Else}
    !if "${MODE}" == "install"
      MessageBox MB_OK|MB_ICONINFORMATION "${APP_NAME} 正在运行，将先关闭它再继续安装。$\r$\n$\r$\n托盘里的实例也会一起关闭；退出前会保存下载会话，已下载的文件不受影响。" /SD IDOK
    !endif
    DetailPrint "通知 ${APP_NAME} 优雅退出：$INSTDIR\${APP_EXE} --quit"
    ${If} ${FileExists} "$INSTDIR\${APP_EXE}"
      nsExec::ExecToStack '"$INSTDIR\${APP_EXE}" --quit'
      Pop $R8   ; 退出码（契约：0=accepted 或 no-instance；1=refused（对端不认识 --quit，
                ; 通常是版本不匹配）；2=timeout（连上了但等退出超时）；失败也可能是
                ; error/timeout 字符串。判据只用 tasklist 复查，本退出码只进日志）
      Pop $R3   ; 标准输出
      DetailPrint "--quit 返回：$R8"
      !insertmacro TraceWrite "[${MODE}] 已调用 $INSTDIR\${APP_EXE} --quit，退出码=$R8 输出=$R3"
    ${Else}
      ; 进程在跑但当前安装目录没有 exe（例如从别处跑的便携版）：无法代为退出，
      ; 直接给出可执行提示，不要空转整个等待预算。
      !if "${MODE}" == "install"
        !insertmacro TraceWrite "[${MODE}] 进程在跑但 $INSTDIR\${APP_EXE} 不存在，Abort"
        MessageBox MB_OK|MB_ICONSTOP "检测到 ${APP_EXE} 正在运行，但它不在本次安装目录 $INSTDIR 下，安装程序无法代为关闭。$\r$\n$\r$\n请先从托盘菜单「退出」${APP_NAME}（或在任务管理器结束 ${APP_EXE}），然后重新运行安装程序。" /SD IDOK
        Abort
      !else
        DetailPrint "警告：$INSTDIR\${APP_EXE} 不存在，且仍有 ${APP_EXE} 在运行（可能来自其他目录）；后续删除可能部分失败。"
      !endif
    ${EndIf}
    ; 真实等待：**墙钟上限 20s**（`STOP_APP_BUDGET_MS`），每轮都以 tasklist 结果判定。
    ; 停机判定只认 tasklist；Sleep 只是两轮之间的间隔，不参与判定。
    StrCpy $R9 0
    ; $R1 预置 0：首轮就判定「已退出」时循环体从不执行，若不预置 TRACE 里会是空串。
    ; 报 0 是诚实的（首轮探测即已退出，墙钟耗时可忽略），不额外采样以免掩盖真实毫秒。
    StrCpy $R1 0
    ; **大小写是语义**：System 插件里 `.r0` 写的是**用户变量 $0**，`.R0` 才是**寄存器 $R0**。
    ; 2026-11-25 实测：写成 `.r0` 而读 `$R0` → 差值恒为 0，预算墙钟永不触发，
    ; 等待循环只能靠进程自己退出（实测跑到第 226 轮）。探针对照：`.R1-.R0` 得 1203ms ✓。
    System::Call 'kernel32::GetTickCount()i .R0'
    ${Do}
      !insertmacro ProcessIsRunning ${APP_EXE} 12
      ${If} $R7 == "0"
        !insertmacro TraceWrite "[${MODE}] 复查第 $R9 轮：已退出"
        ${ExitDo}
      ${EndIf}
      Sleep 250
      IntOp $R9 $R9 + 1
      System::Call 'kernel32::GetTickCount()i .R1'
      IntOp $R1 $R1 - $R0
      ${If} $R1 >= ${STOP_APP_BUDGET_MS}
        ${ExitDo}
      ${EndIf}
    ${Loop}
    !ifdef TRACE
      ; 本步合计 = `--quit` 阻塞时长 + `$R1`（--quit 之后才开始的复查等待）。
      ; 分母固定的那个数才是用户体感；`$R1` 单独看会是 0，容易误读成"没等"。
      System::Call 'kernel32::GetTickCount()i .R2'
      IntOp $R2 $R2 - $StopStepStartTick
      !insertmacro TraceWrite "[${MODE}] 墙钟（本步合计：--quit + 复查）=$R2 ms；复查段=$R1 ms；轮次=$R9"
    !endif
    ${If} $R7 == "1"
      !insertmacro TraceWrite "[${MODE}] 等待结束：仍在运行（轮次=$R9，实测=$R1 ms）"
      !if "${MODE}" == "install"
        ; 这里**不** taskkill：task-64 的边界明确禁止（强杀会跳过面板退出流程里的保存会话/停 aria2c，
        ; 等于让安装器替面板做它自己的责任）。20s 还没退 = 面板卡死在退出流程，唯一诚实的做法是报错。
        ;
        ; 文件占用那一类竞态是**另一件事**，交给 File 之前的可写守卫（WaitForFileWritable）：
        ;   · 这里防「进程还在」；
        ;   · 那里防「进程已退，但 ariahost.exe 的句柄尚未释放（杀软实时扫描 / 句柄拆除延迟）」。
        ; task-61 的真机 E2E 里出现过后者：TRACE 明确写「--quit 退出码=0、复查第 0 轮已退出」，
        ; 但紧接的 File 直接 Abort，复跑即成功——所以两道守卫不能互相替代。
        MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} 仍在运行（已等待约 20 秒仍未退出），安装无法覆盖被占用的文件，已中止。$\r$\n$\r$\n请从托盘菜单「退出」，或从任务管理器结束 ${APP_EXE} 后重新运行安装程序。$\r$\n（--quit 返回：$R8）" /SD IDOK
        Abort
      !else
        DetailPrint "警告：${APP_NAME} 仍在运行，后续删除可能因文件占用失败；请退出后手工删除残留目录。"
      !endif
    ${Else}
      !insertmacro TraceWrite "[${MODE}] 等待结束：已退出（轮次=$R9，实测=$R1 ms）"
      DetailPrint "${APP_NAME} 已退出。"
    ${EndIf}
  ${EndIf}
  !if "${MODE}" == "uninstall"
    ; C-2：先停 ariahost，再等 2 秒让 Job Object 收掉 aria2c 子进程，然后复查。
    ; 这 2 秒是"给子进程收尾的缓冲"，判定依据仍然是随后的 tasklist 结果。
    ; 注意：这两步在 ${If} $R7 == "0" 之外，**无论刚才有没有探测到实例都会执行**
    ; （历史上曾误以为它在条件块里，实测 TRACE 已证伪）。
    Sleep 2000
    !insertmacro ProcessIsRunning aria2c.exe 10
    !insertmacro TraceWrite "[uninstall] 停 ariahost 后 2s 复查 aria2c → $R7"
    ${If} $R7 == "1"
      ; 残留的 aria2c 可能是用户自己的实例（甚至是别的下载器的），**不能无条件 kill**：
      ; 只有能证明它的映像路径在 AriaHost 的两个数据目录下时才结束它。
      ; 路径来源：wmic（Win10 1809+ 自带；Win11 24H2 起可能已移除 → 取不到就只告警）。
      DetailPrint "检测到 aria2c.exe 仍在运行，正在判定它是否属于 AriaHost…"
      nsExec::ExecToStack "wmic process where $\"name='aria2c.exe'$\" get ExecutablePath /value"
      Pop $R6          ; wmic 退出码
      Pop $R5          ; 输出，形如 "ExecutablePath=C:\…\aria2c.exe"（多个进程会有多行）
      !insertmacro TrimEol $R5

      ; 只处理单实例形态（首行 ExecutablePath=…）；多行/空/无 wmic 都算"不确定"
      StrCpy $R8 $R5 15
      ${If} $R8 != "ExecutablePath="
        !insertmacro TraceWrite "[uninstall] wmic 取路径失败，不做结束；原样输出=$R5"
        DetailPrint "警告：无法取得 aria2c.exe 的映像路径（wmic 不可用或存在多个实例），不做结束；若 $INSTDIR 删除不完整，请手工处理。"
      ${Else}
        StrCpy $R5 $R5 1024 15      ; 跳过 "ExecutablePath=" 前缀，$R5 = 纯路径
        ; 用户平面：$LOCALAPPDATA\AriaHost\（当前上下文即当前用户）
        StrCpy $R3 "$LOCALAPPDATA\${APP_NAME}\"
        ; 服务平面：%ProgramData%\AriaHost\（SetShellVarContext all 时 $APPDATA 指向 ProgramData）
        SetShellVarContext all
        StrCpy $R4 "$APPDATA\${APP_NAME}\"
        !ifdef PER_MACHINE
          ; 全机变体本来就保持 all
        !else
          SetShellVarContext current
        !endif

        StrLen $R9 "$R3"
        StrCpy $R8 $R5 $R9
        ${If} $R8 == "$R3"
          StrCpy $R7 1
        ${Else}
          StrLen $R9 "$R4"
          StrCpy $R8 $R5 $R9
          ${If} $R8 == "$R4"
            StrCpy $R7 1
          ${Else}
            StrCpy $R7 0
          ${EndIf}
        ${EndIf}

        ${If} $R7 == "1"
          !insertmacro TraceWrite "[uninstall] aria2c 属于本产品（$R5），taskkill"
          DetailPrint "结束本产品的 aria2c.exe（$R5）：taskkill /F /IM aria2c.exe"
          nsExec::ExecToLog 'taskkill /F /IM aria2c.exe'
          Sleep 500
        ${Else}
          !insertmacro TraceWrite "[uninstall] aria2c 不属于本产品（$R5），不结束"
          DetailPrint "警告：aria2c.exe（$R5）不在 AriaHost 数据目录下，很可能是你自己启动的实例，不做结束。"
        ${EndIf}
      ${EndIf}
    ${Else}
      DetailPrint "aria2c.exe 已退出。"
    ${EndIf}
  !endif
!macroend

; 安装器侧实例（.onInit 调用）
Function StopAppForInstall
  !insertmacro StopAppBody install
FunctionEnd

; 卸载器侧实例：卸载段（Section "Uninstall"）只能 Call un. 前缀的函数
; （Chapter 4 §4.7.1.1 卸载器只编译 un. 前缀），故同一宏再置入一份。
Function un.StopAppForUninstall
  !insertmacro StopAppBody uninstall
FunctionEnd

; ---- 通用：给快捷方式写 AUMID（Toast 前提，失败不阻断）--------------------
; CreateShortCut 写不了 System.AppUserModel.ID，只能回调用 exe 内的自写
; IPropertyStore 实现（架构 §7.1 R9）。写失败 = Toast 不可用，运行时降级到
; 托盘气泡/面板条幅，属于设计允许的降级，绝不因此让安装失败。
; 入参：!macro 参数 LNKPATH（快捷方式完整路径）
!macro WriteAumidFor LNKPATH
  ${If} ${FileExists} "${LNKPATH}"
    ExecWait '"$INSTDIR\${APP_EXE}" --write-aumid "${LNKPATH}"' $0
    ${If} $0 == 0
      DetailPrint "AUMID 已写入：${LNKPATH}"
    ${Else}
      DetailPrint "警告：--write-aumid 返回 $0（${LNKPATH}）；Toast 将在运行时降级为托盘气泡/面板条幅。安装继续。"
    ${EndIf}
  ${Else}
    DetailPrint "警告：快捷方式不存在，跳过 AUMID：${LNKPATH}"
  ${EndIf}
!macroend

; ---- M5-f：确保「安装用户」的 config.toml 存在，并算出它的绝对路径 --------
; 为什么必须做（这是 P0 的直接成因）：服务以 SYSTEM 身份读配置，自己的 `%APPDATA%` 是
;   systemprofile —— 它读不到用户那份 config.toml 时会**静默退回内置默认值**，于是用户设的
;   下载目录/限速/并发全部失效（`dir=` 落进 `C:\WINDOWS\system32\config\systemprofile`）。
; 两件事：
;   1) 文件不存在就先落一份最小合法 TOML（其余键由程序按默认值补齐）；
;   2) 把绝对路径交给 `--install-service` 助手（经 `AH_CONFIG_PATH`），由它写进
;      `HKLM\...\Services\AriaHost\Parameters\ConfigPath`。
; 为什么默认下载目录用 `$PROFILE\Downloads` 而不是 `~/Downloads`：`~` 由**服务进程**
;   展开会落到 SYSTEM 的 profile（同样的坑）；`$PROFILE` 在这里已是一个绝对路径。
; 为什么用单引号写 TOML：TOML 基本字符串里 `\U` 是非法转义，Windows 路径必须用字面量字符串。
; 返回值：$R2 = config.toml 的绝对路径（调用方**立即**使用，中间不要再动 $R2）。
Function EnsureUserConfig
  StrCpy $R2 "$APPDATA\AriaHost\config.toml"
  ${If} ${FileExists} "$R2"
    DetailPrint "用户配置已存在：$R2（服务将用它）"
    Return
  ${EndIf}

  CreateDirectory "$APPDATA\AriaHost"
  ClearErrors
  FileOpen $R3 "$R2" w
  ${If} ${Errors}
    DetailPrint "警告：无法创建 $R2（服务将退回机器级或内置默认配置）"
    StrCpy $R2 ""
    Return
  ${EndIf}
  FileWrite $R3 "schema_version = 1$\r$\n"
  FileWrite $R3 "$\r$\n"
  FileWrite $R3 "# 由安装程序生成：服务模式（SYSTEM）要读这份配置。$\r$\n"
  FileWrite $R3 "# 下载目录必须是**绝对路径**：服务身份下 `~` 会展开到系统账户目录。$\r$\n"
  FileWrite $R3 "[dl]$\r$\n"
  FileWrite $R3 "dir = '$PROFILE\Downloads'$\r$\n"
  FileClose $R3
  DetailPrint "已生成默认配置：$R2（下载目录 $PROFILE\Downloads）"
FunctionEnd

; ---- A：可选的服务注册（per-user 包，单次 UAC，等真实退出码）--------------
; 为什么不能直接 nsExec：nsExec 无法提权，非管理员下 CreateService 必失败。
; 为什么不能只 ExecShell 了事：ExecShell "runas" 是**异步**的，$0 只表示
;   "提权请求已发出"，拿不到子进程退出码，无法区分"用户取消 UAC"与"注册失败"。
; 做法：让 cmd 以提权身份执行 exe，并把 !errorlevel! 写进 %TEMP% 的回执文件，
;   安装器轮询该文件出现（真实结果，不是 Sleep 假同步），再读码判断。
; 注意点（都踩过或专门规避过）：
;   * %errorlevel% 在**整行解析阶段**就展开，拿到的是启动前的值 → 必须 `/v:on`
;     用 !errorlevel!（延迟展开）。
;   * `echo 0>file` 会把 0 当成句柄号重定向 → 用括号 `(echo !errorlevel!)>file`。
;   * 路径含空格 → 外层再包一层引号并加 `/S`（cmd /S 会恰好剥掉最外层引号）。
;   * 失败**绝不**中止安装：面板与 headless 形态仍可用。
;
; **成败以 SCM 为唯一事实来源**（2026-11-25 裁决）：
;   回执文件只能证明"那个 cmd 跑完了并留下了 code"，证明不了服务到底建没建成——
;   若 UAC 用了**另一个管理员账户**，提权进程的 %TEMP% 是另一个用户的，回执永远读不到，
;   而服务其实可能已经注册成功。所以：
;     · 每 500ms 跑一次 `sc query AriaHost`（**查询服务状态不需要管理员权限**），查到即成功；
;     · 回执只用于区分确定的情况：1223 = 用户取消 UAC；其他非 0 = 确定失败；
;     · 60s 内 SCM 查不到、回执也没读到 → 只能说"未确认"，**不能说"失败"**。
Function RegisterServiceIfRequested
  StrCpy $R4 "$TEMP\ariahost-svc-install.rc"
  Delete "$R4"
  ; M5-f：先确保用户配置存在，再把**安装用户**算好的字面路径传给提权助手。
  ; 为什么由安装器算：提权后 `%APPDATA%` 可能属于另一个管理员账户（UAC 换账号），
  ; 助手自己展开会指到别人家里去。这里用 cmd 的 `set` 传，路径已是字面量。
  Call EnsureUserConfig
  StrCpy $R1 $R2
  DetailPrint "请求提权注册 Windows 服务：$INSTDIR\${APP_EXE} --install-service（可能出现一次 UAC 确认）"
  ; 单行调用：NSIS 没有反斜杠续行（`\` 只在字符串里作字面量），长行必须保持一行。
  ; `set AH_CONFIG_PATH=... &&` 只影响这一次提权进程的环境（不落盘、不影响别的程序）。
  ${If} $R1 == ""
    ExecShell "runas" "$SYSDIR\cmd.exe" "/v:on /S /c $\"$\"$INSTDIR\${APP_EXE}$\" --install-service & (echo !errorlevel!)>$\"$R4$\"$\"" SW_HIDE
  ${Else}
    ; `set "AH_CONFIG_PATH=..."`：**整条赋值必须包在引号里**（task-94）。不加引号时，
  ; 安装用户的 profile 一旦含空格（`C:\Users\姓 名\AppData\...`），cmd 会把空格后的部分
  ; 当成下一条命令，环境变量被截成 `C:\Users\姓` → 助手写进 ConfigPath 的是垃圾路径 →
  ; 第 1 级报废 → 静默回落第 2/3 级（M5-f 那个 P0 在这些用户身上原样复现）。
  ; `set "VAR=value"` 这种写法对含空格的路径是安全形态。
  ExecShell "runas" "$SYSDIR\cmd.exe" "/v:on /S /c $\"set $\"AH_CONFIG_PATH=$R1$\"&& $\"$\"$INSTDIR\${APP_EXE}$\" --install-service & (echo !errorlevel!)>$\"$R4$\"$\"" SW_HIDE
  ${EndIf}

  StrCpy $R5 0     ; 轮次
  StrCpy $R7 ""    ; 回执内容（空 = 没读到）
  StrCpy $R9 0     ; 1 = SCM 里已有该服务
  ${Do}
    ; (1) SCM 是事实来源：查到即成功，立刻退出，不再等回执
    nsExec::ExecToStack 'sc query ${SVC_NAME}'
    Pop $R6        ; sc 退出码：0=存在；1060=不存在（sc 自己的退出码就是 1060）
    Pop $R3        ; sc 输出（不使用，只为配平栈）
    ${If} $R6 == "0"
      StrCpy $R9 1
      ${ExitDo}
    ${EndIf}

    ; (2) 回执只作提示：读到就停（1223=取消，其余非 0=确定失败，0 则继续等 SCM）
    ${If} ${FileExists} "$R4"
      FileOpen $R8 "$R4" r
      FileRead $R8 $R7
      FileClose $R8
      Delete "$R4"
      !insertmacro TrimEol $R7
      ${If} $R7 != "0"
        ${ExitDo}
      ${EndIf}
    ${EndIf}

    Sleep 500
    IntOp $R5 $R5 + 1
  ${LoopWhile} $R5 < 120   ; 120 × 500ms = 60s

  ${If} $R9 == 1
    DetailPrint "服务已注册（sc query ${SVC_NAME} 命中）。服务为手动触发：不会随开机自动运行 aria2。"
    ${If} $R7 != ""
    ${AndIf} $R7 != "0"
      ; 极端情况：服务在，但回执说非 0（例如助手在创建后清理阶段返回失败）
      DetailPrint "注意：--install-service 回执为 $R7，但 sc query 已能查到服务；以 SCM 为准判成功，建议在面板里确认服务状态。"
    ${EndIf}
  ${ElseIf} $R7 == "1223"
    DetailPrint "服务注册被用户取消（Win32 1223 = ERROR_CANCELLED，AH-E5002）。"
    MessageBox MB_OK|MB_ICONEXCLAMATION "${APP_NAME} 已安装完成，服务注册被取消。$\r$\n$\r$\n这不影响使用：面板会以 headless 方式托管 aria2。需要服务时，请打开面板 → 设置 → 「一键装服务」重试。" /SD IDOK
  ${ElseIf} $R7 == "0"
    ; 助手说成功、但 60s 内 SCM 还没查到：不替用户下结论（分支必须排在 `!= ""` 之前）
    DetailPrint "助手报告成功，但 sc query ${SVC_NAME} 在 60s 内未命中。"
    MessageBox MB_OK|MB_ICONINFORMATION "${APP_NAME} 已安装完成。服务注册助手报告成功，但安装程序在 60 秒内没能在系统里查到 ${SVC_NAME} 服务。$\r$\n$\r$\n请在面板 → 设置 里确认「服务」状态；如果显示未安装，点「一键装服务」重试即可。" /SD IDOK
  ${ElseIf} $R7 != ""
    DetailPrint "服务注册失败（--install-service 返回 $R7，且 sc query 查不到服务）。"
    MessageBox MB_OK|MB_ICONEXCLAMATION "${APP_NAME} 已安装完成，但服务注册失败（返回 $R7，参见 AH-E5003）。$\r$\n$\r$\n这不影响使用：面板会以 headless 方式托管 aria2。$\r$\n需要服务时，可用管理员权限执行：$\r$\n  $\"$INSTDIR\${APP_EXE}$\" --install-service$\r$\n或打开面板 → 设置 → 「一键装服务」。" /SD IDOK
  ${Else}
    ; 60s 内既没查到服务、也没读到回执：无法判断是取消 UAC、换了账户还是真失败
    DetailPrint "未确认到服务注册结果（sc query 未命中，且未读到 --install-service 回执）。"
    MessageBox MB_OK|MB_ICONINFORMATION "${APP_NAME} 已安装完成，但**未能确认**服务是否注册成功。$\r$\n$\r$\n安装程序在 60 秒内既没在系统里查到 ${SVC_NAME} 服务，也没拿到注册助手的返回码（例如 UAC 用了另一个管理员账户时会出现这种情况）。$\r$\n$\r$\n请打开面板 → 设置，查看「服务」状态；需要时点「一键装服务」重试。这不影响 AriaHost 的正常使用。" /SD IDOK
  ${EndIf}
FunctionEnd

; ---- 初始化 ---------------------------------------------------------------
Function .onInit
  ; 第一行就留痕：/DTRACE 未定义时该宏为空，不产生任何文件与变量。
  !insertmacro TraceWrite "[init] 安装器启动；exedir=$EXEDIR temp=$TEMP instdir=$INSTDIR"
  !if "${ARCH}" == "x64"
    ${IfNot} ${RunningX64}
      MessageBox MB_ICONSTOP "这是 64 位安装包，请在 64 位 Windows 上运行。" /SD IDOK
      Abort
    ${EndIf}
  !endif

  ; 版本基线是 10.0.17763（Win10 1809 / Server 2019），不是 10.0.10240：
  ; ${AtLeastWin10} 只比 major/minor（WinVer.nsh：WINVER_10 = 10.0.10240），
  ; 会放行 Windows Server 2016（10.0.14393），与需求 N6/Q16、架构 §4.10/§13 的
  ; 「安装器卡 10.0.17763，Server 2016 安装即拒并给 AH-E1001」冲突。
  ; 因此改用构建号比较 ${AtLeastBuild} 17763（WinVer.nsh）。
  ${IfNot} ${AtLeastBuild} 17763
    ${WinVerGetBuild} $0
    MessageBox MB_ICONSTOP "AriaHost 需要 Windows 10 1809（内部版本 17763）或更高版本，或 Windows Server 2019 及以上。$\r$\n$\r$\n当前系统内部版本：$0$\r$\n被拒绝：Windows 10 1803 及更早、Windows Server 2016（14393）。$\r$\n错误码：AH-E1001" /SD IDOK
    Abort
  ${EndIf}

  !ifdef PER_MACHINE
    SetShellVarContext all
    SetRegView 64
  !else
    SetShellVarContext current
  !endif

  ; A6 已废弃（M3）：per-user 包**不再隐藏**服务组件——`--install-service` 自带
  ; 提权助手，per-user 也能通过单次 UAC 注册服务（见 Function RegisterServiceIfRequested）。
  ; 组件页里它仍是可选（`/o`），默认不勾。
  ;
  ; 历史：v2.x 曾在 .onInit 里 Call HidePerUserServiceSection 把该组件置空，
  ; 并因为 ${SEC_SERVICE} 只能在该 Section 之后引用而把隐藏逻辑放到 Section 之后。
  ; 该函数已随本批删除，勿再恢复（per-user 注册服务已是产品能力）。

  ; D1（发版阻塞项，2026-11-26 真机踩过）：默认目录的判据是「那个目录是不是一份真安装」。
  ;
  ; 旧逻辑只要 `$0\ariahost.exe` 存在就采纳注册表里的 InstallDir，于是调试期写进这个键的
  ; 临时路径会变成用户新装的默认目录（v0.1.2 实测：被带进工作区里的测试安装路径，随后
  ; ariahost 因运行在受限路径连 aria2.conf 都写不出来）。文件头也已去掉
  ; `InstallDirRegKey`——那条会让 NSIS 在**任何校验之前**就把该值塞进 $INSTDIR。
  ;
  ; 真安装的判据必须**同时**存在 ariahost.exe 与 uninst.exe：`WriteUninstaller` 紧跟在主程序
  ; 落盘之后，所以「装到一半就失败」的目录不会两样都齐。缺一即判不可信：清掉记录、回落到
  ; 编译期默认目录。用户用 `/D` 显式指定了别的目录时不动它（只在 $INSTDIR 仍等于坏值时回落）。
  ReadRegStr $0 ${REG_ROOT} "${REG_APP_KEY}" "InstallDir"
  ${If} $0 != ""
    ${If} ${FileExists} "$0\${APP_EXE}"
    ${AndIf} ${FileExists} "$0\uninst.exe"
      DetailPrint "检测到已安装的 ${APP_NAME}（$0），将按升级方式处理。"
      StrCpy $INSTDIR "$0"
    ${Else}
      DetailPrint "忽略不可信的安装目录记录（$0）：那里没有完整的 ${APP_EXE} + uninst.exe，改用默认目录。"
      !insertmacro TraceWrite "[init] 丢弃不可信 InstallDir=$0"
      ClearErrors
      DeleteRegValue ${REG_ROOT} "${REG_APP_KEY}" "InstallDir"
      ${If} $INSTDIR == $0
        StrCpy $INSTDIR "${INSTALL_ROOT}\${APP_NAME}"
      ${EndIf}
    ${EndIf}
  ${EndIf}
  ; 兜底：$INSTDIR 为空（注册表键存在但值为空串等边角情况）时也不要往空路径装。
  ${If} $INSTDIR == ""
    StrCpy $INSTDIR "${INSTALL_ROOT}\${APP_NAME}"
  ${EndIf}
  Call StopAppForInstall
FunctionEnd

; ---- 组件 -----------------------------------------------------------------
Section "AriaHost 主程序（必需）" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  SetOverwrite on
  ; 编译期路径必须用 "\"，且优先绝对路径（见文件头「编译期路径规则」的实测结论）。
  ; 清错误标志再 File：`${Errors}` 反映的是**最近一次**相关操作的错误标志，若不先清，
  ; 前面任何一次失败（例如 TRACE 文件打不开、CreateDirectory 失败）都会被误判成
  ; "File 失败"——真机实测过这条假阳性（exit 1 而文件其实装好了）。
  ; ---- 覆盖写之前：等目标 exe 变成「可写」（task-64 守卫）------------------------------
  ; 为什么需要：面板**逻辑上已退出** ≠ `ariahost.exe` 的**文件句柄已释放**。真机观察到过一次
  ; Abort（TRACE 写着「--quit 退出码=0、复查第 0 轮已退出」，紧接的 File 却失败），复跑立刻成功——
  ; 典型成因是杀软实时扫描或句柄拆除延迟，属于必须吸收掉的偶发。
  ; 判据用「能不能以追加方式打开」而不是 tasklist：进程列表为 0 时句柄仍可能被别的东西持有。
  ; 只在目标**已存在**（升级/覆盖）时生效：全新安装 $INSTDIR\ariahost.exe 不存在，立刻跳过、不多等。
  ; 上限 8s（32 × 250ms）；超时**不静默继续**，明确报错 + 非零退出（exit 1）。
  ${If} ${FileExists} "$INSTDIR\${APP_EXE}"
    StrCpy $R9 0
    ${Do}
      ClearErrors
      FileOpen $R2 "$INSTDIR\${APP_EXE}" a
      ${If} ${Errors}
        Sleep 250
        IntOp $R9 $R9 + 1
        ${If} $R9 >= 32
          ${ExitDo}
        ${EndIf}
      ${Else}
        FileClose $R2
        ${ExitDo}
      ${EndIf}
    ${Loop}
    !insertmacro TraceWrite "[install] 可写守卫：轮次=$R9（0=立即可写；>0 = 竞态真实存在，已被吸收）"
    ${If} $R9 >= 32
      !insertmacro TraceWrite "[install] 可写守卫超时：$INSTDIR\${APP_EXE} 8s 内打不开写句柄"
      MessageBox MB_OK|MB_ICONSTOP "安装目录下的 ${APP_EXE} 仍被占用（可能是杀毒软件正在扫描），8 秒内无法写入，安装已中止。$\r$\n$\r$\n请稍后重试；若反复出现，把安装目录加入杀毒软件的排除项。" /SD IDOK
      SetErrorLevel 1
      Abort
    ${EndIf}
  ${EndIf}
  ClearErrors
  File "${SRCDIR}\${APP_EXE}"
  ; 图标：与安装器自己、开始菜单/桌面快捷方式、exe 资源共用同一份 ICO
  File "${SRCDIR}\ariahost.ico"

  ; 内置 aria2c（task-54 A 条）：release.yml 把「已按 SHA-256 硬校验」的 aria2c.exe
  ; 放进 ${SRCDIR}，用户机器上双击即用，不再依赖首次运行的在线下载（双线路在真实
  ; 用户网络下不可用，见 docs/dev/known-limitations.md L6）。
  ;
  ; **默认 fail-closed**（task-54 B3 裁定）：没有核心就**编译失败**。
  ; 为什么不是「release 才传 /DBUNDLED_ARIA2_REQUIRED」：那等于把事故责任押在
  ; 「每个构建入口都记得加那个 define」上——谁改个数顺手 makensis 一次就会产出
  ; 无核心包，而失败要到用户首启才暴露。默认关死、
  ; 想要无核心包必须显式写 /DNO_BUNDLED_ARIA2=1，写错方向才安全。
!ifdef NO_BUNDLED_ARIA2
  ; 显式声明「本包不带核心」：Finish 页文案会明说需要联网下载（见文件头界面段）。
  !insertmacro TraceWrite "[install] NO_BUNDLED_ARIA2：本包不含 aria2c.exe（显式声明）"
!else
  !if /FileExists "${SRCDIR}\aria2c.exe"
  File "${SRCDIR}\aria2c.exe"

  ; 第三方许可与来源说明（GPLv2 二进制分发义务，见 docs/dev/release.md §7）。
  ;
  ; 两份文件都**直接取自仓库**（相对本 .nsi 所在目录），不经 release.yml 暂存：
  ; - `installer\licenses\aria2-COPYING.txt`：GPL-2.0 全文，取自 aria2 release
  ;   `release-1.37.0` 的 win-64bit zip 内 `COPYING`（18092 字节，sha256
  ;   8177f975…）——即「随该二进制一起分发的那份」，未删节、未改写。
  ; - 仓库根 `THIRD-PARTY-NOTICES.md`：组件/版本/来源 URL/SHA-256/GPLv2/源码获取。
  ; 二者与 aria2c.exe **同进同退**：没有核心就没有许可文件，不留孤儿声明；
  ; 有核心就必然有全文——「二进制在、许可丢」这种组合在结构上不可能出现。
  !insertmacro TraceWrite "[install] 装第三方许可：COPYING + THIRD-PARTY-NOTICES.md"
  SetOutPath "$INSTDIR\licenses"
  File "licenses\aria2-COPYING.txt"
  SetOutPath "$INSTDIR"
  File "..\THIRD-PARTY-NOTICES.md"
  !else
    ; 为什么这条 !error 只能是 ASCII 英文（真 CI 实证，run 45598611141 step 11）：
    ; makensis 的**编译期**消息走 ANSI 代码页（Windows runner 是 en-US，ACP=1252），
    ; 中文在 WideCharToMultiByte 阶段直接变成 `?`，日志里读到的是
    ; `!error: ? D:\a\...\dist\x64\aria2c.exe:??????**??**? aria2 ??(task-54 B3)…`
    ; ——一条等于没有信息量的红。`Unicode true` 只保证**装出来的界面**中文正常，
    ; 管不到编译期输出；`/INPUTCHARSET UTF8` 也不管（那是脚本源文件的读取编码）；
    ; `chcp 65001` 同样无效（转换发生在控制台代码页之前的那一层）。
    ; 读者是半年后看 CI 日志的人、日志要跨机器跨代码页，所以：文案 ASCII，解释留注释。
    ;
    ; 这条闸门的语义（task-54 B3）：默认**必须**带 aria2 核心，缺了就不许编过——
    ; 免得产出一个「用户机器上首次启动还要联网下载」的包（真实用户网络下双线路
    ; 均不可用，见 docs/dev/release.md §7）。两条出路：
    ; 1) 带核心：先跑 release.yml 的下载 + SHA-256 硬校验步骤，或手工放入并按
    ;    crates/ariahost-core/src/fetch.rs 的 KNOWN_SHA256 核对；
    ; 2) 确实要出无核心包：显式传 /DNO_BUNDLED_ARIA2=1（Finish 页会明说需联网下载）。
    !error "aria2c.exe is missing from the payload (${SRCDIR}\aria2c.exe). This installer requires the bundled aria2 core by default; a core-less package must opt in explicitly. Fix: let release.yml download and SHA-256-verify the core into dist/, or place aria2c.exe there manually and check it against KNOWN_SHA256. To build a core-less installer on purpose, pass /DNO_BUNDLED_ARIA2=1."
  !endif
!endif

  ; 内置拉丁字体 Inter（OFL-1.1，task-105 A 条）：装到 **exe 旁的 fonts\**。
  ;
  ; 为什么不是装进系统字体（`$WINDIR\Fonts`）：那要管理员权限、要刷字体缓存、
  ; 卸载还得回滚；而 exe 旁的目录由本脚本自己的 File 指令负责，卸载跟着 RMDir 走，
  ; 生命周期与程序本体一致。读取方是 `crates/ariahost-ui/src/fonts.rs` 的
  ; `font_dirs_with()`——它把这个目录排在候选第一位（顺序即优先级）。
  ;
  ; 为什么是 fail-closed 而不是「缺了就跳过」：字体缺失在运行期只表现为
  ; 「字体没变成 Inter」，`inject=` 会回落成系统 provider，用户和 CI 都看不出少了什么；
  ; 只有在编译期报错，才有人会去修。两个文件都是 OFL 授权的真字体（非子集、非改写）。
  !if /FileExists "${__FILEDIR__}\fonts\Inter-Regular.ttf"
  !else
    !error "Bundled font fonts\Inter-Regular.ttf is missing. The installer ships Inter (OFL-1.1) next to the exe so Latin text renders with Inter instead of falling back to Segoe UI; without it the panel silently degrades. Fix: restore installer\fonts\Inter-Regular.ttf (see THIRD-PARTY-NOTICES.md for source and SHA-256)."
  !endif
  !if /FileExists "${__FILEDIR__}\fonts\Inter-SemiBold.ttf"
  !else
    !error "Bundled font fonts\Inter-SemiBold.ttf is missing (true SemiBold face for the Inter family). Fix: restore installer\fonts\Inter-SemiBold.ttf."
  !endif
  !insertmacro TraceWrite "[install] 装内置字体：fonts\Inter-Regular.ttf + fonts\Inter-SemiBold.ttf"
  SetOutPath "$INSTDIR\fonts"
  File "${__FILEDIR__}\fonts\Inter-Regular.ttf"
  File "${__FILEDIR__}\fonts\Inter-SemiBold.ttf"
  ; 许可全文与字体**同进同退**：OFL-1.1 要求随发行副本附带许可。
  SetOutPath "$INSTDIR"
  CreateDirectory "$INSTDIR\licenses"
  SetOutPath "$INSTDIR\licenses"
  File "${__FILEDIR__}\fonts\OFL.txt"
  SetOutPath "$INSTDIR"
  !insertmacro TraceWrite "[install] 许可：licenses\OFL.txt（Inter，SIL Open Font License 1.1）"

  ; 静默模式必须让失败可见（task-55 C 条）：/S 下 MessageBox 被抑制、DetailPrint
  ; 没人看，于是"复制失败"会以 exit 0 收场——automation 以为装好了，实际没有 exe。
  ; 两道判定：${Errors} 捕获 File 的运行时写失败；FileExists 是最终事实（唯一可信）。
  ; 失败一律 SetErrorLevel 1 再 Abort：静默=安静退出但非 0，交互=弹框。
  StrCpy $R8 0
  ${If} ${Errors}
    StrCpy $R8 1
  ${EndIf}
  ${IfNot} ${FileExists} "$INSTDIR\${APP_EXE}"
    StrCpy $R8 1
  ${EndIf}
  ${If} $R8 == "1"
    DetailPrint "错误：主程序未落盘：$INSTDIR\${APP_EXE}"
    !insertmacro TraceWrite "[install] 主程序未落盘，SetErrorLevel 1；instdir=$INSTDIR"
    ${IfNot} ${Silent}
      MessageBox MB_OK|MB_ICONSTOP "安装失败：未能写入 $INSTDIR\${APP_EXE}。$\r$\n$\r$\n常见原因：目标目录无写权限、磁盘空间不足、安全软件拦截。$\r$\n请换一个安装目录或清理磁盘后重试。" /SD IDOK
    ${EndIf}
    SetErrorLevel 1
    Abort
  ${EndIf}

  ; 文件名与架构 §4.10 安装包清单一致（uninst.exe）
  WriteUninstaller "$INSTDIR\uninst.exe"

  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "DisplayName"     "${APP_NAME}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "Publisher"       "${APP_PUBLISHER}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "DisplayIcon"     "$INSTDIR\${APP_EXE}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "UninstallString" '"$INSTDIR\uninst.exe"'
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninst.exe" /S'
  WriteRegDWORD ${REG_ROOT} "${REG_UNINST_KEY}" "NoModify" 1
  WriteRegDWORD ${REG_ROOT} "${REG_UNINST_KEY}" "NoRepair" 1
  WriteRegStr   ${REG_ROOT} "${REG_APP_KEY}"    "InstallDir" "$INSTDIR"

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD ${REG_ROOT} "${REG_UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "开始菜单快捷方式" SEC_SM
  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\ariahost.ico" 0

  ; B：建完 .lnk 立刻写 AUMID（Toast 前提）。失败只写日志，不阻断（见宏内注释）。
  !insertmacro WriteAumidFor $SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk

  CreateShortCut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninst.exe"
SectionEnd

Section /o "桌面快捷方式" SEC_DESKTOP
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\ariahost.ico" 0
  ; 每个 .lnk 都要单独写：AUMID 是**每份快捷方式**的属性，不是全局设置。
  !insertmacro WriteAumidFor $DESKTOP\${APP_NAME}.lnk
SectionEnd

; 服务组件：可选（`/o`，默认**不勾**）。
; - per-user 包：勾选后用 runas 调 --install-service（单次 UAC，失败不阻断安装）；
; - 全机变体：安装器已是 admin，直接调，不再额外弹 UAC。
; 两种是同一份脚手架：per-user 的隐藏逻辑（A6）已在 M3 移除。
Section /o "同时注册为 Windows 服务（Aria2 随系统服务运行）" SEC_SERVICE
!ifdef PER_MACHINE
  ; 全机变体：安装器本身已是管理员上下文，直接调用提权助手，不再额外弹 UAC。
  ; M5-f：同样先确保配置存在；全机变体下 `$APPDATA` 是**安装时那个管理员账户**的
  ; 配置目录（服务账户下无法枚举「所有用户各自的偏好」，这是刻意的取舍：
  ; 至少是一个真实可见的用户目录，而不是 systemprofile）。
  Call EnsureUserConfig
  StrCpy $R1 $R2
  DetailPrint "注册 Windows 服务（当前已是管理员上下文）：$INSTDIR\${APP_EXE} --install-service"
  ${If} $R1 == ""
    nsExec::ExecToLog '"$INSTDIR\${APP_EXE}" --install-service'
  ${Else}
    ; 引号层级：最外层那一对是给 cmd `/c` 的，cmd 会剥掉首尾各一个引号；剥完剩下
  ; `set "AH_CONFIG_PATH=..."&& "…\ariahost.exe" --install-service`，语义正确。
  ; **未实测**（本轮不跑安装器；全机变体不是首发路径）：`$INSTDIR` 含空格时的实际
  ; 命令行仍需真机取证一次（task-94 只保证能被 makensis 编译）。
  nsExec::ExecToLog '"$SYSDIR\cmd.exe" /v:on /S /c "set "AH_CONFIG_PATH=$R1"&& "$INSTDIR\${APP_EXE}" --install-service"'
  ${EndIf}
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONEXCLAMATION "服务注册失败（--install-service 返回 $0，参见 AH-E5003）。$\r$\n$\r$\n${APP_NAME} 已安装，仍可用 headless 形态运行；可在面板设置页重试「一键装服务」。" /SD IDOK
  ${EndIf}
!else
  ; per-user：不能在本进程内调 SCM（非管理员），走单次 UAC 助手并等真实退出码
  Call RegisterServiceIfRequested
!endif
SectionEnd

; ---- A6 已删除：per-user 包的隐藏逻辑（历史见 .onInit 注释）----------------

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN}    "主程序 ariahost.exe 与卸载器（必需）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_SM}      "在开始菜单创建启动与卸载快捷方式（同时写入 AUMID，通知可用）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "在桌面创建启动快捷方式（同样写入 AUMID）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_SERVICE} "安装完成后把 AriaHost 注册为 Windows 服务（手动触发，不随机启动 aria2）。需要一次管理员授权（UAC）；取消或不勾选都不影响使用——面板会以 headless 方式托管 aria2。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---- 卸载 -----------------------------------------------------------------
; 顺序（C 步，任何一步失败都继续下一步——卸载绝不能半途 Abort）：
;   1) 注销服务（runas，单次 UAC；失败不阻断，提示残留）
;   2) 停 ariahost（--quit + 真实等待）→ 等 2s 让 Job Object 收掉 aria2c → 复查
;   3) 删快捷方式 + 注册表卸载项
;   4) 删 $INSTDIR
;   5) 用户数据（%APPDATA%、%LOCALAPPDATA%）默认**保留**，问过才删
; 为什么 1) 在 2) 之前：注销服务要用 --uninstall-service 的提权助手，它需要
; $INSTDIR\ariahost.exe 仍在；先把进程停掉会让这一步失去执行体。
Section "Uninstall"
  !ifdef PER_MACHINE
    SetShellVarContext all
    SetRegView 64
  !else
    SetShellVarContext current
  !endif

  ; === 1) 注销服务（可拒绝 UAC → 保留服务并提示残留，AH-E5003）=============
  StrCpy $R0 0   ; 0=无服务 1=已注销/无残留 2=残留待人工处理
  ReadRegStr $1 HKLM "${SVC_REG_KEY}" "ImagePath"
  ${If} $1 != ""
    ${IfNot} ${FileExists} "$INSTDIR\${APP_EXE}"
      ; 静默卸载下不能让 MessageBox 卡住 automation（task-55 C 条）：弹框只在交互模式出现，
      ; 静默模式靠"服务残留 → 末尾 SetErrorLevel 1 + Abort"让调用方看见失败。
      !insertmacro TraceWrite "[uninstall] 服务在但主程序缺失，无法自动注销"
      ${IfNot} ${Silent}
        MessageBox MB_OK|MB_ICONEXCLAMATION "检测到服务 ${SVC_NAME}，但 $INSTDIR\${APP_EXE} 已不存在，无法自动注销。$\r$\n$\r$\n请以管理员身份执行：sc delete ${SVC_NAME}" /SD IDOK
      ${EndIf}
      StrCpy $R0 2
    ${Else}
      !ifdef PER_MACHINE
        ; 已是管理员：直接执行并取退出码（A3：不再只 Sleep）
        DetailPrint "注销 Windows 服务：$INSTDIR\${APP_EXE} --uninstall-service"
        nsExec::ExecToLog '"$INSTDIR\${APP_EXE}" --uninstall-service'
        Pop $1
        ${If} $1 == "0"
          StrCpy $R0 1
        ${Else}
          !insertmacro TraceWrite "[uninstall] --uninstall-service 返回 $1"
          ${IfNot} ${Silent}
            MessageBox MB_OK|MB_ICONEXCLAMATION "服务注销失败（--uninstall-service 返回 $1，AH-E5003）。$\r$\n$\r$\n请以管理员身份手工执行：sc delete ${SVC_NAME}" /SD IDOK
          ${EndIf}
          StrCpy $R0 2
        ${EndIf}
      !else
        ; per-user 非管理员：nsExec 不能提权 → runas 发起，以服务注册表键是否消失为判据（≤30s）。
        DetailPrint "请求提权注销 Windows 服务：$INSTDIR\${APP_EXE} --uninstall-service"
        ExecShell "runas" "$INSTDIR\${APP_EXE}" "--uninstall-service" SW_HIDE
        StrCpy $2 0
        ${Do}
          Sleep 500
          IntOp $2 $2 + 1
          ReadRegStr $1 HKLM "${SVC_REG_KEY}" "ImagePath"
          ${If} $1 == ""
            ${ExitDo}
          ${EndIf}
        ${LoopWhile} $2 < 60
        ${If} $1 == ""
          StrCpy $R0 1
        ${Else}
          !insertmacro TraceWrite "[uninstall] 提权注销后服务仍在注册（可能取消了 UAC）"
          ${IfNot} ${Silent}
            MessageBox MB_OK|MB_ICONEXCLAMATION "服务 ${SVC_NAME} 仍在注册（可能取消了 UAC 授权，AH-E5003）。$\r$\n$\r$\n服务残留不影响卸载，但下次开机仍可能拉起 aria2；请以管理员身份执行：sc delete ${SVC_NAME}" /SD IDOK
          ${EndIf}
          StrCpy $R0 2
        ${EndIf}
      !endif
    ${EndIf}
  ${EndIf}

  ; === 2) 停进程：先 ariahost，等 2s 让 Job Object 收掉 aria2c，再复查 =====
  ; 卸载段只能 Call un. 前缀函数（Chapter 4 §4.7.1.1），故用 un. 版本；
  ; 该版本不弹框、不 Abort，退出不掉也只告警并继续。
  Call un.StopAppForUninstall

  ; === 3) 快捷方式 + 注册表（先删再删文件：卸载项留着会让"程序列表"指向空路径）=
  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir  "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  DeleteRegKey ${REG_ROOT} "${REG_UNINST_KEY}"
  DeleteRegKey ${REG_ROOT} "${REG_APP_KEY}"

  ; === 4) 删安装目录 ========================================================
  Delete "$INSTDIR\${APP_EXE}"
  ; 卸载要尽力而为（不 Abort），但**静默下也必须能看见失败**（task-55 C 条）：
  ; 删不掉 = 文件仍被占用或权限不足；若照旧 exit 0，automation 会以为卸干净了，
  ; 下次安装直接撞上被占用的旧 exe（v0.1.1 的事故形态）。
  ${If} ${FileExists} "$INSTDIR\${APP_EXE}"
    DetailPrint "错误：主程序删除失败（仍存在）：$INSTDIR\${APP_EXE}"
    !insertmacro TraceWrite "[uninstall] 主程序删除失败（仍存在），记 $R0=2"
    StrCpy $R0 2
  ${EndIf}
  Delete "$INSTDIR\aria2c.exe"
  Delete "$INSTDIR\ariahost.ico"
  Delete "$INSTDIR\uninst.exe"
  Delete "$INSTDIR\uninstall.exe"   ; 旧版本（改名前的产物）
  ; 第三方来源说明放在**顶层**（用户一眼能看到，见安装段注释）；它由本脚本 New 出来，
  ; 卸载必须自己删——漏了就会让 RMDir "$INSTDIR" 失败，只在目录里留一个孤儿 md。
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.md"
  ; 内置字体（task-105 A 条）只删我们放的**两个文件**，再用非 /r 的 RMDir 收目录。
  ; 为什么这里不用 `RMDir /r`（licenses 用的是 /r）：`fonts\` 就在 exe 旁边、名字可猜，
  ; 用户很可能往里丢自己的字体；/r 会把他的文件一起删掉。非 /r 的 RMDir 只在目录空了
  ; 才成功，剩下用户文件时它失败——紧接着的「安装目录仍有残留」判定会把这件事报出来
  ; （$R0=2），既不静默删用户文件，也不假装卸干净了。
  Delete "$INSTDIR\fonts\Inter-Regular.ttf"
  Delete "$INSTDIR\fonts\Inter-SemiBold.ttf"
  RMDir "$INSTDIR\fonts"
  ; 内置核心的许可是我们放的目录（aria2c.exe 与它同生共死），整目录删。
  RMDir /r "$INSTDIR\licenses"
  RMDir "$INSTDIR"
  ; RMDir 只删空目录：目录还在 = 有我们没列到的残留（旧版多放的 dll、用户手放的文件）。
  ; 静默卸载必须能看见（真机实证：task-54 首版漏删顶层 NOTICES，目录就留了下来）。
  ${If} ${FileExists} "$INSTDIR\*.*"
    DetailPrint "警告：安装目录仍有残留文件：$INSTDIR"
    !insertmacro TraceWrite "[uninstall] 安装目录仍有残留，记 $R0=2"
    StrCpy $R0 2
  ${EndIf}

  ; === 5) 服务平面数据 %ProgramData%\AriaHost\（仅管理员可删）==============
  ; 服务平面 = aria2.conf/session/logs/secret/aria2\versions\<ver>\ + staging\（架构 §4.11）
  ; $APPDATA 的指向由 SetShellVarContext 决定：all 时为 %ProgramData%。
  !ifdef PER_MACHINE
    RMDir /r "$APPDATA\${APP_NAME}"
  !else
    SetShellVarContext all
    ${If} ${FileExists} "$APPDATA\${APP_NAME}"
      StrCpy $R1 1
      StrCpy $R3 "$APPDATA\${APP_NAME}"
    ${Else}
      StrCpy $R1 0
    ${EndIf}
    SetShellVarContext current
  !endif

  ; ---- 用户数据：默认保留，问过才删（下载目录永不在作用范围）-------------
  ; 用户平面 = config.toml/logs/session/runtime/secret（%APPDATA%\AriaHost）
  ;            + cache（dht/update、aria2\versions\<ver>、staging）（%LOCALAPPDATA%\AriaHost）
  ; 默认按钮是「否」（/SD IDNO）：静默卸载同样保留用户数据。
  MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除 ${APP_NAME} 的用户数据？$\r$\n$\r$\n配置与日志：$APPDATA\${APP_NAME}$\r$\n版本库与缓存：$LOCALAPPDATA\${APP_NAME}\cache$\r$\n$\r$\n下载目录中的文件永远不会被删除。" /SD IDNO IDNO keep_user_data

  RMDir /r "$APPDATA\${APP_NAME}"
  RMDir /r "$LOCALAPPDATA\${APP_NAME}\cache"
  RMDir /r "$LOCALAPPDATA\${APP_NAME}\logs"
  RMDir    "$LOCALAPPDATA\${APP_NAME}"

  keep_user_data:

  !ifndef PER_MACHINE
    ${If} $R1 == 1
      !insertmacro TraceWrite "[uninstall] 服务平面数据保留（需管理员）：$R3"
      ${IfNot} ${Silent}
        MessageBox MB_OK|MB_ICONINFORMATION "服务平面数据仍在：$R3$\r$\n$\r$\n它属于 %ProgramData% 作用域，需要管理员权限才能删除。$\r$\n保留它可以在下次「一键装服务」时复用（推荐）；要立即删除请以管理员身份执行：$\r$\nrmdir /s /q $\"$R3$\"" /SD IDOK
      ${EndIf}
    ${EndIf}
  !endif

  ; ---- 静默下的失败必须可见（task-55 C 条）------------------------------------
  ; 卸载要「尽力而为」：不中途 Abort（否则可能注册表已删、文件全留的半成品状态），
  ; 但调用方（升级流程 / 脚本 / 用户）必须能区分「卸干净」与「文件还在」——所以把
  ; 上面各处记下的失败统一折算成非零退出码（1）。交互模式下前面已弹过具体原因。
  ${If} $R0 == "2"
    !insertmacro TraceWrite "[uninstall] 有残留，退出码置 1"
    SetErrorLevel 1
  ${EndIf}
SectionEnd
