<#
.SYNOPSIS
    DeepX MSIX 壳包（external location）构建/注册/卸载脚本。

.DESCRIPTION
    壳包只含 manifest + 图标，为 %LOCALAPPDATA%\Programs\DeepX 的现有目录部署
    提供 MSIX Identity（AUMID = DeepX_<pubhash>!DeepX），并声明 Windows App
    Runtime 2.3.1 框架依赖——系统解析依赖，通知等 WinAppSDK API 无需
    Bootstrap / 手动注册 Singleton。

    证书策略：
    - 提供 -CertPfx/-CertPassword：用现有证书签名（发布场景）
    - 否则自签开发证书并（弹 UAC）导入机器级受信任根（Add-AppxPackage 要求）

.PARAMETER Action
    build | register | unregister | all （默认 all）

.PARAMETER InstallDir
    外部位置（真实安装目录）。默认 %LOCALAPPDATA%\Programs\DeepX。

.PARAMETER CertPfx / CertPassword
    已有代码签名证书。

.EXAMPLE
    # 构建 + 注册（自签证书，弹一次 UAC）
    .\build-msix-shell.ps1 -Action all

.EXAMPLE
    # 发布：用商业证书构建并注册
    .\build-msix-shell.ps1 -Action all -CertPfx C:\certs\deepx.pfx -CertPassword ***
#>
param(
    [ValidateSet("build", "register", "unregister", "all")]
    [string]$Action = "all",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\DeepX",
    [string]$CertPfx = "",
    [string]$CertPassword = "",
    [switch]$SkipTrust
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot          # apps/installer
$MsixDir = Join-Path $Root "msix"
$SrcDir = Join-Path $MsixDir "msix-src"
$Pkg = Join-Path $MsixDir "DeepX-shell.msix"
$Kits = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64"
$MakeAppx = Join-Path $Kits "makeappx.exe"
$SignTool = Join-Path $Kits "signtool.exe"
$Manifest = Join-Path $SrcDir "AppxManifest.xml"

function Assert-Tool([string]$Tool, [string]$Name) {
    if (-not (Test-Path $Tool)) { throw "$Name 未找到: $Tool" }
}

function Build-Package {
    Assert-Tool $MakeAppx "makeappx"
    Assert-Tool $SignTool "signtool"
    if (-not (Test-Path $Manifest)) { throw "缺少 manifest: $Manifest" }

    # 打包（/nv：壳包引用外部 exe，跳过打包期文件校验）
    & $MakeAppx pack /nv /d $SrcDir /p $Pkg /o 
    if ($LASTEXITCODE -ne 0) { throw "makeappx pack 失败" }

    # 证书
    $pfx = $CertPfx
    if (-not $pfx) {
        $pfx = Join-Path $MsixDir "DeepX.pfx"
        if (-not (Test-Path $pfx)) { New-DevCert $pfx }
        $CertPassword = "DeepX-spike-2026"
    }

    # 签名
    & $SignTool sign /fd SHA256 /f $pfx /p $CertPassword $Pkg 
    if ($LASTEXITCODE -ne 0) { throw "signtool 签名失败" }
    Write-Host "[msix] 打包+签名完成: $Pkg"
}

function New-DevCert([string]$PfxPath) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=DeepX" `
        -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(3)
    $cer = Join-Path $MsixDir "DeepX.cer"
    Export-Certificate -Cert $cert -FilePath $cer 
    $pwd = ConvertTo-SecureString "DeepX-spike-2026" -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $pwd 
    if (-not $SkipTrust) {
        # Add-AppxPackage 校验机器级受信任根（弹一次 UAC）
        Start-Process certutil -ArgumentList "-addstore", "Root", $cer -Verb RunAs -Wait
    }
    Write-Host "[msix] 已自签开发证书并（如需）导入机器级受信任根"
}

function Register-Package {
    if (-not (Test-Path $Pkg)) { throw "先构建：$Pkg" }
    if (-not (Test-Path (Join-Path $InstallDir "DeepX.exe"))) {
        throw "外部位置缺少 DeepX.exe: $InstallDir"
    }
    Add-AppxPackage -Path $Pkg -ExternalLocation $InstallDir
    $pkg = Get-AppxPackage -Name "DeepX"
    Write-Host "[msix] 注册 OK: $($pkg.PackageFullName)"
    Write-Host "[msix] AUMID: DeepX_$($pkg.PublisherId)!DeepX"
}

function Unregister-Package {
    $pkg = Get-AppxPackage -Name "DeepX" -ErrorAction SilentlyContinue
    if ($pkg) {
        Remove-AppxPackage -Package $pkg.PackageFullName
        Write-Host "[msix] 卸载 OK: $($pkg.PackageFullName)"
    } else {
        Write-Host "[msix] 未注册，跳过卸载"
    }
}

switch ($Action) {
    "build"      { Build-Package }
    "register"   { Register-Package }
    "unregister" { Unregister-Package }
    "all" {
        Build-Package
        Register-Package
    }
}
