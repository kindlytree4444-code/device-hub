<#
.SYNOPSIS
  Device HUB を G HUB の代わりとして入れる: ビルド → 配置 → 常時起動登録 → (任意) G HUB 削除。

.DESCRIPTION
  1. cargo build --release -p device-hub (-SkipBuild で省略し、既存の target\release を使う)
  2. %LOCALAPPDATA%\Programs\DeviceHub\device-hub.exe へコピー
  3. HKCU\...\Run に "device-hub.exe --background" を登録 (ログオン時に最小化で起動)
  4. スタートメニューにショートカットを作成
  5. Device HUB を起動
  6. -RemoveGHub 指定時は、確認のうえ scripts\remove-ghub.ps1 を管理者で実行

  先に「Device HUB で PRO Wireless が見えて設定できる」ことを確かめてから G HUB を消す流れにしている。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install-ghub-replacement.ps1
  powershell -ExecutionPolicy Bypass -File scripts\install-ghub-replacement.ps1 -RemoveGHub
#>
param(
    [switch]$SkipBuild,
    [switch]$RemoveGHub,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub')
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

if (-not $SkipBuild) {
    Step 'ビルド (cargo build --release -p device-hub)'
    Push-Location $root
    try {
        & cargo build --release --locked -p device-hub
        if ($LASTEXITCODE -ne 0) { throw "ビルドに失敗しました (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
}

$src = Join-Path $root 'target\release\device-hub.exe'
if (-not (Test-Path -LiteralPath $src)) { throw "見つかりません: $src (-SkipBuild を外してビルドしてください)" }

Step "配置: $InstallDir"
# 起動中の古い Device HUB はファイルを掴んでいるので止める。
Get-Process -Name 'device-hub' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$exe = Join-Path $InstallDir 'device-hub.exe'
Copy-Item -LiteralPath $src -Destination $exe -Force
foreach ($name in 'LICENSE-MIT', 'LICENSE-APACHE', 'THIRD_PARTY_NOTICES.md') {
    Copy-Item -LiteralPath (Join-Path $root $name) -Destination $InstallDir -Force
}

Step 'ログオン時の自動起動を登録 (HKCU Run: DeviceHub)'
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Set-ItemProperty -Path $run -Name 'DeviceHub' -Value ('"{0}" --background' -f $exe)

Step 'スタートメニューにショートカットを作成'
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'Device HUB.lnk'
$shell = New-Object -ComObject WScript.Shell
$sc = $shell.CreateShortcut($lnk)
$sc.TargetPath = $exe
$sc.WorkingDirectory = $InstallDir
$sc.Description = 'Device HUB (G HUB の代わり)'
$sc.Save()

Step 'Device HUB を起動'
Start-Process -FilePath $exe

Write-Host ''
Write-Host 'インストール完了。Device HUB のサイドバーに PRO Wireless が出て、DPI 等を変更できるか確認してください。' -ForegroundColor Green
Write-Host '(× で閉じても最小化して常駐します。終了したいときは アプリ設定 → 常駐 → 「閉じても常駐」を OFF)'

if ($RemoveGHub) {
    Write-Host ''
    $ans = Read-Host 'Device HUB でマウスを設定できましたか? G HUB を削除します (y/N)'
    if ($ans -match '^[yY]') {
        $script = Join-Path $PSScriptRoot 'remove-ghub.ps1'
        Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -ArgumentList @(
            '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$script`"", '-Force'
        )
    } else {
        Write-Host 'G HUB は残しました。確認後に scripts\remove-ghub.ps1 を管理者で実行してください。'
    }
}
