# blackbox installer for Windows.
#
#   irm https://raw.githubusercontent.com/AnakinSkywalker0/blackbox/main/install.ps1 | iex
#
# Downloads the latest installer from GitHub Releases, checks its SHA-256 against the
# published checksum, and runs it. No admin rights needed. Read it before you run it:
# that is the right habit for any script piped into a shell.

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$base = 'https://github.com/AnakinSkywalker0/blackbox/releases/latest/download'
$file = Join-Path $env:TEMP ("bb-setup-" + [guid]::NewGuid().ToString('N') + '.exe')

try {
    Write-Host 'Downloading blackbox...'
    Invoke-WebRequest "$base/bb-setup-x64.exe" -OutFile $file -UseBasicParsing

    $text = (Invoke-WebRequest "$base/bb-setup-x64.exe.sha256" -UseBasicParsing).Content
    if ($text -is [byte[]]) { $text = [Text.Encoding]::ASCII.GetString($text) }
    $want = ($text -split '\s+')[0].Trim().ToLower()
    $got = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $got) { throw "Checksum mismatch, so nothing was installed. Expected $want, got $got." }
    Write-Host 'Checksum OK. Installing...'

    # Wait for the installer itself only, not the recorder it starts in the background.
    $p = Start-Process $file -ArgumentList '/SILENT', '/NORESTART' -PassThru
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "The installer exited with code $($p.ExitCode)." }

    Write-Host ''
    Write-Host 'blackbox is installed and recording. Open a NEW terminal and run:  bb status'
    Write-Host 'When something feels slow:  bb why "10m ago"'
}
finally {
    if (Test-Path $file) { Remove-Item $file -Force -ErrorAction SilentlyContinue }
}
