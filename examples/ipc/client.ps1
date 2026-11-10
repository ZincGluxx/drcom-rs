#Requires -Version 5.1
<#
.SYNOPSIS
    Dr.COM JSON-lines 客户端示例：拉起 drcom-daemon（stdio）并读取快照。

.DESCRIPTION
    只使用 Windows 自带的 PowerShell / .NET，不需要任何模块或包。
    协议细节见 docs/ipc-protocol.md。

.PARAMETER Daemon
    drcom-daemon 可执行文件路径。默认使用仓库里的 target\debug\drcom-daemon.exe。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\client.ps1
#>
[CmdletBinding()]
param(
    [string]$Daemon = ''
)

$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 在 param 默认值里拿不到 $PSScriptRoot，放到脚本体内解析。
if ([string]::IsNullOrWhiteSpace($Daemon)) {
    $Daemon = Join-Path $PSScriptRoot '..\..\target\debug\drcom-daemon.exe'
}

if (-not (Test-Path -LiteralPath $Daemon)) {
    throw "找不到 daemon：$Daemon`n先运行 cargo build --offline -p drcom-daemon"
}
$Daemon = (Resolve-Path -LiteralPath $Daemon).Path

$startInfo = New-Object System.Diagnostics.ProcessStartInfo
$startInfo.FileName = $Daemon
$startInfo.Arguments = '--stdio --no-startup-probe --no-auto-login'
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$startInfo.StandardErrorEncoding = [System.Text.Encoding]::UTF8

$process = [System.Diagnostics.Process]::Start($startInfo)

function Read-ProtocolLine {
    param([int]$TimeoutMilliseconds = 10000)
    $task = $process.StandardOutput.ReadLineAsync()
    if (-not $task.Wait($TimeoutMilliseconds)) {
        throw "超时：daemon 在 $TimeoutMilliseconds 毫秒内没有返回协议行"
    }
    return $task.Result
}

function Send-Request {
    param([hashtable]$Request)
    # ConvertTo-Json -Compress 生成单行 JSON；WriteLine 追加换行，即一帧。
    $line = $Request | ConvertTo-Json -Compress -Depth 10
    $process.StandardInput.WriteLine($line)
    $process.StandardInput.Flush()
}

function Read-Response {
    param([long]$Id)
    while ($true) {
        $line = Read-ProtocolLine
        if ($null -eq $line) { throw 'daemon 意外关闭了 stdout' }
        $message = $line | ConvertFrom-Json
        if (($message.PSObject.Properties.Name -contains 'id') -and ([long]$message.id -eq $Id)) {
            return $message
        }
        # 状态通知和响应会交错，示例里直接跳过通知。
        if (-not ($message.PSObject.Properties.Name -contains 'event')) {
            throw "收到既不是响应也不是通知的行：$line"
        }
    }
}

function Show-Snapshot {
    param($Snapshot)
    Write-Host ("  状态      : {0} ({1})" -f $Snapshot.connection.state_text, $Snapshot.connection.phase)
    Write-Host ("  在线      : 在线={0}，会话活动={1}，已在线={2}" -f $Snapshot.connection.online, $Snapshot.connection.session_active, $Snapshot.connection.online_duration)
    Write-Host ("  网卡      : {0} / {1} / {2}" -f $Snapshot.adapter.name, $Snapshot.adapter.ipv4, $Snapshot.adapter.mac)
    Write-Host ("  网关/DNS  : {0} / {1}" -f $Snapshot.adapter.gateway, $Snapshot.adapter.dns)
    Write-Host ("  保存账号  : {0}" -f $(if ($Snapshot.settings.username) { $Snapshot.settings.username } else { '(未保存)' }))
    Write-Host ("  提示      : {0}" -f $(if ($Snapshot.tooltip) { $Snapshot.tooltip } else { '(无)' }))
    Write-Host ("  版本      : api={0} app={1}" -f $Snapshot.api_version, $Snapshot.app_version)
}

try {
    $ready = (Read-ProtocolLine) | ConvertFrom-Json
    if ($ready.event -ne 'ready') { throw "首行不是 ready：$ready" }
    Write-Host ("协议 {0} v{1}，api v{2}，daemon {3}" -f $ready.protocol, $ready.protocol_version, $ready.api_version, $ready.app_version)

    $initial = (Read-ProtocolLine) | ConvertFrom-Json
    if ($initial.event -ne 'state') { throw "第二行不是 state：$initial" }
    Write-Host '初始状态：'
    Show-Snapshot $initial.snapshot

    Send-Request @{ id = 1; type = 'snapshot' }
    $response = Read-Response 1
    if (-not $response.ok) { throw "snapshot 请求失败：$($response.error)" }
    Write-Host 'snapshot 请求返回：'
    Show-Snapshot $response.snapshot

    Send-Request @{ id = 2; type = 'ping' }
    $ping = Read-Response 2
    Write-Host ("ping：protocol={0} v{1}，api v{2}" -f $ping.info.protocol, $ping.info.protocol_version, $ping.info.api_version)

    Send-Request @{ id = 3; type = 'shutdown' }
    $shutdown = Read-Response 3
    if (-not $shutdown.ok) { throw "shutdown 请求失败：$($shutdown.error)" }

    if (-not $process.WaitForExit(10000)) { throw 'daemon 没有在 10 秒内退出' }
    Write-Host ("daemon 已退出，退出码 {0}" -f $process.ExitCode)
}
finally {
    if (-not $process.HasExited) {
        $process.Kill()
        $process.WaitForExit() | Out-Null
    }
    $process.Dispose()
}
