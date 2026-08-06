# assemble-winui.ps1 — 组装 winui 壳运行目录（apps/winui/release/winui-app/）
#
# 布局与 Electron 安装结构对齐（安装器/快捷方式/daemon 发现零改动）：
#   winui-app/
#     DeepX.exe                     ← 壳（安装器硬编码入口名）
#     Microsoft.Web.WebView2.Core.dll / WebView2.Core.dll / WebView2Loader.dll
#     resources/
#       deepx-daemon.exe / deepx-workspace.exe / daemon-manifest.json
#       out/renderer/**             ← daemon 静态服务（<exe_dir>/out/renderer）
#     config/config.toml
#
# 前置：just build-daemon + just build-winui + prepare-daemon.mjs（sidecar）

param(
    [string]$RendererRoot = "apps/winui/out/renderer",
    [string]$SidecarDir = "apps/winui/renderer/build/sidecar",
    [string]$ConfigToml = "apps/installer/payload/config/default.toml",
    [string]$OutDir = "apps/winui/release/winui-app"
)

$ErrorActionPreference = "Stop"
$workspaceRoot = (Resolve-Path ".").Path
$outFull = [System.IO.Path]::GetFullPath((Join-Path $workspaceRoot $OutDir))

Write-Host "=== 组装 winui 运行目录 ==="
if (Test-Path -LiteralPath $outFull) {
    Remove-Item -LiteralPath $outFull -Recurse -Force
}
New-Item -ItemType Directory -Path $outFull -Force | Out-Null

# 1. 壳（命名 DeepX.exe，安装器 create_shortcut 硬编码该入口）
$shellExe = Join-Path $workspaceRoot "target/release/deepx-winui.exe"
if (-not (Test-Path -LiteralPath $shellExe -PathType Leaf)) {
    throw "缺少 winui 壳: $shellExe（先跑 just build-winui）"
}
Copy-Item -LiteralPath $shellExe -Destination (Join-Path $outFull "DeepX.exe")

# 2. self-contained WebView2 / WinAppSDK 运行时 DLL（紧邻 exe）
$releaseDir = Join-Path $workspaceRoot "target/release"
Get-ChildItem -LiteralPath $releaseDir -Filter "*.dll" | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $outFull $_.Name)
}

# 2b. WinAppSDK self-contained 资源文件（Mica/控件渲染必需，缺失则窗口创建失败）
Get-ChildItem -LiteralPath $releaseDir -Filter "*.pri" | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $outFull $_.Name)
}

# 2c. WinAppSDK self-contained 语言资源目录（<lang>/*.mui）。
#     每个语言目录含 Microsoft.ui.xaml.dll.mui / Microsoft.UI.Xaml.Phone.dll.mui。
#     XAML 控件初始化时按系统 UI 语言加载对应 MUI 资源（如中文系统的
#     zh-CN\Microsoft.ui.xaml.dll.mui），缺失会导致 MUI 加载失败
#     （ERROR_MUI_FILE_NOT_LOADED 0x80073B01）→ WebView2/XAML 控件初始化失败
#     → 白屏 + stowed exception 闪退（崩溃模块 Microsoft.ui.xaml.dll）。
#     模式匹配 BCP-47 风格目录名（af-ZA、en-us、az-Latn-AZ、sr-Cyrl-RS 等），
#     不会误伤 build/deps/examples 等 cargo 目录。
Get-ChildItem -LiteralPath $releaseDir -Directory | Where-Object {
    $_.Name -match '^[a-z]{2}(-[A-Za-z0-9]+)*$'
} | ForEach-Object {
    Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $outFull $_.Name) -Recurse -Force
}

# 3. resources/ — daemon sidecar + renderer
$resources = Join-Path $outFull "resources"
New-Item -ItemType Directory -Path $resources -Force | Out-Null
foreach ($f in @("deepx-daemon.exe", "deepx-workspace.exe", "daemon-manifest.json")) {
    $src = Join-Path $workspaceRoot (Join-Path $SidecarDir $f)
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
        throw "缺少 sidecar 文件: $src（先跑 just package-winui-desktop 的 prepare-daemon 步骤）"
    }
    Copy-Item -LiteralPath $src -Destination (Join-Path $resources $f)
}

$rendererSrc = Join-Path $workspaceRoot $RendererRoot
if (-not (Test-Path -LiteralPath $rendererSrc -PathType Container)) {
    throw "缺少 renderer 产物: $rendererSrc（先跑 just build-desktop）"
}
$rendererDest = Join-Path $resources "out/renderer"
New-Item -ItemType Directory -Path $rendererDest -Force | Out-Null
Copy-Item -Path (Join-Path $rendererSrc "*") -Destination $rendererDest -Recurse -Force

# 4. config
$configSrc = Join-Path $workspaceRoot $ConfigToml
if (Test-Path -LiteralPath $configSrc -PathType Leaf) {
    $configDir = Join-Path $outFull "config"
    New-Item -ItemType Directory -Path $configDir -Force | Out-Null
    Copy-Item -LiteralPath $configSrc -Destination (Join-Path $configDir "config.toml")
}

# 5. 剔除运行时产物（WebView2 用户数据目录不进安装包）
$runtimeData = Join-Path $outFull "DeepX.exe.WebView2"
if (Test-Path -LiteralPath $runtimeData) {
    Remove-Item -LiteralPath $runtimeData -Recurse -Force
}

$fileCount = (Get-ChildItem -LiteralPath $outFull -Recurse -File).Count
Write-Host "  ✓ $fileCount 个文件 → $outFull"
