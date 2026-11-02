# dataman-mcp.exe answers MCP over stdio: initialize, then tools/list names Dataman's tools.
#   scripts\windows\mcp-smoke.ps1 [-Exe dist\windows\Dataman\dataman-mcp.exe]
param([string]$Exe = "dist\windows\Dataman\dataman-mcp.exe")
$ErrorActionPreference = "Stop"
$cfg = Join-Path ([IO.Path]::GetTempPath()) "dataman-mcp-smoke"
New-Item -ItemType Directory -Force $cfg | Out-Null
$si = New-Object System.Diagnostics.ProcessStartInfo
$si.FileName = (Resolve-Path $Exe).Path
$si.UseShellExecute = $false
$si.RedirectStandardInput = $true
$si.RedirectStandardOutput = $true
$si.Environment["XDG_CONFIG_HOME"] = $cfg
$si.Environment["DATAMAN_MCP_SECRETS"] = "memory"
$p = [System.Diagnostics.Process]::Start($si)
function Send([string]$line) { $p.StandardInput.WriteLine($line); $p.StandardInput.Flush() }
function Reply([int]$id) {
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        $t = $p.StandardOutput.ReadLineAsync()
        if (-not $t.Wait(30000)) { break }
        if ($null -eq $t.Result) { break }
        $m = $t.Result | ConvertFrom-Json
        if ($m.id -eq $id) { return $m }
    }
    throw "no reply to request $id"
}
Send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ci","version":"1"}}}'
$init = Reply 1
Write-Host "server: $($init.result.serverInfo.name) $($init.result.serverInfo.version)"
Send '{"jsonrpc":"2.0","method":"notifications/initialized"}'
Send '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
$tools = (Reply 2).result.tools | ForEach-Object { $_.name }
Write-Host "tools: $($tools -join ', ')"
$p.StandardInput.Close()
if (-not $p.WaitForExit(10000)) { $p.Kill(); throw "dataman-mcp.exe did not exit when its stdin closed" }
if ($tools -notcontains "list_connections") { throw "list_connections is not among the tools" }
Write-Host "PASS dataman-mcp.exe serves MCP over stdio ($($tools.Count) tools)"
