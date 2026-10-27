[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA "YAM"),
    [string]$StationIp = $env:YAM_STATION_IP,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$repo = "nyakokitsu/yam"
$headers = @{ "User-Agent" = "YAM-installer"; "Accept" = "application/vnd.github+json" }

if ($Version -eq "latest") {
    $release = Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$repo/releases/latest"
    $tag = $release.tag_name
} else {
    $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
}

$baseUrl = "https://github.com/$repo/releases/download/$tag"
$binDir = Join-Path $Prefix "bin"
$yamBinary = Join-Path $binDir "yam-bin.exe"
$yamCommand = Join-Path $binDir "yam.cmd"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("yam-" + [guid]::NewGuid())

if ($Uninstall) {
    $adb = Get-Command adb -ErrorAction SilentlyContinue
    if ($adb) {
        $serial = Select-StationSerial
        if ($serial) {
            & adb -s $serial shell 'set -eu; mount -o remount,rw /; trap "mount -o remount,ro /" EXIT; rm -f /etc/init.d/S99yam; if [ -f /data/quazar/yam/yamd.pid ] && kill -0 $(cat /data/quazar/yam/yamd.pid) 2>/dev/null; then kill $(cat /data/quazar/yam/yamd.pid); fi; rm -rf /data/quazar/yam' | Out-Null
        }
    }
    if (Test-Path $Prefix) { Remove-Item -Force -Recurse $Prefix }
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $updatedPath = (($userPath -split ";") | Where-Object { $_ -and $_ -ne $binDir }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $updatedPath, "User")
    [Environment]::SetEnvironmentVariable("YAM_URL", $null, "User")
    Write-Host "YAM CLI removed from $Prefix"
    return
}

function Select-StationSerial {
    $devices = @(& adb devices | Select-Object -Skip 1 | ForEach-Object {
            $fields = $_ -split "\s+"
            if ($fields.Length -ge 2 -and $fields[1] -eq "device") {
                $serial = $fields[0]
                $arch = & adb -s $serial shell uname -m 2>$null
                if ($arch -match "aarch64|arm64") {
                    $model = (& adb -s $serial shell getprop ro.product.model 2>$null).Trim()
                    [PSCustomObject]@{ Serial = $serial; Model = $(if ($model) { $model } else { "Yandex Station" }) }
                }
            }
        })
    if ($devices.Count -eq 0) { return $null }
    if ($devices.Count -eq 1) { return $devices[0].Serial }
    Write-Host "Select Yandex Station:"
    for ($i = 0; $i -lt $devices.Count; $i++) {
        Write-Host ("  {0}) {1} ({2})" -f ($i + 1), $devices[$i].Model, $devices[$i].Serial)
    }
    $selection = Read-Host "Number"
    $number = 0
    if (-not [int]::TryParse($selection, [ref]$number) -or $number -lt 1 -or $number -gt $devices.Count) {
        throw "Invalid Yandex Station selection"
    }
    return $devices[$number - 1].Serial
}

function Find-StationIp([string]$RequestedIp) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedIp)) {
        return ($RequestedIp.Trim() -replace ':5555$', '')
    }
    $adb = Get-Command adb -ErrorAction SilentlyContinue
    if ($adb) {
        $serial = Select-StationSerial
        if ($serial) {
            $script:yamStationSerial = $serial
            $route = & adb -s $serial shell "ip route get 1.1.1.1" 2>$null
            if ($route -match "\bsrc\s+(\d{1,3}(?:\.\d{1,3}){3})") { return $Matches[1] }
            $address = & adb -s $serial shell "ip -4 addr show wlan0" 2>$null
            if ($address -match "\binet\s+(\d{1,3}(?:\.\d{1,3}){3})") { return $Matches[1] }
        }
    }
    $entered = Read-Host "Yandex Station IP address (for example, 192.168.1.42)"
    if ([string]::IsNullOrWhiteSpace($entered)) { return $null }
    return ($entered.Trim() -replace ':5555$', '')
}

function Get-ReleaseAsset([string]$Name) {
    $path = Join-Path $tmp $Name
    Invoke-WebRequest -Headers $headers -Uri "$baseUrl/$Name" -OutFile $path
    return $path
}

try {
    New-Item -ItemType Directory -Force -Path $tmp, $binDir | Out-Null
    $checksums = Get-ReleaseAsset "SHA256SUMS"
    $downloads = @{}
    foreach ($name in @("yam-x86_64-pc-windows-msvc.exe", "yamd-aarch64-linux-musl")) {
        $download = Get-ReleaseAsset $name
        $expected = ((Get-Content $checksums | Where-Object { $_ -match ("\s" + [regex]::Escape($name) + "$") }) -split "\s+")[0]
        if (-not $expected) { throw "No SHA-256 checksum for $name" }
        $actual = (Get-FileHash -Algorithm SHA256 $download).Hash.ToLowerInvariant()
        if ($actual -ne $expected.ToLowerInvariant()) { throw "Checksum mismatch for $name" }
        $downloads[$name] = $download
    }
    Copy-Item -Force $downloads["yam-x86_64-pc-windows-msvc.exe"] $yamBinary

    $stationIp = Find-StationIp $StationIp
    if (-not $stationIp) {
        throw "Yandex Station IP address is required. Set `$env:YAM_STATION_IP or run the installer with -StationIp <address>."
    }
    $adb = Get-Command adb -ErrorAction SilentlyContinue
    if (-not $adb) { throw "adb is required to deploy yamd to the Yandex Station" }
    $serial = $script:yamStationSerial
    if (-not $serial) {
        & adb connect "$stationIp`:5555" | Out-Host
        $serial = "$stationIp`:5555"
        & adb -s $serial get-state | Out-Null
    }
    $devicePrefix = "/data/quazar/yam"
    $yamdBinary = $downloads["yamd-aarch64-linux-musl"]
    & adb -s $serial shell "mkdir -p $devicePrefix/bin $devicePrefix/data" | Out-Null
    & adb -s $serial push $yamdBinary "$devicePrefix/bin/yamd.new" | Out-Null
    $startCommand = 'chmod 0755 /data/quazar/yam/bin/yamd.new; if [ -f /data/quazar/yam/yamd.pid ] && kill -0 $(cat /data/quazar/yam/yamd.pid) 2>/dev/null; then kill $(cat /data/quazar/yam/yamd.pid); fi; mv /data/quazar/yam/bin/yamd.new /data/quazar/yam/bin/yamd; start-stop-daemon --start --background --make-pidfile --pidfile /data/quazar/yam/yamd.pid --startas /data/quazar/yam/bin/yamd -- --data-dir /data/quazar/yam/data'
    & adb -s $serial shell $startCommand | Out-Null
    $bootScript = @'
#!/bin/sh
case "${1:-}" in
  start)
    prefix=/data/quazar/yam
    pidfile="$prefix/yamd.pid"
    [ -x "$prefix/bin/yamd" ] || exit 0
    mkdir -p "$prefix/data"
    if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then exit 0; fi
    rm -f "$pidfile"
    start-stop-daemon --start --background --make-pidfile --pidfile "$pidfile" --startas "$prefix/bin/yamd" -- --data-dir "$prefix/data"
    ;;
  stop)
    prefix=/data/quazar/yam
    [ -f "$prefix/yamd.pid" ] && kill "$(cat "$prefix/yamd.pid")" 2>/dev/null || true
    rm -f "$prefix/yamd.pid"
    ;;
esac
'@
    $bootScript64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($bootScript))
    $bootCommand = "set -eu; mount -o remount,rw /; trap 'mount -o remount,ro /' EXIT; echo '$bootScript64' | base64 -d > /etc/init.d/S99yam; chmod 0755 /etc/init.d/S99yam"
    & adb -s $serial shell $bootCommand | Out-Null

    $yamUrl = "http://$stationIp`:8099"
    [Environment]::SetEnvironmentVariable("YAM_URL", $yamUrl, "User")
    $env:YAM_URL = $yamUrl
    @"
@echo off
if not defined YAM_URL set "YAM_URL=$yamUrl"
"%~dp0yam-bin.exe" %*
"@ | Set-Content -Encoding ascii $yamCommand

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not (($userPath -split ";") -contains $binDir)) {
        [Environment]::SetEnvironmentVariable("Path", (($userPath, $binDir | Where-Object { $_ }) -join ";"), "User")
    }
    $env:Path = "$binDir;$env:Path"
    Write-Host "YAM $tag installed in $Prefix and deployed to $stationIp"
    Write-Host "yamd is configured to start automatically after the speaker reboots."
    Write-Host "Use yam list or yam --url $yamUrl list"
} finally {
    if (Test-Path $tmp) { Remove-Item -Force -Recurse $tmp }
}
