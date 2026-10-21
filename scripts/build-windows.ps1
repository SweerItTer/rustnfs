#Requires -Version 5.1
<#
Prerequisites: Rust stable >= 1.88, the MSVC C++ build tools and Windows SDK.
Run in Developer PowerShell for Visual Studio / Build Tools.
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($env:OS -ne 'Windows_NT') {
    throw '此脚本面向 Windows / MSVC；请在 Windows 的 Developer PowerShell 中运行。'
}
if ($null -eq (Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue)) {
    throw '找不到 cargo。请安装 Rust stable（至少 1.88），再重新打开 Developer PowerShell。'
}

$projectDirectory = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not (Test-Path -LiteralPath (Join-Path $projectDirectory 'Cargo.lock') -PathType Leaf)) {
    throw '找不到 Cargo.lock。请使用完整的源码包，保留锁定的依赖文件。'
}

Push-Location -LiteralPath $projectDirectory
try {
    & cargo build --locked --release --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw "构建失败（退出码 ${LASTEXITCODE}）。请检查 Rust 版本、MSVC C++ Build Tools 和 Windows SDK。"
    }
    $outputDirectory = Join-Path $projectDirectory 'target\x86_64-pc-windows-msvc\release'
    Write-Host '构建完成。请将以下两个文件放在同一运行目录：'
    Write-Host (Join-Path $outputDirectory 'rustnfs-gui.exe')
    Write-Host (Join-Path $outputDirectory 'rustnfsd.exe')
}
finally {
    Pop-Location
}
