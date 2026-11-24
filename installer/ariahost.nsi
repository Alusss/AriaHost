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
;     见 ci.yml 的 windows job「安装脚本编译门禁」与 release.yml 的 build job）。
;     已验证：语法/宏/MUI2 页面、Section ID 前向引用、UTF-8 无 BOM + /INPUTCHARSET UTF8
;     的中文文案、${SRCDIR} 载荷路径。**未验证**：真机装/卸行为、per-user 与
;     /DPER_MACHINE 的运行时差异、/S 静默、AUMID 写入、服务注册助手（M3 门禁项）。
;     人工自检清单见 docs/dev/release.md §5。
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
;  本脚本假设的 --quit 退出码（M1 定稿时确认，若不同只改 QuitRunningInstance）：
;    0=目标实例已退出 / 1=没有运行中的实例 / 2=超时未退出（其余非 0 同样按失败处理）
;
;  注意：本脚本**不弹** aria2 来源页（v2.0 决定），也不分发 aria2c.exe；
;  dist\<arch>\ 里存在 aria2c.exe 时才会一并安装（打包口径见 docs/dev/release.md §7）。
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
; SectionSetText 来自 Sections.nsh（MUI2 通常会间接带入，但不要依赖它的内部实现：
; 显式 include 一次成本为 0——该文件有 include guard，重复包含无副作用）。
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
InstallDir "${INSTALL_ROOT}\${APP_NAME}"
InstallDirRegKey ${REG_ROOT} "${REG_APP_KEY}" "InstallDir"
ShowInstDetails show
ShowUninstDetails show
BrandingText "${APP_NAME} ${VERSION} (${ARCH})"

; 版本信息资源块。
; - LegalCopyright 已按 MIT OR Apache-2.0 双许可填写（架构 §13 S1 已定：2026-11）。
; - 首版安装包**不含许可页 / EULA 页**：双许可不需要用户先接受条款，属 Q13/S3 决定，
;   勿自行新增 MUI_LICENSEPAGE（加了会改变安装流程与验收 #1 口径）。
; - 版权行只放所有者署名，不要拼 aria2 的 GPLv2 声明（aria2 不由本安装包分发，见 release.md §7）。
VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName"     "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "CompanyName"     "${APP_PUBLISHER}"
VIAddVersionKey "LegalCopyright"  "Copyright (c) 2026 Alusss"

; ---- 界面 -----------------------------------------------------------------
!define MUI_ABORTWARNING
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
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; ---- 通用：先让正在运行的实例退出（A4 / EG-13）----------------------------
; 入参：$R9 = 目标 exe 路径。装/卸载共用；无法退出时告警并 Abort，
; 避免走到 File/Delete 因文件占用失败（升级失败的头号原因）。
;
; 为什么抽成 !macro：卸载段（Section "Uninstall"）**不能**调用普通函数
; （Chapter 4 §4.7.1.1：卸载器里只编译 un. 前缀的函数），所以同一段逻辑要各插一份：
; 一份编进安装器（QuitRunningInstance，.onInit 调用），
; 一份编进卸载器（un.QuitRunningInstance，Section "Uninstall" 调用）。
; 约束：宏体内**不要**写 label，否则两次插入会标签重名（编译期报错）。
!macro QuitRunningInstanceBody
  ${IfNot} ${FileExists} "$R9"
    Return
  ${EndIf}
  DetailPrint "通知已在运行的实例退出：$R9 --quit"
  nsExec::ExecToStack '"$R9" --quit'
  Pop $1   ; 退出码（成功时为数字，失败可能是 error/timeout 字符串）
  Pop $2   ; 标准输出
  ${If} $1 == "0"
    DetailPrint "--quit：旧实例已退出。"
  ${ElseIf} $1 == "1"
    DetailPrint "--quit：没有运行中的实例（返回 1），继续。"
  ${Else}
    MessageBox MB_OK|MB_ICONSTOP "面板正在运行，已中止。$\r$\n$\r$\n请先从托盘菜单「退出」${APP_NAME}，或在任务管理器结束 ${APP_EXE} 后重试。$\r$\n（--quit 返回：$1）"
    Abort
  ${EndIf}
!macroend

Function QuitRunningInstance
  !insertmacro QuitRunningInstanceBody
FunctionEnd

; 卸载器专用副本：卸载段只能 Call 这个（普通函数不会编进卸载器）。
Function un.QuitRunningInstance
  !insertmacro QuitRunningInstanceBody
FunctionEnd

; ---- 初始化 ---------------------------------------------------------------
Function .onInit
  !if "${ARCH}" == "x64"
    ${IfNot} ${RunningX64}
      MessageBox MB_ICONSTOP "这是 64 位安装包，请在 64 位 Windows 上运行。"
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
    MessageBox MB_ICONSTOP "AriaHost 需要 Windows 10 1809（内部版本 17763）或更高版本，或 Windows Server 2019 及以上。$\r$\n$\r$\n当前系统内部版本：$0$\r$\n被拒绝：Windows 10 1803 及更早、Windows Server 2016（14393）。$\r$\n错误码：AH-E1001"
    Abort
  ${EndIf}

  !ifdef PER_MACHINE
    SetShellVarContext all
    SetRegView 64
  !else
    SetShellVarContext current
  !endif

  ; A6：per-user 包隐藏服务组件（R9 表格：置灰/隐藏，不生硬失败）。
  ; 服务注册改由面板设置页的「一键装服务」触发一次 UAC（--install-service）。
  ;
  ; 为什么这里只发 Call、不直接写 SectionSetText：${SEC_SERVICE} 是 Section 定义
  ; **之后**才存在的常量（Chapter 4 §4.6.1.2：引用 section index 的代码必须位于该
  ; Section 之下），而 .onInit 在脚本里更靠前 —— 直接引用会编译失败或参数错位
  ; （隐藏错组件）。隐藏逻辑放在 Section 之后的 Function HidePerUserServiceSection。
  !ifndef PER_MACHINE
    Call HidePerUserServiceSection
  !endif

  ; A4：覆盖前先让旧实例退出，不要等 File 失败
  ReadRegStr $0 ${REG_ROOT} "${REG_APP_KEY}" "InstallDir"
  ${If} $0 == ""
    StrCpy $0 "$INSTDIR"
  ${EndIf}
  ${If} ${FileExists} "$0\${APP_EXE}"
    DetailPrint "检测到已安装的 ${APP_NAME}（$0），将覆盖安装。"
    StrCpy $R9 "$0\${APP_EXE}"
    Call QuitRunningInstance
  ${EndIf}
FunctionEnd

; ---- 组件 -----------------------------------------------------------------
Section "AriaHost 主程序（必需）" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  SetOverwrite on
  ; 编译期路径必须用 "\"，且优先绝对路径（见文件头「编译期路径规则」的实测结论）。
  File "${SRCDIR}\${APP_EXE}"

  ; 可选：dist 中存在 aria2c.exe 时一并安装（默认不随包分发，见 docs/dev/release.md §7）
!if /FileExists "${SRCDIR}\aria2c.exe"
  File "${SRCDIR}\aria2c.exe"
!endif

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
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  ; A5：CreateShortCut 写不了 System.AppUserModel.ID → Toast 依赖它（R9 裁决一/三）。
  ; 写失败不阻断安装：notify.backend 的降级判定交给运行时（Toast→托盘气泡→条幅）。
  ${If} ${FileExists} "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
    ExecWait '"$INSTDIR\${APP_EXE}" --write-aumid "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"' $0
    ${If} $0 == 0
      DetailPrint "已写入 AUMID，Toast 通知可用。"
    ${Else}
      DetailPrint "警告：--write-aumid 返回 $0，AUMID 未写入；通知将在运行时降级到托盘气泡/面板条幅（架构 §7.1 裁决三）。安装继续。"
    ${EndIf}
  ${EndIf}

  CreateShortCut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninst.exe"
SectionEnd

Section /o "桌面快捷方式" SEC_DESKTOP
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  ${If} ${FileExists} "$DESKTOP\${APP_NAME}.lnk"
    ExecWait '"$INSTDIR\${APP_EXE}" --write-aumid "$DESKTOP\${APP_NAME}.lnk"' $0
  ${EndIf}
SectionEnd

; 服务组件：per-user 包里被 .onInit 隐藏（不可达）；全机变体里可勾选并直接注册。
Section /o "安装后注册 Windows 服务（需要管理员权限）" SEC_SERVICE
!ifdef PER_MACHINE
  ; 全机变体：安装器本身已是管理员上下文，直接调用提权助手，不再额外弹 UAC
  DetailPrint "注册 Windows 服务：$INSTDIR\${APP_EXE} --install-service"
  nsExec::ExecToLog '"$INSTDIR\${APP_EXE}" --install-service'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONEXCLAMATION "服务注册失败（--install-service 返回 $0，AH-E5003）。$\r$\n$\r$\n${APP_NAME} 已安装，仍可用 headless 形态运行；可在面板设置页重试「一键装服务」。"
  ${EndIf}
!else
  ; R9：per-user 上下文无法 CreateService —— 本段不会执行，保留定义只为组件页宏引用。
  DetailPrint "per-user 安装不注册服务（见面板设置页「一键装服务」）。"
!endif
SectionEnd

; ---- A6：per-user 包隐藏服务组件 ------------------------------------------
; 必须定义在 Section "…" SEC_SERVICE **之后**：Section 的 ID 常量只在该 Section
; 之后存在（Chapter 4 §4.6.1.2），本函数引用了 ${SEC_SERVICE}。
; 调用点：Function .onInit（非 PER_MACHINE 分支）。放在 .onInit 里直接写
; SectionSetText 会在首次 makensis 时就失败/错位。
; SectionSetText 的 "" 语义：Section 文本置空即隐藏该组件（Reference/SectionSetText）。
Function HidePerUserServiceSection
  SectionSetText ${SEC_SERVICE} ""
FunctionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN}    "主程序 ariahost.exe 与卸载器（必需）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_SM}      "在开始菜单创建启动与卸载快捷方式（同时写入 AUMID，通知可用）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "在桌面创建启动快捷方式。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_SERVICE} "调用 ariahost.exe --install-service 注册 Windows 服务（仅全机安装变体可见）。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---- 卸载 -----------------------------------------------------------------
Section "Uninstall"
  !ifdef PER_MACHINE
    SetShellVarContext all
    SetRegView 64
  !else
    SetShellVarContext current
  !endif

  ; A4：卸载前也让面板退出，否则 Delete 会因文件占用失败。
  ; 必须调 un. 前缀的那个：卸载器里不存在普通函数 QuitRunningInstance
  ; （Chapter 4 §4.7.1.1），调它会在编译期报「函数未定义」。
  StrCpy $R9 "$INSTDIR\${APP_EXE}"
  Call un.QuitRunningInstance

  ; ---- A2/A3：注销服务（可拒绝 UAC → 保留服务并提示残留，AH-E5003）--------
  StrCpy $R0 0   ; 0=无服务 1=已注销 2=残留待人工处理
  ReadRegStr $1 HKLM "${SVC_REG_KEY}" "ImagePath"
  ${If} $1 != ""
    ${IfNot} ${FileExists} "$INSTDIR\${APP_EXE}"
      MessageBox MB_OK|MB_ICONEXCLAMATION "检测到服务 ${SVC_NAME}，但 $INSTDIR\${APP_EXE} 已不存在，无法自动注销。$\r$\n$\r$\n请以管理员身份执行：sc delete ${SVC_NAME}"
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
          MessageBox MB_OK|MB_ICONEXCLAMATION "服务注销失败（--uninstall-service 返回 $1，AH-E5003）。$\r$\n$\r$\n请以管理员身份手工执行：sc delete ${SVC_NAME}"
          StrCpy $R0 2
        ${EndIf}
      !else
        ; per-user 非管理员：nsExec 不能提权，用 runas 发起后以服务注册表键是否消失为判据（≤30s）。
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
          MessageBox MB_OK|MB_ICONEXCLAMATION "服务 ${SVC_NAME} 仍在注册（可能取消了 UAC 授权，AH-E5003）。$\r$\n$\r$\n服务残留不影响卸载，但下次开机仍可能拉起 aria2；请以管理员身份执行：sc delete ${SVC_NAME}"
          StrCpy $R0 2
        ${EndIf}
      !endif
    ${EndIf}
  ${EndIf}

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\aria2c.exe"
  Delete "$INSTDIR\uninst.exe"
  Delete "$INSTDIR\uninstall.exe"   ; 旧版本（改名前的产物）
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir  "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  DeleteRegKey ${REG_ROOT} "${REG_UNINST_KEY}"
  DeleteRegKey ${REG_ROOT} "${REG_APP_KEY}"

  ; ---- A2：服务平面数据 %ProgramData%\AriaHost\（仅管理员可删）------------
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

  ; ---- 用户数据：默认保留，必须问过才删（下载目录永不在作用范围）---------
  ; 用户平面 = config.toml/logs/session/runtime/secret（%APPDATA%\AriaHost）
  ;            + cache（dht/update、aria2\versions\<ver>、staging）（%LOCALAPPDATA%\AriaHost）
  MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除 ${APP_NAME} 的用户数据？$\r$\n$\r$\n配置与日志：$APPDATA\${APP_NAME}$\r$\n版本库与缓存：$LOCALAPPDATA\${APP_NAME}\cache$\r$\n$\r$\n下载目录中的文件永远不会被删除。" /SD IDNO IDNO keep_user_data

  RMDir /r "$APPDATA\${APP_NAME}"
  RMDir /r "$LOCALAPPDATA\${APP_NAME}\cache"
  RMDir /r "$LOCALAPPDATA\${APP_NAME}\logs"
  RMDir    "$LOCALAPPDATA\${APP_NAME}"

  keep_user_data:

  !ifndef PER_MACHINE
    ${If} $R1 == 1
      MessageBox MB_OK|MB_ICONINFORMATION "服务平面数据仍在：$R3$\r$\n$\r$\n它属于 %ProgramData% 作用域，需要管理员权限才能删除。$\r$\n保留它可以在下次「一键装服务」时复用（推荐）；要立即删除请以管理员身份执行：$\r$\nrmdir /s /q $\"$R3$\""
    ${EndIf}
  !endif
SectionEnd
