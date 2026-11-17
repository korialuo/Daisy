<#
.SYNOPSIS
  在 Windows 开发机上装齐 Daisy 的构建依赖（VS 与 Git 需已装好）。

.DESCRIPTION
  只装「Rust 之外」的东西——VS Build Tools 的 MSVC 与 Windows SDK、Git 由
  Visual Studio Installer 负责，本脚本负责其余全部，并且是幂等的：装过的会跳过。

  包管理器：有 winget 用 winget，没有就用 Chocolatey。
  **Win10 LTSC / Windows Server 没有 Microsoft Store，本来就没有 winget**，
  先装 Chocolatey（脚本会打印现成的安装命令）。

  装什么：
    Rust(rustup)  CMake  Ninja  LLVM(libclang)  Strawberry Perl  NASM  PowerShell 7
    vcpkg（含 openssl/zlib 静态库）

  另外做三件影响构建成败/速度的事：
    1. 打开 Win32 长路径（FreeRDP + vcpkg 的构建目录极深，260 字符会拦截）；
    2. git 全局 core.longpaths；
    3. 给 Defender 加构建目录排除（可选，编译速度差一倍以上）。

.PARAMETER VcpkgRoot
  vcpkg 安装位置，默认 C:\dev\vcpkg（短路径，别放用户目录深层）。

.PARAMETER SkipVcpkg
  不装 vcpkg 与 openssl/zlib（只想先把工具链装齐时用）。

.PARAMETER SkipDefenderExclusion
  不加 Defender 排除项。

.EXAMPLE
  # 在「以管理员身份运行」的 PowerShell 里
  pwsh -ExecutionPolicy Bypass -File scripts\install-deps-windows.ps1
#>

[CmdletBinding()]
param(
    [string]$VcpkgRoot = 'C:\dev\vcpkg',
    [switch]$SkipVcpkg,
    [switch]$SkipDefenderExclusion
)

$ErrorActionPreference = 'Stop'

function Write-Step([string]$Message) {
    Write-Host ""
    Write-Host "[deps] $Message" -ForegroundColor Cyan
}

function Write-Ok([string]$Message) { Write-Host "  [OK]   $Message" -ForegroundColor Green }
function Write-Bad([string]$Message) { Write-Host "  [MISS] $Message" -ForegroundColor Red }

# 装完包之后当前进程的 PATH 是旧的，从注册表重新读一遍。
# 与当前会话的 PATH 做并集：如果本来在 VS 开发者环境里（cl.exe 已在 PATH 上），
# 直接覆盖会把 cl.exe 弄丢。
function Update-SessionPath {
    $current = $env:Path -split ';'
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $all = @($current) + ($machine -split ';') + ($user -split ';')
    $seen = New-Object System.Collections.Generic.HashSet[string] ([StringComparer]::OrdinalIgnoreCase)
    $merged = New-Object System.Collections.Generic.List[string]
    foreach ($p in $all) {
        if ([string]::IsNullOrWhiteSpace($p)) { continue }
        if ($seen.Add($p.TrimEnd('\'))) { $merged.Add($p) }
    }
    $env:Path = $merged -join ';'
}

function Find-VsInstall {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $null }
    return (& $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath)
}

# ---- 0. 前置检查 --------------------------------------------------------
$principal = [Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Step "环境"
$osBuild = [Environment]::OSVersion.Version.Build
Write-Host ("  OS      : {0} (build {1})" -f (Get-CimInstance Win32_OperatingSystem).Caption, $osBuild)
Write-Host ("  PowerShell: {0}" -f $PSVersionTable.PSVersion)
if ($osBuild -lt 19041) {
    Write-Host "  提示：系统低于 19041（Win10 LTSC 2019 是 17763）。这类机器：VS2022 装不上（要求 1909+），" -ForegroundColor Yellow
    Write-Host "        用 VS2019，并在 VS Installer 的「单个组件」里勾 Windows 10 SDK 10.0.19041 或更高。" -ForegroundColor Yellow
}
if (-not $isAdmin) {
    Write-Host "  注意：非管理员运行。长路径、Defender 排除会跳过，vcpkg 装到用户目录仍可继续。" -ForegroundColor Yellow
}

$vs = Find-VsInstall
if (-not $vs) {
    Write-Host "  警告：没找到带 C++ 生成工具的 Visual Studio。" -ForegroundColor Yellow
    Write-Host "        先把 VS Installer 跑完（勾「使用 C++ 的桌面开发」），否则后面编不了 C。" -ForegroundColor Yellow
} else {
    Write-Ok "Visual Studio: $vs"
}

# ---- 1. 选包管理器 ------------------------------------------------------
# Win10 LTSC / Windows Server 没有 Microsoft Store，也就没有 winget。
# 这类机器用 Chocolatey：不依赖 Store，离线镜像场景也能用。
Write-Step "选择包管理器"
$backend = $null
if (Get-Command winget -ErrorAction SilentlyContinue) {
    $backend = 'winget'
    Write-Ok "winget $((winget --version) -join '')"
    winget source update 2>&1 | Out-Null
} elseif (Get-Command choco -ErrorAction SilentlyContinue) {
    $backend = 'choco'
    Write-Ok "choco $((choco --version) -join '')"
} else {
    Write-Bad "既没有 winget 也没有 choco"
    $tip = @"
  Win10 LTSC 没有 Microsoft Store，winget 即使装上也可能起不来。用 Chocolatey：
    管理员 PowerShell 里依次执行
      Set-ExecutionPolicy Bypass -Scope Process -Force
      [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
      iex ((New-Object Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
  装完重开一个管理员 PowerShell，再跑本脚本。
"@
    Write-Host $tip -ForegroundColor Yellow
    exit 1
}

if ($backend -eq 'choco' -and -not $isAdmin) {
    Write-Bad "choco 需要管理员权限，请用「以管理员身份运行」重开 PowerShell"
    exit 1
}

# ---- 2. 批量安装 --------------------------------------------------------
# 顺序无关，包管理器自己处理依赖；这里按「越靠前越基础」排。
$packages = @(
    @{ Winget = 'Rustlang.Rustup'; Choco = 'rustup.install'; Name = 'Rust (rustup)' },
    @{ Winget = 'Kitware.CMake'; Choco = 'cmake'; Name = 'CMake' },
    @{ Winget = 'Ninja-build.Ninja'; Choco = 'ninja'; Name = 'Ninja' },
    @{ Winget = 'LLVM.LLVM'; Choco = 'llvm'; Name = 'LLVM (libclang)' },
    @{ Winget = 'StrawberryPerl.StrawberryPerl'; Choco = 'strawberryperl'; Name = 'Strawberry Perl' },
    @{ Winget = 'NASM.NASM'; Choco = 'nasm'; Name = 'NASM' },
    @{ Winget = 'Microsoft.PowerShell'; Choco = 'powershell-core'; Name = 'PowerShell 7' },
    @{ Winget = 'Git.Git'; Choco = 'git'; Name = 'Git for Windows' }
)

foreach ($pkg in $packages) {
    # 单个包失败不中断：最后统一体检，缺什么一目了然。
    if ($backend -eq 'winget') {
        Write-Step "安装 $($pkg.Name)  [winget: $($pkg.Winget)]"
        winget install --id $pkg.Winget -e --silent `
            --accept-package-agreements --accept-source-agreements 2>&1 |
            ForEach-Object { if ($_ -match 'already installed|已安装|No applicable|没有可用的升级') { Write-Ok "$($pkg.Name) 已存在" } }
    } else {
        Write-Step "安装 $($pkg.Name)  [choco: $($pkg.Choco)]"
        choco install -y --no-progress $pkg.Choco 2>&1 |
            ForEach-Object { if ($_ -match 'already installed|已经安装') { Write-Ok "$($pkg.Name) 已存在" } }
    }
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  返回 $LASTEXITCODE（可能已装过或需要交互），继续" -ForegroundColor Yellow
    }
    Update-SessionPath
}

# ---- 3. 长路径 ----------------------------------------------------------
Write-Step "启用 Win32 长路径"
if ($isAdmin) {
    $regPath = 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem'
    $current = (Get-ItemProperty -Path $regPath -Name LongPathsEnabled -ErrorAction SilentlyContinue).LongPathsEnabled
    if ($current -eq 1) {
        Write-Ok "LongPathsEnabled 已开启"
    } else {
        New-ItemProperty -Path $regPath -Name LongPathsEnabled -Value 1 -PropertyType DWord -Force | Out-Null
        Write-Ok "LongPathsEnabled = 1（已写入注册表）"
    }
} else {
    Write-Host "  跳过（需要管理员）" -ForegroundColor Yellow
}

$git = Get-Command git -ErrorAction SilentlyContinue
if ($git) {
    git config --system core.longpaths true 2>&1 | Out-Null
    git config --global core.longpaths true 2>&1 | Out-Null
    Write-Ok "git core.longpaths = true"
} else {
    Write-Bad "git 不在 PATH 里（VS Installer 装完需要重开终端）"
}

# ---- 4. Defender 排除 ---------------------------------------------------
if (-not $SkipDefenderExclusion -and $isAdmin) {
    Write-Step "为构建目录加 Defender 排除（编译速度）"
    $excludeDirs = @('C:\dev', $VcpkgRoot)
    foreach ($dir in $excludeDirs) {
        try {
            Add-MpPreference -ExclusionPath $dir -ErrorAction Stop
            Write-Ok "已排除 $dir"
        } catch {
            Write-Host "  跳过 $dir（$($_.Exception.Message)）" -ForegroundColor Yellow
        }
    }
}

# ---- 5. vcpkg -----------------------------------------------------------
if (-not $SkipVcpkg) {
    Write-Step "vcpkg（OpenSSL / zlib 的静态版）"
    $vcpkgExe = Join-Path $VcpkgRoot 'vcpkg.exe'
    if (-not (Test-Path $vcpkgExe)) {
        if (-not $git) { Write-Bad "没有 git，无法克隆 vcpkg"; exit 1 }
        if (-not (Test-Path $VcpkgRoot)) { New-Item -ItemType Directory -Path $VcpkgRoot -Force | Out-Null }
        git clone --depth 1 https://github.com/microsoft/vcpkg $VcpkgRoot
        if ($LASTEXITCODE -ne 0) { Write-Bad "克隆 vcpkg 失败"; exit 1 }
        & (Join-Path $VcpkgRoot 'bootstrap-vcpkg.bat') -disableMetrics
        if ($LASTEXITCODE -ne 0) { Write-Bad "bootstrap-vcpkg 失败"; exit 1 }
    } else {
        Write-Ok "vcpkg 已存在于 $VcpkgRoot"
    }

    [Environment]::SetEnvironmentVariable('VCPKG_ROOT', $VcpkgRoot, 'User')
    $env:VCPKG_ROOT = $VcpkgRoot
    Write-Ok "VCPKG_ROOT = $VcpkgRoot（已写入用户环境变量，新终端生效）"

    # 工具集跟着实际装的 VS 走：VS2019 只有 v142，不钉住 vcpkg 会去找 v143。
    $cl = Get-Command cl.exe -ErrorAction SilentlyContinue
    if ($cl) {
        if (-not $env:VCPKG_PLATFORM_TOOLSET) {
            $clOut = & cl 2>&1 | Out-String
            $minor = if ($clOut -match 'Version\s+19\.(\d+)') { [int]$Matches[1] } else { 30 }
            $env:VCPKG_PLATFORM_TOOLSET = if ($minor -ge 30) { 'v143' } else { 'v142' }
            Write-Ok "VCPKG_PLATFORM_TOOLSET = $env:VCPKG_PLATFORM_TOOLSET（cl 19.$minor）"
        }
        Write-Step "安装 openssl / zlib 静态库（首次约 5-10 分钟）"
        & $vcpkgExe install 'openssl:x64-windows-static' 'zlib:x64-windows-static'
        if ($LASTEXITCODE -ne 0) {
            Write-Host "  vcpkg install 失败；build-freerdp.ps1 首次构建时会重试（它在 vcvars 环境里跑）" -ForegroundColor Yellow
        } else {
            Write-Ok "openssl / zlib 静态库就绪"
        }
    } else {
        Write-Host "  当前会话没有 cl.exe（没在 VS 开发者环境里），跳过 openssl/zlib 安装。" -ForegroundColor Yellow
        Write-Host "  不影响：scripts\build-freerdp.ps1 首次运行会自动装（它会导入 vcvars64）。" -ForegroundColor Yellow
    }
}

# ---- 6. 体检 ------------------------------------------------------------
Write-Step "体检"
$checks = @(
    @{ Name = 'cl.exe (MSVC)'; Test = { (Get-Command cl.exe -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'rc.exe (资源编译)'; Test = {
            $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
            (Test-Path $sdkRoot) -and (Get-ChildItem $sdkRoot -Recurse -Filter rc.exe -ErrorAction SilentlyContinue | Select-Object -First 1) -ne $null
        } },
    @{ Name = 'cmake'; Test = { (Get-Command cmake -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'ninja'; Test = { (Get-Command ninja -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'nasm'; Test = { (Get-Command nasm -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'perl'; Test = { (Get-Command perl -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'rustup'; Test = { (Get-Command rustup -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'cargo'; Test = { (Get-Command cargo -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'git'; Test = { (Get-Command git -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'pwsh 7'; Test = { (Get-Command pwsh -ErrorAction SilentlyContinue) -ne $null } },
    @{ Name = 'libclang.dll'; Test = {
            if ($env:LIBCLANG_PATH) { return (Test-Path (Join-Path $env:LIBCLANG_PATH 'libclang.dll')) }
            (Test-Path 'C:\Program Files\LLVM\bin\libclang.dll')
        } },
    @{ Name = 'vcpkg.exe'; Test = { (Test-Path (Join-Path $VcpkgRoot 'vcpkg.exe')) } }
)

$missing = @()
foreach ($c in $checks) {
    if (& $c.Test) { Write-Ok $c.Name } else { Write-Bad $c.Name; $missing += $c.Name }
}

Write-Step "结果"
if ($missing.Count -eq 0) {
    Write-Host "  全部就绪。" -ForegroundColor Green
    Write-Host ""
    Write-Host "  下一步（把 Daisy 源码放到 C:\dev\daisy 之后）：" -ForegroundColor Cyan
    Write-Host "    pwsh -File scripts\build-freerdp.ps1 -ConfigureOnly   # 先只验证配置"
    Write-Host "    pwsh -File scripts\build-freerdp.ps1 -Clean          # 再正式编 FreeRDP"
    Write-Host "    pwsh -File scripts\build-windows.ps1                 # 出 exe + zip"
    Write-Host ""
    Write-Host "  注意：PATH 变更需要重开终端（尤其是 cl.exe / cargo / pwsh）。"
} else {
    Write-Host ("  还缺 {0} 项：{1}" -f $missing.Count, ($missing -join ', ')) -ForegroundColor Red
    Write-Host "  多数情况是「装完但 PATH 没刷新」——重开一个终端再跑一次本脚本。" -ForegroundColor Yellow
    Write-Host "  cl.exe 缺失通常是 VS 没勾「使用 C++ 的桌面开发」，或不在 VS 开发者环境里。" -ForegroundColor Yellow
    exit 1
}
