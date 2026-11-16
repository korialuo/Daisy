<#
.SYNOPSIS
  在 Windows 上构建 Daisy 并打包成便携目录 + zip。

.DESCRIPTION
  前置条件（缺一不可）：
    1. Visual Studio Build Tools（MSVC 链接器 + rc.exe）
    2. Rust 工具链（rustup，msvc 目标）
    3. LLVM —— bindgen 需要 libclang 解析 FreeRDP 头文件
       winget install LLVM.LLVM
    4. FreeRDP 静态库 —— 先跑 scripts\build-freerdp.ps1
       （build.rs 会去 vendor\dist\freerdp-windows-* 找）

  产物：dist\Daisy-windows-x64\daisy.exe 以及同名 zip。

.PARAMETER Debug
  构建 debug 版（默认 release）。

.EXAMPLE
  pwsh -File scripts/build-windows.ps1
#>

[CmdletBinding()]
param(
    [switch]$Debug
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Profile = if ($Debug) { 'debug' } else { 'release' }
$Stage = Join-Path $Root "dist\Daisy-windows-x64"
$Zip = Join-Path $Root "dist\Daisy-windows-x64.zip"

function Write-Step([string]$Message) {
    Write-Host ""
    Write-Host "[build-windows] $Message" -ForegroundColor Cyan
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "找不到 cargo。先装 rustup：https://rustup.rs"
}

$freerdp = Get-ChildItem (Join-Path $Root 'vendor\dist') -Directory -Filter 'freerdp-windows-*' -ErrorAction SilentlyContinue |
    Sort-Object Name | Select-Object -Last 1
if (-not $freerdp) {
    Write-Error "vendor\dist 下没有 freerdp-windows-* 产物。先跑 scripts\build-freerdp.ps1。"
}
Write-Step "FreeRDP：$($freerdp.FullName)"

# libclang：bindgen 靠它解析头文件。没有就提前报，别等到链接阶段。
if (-not $env:LIBCLANG_PATH) {
    $llvm = Get-ChildItem 'C:\Program Files\LLVM\bin\libclang.dll' -ErrorAction SilentlyContinue
    if ($llvm) {
        $env:LIBCLANG_PATH = Split-Path -Parent $llvm.FullName
        Write-Step "自动设置 LIBCLANG_PATH=$env:LIBCLANG_PATH"
    } else {
        Write-Warning "没找到 libclang.dll。若 bindgen 报错，装 LLVM：winget install LLVM.LLVM"
    }
}

$cargoArgs = @('build', '-p', 'daisy-app')
if (-not $Debug) { $cargoArgs += '--release' }

Write-Step "cargo $($cargoArgs -join ' ')"
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { Write-Error "构建失败" }

Write-Step "整理产物到 $Stage"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Path $Stage -Force | Out-Null

$exe = Join-Path $Root "target\$Profile\daisy-app.exe"
if (-not (Test-Path $exe)) { Write-Error "找不到 $exe" }
# 名字统一成 daisy.exe：资源管理器里显示的名字就是文件名，不要留 -app 尾巴
Copy-Item $exe (Join-Path $Stage 'daisy.exe')

foreach ($extra in @('README.md', 'LICENSE')) {
    $path = Join-Path $Root $extra
    if (Test-Path $path) { Copy-Item $path $Stage }
}

Write-Step "打 zip：$Zip"
if (Test-Path $Zip) { Remove-Item -Force $Zip }
Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip

Write-Step "完成"
Write-Host "  可执行：$Stage\daisy.exe"
Write-Host "  压缩包：$Zip（{0:N1} MB）" -f ((Get-Item $Zip).Length / 1MB)
Write-Host ""
Write-Host "  自检建议：" -ForegroundColor Yellow
Write-Host "    1. 双击 daisy.exe 不应出现黑色控制台窗口"
Write-Host "    2. 资源管理器里应显示 Daisy 图标（不是白纸）"
Write-Host "    3. 首次运行后 %APPDATA%\Daisy 下应有 daisy.db 与 key.bin"
