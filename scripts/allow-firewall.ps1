#Requires -Version 5.1
#Requires -RunAsAdministrator
<#
Run manually from an elevated Windows PowerShell. Supports -WhatIf.
Only rules owned by this script for this exact executable path are updated.
Reference: https://learn.microsoft.com/powershell/module/netsecurity/new-netfirewallrule
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($env:OS -ne 'Windows_NT') {
    throw '此脚本只能在 Windows 上运行。'
}

$rootDirectory = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$configPath = Join-Path $rootDirectory 'rustnfs.json'
$programPath = Join-Path $rootDirectory 'rustnfsd.exe'

if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
    throw "找不到配置 ${configPath}。请先在 GUI 中保存共享目录和允许的客户端。"
}
if (-not (Test-Path -LiteralPath $programPath -PathType Leaf)) {
    throw "找不到服务程序 ${programPath}。请保持完整的解压目录结构。"
}
$programPath = (Get-Item -LiteralPath $programPath).FullName
$configuration = Get-Content -LiteralPath $configPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($null -eq $configuration -or $configuration -is [System.Array]) {
    throw 'rustnfs.json 的根节点必须是配置对象。'
}

function Get-ConfiguredPort {
    param([string] $FieldName, [int] $DefaultValue)
    $property = $configuration.PSObject.Properties[$FieldName]
    $value = $DefaultValue
    if ($null -ne $property) {
        $value = $property.Value
    }
    if (($value -isnot [int] -and $value -isnot [long]) -or $value -lt 1 -or $value -gt 65535) {
        throw "配置字段 $FieldName 必须是 1 到 65535 之间的整数。"
    }
    return [int] $value
}

$ports = @(
    Get-ConfiguredPort 'portmap_port' 111
    Get-ConfiguredPort 'mount_port' 20048
    Get-ConfiguredPort 'nfs_port' 2049
)
if (@($ports | Select-Object -Unique).Count -ne 3) {
    throw 'Portmapper、MOUNT、NFS 三个端口必须各不相同。'
}

$clientProperty = $configuration.PSObject.Properties['allowed_clients']
if ($null -eq $clientProperty -or $clientProperty.Value -isnot [System.Array]) {
    throw '配置必须包含 allowed_clients 数组，不能使用 Any、通配符或空白值。'
}
$configuredClients = @($clientProperty.Value)
if ($configuredClients.Count -lt 1 -or $configuredClients.Count -gt 64) {
    throw 'allowed_clients 必须包含 1 到 64 个明确的 IPv4 地址或网段。'
}

$remoteAddresses = [System.Collections.Generic.List[string]]::new()
$ranges = [System.Collections.Generic.List[object]]::new()
foreach ($entry in $configuredClients) {
    if ($entry -isnot [string]) {
        throw 'allowed_clients 的每一项必须是 IPv4 地址或 CIDR 字符串。'
    }
    $client = $entry.Trim()
    $match = [regex]::Match($client, '^(?<ip>(?:[0-9]{1,3}\.){3}[0-9]{1,3})(?:/(?<prefix>[0-9]{1,2}))?$')
    if (-not $match.Success) {
        throw "无效的客户端地址：${client}。请使用如 192.168.1.50/32 或 192.168.1.0/24 的 IPv4 地址。"
    }
    $literal = $match.Groups['ip'].Value
    $parsedAddress = $null
    if (-not [System.Net.IPAddress]::TryParse($literal, [ref] $parsedAddress) -or
        $parsedAddress.AddressFamily -ne [System.Net.Sockets.AddressFamily]::InterNetwork -or
        $parsedAddress.ToString() -ne $literal) {
        throw "无效的标准 IPv4 地址：${literal}。"
    }
    $prefix = 32
    if ($match.Groups['prefix'].Success) {
        $prefix = [int] $match.Groups['prefix'].Value
    }
    if ($prefix -lt 1 -or $prefix -gt 32) {
        throw "客户端 $client 的前缀必须为 1 到 32；此脚本拒绝向所有 IPv4 客户端开放。"
    }

    $bytes = $parsedAddress.GetAddressBytes()
    [uint64] $numericAddress = [uint64] $bytes[0] * 16777216 + [uint64] $bytes[1] * 65536 + [uint64] $bytes[2] * 256 + $bytes[3]
    [uint64] $blockSize = [math]::Pow(2, 32 - $prefix)
    [uint64] $rangeStart = [math]::Floor($numericAddress / [double] $blockSize) * $blockSize
    [uint64] $rangeEnd = $rangeStart + $blockSize
    $ranges.Add([pscustomobject] @{ Start = $rangeStart; End = $rangeEnd })

    # Normalize CIDRs exactly as the server's network membership check does.
    $canonical = '{0}.{1}.{2}.{3}/{4}' -f (($rangeStart -shr 24) -band 255), (($rangeStart -shr 16) -band 255), (($rangeStart -shr 8) -band 255), ($rangeStart -band 255), $prefix
    if (-not $remoteAddresses.Contains($canonical)) {
        $remoteAddresses.Add($canonical)
    }
}

# Reject an all-IPv4 union too, such as 0.0.0.0/1 plus 128.0.0.0/1.
[uint64] $coveredUntil = 0
foreach ($range in ($ranges | Sort-Object Start, End)) {
    if ($range.Start -gt $coveredUntil) {
        break
    }
    if ($range.End -gt $coveredUntil) {
        $coveredUntil = $range.End
    }
}
if ($coveredUntil -ge [uint64] 4294967296) {
    throw 'allowed_clients 合起来覆盖所有 IPv4 地址；请缩小为实际开发设备或网段。'
}

Import-Module NetSecurity -ErrorAction Stop
$hashAlgorithm = [System.Security.Cryptography.SHA256]::Create()
try {
    $hashBytes = $hashAlgorithm.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($programPath.ToLowerInvariant()))
    $pathId = ([System.BitConverter]::ToString($hashBytes)).Replace('-', '').Substring(0, 16)
}
finally {
    $hashAlgorithm.Dispose()
}
$ownershipMarker = 'Managed by RustNFS allow-firewall.ps1 (schema 1)'
$rulePlans = @()

# Validate both existing rules before changing either one. Never delete rules.
foreach ($protocol in @('TCP', 'UDP')) {
    $ruleName = "RustNFS-$pathId-$protocol"
    $existing = @(Get-NetFirewallRule -PolicyStore PersistentStore -Name $ruleName -ErrorAction SilentlyContinue)
    if ($existing.Count -gt 1) {
        throw "发现多个同名规则 ${ruleName}，未作修改。请手动检查。"
    }
    if ($existing.Count -eq 1) {
        $application = @($existing[0] | Get-NetFirewallApplicationFilter -ErrorAction Stop)
        if ($existing[0].Description -ne $ownershipMarker -or $application.Count -ne 1 -or
            -not [string]::Equals([string] $application[0].Program, $programPath, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "已有规则 $ruleName 不属于本脚本和程序路径，未作修改。"
        }
    }
    $rulePlans += [pscustomobject] @{ Name = $ruleName; Protocol = $protocol; Exists = ($existing.Count -eq 1) }
}

$changedCount = 0
foreach ($plan in $rulePlans) {
    $settings = @{
        PolicyStore = 'PersistentStore'
        Description = $ownershipMarker
        Enabled = 'True'
        Direction = 'Inbound'
        Action = 'Allow'
        Profile = 'Private'
        EdgeTraversalPolicy = 'Block'
        Program = $programPath
        Protocol = $plan.Protocol
        LocalPort = [string[]] $ports
        RemoteAddress = [string[]] ($remoteAddresses.ToArray())
    }
    $target = "规则 $($plan.Name)：${programPath}；$($plan.Protocol) 端口 $($ports -join ',')；客户端 $($remoteAddresses -join ',')；仅 Private"
    if ($PSCmdlet.ShouldProcess($target, '创建或更新 RustNFS 入站防火墙规则')) {
        if ($plan.Exists) {
            Set-NetFirewallRule -Name $plan.Name -NewDisplayName "RustNFS $($plan.Protocol)" @settings | Out-Null
        }
        else {
            New-NetFirewallRule -Name $plan.Name -DisplayName "RustNFS $($plan.Protocol)" -Group 'RustNFS' @settings | Out-Null
        }
        $changedCount++
    }
}

if ($WhatIfPreference) {
    Write-Host '预览完成，未修改防火墙。去掉 -WhatIf 后可应用这些规则。'
}
else {
    Write-Host "已创建或更新 $changedCount 条 RustNFS 规则。仅在 Private（专用）网络生效。"
}
Write-Host '未更改网络类型、其他程序规则或防火墙开关。配置改变后可重复运行此脚本。'
