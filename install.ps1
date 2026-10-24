[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA "YAM"),
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
    if (Test-Path $Prefix) { Remove-Item -Force -Recurse $Prefix }
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $updatedPath = (($userPath -split ";") | Where-Object { $_ -and $_ -ne $binDir }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $updatedPath, "User")
    [Environment]::SetEnvironmentVariable("YAM_URL", $null, "User")
    Write-Host "YAM CLI removed from $Prefix"
    return
}

function Find-StationIp {
    $adb = Get-Command adb -ErrorAction SilentlyContinue
    if ($adb) {
        $serial = (& adb devices | Select-Object -Skip 1 | ForEach-Object {
            $fields = $_ -split "\s+"
            if ($fields.Length -ge 2 -and $fields[1] -eq "device") { $fields[0] }
        } | Select-Object -First 1)
        if ($serial) {
            $route = & adb -s $serial shell "ip route get 1.1.1.1" 2>$null
            if ($route -match "\bsrc\s+(\d{1,3}(?:\.\d{1,3}){3})") { return $Matches[1] }
            $address = & adb -s $serial shell "ip -4 addr show wlan0" 2>$null
            if ($address -match "\binet\s+(\d{1,3}(?:\.\d{1,3}){3})") { return $Matches[1] }
        }
    }
    return Read-Host "Yandex Station IP address"
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
        $expected = ((Get-Content $checksums | Where-Object { $_ -match ("\\s" + [regex]::Escape($name) + "$") }) -split "\\s+")[0]
        if (-not $expected) { throw "No SHA-256 checksum for $name" }
        $actual = (Get-FileHash -Algorithm SHA256 $download).Hash.ToLowerInvariant()
        if ($actual -ne $expected.ToLowerInvariant()) { throw "Checksum mismatch for $name" }
        $downloads[$name] = $download
    }
    Copy-Item -Force $downloads["yam-x86_64-pc-windows-msvc.exe"] $yamBinary

    $stationIp = Find-StationIp
    if (-not $stationIp) { throw "Yandex Station IP address is required" }
    $adb = Get-Command adb -ErrorAction SilentlyContinue
    if (-not $adb) { throw "adb is required to deploy yamd to the Yandex Station" }
    $serial = (& adb devices | Select-Object -Skip 1 | ForEach-Object {
        $fields = $_ -split "\s+"
        if ($fields.Length -ge 2 -and $fields[1] -eq "device") { $fields[0] }
    } | Select-Object -First 1)
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
    Write-Host "Use yam list or yam --url $yamUrl list"
} finally {
    if (Test-Path $tmp) { Remove-Item -Force -Recurse $tmp }
}
