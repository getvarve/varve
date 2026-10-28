# Upgrades a Varve Community 0.1.7 install on Windows to the Varve built from this commit, the way
# a user does, and checks what is left (app/src-tauri/windows/hooks.nsh, install.ps1). CI runs it
# on a fresh runner (.github/workflows/ci.yml, Migration (windows)); it changes the machine.
#
# Usage: pwsh -File scripts/ci/migration-windows.ps1 -Kind nsis|nsis-update|msi -Old <0.1.7 installer> -New <new installer>
#   nsis: the per-user setups, the new one run as a fresh install, plus install.ps1.
#   nsis-update: the per-user setups, the new one run the way the 0.1.7 in-app updater runs it
#     (/P /UPDATE), while the old app is still quitting.
#   msi: the per-machine MSI packages.
#Requires -Version 7

param(
    [Parameter(Mandatory = $true)][ValidateSet('nsis', 'nsis-update', 'msi')][string]$Kind,
    [Parameter(Mandatory = $true)][string]$Old,
    [Parameter(Mandatory = $true)][string]$New
)

$ErrorActionPreference = 'Stop'

$appName = 'Varve'
$legacyName = 'Varve Community'
$uninstallRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
$dataDir = Join-Path $env:APPDATA 'Varve'
# The old uninstaller deletes these two (named after the bundle id) when its app data box is
# ticked. The WebView2 profile, with the app's local storage, lives in the second.
$bundleDirs = @((Join-Path $env:APPDATA 'com.getvarve.desktop'), (Join-Path $env:LOCALAPPDATA 'com.getvarve.desktop'))
$taskbarPins = Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'
$startMenu = [Environment]::GetFolderPath('Programs')
$desktop = [Environment]::GetFolderPath('Desktop')
$failures = [System.Collections.Generic.List[string]]::new()

function Write-Section {
    param([string]$Title)
    Write-Host ''
    Write-Host "==== $Title"
}

function Assert-That {
    param([bool]$Condition, [string]$What)
    if ($Condition) {
        Write-Host "PASS  $What"
    }
    else {
        Write-Host "FAIL  $What"
        $failures.Add($What)
    }
}

# Runs an installer and waits for it, not for every child it leaves running. Reading Handle first
# keeps ExitCode available.
function Invoke-Installer {
    param([string]$File, [string[]]$Arguments)
    Write-Host "> $File $($Arguments -join ' ')"
    $process = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru
    $null = $process.Handle
    $process.WaitForExit()
    Write-Host "  exit code $($process.ExitCode)"
    if ($process.ExitCode -ne 0) {
        throw "$File exited with code $($process.ExitCode)"
    }
}

# Every installed program whose display name is Varve or Varve Community, from the per-user and the
# per-machine (64 and 32 bit) uninstall keys.
function Get-VarveProduct {
    $roots = @(
        $uninstallRoot,
        'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    foreach ($root in $roots) {
        Get-ChildItem -Path $root -ErrorAction SilentlyContinue | ForEach-Object {
            $item = Get-ItemProperty -Path $_.PSPath
            if ($item.DisplayName -in @($appName, $legacyName)) {
                [pscustomobject]@{
                    Key             = $_.Name
                    DisplayName     = $item.DisplayName
                    DisplayVersion  = $item.DisplayVersion
                    InstallLocation = $item.InstallLocation
                    UninstallString = $item.UninstallString
                }
            }
        }
    }
}

function Get-InstallLocation {
    param([string]$Name)
    $recorded = (Get-ItemProperty -Path "$uninstallRoot\$Name" -Name 'InstallLocation' -ErrorAction SilentlyContinue).InstallLocation
    if ([string]::IsNullOrWhiteSpace($recorded)) {
        return $null
    }
    return $recorded.Trim().Trim('"').TrimEnd('\')
}

function Show-State {
    param([string]$Title)
    Write-Section $Title
    Write-Host '-- Varve products in the uninstall keys'
    Get-VarveProduct | Format-List | Out-String -Width 200 | Write-Host
    Write-Host '-- HKCU\Software\getvarve'
    Get-ChildItem -Path 'HKCU:\Software\getvarve' -Recurse -ErrorAction SilentlyContinue | Out-String -Width 200 | Write-Host
    foreach ($dir in @(
            (Join-Path $env:LOCALAPPDATA $appName),
            (Join-Path $env:LOCALAPPDATA $legacyName),
            (Join-Path $env:ProgramFiles $appName),
            (Join-Path $env:ProgramFiles $legacyName),
            $dataDir,
            $bundleDirs[0],
            $bundleDirs[1])) {
        if (Test-Path -Path $dir) {
            Write-Host "-- $dir"
            Get-ChildItem -Path $dir -Force | Format-Table -AutoSize Mode, Length, Name | Out-String -Width 200 | Write-Host
        }
        else {
            Write-Host "-- $dir (missing)"
        }
    }
    Write-Host '-- Shortcuts'
    Get-ChildItem -Path $startMenu, $desktop, $taskbarPins -Filter 'Varve*.lnk' -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "   $($_.FullName) -> $(Get-ShortcutTarget $_.FullName)" }
}

function Get-ShortcutTarget {
    param([string]$Path)
    if (-not (Test-Path -Path $Path)) {
        return $null
    }
    return (New-Object -ComObject WScript.Shell).CreateShortcut($Path).TargetPath
}

# Writes a marker in the Varve data folder and in both bundle id folders.
function Write-DataMarker {
    Write-Section 'App data markers'
    foreach ($dir in @($dataDir) + $bundleDirs) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $file = Join-Path $dir 'migration-marker.txt'
        Set-Content -Path $file -Value 'written before the upgrade'
        Write-Host "wrote $file"
    }
}

function Assert-DataMarker {
    foreach ($dir in @($dataDir) + $bundleDirs) {
        $file = Join-Path $dir 'migration-marker.txt'
        Assert-That (Test-Path -Path $file) "the app data marker $file is still there"
    }
}

# install.ps1 defines Find-VarveInstall next to its install code. This returns that function's
# definition, to dot-source and run on its own.
function Get-FindVarveInstall {
    $root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    $script = Join-Path $root 'install.ps1'
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($script, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) {
        throw "install.ps1 does not parse: $($errors -join '; ')"
    }
    $definition = $ast.Find({
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Find-VarveInstall'
        }, $true)
    if ($null -eq $definition) {
        throw 'install.ps1 has no Find-VarveInstall function'
    }
    return [scriptblock]::Create($definition.Extent.Text)
}

function Test-Nsis {
    param([bool]$Update)
    Show-State 'Before'

    Write-Section 'Install Varve Community 0.1.7 (NSIS, silent)'
    Invoke-Installer -File $Old -Arguments @('/S')
    $legacyDir = Get-InstallLocation -Name $legacyName
    Write-Host "InstallLocation of $legacyName`: $legacyDir"
    Assert-That (Test-Path -Path "$uninstallRoot\$legacyName") "the uninstall key '$legacyName' exists"
    Assert-That ($null -ne $legacyDir -and (Test-Path -Path (Join-Path $legacyDir 'varve.exe'))) "varve.exe is in the $legacyName folder"
    Assert-That (Test-Path -Path (Join-Path $startMenu "$legacyName.lnk")) "the Start menu has '$legacyName.lnk'"
    $hadDesktop = Test-Path -Path (Join-Path $desktop "$legacyName.lnk")
    Write-Host "desktop shortcut '$legacyName.lnk': $hadDesktop"

    # Pinning to the taskbar puts a copy of the shortcut here. Only the file is made: Windows
    # offers no supported way to pin from a script, and the hook only looks at the file.
    Write-Section 'Taskbar pin'
    New-Item -ItemType Directory -Force -Path $taskbarPins | Out-Null
    $pin = Join-Path $taskbarPins "$legacyName.lnk"
    Copy-Item -Path (Join-Path $startMenu "$legacyName.lnk") -Destination $pin
    Write-Host "$pin -> $(Get-ShortcutTarget $pin)"

    if (-not $Update) {
        Write-Section 'install.ps1 over the old install (VARVE_VERSION=v0.1.7)'
        # The whole script, as a user runs it: download, checksum, silent install, and finding the
        # install under the old name.
        $root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
        $env:VARVE_VERSION = 'v0.1.7'
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'install.ps1')
        $installExit = $LASTEXITCODE
        Remove-Item -Path 'env:VARVE_VERSION'
        Assert-That ($installExit -eq 0) "install.ps1 v0.1.7 exits with 0 (got $installExit)"
        . (Get-FindVarveInstall)
        $found = Find-VarveInstall -AppName $appName -LegacyName $legacyName
        Write-Host "Find-VarveInstall: Dir=$($found.Dir) LegacyLeft=$($found.LegacyLeft)"
        Assert-That ($found.Dir -eq $legacyDir) 'Find-VarveInstall finds the old install under its old name'
        Assert-That (-not $found.LegacyLeft) 'Find-VarveInstall reports no leftover while only the old install exists'
    }

    Write-DataMarker

    Show-State 'With Varve Community 0.1.7'

    if ($Update) {
        # The in-app updater starts the setup with these arguments and then quits the app. Start
        # the old app and stop it a few seconds after the setup started, which the setup waits for.
        Write-Section 'Install Varve from this commit (NSIS, /P /UPDATE, old app quitting)'
        $app = Start-Process -FilePath (Join-Path $legacyDir 'varve.exe') -PassThru
        Start-Sleep -Seconds 5
        Assert-That (-not $app.HasExited) 'the old app is running'
        Write-Host "> $New /P /UPDATE"
        $setup = Start-Process -FilePath $New -ArgumentList @('/P', '/UPDATE') -PassThru
        $null = $setup.Handle
        Start-Sleep -Seconds 3
        Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
        Get-Process -Name 'varve' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        $setup.WaitForExit()
        Write-Host "  exit code $($setup.ExitCode)"
        if ($setup.ExitCode -ne 0) {
            throw "$New exited with code $($setup.ExitCode)"
        }
    }
    else {
        Write-Section 'Install Varve from this commit (NSIS, silent)'
        Invoke-Installer -File $New -Arguments @('/S')
    }

    Show-State 'After the upgrade'

    Write-Section 'Checks'
    $newDir = Get-InstallLocation -Name $appName
    Write-Host "InstallLocation of $appName`: $newDir"
    Assert-That (Test-Path -Path "$uninstallRoot\$appName") "the uninstall key '$appName' exists"
    Assert-That ($null -ne $newDir -and (Test-Path -Path (Join-Path $newDir 'varve.exe'))) "its InstallLocation holds varve.exe"
    Assert-That (-not (Test-Path -Path "$uninstallRoot\$legacyName")) "the uninstall key '$legacyName' is gone"
    Assert-That (-not (Test-Path -Path $legacyDir)) "the folder $legacyDir is gone"
    Assert-That (-not (Test-Path -Path "HKCU:\Software\getvarve\$legacyName")) "HKCU\Software\getvarve\$legacyName is gone"
    Assert-DataMarker
    $newExe = if ($null -ne $newDir) { Join-Path $newDir 'varve.exe' } else { '(none)' }
    Assert-That ((Get-ShortcutTarget (Join-Path $startMenu "$appName.lnk")) -eq $newExe) "the Start menu has '$appName.lnk', pointing at $newExe"
    Assert-That (-not (Test-Path -Path (Join-Path $startMenu "$legacyName.lnk"))) "the Start menu has no '$legacyName.lnk'"
    Assert-That (-not (Test-Path -Path (Join-Path $desktop "$legacyName.lnk"))) "the desktop has no '$legacyName.lnk'"
    if ($hadDesktop) {
        Assert-That ((Get-ShortcutTarget (Join-Path $desktop "$appName.lnk")) -eq $newExe) "the desktop has '$appName.lnk', pointing at $newExe"
    }
    Assert-That ((Get-ShortcutTarget $pin) -eq $newExe) "the taskbar pin points at $newExe"
    . (Get-FindVarveInstall)
    $found = Find-VarveInstall -AppName $appName -LegacyName $legacyName
    Write-Host "Find-VarveInstall: Dir=$($found.Dir) LegacyLeft=$($found.LegacyLeft)"
    Assert-That ($found.Dir -eq $newDir) 'Find-VarveInstall finds the new install'
    Assert-That (-not $found.LegacyLeft) 'Find-VarveInstall reports no leftover old install'
    $products = @(Get-VarveProduct)
    Assert-That ($products.Count -eq 1 -and $products[0].DisplayName -eq $appName) 'exactly one Varve product is installed, named Varve'
}

function Test-Msi {
    Show-State 'Before'

    Write-Section 'Install Varve Community 0.1.7 (MSI, quiet)'
    $logs = Join-Path $env:RUNNER_TEMP 'msi-logs'
    New-Item -ItemType Directory -Force -Path $logs | Out-Null
    Invoke-Installer -File 'msiexec.exe' -Arguments @('/i', "`"$Old`"", '/qn', '/norestart', '/l*v', "`"$(Join-Path $logs 'old.log')`"")
    $products = @(Get-VarveProduct)
    Assert-That ($products.Count -eq 1 -and $products[0].DisplayName -eq $legacyName) "exactly one product is installed, named '$legacyName'"

    Write-DataMarker

    Show-State 'With Varve Community 0.1.7'

    Write-Section 'Install Varve from this commit (MSI, quiet)'
    Invoke-Installer -File 'msiexec.exe' -Arguments @('/i', "`"$New`"", '/qn', '/norestart', '/l*v', "`"$(Join-Path $logs 'new.log')`"")

    Show-State 'After the upgrade'

    Write-Section 'Checks'
    $products = @(Get-VarveProduct)
    Assert-That ($products.Count -eq 1) "exactly one Varve product is installed (found $($products.Count))"
    Assert-That ($products.Count -ge 1 -and $products[0].DisplayName -eq $appName) "it is named '$appName'"
    $exes = @(Get-ChildItem -Path (Join-Path $env:ProgramFiles $appName), (Join-Path $env:ProgramFiles $legacyName) -Filter 'varve.exe' -ErrorAction SilentlyContinue)
    $exes | ForEach-Object { Write-Host "   $($_.FullName)" }
    Assert-That ($exes.Count -eq 1 -and $exes[0].Directory.Name -eq $appName) "one varve.exe, in Program Files\$appName"
    Assert-DataMarker
}

try {
    foreach ($file in @($Old, $New)) {
        if (-not (Test-Path -Path $file)) {
            throw "missing installer: $file"
        }
    }
    if ($Kind -eq 'nsis') {
        Test-Nsis -Update $false
    }
    elseif ($Kind -eq 'nsis-update') {
        Test-Nsis -Update $true
    }
    else {
        Test-Msi
    }
}
catch {
    Write-Host "ERROR $($_.Exception.Message)"
    $failures.Add($_.Exception.Message)
}

Write-Section 'Result'
if ($failures.Count -gt 0) {
    $failures | ForEach-Object { Write-Host "::error::$_" }
    exit 1
}
Write-Host "Every check passed ($Kind)."
