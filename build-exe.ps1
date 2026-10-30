#Requires -Version 5.1
<#
.SYNOPSIS
    asset3d 一键打包（Windows x64 / Release）。

.DESCRIPTION
    1. 定位 cargo 并校验可用性；
    2. cargo build --release --bin asset3d --bin asset3d-gui；
    3. 复制两个二进制到 dist\asset3d.exe、dist\asset3d-gui.exe；
    4. 用 dist\asset3d.exe --manifest 生成 dist\plugin.json（UTF-8 无 BOM）；
    5. 打包版本化 zip：dist\asset3d-<version>-win-x64.zip；
    6. 打印全部产物的绝对路径与大小。

    版本号唯一真源：本仓库 Cargo.toml 的 [package] version。
    本脚本只读 Cargo.toml / 源码，不修改任何 git 跟踪的构建配置；
    不额外调用 strip，不改二进制文件名，沿用 Cargo.toml 既有的 [profile.release]。

.PARAMETER SkipZip
    只出二进制与 plugin.json，跳过 zip 打包。

.EXAMPLE
    .\build-exe.ps1
.EXAMPLE
    .\build-exe.ps1 -SkipZip
#>
[CmdletBinding()]
param(
    [switch]$SkipZip
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Rust 的 println 输出 UTF-8 字节，先把控制台解码固定为 UTF-8（无 BOM），否则中文会乱码。
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
try { [Console]::OutputEncoding = $Utf8NoBom } catch { }
$OutputEncoding = $Utf8NoBom

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$TotalSteps = 6
$StepNo = 0
$Step = '准备'
$Watch = [System.Diagnostics.Stopwatch]::StartNew()

function Write-Step {
    param([string]$Title)
    $script:StepNo++
    $script:Step = $Title
    Write-Host ''
    Write-Host ('[{0}/{1}] {2}' -f $script:StepNo, $script:TotalSteps, $Title) -ForegroundColor Cyan
}

function Write-Info {
    param([string]$Message)
    Write-Host ('        ' + $Message) -ForegroundColor DarkGray
}

function Stop-Failed {
    param([string]$Message)
    Write-Host ''
    Write-Host ('[失败] 步骤 {0}：{1}' -f $script:StepNo, $script:Step) -ForegroundColor Red
    Write-Host ('       原因：' + $Message) -ForegroundColor Red
    Write-Host ('       已立即中止，后续步骤未执行。已耗时 {0:N1} 秒。' -f $script:Watch.Elapsed.TotalSeconds) -ForegroundColor Red
    exit 1
}

function Format-Size {
    param([long]$Bytes)
    if ($Bytes -ge 1GB) { return ('{0:N2} GB' -f ($Bytes / 1GB)) }
    if ($Bytes -ge 1MB) { return ('{0:N2} MB' -f ($Bytes / 1MB)) }
    if ($Bytes -ge 1KB) { return ('{0:N1} KB' -f ($Bytes / 1KB)) }
    return ('{0} B' -f $Bytes)
}

Write-Host '============================================================' -ForegroundColor White
Write-Host '  asset3d 一键打包（Windows x64 / Release）' -ForegroundColor White
Write-Host ('  仓库：' + $RepoRoot) -ForegroundColor White
Write-Host '============================================================' -ForegroundColor White

# ---------------------------------------------------------------- 步骤 1
Write-Step '校验构建环境（cargo）'

$Cargo = $null
$cargoCmd = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
if ($cargoCmd) {
    $Cargo = $cargoCmd.Source
}
else {
    $fallback = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (Test-Path -LiteralPath $fallback) { $Cargo = $fallback }
}
if (-not $Cargo) {
    Stop-Failed '未找到 cargo。请安装 Rust 工具链（rustup），或把 %USERPROFILE%\.cargo\bin 加入 PATH 后重跑。'
}
if (-not (Test-Path -LiteralPath $Cargo)) {
    Stop-Failed ('cargo 路径不存在：' + $Cargo)
}

$cargoVersion = (& $Cargo --version) 2>&1
if ($LASTEXITCODE -ne 0) {
    Stop-Failed ('cargo 无法执行（退出码 ' + $LASTEXITCODE + '）：' + ($cargoVersion | Out-String).Trim())
}
Write-Info ('cargo  ：' + $Cargo)
Write-Info ('版本   ：' + ($cargoVersion | Out-String).Trim())

$cargoToml = Join-Path $RepoRoot 'Cargo.toml'
if (-not (Test-Path -LiteralPath $cargoToml)) {
    Stop-Failed ('缺少 Cargo.toml：' + $cargoToml)
}
$tomlText = Get-Content -LiteralPath $cargoToml -Raw -Encoding UTF8

$pkgSection = [regex]::Match($tomlText, '(?ms)^\[package\](?<body>.*?)(?=^\[|\Z)')
if (-not $pkgSection.Success) {
    Stop-Failed 'Cargo.toml 中没有 [package] 段，无法确定版本号。'
}
$verMatch = [regex]::Match($pkgSection.Groups['body'].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $verMatch.Success) {
    Stop-Failed 'Cargo.toml 的 [package] 段没有 version 字段，无法确定版本号。'
}
$Version = $verMatch.Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Version)) {
    Stop-Failed 'Cargo.toml 的 [package] version 为空。'
}
Write-Info ('版本   ：' + $Version + '（唯一真源：Cargo.toml [package] version）')

foreach ($rel in @('src\main.rs', 'src\bin\asset3d-gui.rs')) {
    $full = Join-Path $RepoRoot $rel
    if (-not (Test-Path -LiteralPath $full)) {
        Stop-Failed ('缺少源码入口：' + $full)
    }
}

$DistDir = Join-Path $RepoRoot 'dist'
$ReleaseDir = Join-Path $RepoRoot 'target\release'

# ---------------------------------------------------------------- 步骤 2
Write-Step '编译两个二进制（cargo build --release）'

Push-Location $RepoRoot
try {
    & $Cargo build --release --bin asset3d --bin asset3d-gui
    $buildRc = $LASTEXITCODE
}
finally {
    Pop-Location
}
if ($buildRc -ne 0) {
    Stop-Failed ('cargo build --release 失败（退出码 ' + $buildRc + '）。修复上面的编译错误后重跑。')
}
Write-Info '编译完成。'

# ---------------------------------------------------------------- 步骤 3
Write-Step '复制二进制到 dist\'

if (-not (Test-Path -LiteralPath $DistDir)) {
    New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
}

foreach ($bin in @('asset3d', 'asset3d-gui')) {
    $src = Join-Path $ReleaseDir ($bin + '.exe')
    if (-not (Test-Path -LiteralPath $src)) {
        Stop-Failed ('编译产物缺失：' + $src)
    }
    $srcSize = (Get-Item -LiteralPath $src).Length
    if ($srcSize -le 0) {
        Stop-Failed ('编译产物为 0 字节：' + $src)
    }
    $dst = Join-Path $DistDir ($bin + '.exe')
    Copy-Item -LiteralPath $src -Destination $dst -Force
    Write-Info ('{0}.exe  ->  {1}  ({2})' -f $bin, $dst, (Format-Size $srcSize))
}

# ---------------------------------------------------------------- 步骤 4
Write-Step '生成 plugin.json（asset3d.exe --manifest，UTF-8 无 BOM）'

$ManifestExe = Join-Path $DistDir 'asset3d.exe'
$manifestErrFile = Join-Path $env:TEMP ('asset3d-manifest-{0}.err' -f $PID)

$manifestLines = & $ManifestExe --manifest 2> $manifestErrFile
$manifestRc = $LASTEXITCODE
if ($manifestRc -ne 0) {
    $errText = ''
    if (Test-Path -LiteralPath $manifestErrFile) { $errText = (Get-Content -LiteralPath $manifestErrFile -Raw) }
    if ($errText) { Write-Info ('stderr：' + $errText.Trim()) }
    Stop-Failed ('asset3d.exe --manifest 执行失败（退出码 ' + $manifestRc + '）。')
}

$manifestText = (@($manifestLines) -join "`n")
if ([string]::IsNullOrWhiteSpace($manifestText)) {
    Stop-Failed 'asset3d.exe --manifest 没有任何输出，拒绝生成空的 plugin.json。'
}
$manifestText = $manifestText.TrimEnd() + "`n"

try {
    $manifestJson = $manifestText | ConvertFrom-Json
}
catch {
    Stop-Failed ('--manifest 输出不是合法 JSON：' + $_.Exception.Message)
}
if ($manifestJson.id -ne 'asset3d') {
    Stop-Failed ('清单 id 异常，期望 asset3d，实际：' + $manifestJson.id)
}
if ($manifestJson.version -ne $Version) {
    Stop-Failed ('清单 version（' + $manifestJson.version + '）与 Cargo.toml（' + $Version + '）不一致，拒绝产出漂移的 plugin.json。')
}

$pluginJsonPath = Join-Path $DistDir 'plugin.json'
[System.IO.File]::WriteAllText($pluginJsonPath, $manifestText, $Utf8NoBom)

$head = [System.IO.File]::ReadAllBytes($pluginJsonPath)
if ($head.Length -ge 3 -and $head[0] -eq 0xEF -and $head[1] -eq 0xBB -and $head[2] -eq 0xBF) {
    Stop-Failed 'plugin.json 写入后带 UTF-8 BOM，与约定不符。'
}
Write-Info ('plugin.json  ->  ' + $pluginJsonPath + '  (' + (Format-Size $head.Length) + '，无 BOM)')

if (Test-Path -LiteralPath $manifestErrFile) { Remove-Item -LiteralPath $manifestErrFile -Force -ErrorAction SilentlyContinue }

# ---------------------------------------------------------------- 步骤 5
$ZipPath = $null
if ($SkipZip) {
    Write-Step '跳过 zip 打包（-SkipZip）'
    Write-Info '未生成 zip。'
}
else {
    Write-Step '打包版本化 zip'

    $ZipPath = Join-Path $DistDir ('asset3d-{0}-win-x64.zip' -f $Version)
    $zipItems = @(
        (Join-Path $DistDir 'asset3d.exe'),
        (Join-Path $DistDir 'asset3d-gui.exe'),
        (Join-Path $DistDir 'plugin.json')
    )
    try {
        Compress-Archive -Path $zipItems -DestinationPath $ZipPath -CompressionLevel Optimal -Force
    }
    catch {
        Stop-Failed ('压缩失败：' + $_.Exception.Message)
    }
    $zipSize = (Get-Item -LiteralPath $ZipPath).Length
    if ($zipSize -le 0) {
        Stop-Failed ('zip 产物为 0 字节：' + $ZipPath)
    }
    Write-Info ('{0}  ({1})' -f $ZipPath, (Format-Size $zipSize))
}

# ---------------------------------------------------------------- 步骤 6
Write-Step '汇总产物'

$Watch.Stop()
$elapsed = $Watch.Elapsed

$artifacts = @(
    (Join-Path $DistDir 'asset3d.exe'),
    (Join-Path $DistDir 'asset3d-gui.exe'),
    $pluginJsonPath
)
if ($ZipPath) { $artifacts += $ZipPath }

foreach ($item in $artifacts) {
    if (-not (Test-Path -LiteralPath $item)) {
        Stop-Failed ('产物缺失：' + $item)
    }
}

Write-Host ''
Write-Host ('[完成] asset3d 打包成功（版本 ' + $Version + '）') -ForegroundColor Green
Write-Host '产物（绝对路径）与大小：' -ForegroundColor Green
foreach ($item in $artifacts) {
    $size = (Get-Item -LiteralPath $item).Length
    Write-Host ('  {0}' -f $item) -ForegroundColor White
    Write-Host ('      {0}' -f (Format-Size $size)) -ForegroundColor DarkGray
}
Write-Host ''
Write-Host ('zip 同名解压即得插件目录三件套；plugin.json 已由二进制的 --manifest 生成，与代码内工具表保持同源。') -ForegroundColor DarkGray
Write-Host ('总耗时：{0:N1} 秒' -f $elapsed.TotalSeconds) -ForegroundColor Green
exit 0
