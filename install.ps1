# Install Varve from GitHub releases with its Windows installer.
# https://github.com/getvarve/varve
#Requires -Version 5.1

$ErrorActionPreference = 'Stop'

# --- presentation: logo, step lines, and a summary box ---------------------------------------
# Varve brand: the violet of the year-rings mark (assets/logo, packages/ui tokens.css).
$script:Stops = @(@(91, 75, 224), @(139, 124, 255), @(179, 169, 255))
$script:Accent = '139;124;255'
$script:Tagline = 'A calm desktop manager for PostgreSQL, MySQL, MariaDB and Redis.'
# The file stays ASCII so Windows PowerShell 5.1 reads it correctly without a BOM:
# # = full block, = | a b c d = the double-line box pieces.
$script:LogoBlock = @(
    '##b   ##b #####b ######b ##b   ##b#######b',
    '##|   ##|##a==##b##a==##b##|   ##|##a====d',
    '##|   ##|#######|######ad##|   ##|#####b  ',
    'c##b ##ad##a==##|##a==##bc##b ##ad##a==d  ',
    ' c####ad ##|  ##|##|  ##| c####ad #######b',
    '  c===d  c=d  c=dc=d  c=d  c===d  c======d'
)
$script:LogoAscii = @(
    '__     __     _      ____   __     __  _____ ',
    '\ \   / /    / \    |  _ \  \ \   / / | ____|',
    ' \ \ / /    / _ \   | |_) |  \ \ / /  |  _|  ',
    '  \ V /    / ___ \  |  _ <    \ V /   | |___ ',
    '   \_/    /_/   \_\ |_| \_\    \_/    |_____|'
)

$script:Esc = [char]27
$script:Vt = $false
try {
    $script:Vt = [bool]$Host.UI.SupportsVirtualTerminal
}
catch {
    $script:Vt = $false
}
if (-not [string]::IsNullOrEmpty($env:WT_SESSION) -or $PSVersionTable.PSVersion.Major -ge 7) {
    $script:Vt = $true
}
$script:Color = $script:Vt -and [string]::IsNullOrEmpty($env:NO_COLOR) -and -not [Console]::IsOutputRedirected
$script:Unicode = $false
if ($script:Vt) {
    try {
        [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
        $script:Unicode = $true
    }
    catch {
        $script:Unicode = $false
    }
}
$script:LogoBlock = $script:LogoBlock | ForEach-Object {
    $_.Replace('#', [char]0x2588).Replace('=', [char]0x2550).Replace('|', [char]0x2551).Replace('a', [char]0x2554).Replace('b', [char]0x2557).Replace('c', [char]0x255A).Replace('d', [char]0x255D)
}
if ($script:Unicode) {
    $script:MarkOk = [string][char]0x2713
    $script:MarkErr = [string][char]0x2717
    $script:Box = @([char]0x256D, [char]0x256E, [char]0x2570, [char]0x256F, [char]0x2500, [char]0x2502)
}
else {
    $script:MarkOk = '+'
    $script:MarkErr = 'x'
    $script:Box = @('+', '+', '+', '+', '-', '|')
}

function Format-VarveStyle {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text,
        [Parameter(Mandatory = $true)][string]$Code
    )
    if (-not $script:Color -or $Text.Length -eq 0) {
        return $Text
    }
    return "$($script:Esc)[$($Code)m$Text$($script:Esc)[0m"
}

function Get-VarveGradient {
    param([int]$Column, [int]$Span, [bool]$Shadow)
    $fraction = $Column / [Math]::Max(1, $Span - 1)
    if ($fraction -lt 0.5) {
        $from = $script:Stops[0]; $to = $script:Stops[1]; $local = $fraction * 2
    }
    else {
        $from = $script:Stops[1]; $to = $script:Stops[2]; $local = ($fraction - 0.5) * 2
    }
    $rgb = 0..2 | ForEach-Object { [int][Math]::Round($from[$_] + ($to[$_] - $from[$_]) * $local) }
    if ($Shadow) {
        $rgb = $rgb | ForEach-Object { [int][Math]::Round($_ * 0.55) }
        return "22;38;2;$($rgb[0]);$($rgb[1]);$($rgb[2])"
    }
    return "1;38;2;$($rgb[0]);$($rgb[1]);$($rgb[2])"
}

function Write-VarveLogo {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param()
    if ($script:Unicode) { $rows = $script:LogoBlock } else { $rows = $script:LogoAscii }
    $span = $rows[0].Length
    $columns = 80
    try {
        $columns = $Host.UI.RawUI.WindowSize.Width
    }
    catch {
        $columns = 80
    }
    Write-Host ''
    if ($span + 2 -le $columns) {
        foreach ($row in $rows) {
            if (-not $script:Color) {
                Write-Host "  $($row.TrimEnd())"
                continue
            }
            $builder = New-Object System.Text.StringBuilder
            [void]$builder.Append('  ')
            $last = ''
            for ($i = 0; $i -lt $row.Length; $i++) {
                $ch = $row[$i]
                if ($ch -eq ' ') {
                    [void]$builder.Append(' ')
                    continue
                }
                $code = Get-VarveGradient -Column $i -Span $span -Shadow ($script:Unicode -and $ch -ne [char]0x2588)
                if ($code -ne $last) {
                    [void]$builder.Append("$($script:Esc)[$($code)m")
                    $last = $code
                }
                [void]$builder.Append($ch)
            }
            [void]$builder.Append("$($script:Esc)[0m")
            Write-Host $builder.ToString()
        }
        Write-Host ''
        Write-Host ''
    }
    Write-Host "  $(Format-VarveStyle -Text $script:Tagline -Code '1')  $(Format-VarveStyle -Text 'installer' -Code '2')"
    Write-Host ''
}

function Write-VarveStep {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param(
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string]$Detail
    )
    $mark = Format-VarveStyle -Text $script:MarkOk -Code '32'
    Write-Host ("  {0} {1} {2}" -f $mark, $Label.PadRight(10), (Format-VarveStyle -Text $Detail -Code '2'))
}

function Write-VarvePending {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param([Parameter(Mandatory = $true)][string]$Label)
    if ($script:Color) {
        Write-Host -NoNewline ("  {0} {1}" -f (Format-VarveStyle -Text '...' -Code "38;2;$($script:Accent)"), $Label)
    }
}

function Clear-VarvePending {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param()
    if ($script:Color) {
        Write-Host -NoNewline "`r$($script:Esc)[2K"
    }
}

function Write-VarveFailure {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param([Parameter(Mandatory = $true)][string]$Message)
    Clear-VarvePending
    $mark = Format-VarveStyle -Text $script:MarkErr -Code '1;31'
    Write-Host ("  {0} {1}" -f $mark, (Format-VarveStyle -Text $Message -Code '31'))
}

function Get-VarveDisplayPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    $userHome = $env:USERPROFILE
    if (-not [string]::IsNullOrEmpty($userHome) -and $Path.StartsWith($userHome, [StringComparison]::OrdinalIgnoreCase)) {
        return '~' + $Path.Substring($userHome.Length)
    }
    return $Path
}

function Write-VarveSummary {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    param([Parameter(Mandatory = $true)][object[]]$Lines)
    # Each line is @(plain, painted).
    $width = ($Lines | ForEach-Object { $_[0].Length } | Measure-Object -Maximum).Maximum
    $columns = 80
    try {
        $columns = $Host.UI.RawUI.WindowSize.Width
    }
    catch {
        $columns = 80
    }
    Write-Host ''
    if ($width + 10 -gt $columns) {
        foreach ($line in $Lines) {
            Write-Host "  $($line[1])"
        }
        Write-Host ''
        return
    }
    $edge = [string]$script:Box[4] * ($width + 6)
    $side = Format-VarveStyle -Text ([string]$script:Box[5]) -Code '2'
    Write-Host ('  ' + (Format-VarveStyle -Text ("$($script:Box[0])$edge$($script:Box[1])") -Code '2'))
    Write-Host ("  $side" + (' ' * ($width + 6)) + $side)
    foreach ($line in $Lines) {
        Write-Host ("  $side   " + $line[1] + (' ' * ($width - $line[0].Length)) + "   $side")
    }
    Write-Host ("  $side" + (' ' * ($width + 6)) + $side)
    Write-Host ('  ' + (Format-VarveStyle -Text ("$($script:Box[2])$edge$($script:Box[3])") -Code '2'))
    Write-Host ''
}

function Get-VarveVersion {
    param([string]$Raw)
    if ([string]::IsNullOrWhiteSpace($Raw)) {
        throw 'empty version'
    }
    if ($Raw -notmatch '^[A-Za-z0-9._-]+$') {
        throw "invalid version: $Raw"
    }
    if ($Raw.StartsWith('v')) {
        return $Raw
    }
    return "v$Raw"
}

# The URL a request ends at after redirects, or $null when the request fails.
function Get-VarveEffectiveUrl {
    param([Parameter(Mandatory = $true)][string]$Url)
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($null -ne $curl) {
        $effective = & curl.exe -fsSL --proto '=https' --tlsv1.2 -o NUL -w '%{url_effective}' $Url
        if ($LASTEXITCODE -ne 0) {
            return $null
        }
        return "$effective"
    }
    try {
        $response = Invoke-WebRequest -Uri $Url -Method Head -UseBasicParsing
    }
    catch {
        return $null
    }
    # Windows PowerShell 5.1 keeps the final URL on ResponseUri, PowerShell 7 on the request message.
    $base = $response.BaseResponse
    if ($null -ne $base.ResponseUri) {
        return $base.ResponseUri.AbsoluteUri
    }
    return $base.RequestMessage.RequestUri.AbsoluteUri
}

function Save-VarveFile {
    param(
        [Parameter(Mandatory = $true)][string]$Url,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
    if ($null -ne $curl) {
        & curl.exe -fsSL --retry 3 --retry-delay 2 --proto '=https' --tlsv1.2 --output $Destination $Url
        if ($LASTEXITCODE -ne 0) {
            throw "download failed: $Url"
        }
        return
    }
    Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing
}

function Get-VarveChecksum {
    param(
        [Parameter(Mandatory = $true)][string]$SumsPath,
        [Parameter(Mandatory = $true)][string]$Asset
    )
    foreach ($line in (Get-Content -Path $SumsPath)) {
        if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(\S+)\s*$') {
            if ($Matches[2] -eq $Asset) {
                return $Matches[1].ToLowerInvariant()
            }
        }
    }
    throw "checksum mismatch: SHA256SUMS has no entry for $Asset"
}

# Where the installer put the app, and whether an old Varve Community install is left next to it.
# The installer reuses the folder of an earlier install, which may not be the default. Its
# uninstall key and default folder are named after the product: Varve, or the name up to 0.1.7,
# which an older release installs under (-LegacyFirst looks there first). CI runs this function
# on its own (.github/workflows/ci.yml, Migration (windows)).
function Find-VarveInstall {
    param(
        [Parameter(Mandatory = $true)][string]$AppName,
        [Parameter(Mandatory = $true)][string]$LegacyName,
        [switch]$LegacyFirst
    )
    $installDir = $null
    $legacyDir = $null
    $names = if ($LegacyFirst) { @($LegacyName, $AppName) } else { @($AppName, $LegacyName) }
    foreach ($name in $names) {
        $dir = Join-Path $env:LOCALAPPDATA $name
        $uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$name"
        $recorded = (Get-ItemProperty -Path $uninstallKey -Name 'InstallLocation' -ErrorAction SilentlyContinue).InstallLocation
        if (-not [string]::IsNullOrWhiteSpace($recorded)) {
            $dir = $recorded.Trim().Trim('"').TrimEnd('\')
        }
        if ($name -eq $LegacyName) {
            $legacyDir = $dir
        }
        if ($null -eq $installDir -and (Test-Path -Path (Join-Path $dir 'varve.exe'))) {
            $installDir = $dir
        }
    }
    $legacyKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$LegacyName"
    return [pscustomobject]@{
        Dir        = $installDir
        LegacyLeft = ($null -ne $installDir) -and ($installDir -ne $legacyDir) -and (Test-Path -Path $legacyKey)
    }
}

try {
    if ($env:OS -ne 'Windows_NT') {
        throw 'unsupported platform: install.ps1 is for Windows. Use install.sh on macOS and Linux.'
    }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if (-not [string]::IsNullOrEmpty($env:PROCESSOR_ARCHITEW6432)) {
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
        throw "unsupported platform: windows $arch. Varve publishes a Windows x64 build."
    }

    Write-VarveLogo

    $repo = 'getvarve/varve'
    $github = "https://github.com/$repo"
    $appName = 'Varve'
    # Up to 0.1.7 the app was called Varve Community. The installer removes that install (see
    # app/src-tauri/windows/hooks.nsh); an older release installs under the old name.
    $legacyName = 'Varve Community'
    $asset = 'varve-windows-x64-setup.exe'

    if (-not [string]::IsNullOrWhiteSpace($env:VARVE_VERSION)) {
        $version = Get-VarveVersion $env:VARVE_VERSION
    }
    else {
        $latestUrl = "$github/releases/latest"
        $effective = Get-VarveEffectiveUrl -Url $latestUrl
        if ([string]::IsNullOrWhiteSpace($effective)) {
            throw "could not find the latest release at $latestUrl"
        }
        if ($effective -notmatch '/releases/tag/([^/?#]+)$') {
            throw "unexpected latest-release URL: $effective"
        }
        $version = Get-VarveVersion $Matches[1]
    }

    Write-VarveStep -Label 'Version' -Detail $version
    if ($arch -eq 'ARM64') {
        Write-VarveStep -Label 'Platform' -Detail 'windows x64 (runs under emulation on ARM64)'
    }
    else {
        Write-VarveStep -Label 'Platform' -Detail 'windows x64'
    }

    # Replacing the files of a running app fails halfway; ask first.
    if ($null -ne (Get-Process -Name 'varve' -ErrorAction SilentlyContinue)) {
        throw "$appName is running. Quit it and run the installer again."
    }

    $tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("varve-install-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
    try {
        $setup = Join-Path $tempRoot $asset
        $sums = Join-Path $tempRoot 'SHA256SUMS'
        $base = "$github/releases/download/$version"
        Write-VarvePending -Label "Downloading $asset"
        Save-VarveFile -Url "$base/$asset" -Destination $setup
        Save-VarveFile -Url "$base/SHA256SUMS" -Destination $sums
        Clear-VarvePending
        $size = (Get-Item -Path $setup).Length / 1MB
        Write-VarveStep -Label 'Download' -Detail ("{0} ({1:N1} MB)" -f $asset, $size)

        $expected = Get-VarveChecksum -SumsPath $sums -Asset $asset
        $actual = (Get-FileHash -Algorithm SHA256 -Path $setup).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "checksum mismatch for ${asset}: expected $expected actual $actual"
        }
        Write-VarveStep -Label 'Checksum' -Detail 'SHA-256 matches SHA256SUMS'

        # The NSIS installer installs for the current user without admin rights; /S runs it silently.
        Write-VarvePending -Label "Installing $appName"
        # -Wait would also wait for every child the installer leaves running (the WebView2
        # bootstrapper can), so wait for the installer process itself. Reading Handle first keeps
        # ExitCode available on Windows PowerShell 5.1.
        $process = Start-Process -FilePath $setup -ArgumentList '/S' -PassThru
        $null = $process.Handle
        $process.WaitForExit()
        Clear-VarvePending
        if ($process.ExitCode -ne 0) {
            throw "the installer exited with code $($process.ExitCode)"
        }
    }
    finally {
        if (Test-Path -Path $tempRoot) {
            Remove-Item -Recurse -Force -Path $tempRoot
        }
    }

    # Releases up to 0.1.7 install as Varve Community.
    $legacyRelease = $false
    $requested = $null
    if ([version]::TryParse($version.TrimStart('v'), [ref]$requested)) {
        $legacyRelease = $requested -le [version]'0.1.7'
    }
    $found = Find-VarveInstall -AppName $appName -LegacyName $legacyName -LegacyFirst:$legacyRelease
    if ($legacyRelease) {
        $appName = $legacyName
    }
    if ($null -eq $found.Dir) {
        throw "the installer finished, but varve.exe is missing from $(Join-Path $env:LOCALAPPDATA $appName)"
    }
    $installDir = $found.Dir
    $exe = Join-Path $installDir 'varve.exe'
    # The installer removes the old app after installing Varve; say so when that did not work.
    $legacyLeft = $found.LegacyLeft
    Write-VarveStep -Label 'Installed' -Detail (Get-VarveDisplayPath -Path $installDir)

    $installed = $version.TrimStart('v')
    $productVersion = (Get-Item -Path $exe).VersionInfo.ProductVersion
    if (-not [string]::IsNullOrWhiteSpace($productVersion)) {
        $installed = $productVersion.Trim()
    }

    $title = "$appName $installed is installed"
    $open = Format-VarveStyle -Text $appName -Code '1'
    $lines = @(
        , @("$($script:MarkOk) $title", "$(Format-VarveStyle -Text $script:MarkOk -Code '1;32') $(Format-VarveStyle -Text $title -Code '1')")
        , @('', '')
        , @("Location  $(Get-VarveDisplayPath -Path $installDir)", "$(Format-VarveStyle -Text 'Location' -Code '2')  $(Get-VarveDisplayPath -Path $installDir)")
        , @("Next      open $appName from the Start menu", "$(Format-VarveStyle -Text 'Next' -Code '2')      open $open from the Start menu")
        , @('Remove    Settings > Apps > Installed apps', "$(Format-VarveStyle -Text 'Remove' -Code '2')    Settings > Apps > Installed apps")
    )
    if ($legacyLeft) {
        $lines += , @("Note      $legacyName is still installed; remove it in Installed apps", "$(Format-VarveStyle -Text 'Note' -Code '2')      $legacyName is still installed; remove it in Installed apps")
    }
    Write-VarveSummary -Lines $lines
}
catch {
    Write-VarveFailure -Message $_.Exception.Message
    exit 1
}
