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

# 先把用户配的 vcpkg 位置钉死：vcvars64.bat 会把它**改成 VS 自带的那个**
# （`VCPKG_ROOT=<VS>\VC\vcpkg`），而 VS 自带的是「只有清单模式」的实例，
# 接着调 vcpkg 就报「does not have a classic mode instance」——看起来像 vcpkg
# 装坏了，其实是路径被换掉了。
$VcpkgRoot = $env:VCPKG_ROOT

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
# 返回 vcvars64.bat 路径 + 该 VS 的**主版本号**（16=2019 / 17=2022 / 18=2026）。
# 主版本号是唯一能可靠区分工具集的依据：
#   * 用 cl.exe 的 19.x 小版本号判不出来——VS2022 17.10 之后也发 19.4x，
#     而它的工具集仍然叫 v143；VS2026 才是 v144。
#   * `-requires VC.Tools.x86.x64` 会把没装 C++ 工作负载的实例过滤掉，
#     否则会选中一个连 VC 目录都没有的 VS（本机装 VS2026 时就踩过）。
function Find-VcVars {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    $install = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if (-not $install) { return $null }
    $vcvars = Join-Path $install 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) { return $null }
    $ver = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationVersion
    $major = if ($ver -match '^(\d+)\.') { [int]$Matches[1] } else { 17 }
    return [pscustomobject]@{ VcVars = $vcvars; Major = $major; Install = $install }
}

# 用 where.exe 而不是 Get-Command：导入 vcvars 之后 PATH 变了，
# PowerShell 的命令发现在某些版本上会拿旧结果，where 每次真去翻文件系统。
#
# 必须走 `cmd /c ... 2>nul`：PowerShell 5.1 里原生程序的 stderr 会被包成
# ErrorRecord，本脚本开了 $ErrorActionPreference='Stop'，找不到 cl.exe 时
# `& where.exe cl.exe 2>$null` 会**直接终结整个脚本**——表现为「什么都没干就退出」。
function Get-ClPath {
    $p = (& cmd /c 'where cl.exe 2>nul' | Select-Object -First 1)
    if ($p -and (Test-Path $p)) { return $p }
    return $null
}

if (-not (Get-ClPath)) {
    $found = Find-VcVars
    if (-not $found) {
        Write-Error @"
找不到 MSVC 编译器（cl.exe），也没找到带 C++ 生成工具的 Visual Studio。

装 VS2022 Build Tools，勾「使用 C++ 的桌面开发」（含 MSVC + Windows SDK）：
  https://aka.ms/vs/17/release/vs_BuildTools.exe

静默安装（管理员 PowerShell）：
  .\vs_BuildTools.exe --quiet --wait --norestart --nocache --installPath C:\BT2022 ``
      --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended

装完重开终端再跑本脚本；也可以手动开「x64 Native Tools Command Prompt」执行。
"@
    }
    $vcvars = $found.VcVars
    Write-Step "导入 MSVC 环境：$vcvars（VS 主版本 $($found.Major)）"
    $lines = & cmd /c "`"$vcvars`" >nul 2>&1 && set"
    foreach ($line in @($lines)) {
        if ($line -match '^([^=]+)=(.*)$') {
            Set-Item -Path ("env:" + $Matches[1]) -Value $Matches[2] -ErrorAction SilentlyContinue
        }
    }
    # vcvars64.bat 覆盖了 VCPKG_ROOT（见上面 $VcpkgRoot 的注释），这里改回来
    $env:VCPKG_ROOT = $VcpkgRoot
    if (-not (Get-ClPath)) {
        Write-Error "导入 vcvars64.bat 后仍找不到 cl.exe。请手动在「x64 Native Tools Command Prompt」里运行本脚本。"
    }
}

if (-not $env:VCPKG_PLATFORM_TOOLSET) {
    $clPath = Get-ClPath
    $clOut = & $clPath 2>&1 | Out-String
    if ($clOut -match 'Version\s+19\.(\d+)' -and [int]$Matches[1] -lt 20) {
        Write-Error "检测到 VS2017 或更早的工具集（cl 19.$($Matches[1])）。FreeRDP 3.x 与 Rust 的 msvc 目标都要求 VS2019 及以上。"
    }
    # 工具集名跟着 VS 主版本走，不跟 cl 的小版本走（见 Find-VcVars 的注释）。
    # cl.exe 本来就在 PATH 上的情况（从「Native Tools」提示符里跑）也要问一次 vswhere。
    if (-not $found) { $found = Find-VcVars }
    $major = if ($found) { $found.Major } else { 17 }
    $env:VCPKG_PLATFORM_TOOLSET = switch ($major) {
        16 { 'v142' }
        17 { 'v143' }
        18 { 'v144' }
        default { 'v143' }
    }
    Write-Step "VCPKG_PLATFORM_TOOLSET=$($env:VCPKG_PLATFORM_TOOLSET)（VS 主版本 $major；要覆盖就自己设这个环境变量）"
}

Write-Step "安装 vcpkg 依赖（openssl、zlib 的静态版）"
# 两个坑：
#   * `--classic`——2025 年之后的 vcpkg-tool 在「没有 vcpkg.json 的目录」里默认按
#     清单模式处理，直接报「Could not locate a manifest (vcpkg.json) above the
#     current working directory / This vcpkg distribution does not have a classic
#     mode instance」。我们从仓库根调用，本来就没有清单，必须显式要经典模式。
#   * 走 `cmd /c`——本脚本 `$ErrorActionPreference = 'Stop'`，而 PowerShell 5.1
#     会把原生程序写到 stderr 的每一行包成 ErrorRecord；vcpkg 的进度/警告都走
#     stderr，直接把脚本打断在半路（现象就是「明明手工跑得通，脚本却报错退出」）。
$vcpkgCmd = "`"$VcpkgRoot\vcpkg.exe`" install --classic openssl:x64-windows-static zlib:x64-windows-static"
& cmd /c $vcpkgCmd
if ($LASTEXITCODE -ne 0) { Write-Error "vcpkg install 失败" }

if ($Clean -and (Test-Path $Build)) {
    Write-Step "清空构建目录 $Build"
    Remove-Item -Recurse -Force $Build
}

Write-Step "配置：源码=$Src"
Write-Step "配置：构建=$Build"
Write-Step "配置：安装=$Prefix"

# 与 macOS 版的选项逐条对应，改一边时记得改另一边。
#
# 带变量的参数一律用 `-f` 拼成**一个**字符串再放进数组：`'前缀' + (Join-Path ...)`
# 这种写法在 PowerShell 5.1 里可能被拆成两个 argv（cmake 会报
# 「Ignoring extra path from command line」而且工具链根本没生效）。
$toolchainFile = Join-Path $VcpkgRoot 'scripts\buildsystems\vcpkg.cmake'
$cmakeArgs = @(
    '-S', $Src,
    '-B', $Build,
    '-G', 'Ninja',
    '-DCMAKE_BUILD_TYPE=Release',
    ('-DCMAKE_INSTALL_PREFIX={0}' -f $Prefix),
    # 静态 CRT：Rust 侧由仓库根的 .cargo/config.toml 用 +crt-static 钉成 /MT
    # （msvc 目标默认是 /MD），FreeRDP 必须跟着走，否则 LNK2038 运行时不匹配。
    '-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded',
    ('-DCMAKE_TOOLCHAIN_FILE={0}' -f $toolchainFile),
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
