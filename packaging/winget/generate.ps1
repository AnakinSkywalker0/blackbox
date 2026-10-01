<#
.SYNOPSIS
  Generates the winget manifest files for a blackbox release.

.EXAMPLE
  ./generate.ps1 -Version 0.3.0 -Sha256 <hash of bb-v0.3.0-setup-x64.exe> -OutDir ./out

  Writes out/manifests/a/AnakinSkywalker0/blackbox/0.3.0/*.yaml, ready to submit
  to microsoft/winget-pkgs. See packaging/winget/README.md.
#>
param(
  [Parameter(Mandatory)] [string] $Version,
  [Parameter(Mandatory)] [ValidatePattern('^[0-9A-Fa-f]{64}$')] [string] $Sha256,
  [string] $OutDir = './winget-out',
  [string] $Repo = 'AnakinSkywalker0/blackbox',
  [string] $ReleaseDate = (Get-Date -Format 'yyyy-MM-dd')
)
$ErrorActionPreference = 'Stop'

$id        = 'AnakinSkywalker0.blackbox'
$url       = "https://github.com/$Repo/releases/download/v$Version/bb-v$Version-setup-x64.exe"
# Must match AppId in installer/blackbox.iss, plus Inno's "_is1" suffix.
$productCode = '{6F291038-731F-4998-AADF-C25DCFC0032C}_is1'
$dir = Join-Path $OutDir "manifests/a/AnakinSkywalker0/blackbox/$Version"
New-Item -ItemType Directory -Force $dir | Out-Null

$schema = '1.10.0'
$header = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.{0}.{1}.schema.json
"@

function Write-Manifest($name, $type, $body) {
  $text = ($header -f $type, $schema) + "`n" + $body.Trim() + "`nManifestType: $type`nManifestVersion: $schema`n"
  [IO.File]::WriteAllText((Join-Path $dir "$id.$name"), $text.Replace("`r`n", "`n"), (New-Object Text.UTF8Encoding($false)))
}

Write-Manifest 'yaml' 'version' @"
PackageIdentifier: $id
PackageVersion: $Version
DefaultLocale: en-US
"@

Write-Manifest 'installer.yaml' 'installer' @"
PackageIdentifier: $id
PackageVersion: $Version
InstallerType: inno
Scope: user
InstallModes:
- interactive
- silent
- silentWithProgress
UpgradeBehavior: install
ReleaseDate: $ReleaseDate
Installers:
- Architecture: x64
  InstallerUrl: $url
  InstallerSha256: $($Sha256.ToUpper())
  ProductCode: '$productCode'
"@

Write-Manifest 'locale.en-US.yaml' 'defaultLocale' @"
PackageIdentifier: $id
PackageVersion: $Version
PackageLocale: en-US
Publisher: AnakinSkywalker0
PublisherUrl: https://github.com/$Repo
PublisherSupportUrl: https://github.com/$Repo/issues
PackageName: blackbox
PackageUrl: https://github.com/$Repo
License: MIT
LicenseUrl: https://github.com/$Repo/blob/main/LICENSE
ShortDescription: A flight recorder for your computer's performance. Ask why it was slow.
Description: |-
  blackbox records your machine's vitals in the background (CPU, memory, disk, GPU,
  temperature, battery and the busiest programs). When something felt slow, 'bb why 15:40'
  explains the likely cause in plain English, with evidence. Everything stays on your
  machine and nothing is sent anywhere. The only network use is 'bb update', which runs
  only when you type it.
Moniker: blackbox
Tags:
- performance
- monitoring
- diagnostics
- system-monitor
- troubleshooting
- cli
ReleaseNotesUrl: https://github.com/$Repo/releases/tag/v$Version
"@

Write-Host "Wrote manifests to $dir"
Get-ChildItem $dir | ForEach-Object { Write-Host "  $($_.Name)" }
