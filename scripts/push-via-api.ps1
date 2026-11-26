﻿#Requires -Version 5.1
<#
  推送中继：经 GitHub REST API（api.github.com）把本地提交重放到远端。

  为什么需要它：本机到 github.com 的 443 直连不稳定（小页面能通、稍大的响应就卡死），
  SSH 也因同样原因在密钥交换阶段失败；而 api.github.com 稳定可达，gh 已登录。
  因此当 `git push` 反复失败时，用本脚本走 API 推送。

  已知限制：
  - 远端提交的 committer 时区会被 GitHub 归一到 +0000，因此远端 SHA 与本地 SHA 不同。
    内容（tree）逐字节一致，可用 `git rev-parse HEAD^{tree}` 与远端 commit 的 tree 比对核对。
  - 因此本地与远端历史「同内容不同 SHA」：以后仍用本脚本推送，不要混用 `git push`（会非快进）。

  用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/push-via-api.ps1
  依赖：GitHub CLI（gh）已登录，且对目标仓库有 contents:write 权限。
#>

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$repo = 'Alusss/Ariahost'
$branch = 'main'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $root

$gh = (Get-Command gh -ErrorAction SilentlyContinue).Source
if (-not $gh) {
  foreach ($p in @("$env:ProgramFiles\GitHub CLI\gh.exe", "${env:ProgramFiles(x86)}\GitHub CLI\gh.exe")) {
    if (Test-Path $p) { $gh = $p; break }
  }
}
if (-not $gh) { throw 'configured gh CLI (gh.exe) not found' }

function Invoke-GhJson([string]$method, [string]$endpoint, [string]$bodyFile) {
  $gargs = @('api', '--method', $method, $endpoint, '-H', 'Accept: application/vnd.github+json')
  if ($bodyFile) { $gargs += @('--input', $bodyFile) }
  $out = & $gh @gargs 2>&1
  if ($LASTEXITCODE -ne 0) { throw "gh api $method $endpoint failed: $out" }
  return ($out | Out-String | ConvertFrom-Json)
}

function Write-Json([string]$path, $obj) {
  $json = $obj | ConvertTo-Json -Depth 12 -Compress
  [System.IO.File]::WriteAllText($path, $json, (New-Object System.Text.UTF8Encoding($false)))
}

# 上一次推送到的本地提交（首次运行时取远端 main 的 SHA，若本地没有该对象则退回首个提交）
$markerFile = Join-Path $root '.git\ariahost-pushed-sha'
$ref = Invoke-GhJson 'GET' "repos/$repo/git/ref/heads/$branch" $null
$remoteHead = $ref.object.sha
Write-Host "remote $branch = $remoteHead"

$base = $null
if (Test-Path $markerFile) {
  $cand = (Get-Content $markerFile -Raw).Trim()
  & git cat-file -e "$cand^{commit}" 2>$null
  if ($LASTEXITCODE -eq 0) { $base = $cand }
}
if (-not $base) {
  & git cat-file -e "$remoteHead^{commit}" 2>$null
  if ($LASTEXITCODE -eq 0) { $base = $remoteHead } else { $base = (& git rev-list --max-parents=0 HEAD | Select-Object -Last 1).Trim() }
}
Write-Host "local base    = $base"

$locals = @(& git rev-list --reverse "$base..HEAD")
if ($locals.Count -eq 0) { Write-Host 'nothing to push'; exit 0 }
Write-Host "commits to push: $($locals.Count)"

# 本次推送的目标提交：**在开始时就固定下来**。
# 否则推送期间若有别的进程/成员提交了新 commit，脚本末尾重读 HEAD 会把
# 「没推过的提交」写进 marker，导致下次推送静默跳过它们（已在 2026-11-25 发生过两次）。
$pushTarget = $locals[-1]
$pushTargetTree = (& git rev-parse "$pushTarget^{tree}").Trim()
Write-Host "push target   = $pushTarget (tree $($pushTargetTree.Substring(0,8)))"

$remoteTree = (Invoke-GhJson 'GET' "repos/$repo/git/commits/$remoteHead" $null).tree.sha
$prevCommit = $remoteHead
$tmp = Join-Path $env:TEMP ('ghpush-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null

foreach ($c in $locals) {
  $parent = (& git rev-parse "$c^").Trim()
  $subject = (& git log -1 --format=%s $c).Trim()
  $bodyText = (& git log -1 --format=%b $c)
  $msg = if ($bodyText -and $bodyText.Trim()) { "$subject`n`n$($bodyText.TrimEnd())" } else { $subject }
  Write-Host ""
  Write-Host "=== $($c.Substring(0, 8)) $subject"

  $status = & git -c core.quotepath=false diff --name-status $parent $c
  $entries = @()
  foreach ($line in $status) {
    if (-not $line.Trim()) { continue }
    $parts = $line -split "`t"
    $st = $parts[0]
    $path = $parts[1]
    if ($st -eq 'D') {
      $entries += [pscustomobject]@{ path = $path; mode = '100644'; type = 'blob'; sha = $null }
      Write-Host "  D $path"
      continue
    }
    $blobSha = (& git -c core.quotepath=false rev-parse "${c}:$path").Trim()
    $bin = Join-Path $tmp 'blob.bin'
    if (Test-Path $bin) { Remove-Item $bin -Force }
    & cmd /c "git -C `"$root`" cat-file blob $blobSha > `"$bin`""
    $bytes = [System.IO.File]::ReadAllBytes($bin)
    $payload = Join-Path $tmp 'blob.json'
    Write-Json $payload @{ content = [Convert]::ToBase64String($bytes); encoding = 'base64' }
    $blob = Invoke-GhJson 'POST' "repos/$repo/git/blobs" $payload
    $entries += [pscustomobject]@{ path = $path; mode = '100644'; type = 'blob'; sha = $blob.sha }
    Write-Host ("  {0} {1} ({2} B)" -f $st, $path, $bytes.Length)
  }

  $treePayload = Join-Path $tmp 'tree.json'
  Write-Json $treePayload @{ base_tree = $remoteTree; tree = $entries }
  $newTree = Invoke-GhJson 'POST' "repos/$repo/git/trees" $treePayload

  $meta = (& git log -1 --format='%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI' $c) -split "`0"
  $commitPayload = Join-Path $tmp 'commit.json'
  Write-Json $commitPayload @{
    message   = $msg
    tree      = $newTree.sha
    parents   = @($prevCommit)
    author    = @{ name = $meta[0]; email = $meta[1]; date = $meta[2] }
    committer = @{ name = $meta[3]; email = $meta[4]; date = $meta[5] }
  }
  $newCommit = Invoke-GhJson 'POST' "repos/$repo/git/commits" $commitPayload
  Write-Host "  commit -> $($newCommit.sha)"
  $prevCommit = $newCommit.sha
  $remoteTree = $newTree.sha
}

$refPayload = Join-Path $tmp 'ref.json'
Write-Json $refPayload @{ sha = $prevCommit; force = $false }
Invoke-GhJson 'PATCH' "repos/$repo/git/refs/heads/$branch" $refPayload | Out-Null
Set-Content -Path $markerFile -Value $pushTarget -NoNewline

$localTree = $pushTargetTree
$remoteTreeNow = (Invoke-GhJson 'GET' "repos/$repo/git/commits/$prevCommit" $null).tree.sha
Write-Host ""
Write-Host "remote $branch updated to $prevCommit"
Write-Host "tree check: local=$localTree remote=$remoteTreeNow equal=$($localTree -eq $remoteTreeNow)"
Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
