<#
Starts the Telegram bot automatically when you log in to Windows.

  .\scripts\windows-autostart.ps1              install (or update) and start now
  .\scripts\windows-autostart.ps1 -Uninstall   remove the task and stop the bot

Run `cargo build --release` first. The bot reads .env and config\meters.json from the project
folder. A copy of the program is kept in %LOCALAPPDATA%\desco-sentinel so rebuilding never
fails on a locked file; run this script again after a rebuild to update that copy.
Log file: %LOCALAPPDATA%\desco-sentinel\bot.log
#>
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$taskName = 'DescoSentinelBot'
$repo = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $env:LOCALAPPDATA 'desco-sentinel'
$exe = Join-Path $dest 'desco-sentinel.exe'
$log = Join-Path $dest 'bot.log'

function Stop-Bot {
    Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue | Stop-ScheduledTask
    Get-Process -Name 'desco-sentinel' -ErrorAction SilentlyContinue | Stop-Process -Force
}

if ($Uninstall) {
    Stop-Bot
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host 'Autostart removed and bot stopped.'
    return
}

$built = Join-Path $repo 'target\release\desco-sentinel.exe'
if (-not (Test-Path $built)) { throw "Build first: cargo build --release  (missing $built)" }
if (-not (Test-Path (Join-Path $repo '.env'))) { Write-Warning '.env not found: the bot needs TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID.' }

Stop-Bot
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item -LiteralPath $built -Destination $exe -Force

# Supervisor: run the bot, and start it again 30 s after it exits for any reason.
$script = @"
Set-Location -LiteralPath '$repo'
while (`$true) {
    if ((Test-Path '$log') -and ((Get-Item '$log').Length -gt 2MB)) { Remove-Item '$log' -Force }
    `$cmdArgs = '/c ""' + '$exe' + '" bot >> "' + '$log' + '" 2>&1"'
    Start-Process -FilePath cmd.exe -ArgumentList `$cmdArgs -WindowStyle Hidden -Wait
    Start-Sleep -Seconds 30
}
"@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))

$user = "$env:USERDOMAIN\$env:USERNAME"
$action = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -EncodedCommand $encoded"
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName $taskName
Write-Host "Installed task '$taskName' (starts at logon) and started it. Log: $log"
