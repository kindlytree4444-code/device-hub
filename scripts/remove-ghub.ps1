<#
.SYNOPSIS
  Logicool G HUB を完全に削除する (Device HUB へ置き換えたあとに実行)。

.DESCRIPTION
  1. G HUB のプロセスとサービスを止める
  2. G HUB の設定を Desktop に ZIP でバックアップ (-NoBackup で省略)
  3. 公式アンインストーラ (lghub_updater.exe --uninstall) を実行
  4. 残りかす (Program Files / ProgramData / AppData / Run キー / サービス) を消す
  5. -RemoveDrivers 指定時のみ、G HUB の仮想デバイス用ドライバ (logi_joy_*.inf) を削除

  マウス本体の設定 (オンボードメモリ) は消えない。Logicool の標準 HID ドライバにも触らない。
  管理者 PowerShell で実行すること。-WhatIf で何を消すかだけ表示できる。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\remove-ghub.ps1 -WhatIf
  powershell -ExecutionPolicy Bypass -File scripts\remove-ghub.ps1
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [switch]$NoBackup,
    [switch]$RemoveDrivers,
    [switch]$Force
)

$ErrorActionPreference = 'Continue'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error '管理者として実行してください (スタートを右クリック → ターミナル (管理者))。'
    exit 1
}

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

if (-not $Force -and -not $WhatIfPreference) {
    Write-Host 'G HUB を削除します。Device HUB でマウスが設定できることを先に確認してください。' -ForegroundColor Yellow
    $ans = Read-Host '続行しますか? (y/N)'
    if ($ans -notmatch '^[yY]') { Write-Host '中止しました。'; exit 0 }
}

# ---------------------------------------------------------------- 1. 停止
Step 'G HUB のプロセスを停止'
$procs = 'lghub', 'lghub_agent', 'lghub_updater', 'lghub_system_tray', 'lghub_software_manager'
foreach ($p in Get-Process -Name $procs -ErrorAction SilentlyContinue) {
    if ($PSCmdlet.ShouldProcess("$($p.Name) (PID $($p.Id))", 'Stop-Process')) {
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    }
}

Step 'G HUB のサービスを停止'
$services = Get-Service -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'LGHUB*' -or $_.DisplayName -like 'LGHUB*' -or $_.DisplayName -like 'Logitech G HUB*' }
foreach ($s in $services) {
    if ($PSCmdlet.ShouldProcess($s.Name, 'Stop-Service')) {
        Stop-Service -Name $s.Name -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------- 2. バックアップ
$dataDirs = @(
    (Join-Path $env:LOCALAPPDATA 'LGHUB'),
    (Join-Path $env:APPDATA 'lghub'),
    (Join-Path $env:ProgramData 'LGHUB')
) | Where-Object { Test-Path -LiteralPath $_ }

if (-not $NoBackup -and $dataDirs.Count -gt 0) {
    $zip = Join-Path ([Environment]::GetFolderPath('Desktop')) ("LGHUB-backup-{0:yyyyMMdd-HHmmss}.zip" -f (Get-Date))
    Step "設定をバックアップ: $zip"
    if ($PSCmdlet.ShouldProcess($zip, 'Compress-Archive')) {
        $staging = Join-Path $env:TEMP ('lghub-backup-' + [guid]::NewGuid())
        New-Item -ItemType Directory -Path $staging | Out-Null
        foreach ($d in $dataDirs) {
            # settings.db / settings.json など設定だけ (キャッシュ・ログ・ダウンロード済みパッケージは除く)。
            $dest = Join-Path $staging ((Split-Path $d -Parent | Split-Path -Leaf) + '_' + (Split-Path $d -Leaf))
            New-Item -ItemType Directory -Path $dest -Force | Out-Null
            Get-ChildItem -LiteralPath $d -File -Include '*.db', '*.json' -Recurse -ErrorAction SilentlyContinue |
                Where-Object { $_.Length -lt 64MB } |
                ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $dest -ErrorAction SilentlyContinue }
        }
        Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------- 3. 公式アンインストーラ
Step '公式アンインストーラを実行'
$uninstallRoots = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall',
    'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall'
)
$entries = foreach ($r in $uninstallRoots) {
    Get-ChildItem -Path $r -ErrorAction SilentlyContinue | ForEach-Object {
        $p = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
        if ($p.DisplayName -match '^(Logitech G HUB|LGHUB|G HUB)') { $p }
    }
}
$ghubDir = Join-Path $env:ProgramFiles 'LGHUB'
$updater = Join-Path $ghubDir 'lghub_updater.exe'
if ($entries) {
    foreach ($e in $entries) {
        Write-Host "    $($e.DisplayName) $($e.DisplayVersion): $($e.UninstallString)"
    }
}
if (Test-Path -LiteralPath $updater) {
    if ($PSCmdlet.ShouldProcess($updater, '--uninstall')) {
        $proc = Start-Process -FilePath $updater -ArgumentList '--uninstall' -PassThru -Wait
        Write-Host "    アンインストーラ終了コード: $($proc.ExitCode)"
    }
} elseif ($entries) {
    foreach ($e in $entries) {
        $cmd = $e.UninstallString
        if (-not $cmd) { continue }
        if ($PSCmdlet.ShouldProcess($cmd, 'run uninstaller')) {
            if ($cmd -match '^\s*"([^"]+)"\s*(.*)$') { $exe = $Matches[1]; $argList = $Matches[2] }
            else { $exe = $cmd; $argList = "" }
            if ($argList) { Start-Process -FilePath $exe -ArgumentList $argList -Wait }
            else { Start-Process -FilePath $exe -Wait }
        }
    }
} else {
    Write-Host '    G HUB のアンインストーラが見つかりません (既に削除済み?) — 残りかすの掃除だけ行います。'
}

# アンインストーラが裏で再起動したプロセスを再度止める。
Get-Process -Name $procs -ErrorAction SilentlyContinue | ForEach-Object {
    if ($PSCmdlet.ShouldProcess("$($_.Name)", 'Stop-Process')) { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
}

# ---------------------------------------------------------------- 4. 残りかす
Step 'サービスの登録を削除'
$services = Get-Service -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'LGHUB*' -or $_.DisplayName -like 'LGHUB*' -or $_.DisplayName -like 'Logitech G HUB*' }
foreach ($s in $services) {
    if ($PSCmdlet.ShouldProcess($s.Name, 'sc.exe delete')) {
        & sc.exe delete $s.Name | Out-Null
    }
}

Step 'スタートアップ登録を削除'
foreach ($key in 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run', 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run') {
    $props = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
    if (-not $props) { continue }
    foreach ($name in $props.PSObject.Properties.Name) {
        $val = [string]$props.$name
        if ($name -like 'LGHUB*' -or $val -match '\\LGHUB\\') {
            if ($PSCmdlet.ShouldProcess("$key\$name", 'Remove-ItemProperty')) {
                Remove-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue
            }
        }
    }
}
Get-ScheduledTask -ErrorAction SilentlyContinue | Where-Object { $_.TaskName -like '*LGHUB*' -or $_.TaskName -like '*G HUB*' } |
    ForEach-Object {
        if ($PSCmdlet.ShouldProcess($_.TaskName, 'Unregister-ScheduledTask')) {
            Unregister-ScheduledTask -TaskName $_.TaskName -TaskPath $_.TaskPath -Confirm:$false -ErrorAction SilentlyContinue
        }
    }

Step 'フォルダを削除'
$folders = @(
    $ghubDir,
    (Join-Path $env:ProgramData 'LGHUB'),
    (Join-Path $env:LOCALAPPDATA 'LGHUB'),
    (Join-Path $env:APPDATA 'lghub'),
    (Join-Path $env:APPDATA 'G HUB'),
    (Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Logitech G HUB.lnk'),
    (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Logitech G HUB.lnk'),
    (Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) 'Logitech G HUB.lnk'),
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Logitech G HUB.lnk')
)
foreach ($f in $folders) {
    if (Test-Path -LiteralPath $f) {
        if ($PSCmdlet.ShouldProcess($f, 'Remove-Item')) {
            Remove-Item -LiteralPath $f -Recurse -Force -ErrorAction SilentlyContinue
            if (Test-Path -LiteralPath $f) { Write-Warning "削除できませんでした (使用中?): $f — 再起動後にもう一度実行してください" }
        }
    }
}

# ---------------------------------------------------------------- 5. 仮想デバイスドライバ (任意)
if ($RemoveDrivers) {
    Step 'G HUB の仮想デバイスドライバ (logi_joy_*) を削除'
    $drivers = Get-WindowsDriver -Online -ErrorAction SilentlyContinue |
        Where-Object { $_.OriginalFileName -match '\\logi_joy_[^\\]+\.inf$' }
    foreach ($d in $drivers) {
        $label = "$($d.Driver) ($([IO.Path]::GetFileName($d.OriginalFileName)))"
        if ($PSCmdlet.ShouldProcess($label, 'pnputil /delete-driver /uninstall /force')) {
            & pnputil.exe /delete-driver $d.Driver /uninstall /force
        }
    }
    if (-not $drivers) { Write-Host '    該当ドライバはありません' }
}

Write-Host ''
if ($WhatIfPreference) {
    Write-Host '予行演習 (-WhatIf) のため何も削除していません。上の "WhatIf:" 行が削除される対象です (無ければ G HUB は残っていません)。' -ForegroundColor Yellow
    exit 0
}
Write-Host 'G HUB の削除が完了しました。PC を再起動すると残ったファイルロックも解放されます。' -ForegroundColor Green
Write-Host 'マウスの設定は本体 (オンボードメモリ) に残っています。Device HUB で変更できます。'
