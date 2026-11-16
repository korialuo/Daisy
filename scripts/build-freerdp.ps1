<#
.SYNOPSIS
  在 Windows 上构建 FreeRDP 静态库（供 Daisy 以库模式集成）。

.DESCRIPTION
  与 macOS 的 scripts/build-freerdp.sh 一一对应，选项保持同步：
  只构建库、不构建客户端可执行文件；关掉音频/视频/打印/智能卡等
  我们用不到的重依赖；静态库 + 静态 CRT，让产物只依赖系统 DLL。

  与 macOS 版的差异：
    * OpenSSL 与 zlib 由 vcpkg 提供（静态三元组 x64-windows-static），
      不再用 Homebrew；
    * 用 CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded 让 FreeRDP 与 Rust
      使用同一套静态 CRT——混用会导致 LNK2038 / 运行时堆不同源；
    * 产出目录 vendor\dist\freerdp-windows-x64，与 build.rs 的
      `freerdp-windows-*` 前缀匹配。

.PARAMETER Clean
  先删掉构建目录再重来。

.PARAMETER ConfigureOnly
  只跑 CMake 配置，验证选项可用，不编译。

.EXAMPLE
  pwsh -File scripts/build-freerdp.ps1
  pwsh -File scripts/build-freerdp.ps1 -Clean
#>

[CmdletBinding()]
param(
    [switch]$Clean,
    [switch]$ConfigureOnly
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Src = Join-Path $Root 'vendor\freerdp'
$Build = Join-Path $Root 'vendor\build\freerdp-windows-x64'
$Prefix = Join-Path $Root 'vendor\dist\freerdp-windows-x64'

function Write-Step([string]$Message) {
    Write-Host ""
    Write-Host "[build-freerdp] $Message" -ForegroundColor Cyan
}

if (-not (Test-Path (Join-Path $Src 'CMakeLists.txt'))) {
    Write-Error "未找到 FreeRDP 源码（$Src）。先在仓库根执行 scripts\fetch-freerdp.sh，或手动把源码放到 vendor\freerdp。"
}

# ---- 依赖检查 ----------------------------------------------------------
if (-not $env:VCPKG_ROOT) {
    Write-Error @"
未设置 VCPKG_ROOT。

vcpkg 提供 OpenSSL 与 zlib 的静态库，是这套构建的硬依赖：

  git clone https://github.com/microsoft/vcpkg $env:USERPROFILE\vcpkg
  & `"$env:USERPROFILE\vcpkg\bootstrap-vcpkg.bat`"
  [Environment]::SetEnvironmentVariable('VCPKG_ROOT', `"$env:USERPROFILE\vcpkg`", 'User')

设好后**重开终端**再跑本脚本。
"@
}

foreach ($tool in @('cmake', 'ninja')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Error "缺少 $tool。安装：winget install Kitware.CMake / winget install Ninja-build.Ninja"
    }
}

# ---- MSVC 环境 ---------------------------------------------------------
# 两个坑一次解决：
#   1) 本脚本用 Ninja 生成器，不走 MSBuild 的自动探测——没有 cl.exe 时
#      CMake 报的是「No CMAKE_C_COMPILER」，看着像 CMake 装坏了。
#   2) vcpkg 按 v143（VS2022）找工具集；机器上只有 VS2019 时，编 OpenSSL
#      会报「could not find v143」，而这句话不会告诉你该改什么。
# 所以这里先找 vcvars64.bat 把环境导进来，再按 cl.exe 的实际版本钉工具集。
function Find-VcVars {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    $install = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $install) { return $null }
    $vcvars = Join-Path $install 'VC\Auxiliary\Build\vcvars64.bat'
    if (Test-Path $vcvars) { return $vcvars }
    return $null
}

# 用 where.exe 而不是 Get-Command：导入 vcvars 之后 PATH 变了，
# PowerShell 的命令发现在某些版本上会拿旧结果，where 每次真去翻文件系统。
function Get-ClPath {
    $p = & where.exe cl.exe 2>$null | Select-Object -First 1
    if ($p -and (Test-Path $p)) { return $p }
    return $null
}

if (-not (Get-ClPath)) {
    $vcvars = Find-VcVars
    if (-not $vcvars) {
        Write-Error @"
找不到 MSVC 编译器（cl.exe），也没找到带 C++ 生成工具的 Visual Studio。

装 VS2019 或 VS2022 Build Tools，勾「使用 C++ 的桌面开发」（含 MSVC + Windows SDK）：
  winget install Microsoft.VisualStudio.2022.BuildTools

装完重开终端再跑本脚本；也可以手动开「x64 Native Tools Command Prompt」执行。
"@
    }
    Write-Step "导入 MSVC 环境：$vcvars"
    $lines = & cmd /c "`"$vcvars`" >nul 2>&1 && set"
    foreach ($line in @($lines)) {
        if ($line -match '^([^=]+)=(.*)$') {
            Set-Item -Path ("env:" + $Matches[1]) -Value $Matches[2] -ErrorAction SilentlyContinue
        }
    }
    if (-not (Get-ClPath)) {
        Write-Error "导入 vcvars64.bat 后仍找不到 cl.exe。请手动在「x64 Native Tools Command Prompt」里运行本脚本。"
    }
}

if (-not $env:VCPKG_PLATFORM_TOOLSET) {
    $clPath = Get-ClPath
    $clOut = & $clPath 2>&1 | Out-String
    $minor = if ($clOut -match 'Version\s+19\.(\d+)') { [int]$Matches[1] } else { 30 }
    if ($minor -lt 20) {
        Write-Error "检测到 VS2017 或更早的工具集（cl 19.$minor）。FreeRDP 3.x 与 Rust 的 msvc 目标都要求 VS2019 及以上。"
    }
    $env:VCPKG_PLATFORM_TOOLSET = if ($minor -ge 30) { 'v143' } else { 'v142' }
    Write-Step "VCPKG_PLATFORM_TOOLSET=$($env:VCPKG_PLATFORM_TOOLSET)（按 cl.exe 19.$minor 判定；要覆盖就自己设这个环境变量）"
}

Write-Step "安装 vcpkg 依赖（openssl、zlib 的静态版）"
& "$env:VCPKG_ROOT\vcpkg.exe" install openssl:x64-windows-static zlib:x64-windows-static
if ($LASTEXITCODE -ne 0) { Write-Error "vcpkg install 失败" }

if ($Clean -and (Test-Path $Build)) {
    Write-Step "清空构建目录 $Build"
    Remove-Item -Recurse -Force $Build
}

Write-Step "配置：源码=$Src"
Write-Step "配置：构建=$Build"
Write-Step "配置：安装=$Prefix"

# 与 macOS 版的选项逐条对应，改一边时记得改另一边。
$cmakeArgs = @(
    '-S', $Src,
    '-B', $Build,
    '-G', 'Ninja',
    '-DCMAKE_BUILD_TYPE=Release',
    '-DCMAKE_INSTALL_PREFIX=' + $Prefix,
    # 静态 CRT：Rust 侧由仓库根的 .cargo/config.toml 用 +crt-static 钉成 /MT
    # （msvc 目标默认是 /MD），FreeRDP 必须跟着走，否则 LNK2038 运行时不匹配。
    '-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded',
    '-DCMAKE_TOOLCHAIN_FILE=' + (Join-Path $env:VCPKG_ROOT 'scripts\buildsystems\vcpkg.cmake'),
    '-DVCPKG_TARGET_TRIPLET=x64-windows-static',
    '-DBUILD_SHARED_LIBS=OFF',
    '-DFREERDP_UNIFIED_BUILD=ON',
    '-DWITH_VERBOSE_WINPR_ASSERT=OFF',
    # 只要库：客户端可执行文件、服务端、示例全部不要
    '-DWITH_CLIENT_COMMON=ON',
    '-DWITH_CLIENT=OFF',
    '-DWITH_SERVER=OFF',
    '-DWITH_CHANNELS=ON',
    '-DWITH_CLIENT_CHANNELS=ON',
    '-DCHANNEL_URBDRC=OFF',
    '-DWITH_SAMPLE=OFF',
    '-DWITH_MANPAGES=OFF',
    # 音视频与设备重依赖：Daisy v1 不需要
    '-DWITH_FFMPEG=OFF',
    '-DWITH_SWSCALE=OFF',
    '-DWITH_CAIRO=OFF',
    '-DWITH_CUPS=OFF',
    '-DWITH_PCSC=OFF',
    '-DWITH_OPUS=OFF',
    '-DWITH_GSSAPI=OFF',
    '-DWITH_MACAUDIO=OFF',
    '-DWITH_ALSA=OFF',
    '-DWITH_PULSE=OFF',
    # 加密后端：只要 OpenSSL
    '-DWITH_OPENSSL=ON',
    '-DWITH_MBEDTLS=OFF',
    '-DWITH_LIBRESSL=OFF',
    # X11 / Wayland 是 Unix 的窗口系统，Windows 上无关但显式关掉更清楚
    '-DWITH_X11=OFF',
    '-DWITH_WAYLAND=OFF',
    '-DBUILD_TESTING=OFF',
    '-DWITH_CCACHE=OFF',
    '-DWITH_CLANG_FORMAT=OFF'
)

Write-Step "CMake 配置"
& cmake @cmakeArgs
if ($LASTEXITCODE -ne 0) { Write-Error "CMake 配置失败（常见原因：缺 NASM 或 Perl，winget install NASM.NASM / StrawberryPerl.StrawberryPerl）" }

if ($ConfigureOnly) {
    Write-Step "仅配置模式，结束"
    exit 0
}

Write-Step "构建"
& cmake --build $Build --parallel
if ($LASTEXITCODE -ne 0) { Write-Error "构建失败" }

Write-Step "安装到 $Prefix"
& cmake --install $Build
if ($LASTEXITCODE -ne 0) { Write-Error "安装失败" }

Write-Step "完成"
$headers = (Get-ChildItem -Recurse -Filter *.h (Join-Path $Prefix 'include') | Measure-Object).Count
Write-Host "  头文件：$headers 个"
Write-Host "  静态库："
Get-ChildItem (Join-Path $Prefix 'lib') -Filter *.lib |
    ForEach-Object { Write-Host ("    {0}  {1:N1} MB" -f $_.Name, ($_.Length / 1MB)) }
