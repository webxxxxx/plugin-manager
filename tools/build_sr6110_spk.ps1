$ErrorActionPreference = "Stop"

$REPO = Split-Path $PSScriptRoot -Parent
$BASE = Split-Path $REPO -Parent
$SRC = Join-Path $BASE "sr6110_rebuild\thirdparty\data"
$STAGE = Join-Path $REPO "target\spk-src"
$OUT = Join-Path $BASE "sr6110_rebuild\plugins-dist\spk"
$PLUGIN = Join-Path $STAGE "plugin.exe"
$KEY = Join-Path $REPO "samples\keys\sample-2026.key"
$PUB = Join-Path $REPO "samples\keys\sample-2026.pub"
$CHECK = Join-Path $REPO "tools\check_spk.mjs"
New-Item -ItemType Directory -Force $OUT | Out-Null
New-Item -ItemType Directory -Force $STAGE | Out-Null
if (-not (Test-Path $SRC)) {
    throw "thirdparty source not found: $SRC (expected the sr6110_rebuild checkout next to this repo)"
}

if (-not (Test-Path $KEY)) {
    node (Join-Path $REPO "tools\gen_sample_key.mjs")
    if ($LASTEXITCODE -ne 0) { throw "cannot generate the sample key" }
}
Push-Location $REPO
try {
    cargo build
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}
Copy-Item -Force (Join-Path $REPO "target\debug\svcd.exe") $PLUGIN

function Read-Lf([string]$path) {
    return (Get-Content $path -Raw).Replace("`r`n", "`n")
}

function Write-Lf([string]$path, [string]$text) {
    New-Item -ItemType Directory -Force (Split-Path $path -Parent) | Out-Null
    [IO.File]::WriteAllText($path, $text.Replace("`r`n", "`n"))
}

function Assert-NoAbsolute([string]$path) {
    $hits = Select-String -Path $path -Pattern "/etc/sr" -ErrorAction SilentlyContinue
    if ($hits) { throw "absolute /etc/sr path left in $path`n$($hits.Line -join "`n")" }
}

function Copy-Into([string]$from, [string]$to) {
    New-Item -ItemType Directory -Force (Split-Path $to -Parent) | Out-Null
    Copy-Item -Force $from $to
}

# ---------------------------------------------------------------- meow
$dir = Join-Path $STAGE "meow"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
Copy-Into "$SRC\bin\meow" "$dir\payload\bin\meow"

# service entry built from the original autostart script
$meow = Read-Lf "$SRC\autostart\20-meow"
$meow = $meow.Replace(
    "PATH=/etc/sr/runtime:/etc/sr/bin:/kmodule/bin:/bin:/sbin:/usr/bin:/usr/sbin",
    @'
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
PATH="$BIN:/kmodule/bin:/bin:/sbin:/usr/bin:/usr/sbin"
'@.TrimEnd("`n"))
$meow = $meow.Replace(
    "MEOW_BIN=/etc/sr/bin/meow`nMEOW_CFG=/etc/sr/meow/config.yaml`nMEOW_FW=/etc/sr/sh/meow-fw.sh",
    "MEOW_BIN=`$BIN/meow`nMEOW_CFG=`${MEOW_HOME:-`$ROOT/config/meow}/config.yaml`nMEOW_FW=`$BIN/meow-fw.sh")
$meow = $meow.Replace("LOG=/var/log/meow.log`n", "")
$meow = $meow.Replace("RUN=/var/run/meow", 'RUN=${MEOW_RUN:-/var/run/meow}')
$meow = $meow.Replace('log() { echo "$(date ''+%F %T'') [meow] $*" >> "$LOG" 2>/dev/null; }', 'log() { echo "$(date ''+%F %T'') [meow] $*" >&2; }')
$meow = $meow.Replace('mkdir -p "$RUN" "$(dirname "$LOG")" 2>/dev/null', 'mkdir -p "$RUN" 2>/dev/null')
$meow = $meow.Replace(
    "    if [ -f /etc/sr/meow/fix-rules.sh ]; then`n        MEOW_CONF=`"`$MEOW_CFG`" sh /etc/sr/meow/fix-rules.sh >>`"`$LOG`" 2>&1 \",
    "    if [ -f `"`$BIN/fix-rules.sh`" ]; then`n        MEOW_CONF=`"`$MEOW_CFG`" sh `"`$BIN/fix-rules.sh`" >&2 \")
$meow = $meow.Replace(
    '    "$MEOW_BIN" -d "$_dir" -f "$MEOW_CFG" >>"$LOG" 2>&1 &',
    '    ( cd "$_dir" && exec "$MEOW_BIN" -d "$_dir" -f "$MEOW_CFG" ) 1>&2 &')
$meow = $meow.Replace(' >>"$LOG" 2>&1', '').Replace('>>"$LOG" 2>&1', '')
$meow = $meow.Replace('meow exited before listening (see $LOG)', 'meow exited before listening')
$meow = $meow.Replace('"$MEOW_FW" up', 'sh "$MEOW_FW" up >&2').Replace('"$MEOW_FW" down', 'sh "$MEOW_FW" down >&2')
$meow = $meow.Replace('/etc/sr/firewall.d/', 'the firmware firewall rules')
$meow = $meow.Replace('/etc/sr/sh/meow-fw.sh', 'meow-fw.sh')
$meow = $meow.Replace('/etc/sr/sr.conf', 'the firmware config')
$meow = $meow.Replace('/etc/sr/meow/config.yaml', 'config/meow/config.yaml')
$meow = $meow + @'

case "${1:-$SVCD_ACTION}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  *) exit 64 ;;
esac
'@
Write-Lf "$dir\payload\bin\meow.sh" $meow
Assert-NoAbsolute "$dir\payload\bin\meow.sh"

# firewall helper: code is path-free, only comments mention the old layout
$fw = Read-Lf "$SRC\sh\meow-fw.sh"
$fw = $fw.Replace('/etc/sr/autostart/20-meow', 'the plugin entry')
$fw = $fw.Replace('/etc/sr/meow/config.yaml', 'the plugin config')
$fw = $fw.Replace('/etc/sr/firewall.d', 'the firmware firewall rules')
$fw = $fw.Replace('/etc/sr/sr.conf', 'the firmware config')
$fw = $fw.Replace("LOG=/var/log/meow.log`n", "")
$fw = $fw.Replace('log() { echo "$(date ''+%F %T'') [meow-fw] $*" >> "$LOG" 2>/dev/null; }', 'log() { echo "$(date ''+%F %T'') [meow-fw] $*" >&2; }')
Write-Lf "$dir\payload\bin\meow-fw.sh" $fw
Assert-NoAbsolute "$dir\payload\bin\meow-fw.sh"

# fix-rules helper resolves the config relative to the package
$fix = Read-Lf "$SRC\meow\fix-rules.sh"
$fix = $fix.Replace(
    "CONFIG=`${MEOW_CONF:-/etc/sr/meow/config.yaml}",
    @'
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
CONFIG=${MEOW_CONF:-$ROOT/config/meow/config.yaml}
'@.TrimEnd("`n"))
$fix = $fix.Replace('/etc/sr/autostart/20-meow', 'the plugin entry')
$fix = $fix.Replace('/plugin/meow/config.yaml', 'a different device layout')
$fix = $fix.Replace('/etc/sr/meow/', 'config/meow/')
Write-Lf "$dir\payload\bin\fix-rules.sh" $fix
Assert-NoAbsolute "$dir\payload\bin\fix-rules.sh"

# config data tree
Copy-Item -Recurse -Force "$SRC\meow" "$dir\payload\config\meow"
Remove-Item -Force "$dir\payload\config\meow\fix-rules.sh" -ErrorAction SilentlyContinue
$cfg = Read-Lf "$SRC\meow\config.yaml"
$cfg = $cfg.Replace('external-ui: /etc/sr/meow/yacd', 'external-ui: yacd')
$cfg = $cfg.Replace('path: /etc/sr/meow/cn_domains.yaml', 'path: cn_domains.yaml')
$cfg = $cfg.Replace('mmdb-path: /etc/sr/meow/Country.mmdb', 'mmdb-path: Country.mmdb')
Write-Lf "$dir\payload\config\meow\config.yaml" $cfg
Assert-NoAbsolute "$dir\payload\config\meow\config.yaml"

$examplePath = "$SRC\meow\config.yaml.example"
if (Test-Path $examplePath) {
    $example = Read-Lf $examplePath
    $example = $example.Replace('external-ui: /etc/sr/meow/yacd', 'external-ui: yacd')
    $example = $example.Replace('path: /etc/sr/meow/cn_domains.yaml', 'path: cn_domains.yaml')
    $example = $example.Replace('mmdb-path: /etc/sr/meow/Country.mmdb', 'mmdb-path: Country.mmdb')
    Write-Lf "$dir\payload\config\meow\config.yaml.example" $example
    Assert-NoAbsolute "$dir\payload\config\meow\config.yaml.example"
}

# ---------------------------------------------------------------- nas
$dir = Join-Path $STAGE "nas"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
foreach ($b in @('sshfs', 'dbclient', 'fusermount3')) {
    Copy-Into "$SRC\bin\$b" "$dir\payload\bin\$b"
}

$nas = @'
#!/bin/sh
# NAS (sshfs) plugin entry. Everything in the package is resolved relative to
# this script; host state (mount point, SSH key) comes from config/nas.conf.
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
CONF=${NAS_CONF:-$ROOT/config/nas.conf}
[ -f "$CONF" ] && . "$CONF"
PATH="$BIN:/kmodule/bin:/bin:/sbin:/usr/bin:/usr/sbin"
export PATH
MNT=${NAS_MNT:-/var/nas}

log() { echo "$(date '+%F %T') [nas] $*" >&2; }
mounted() { mount 2>/dev/null | grep -q " on $MNT "; }

start() {
    if mounted; then
        log "already mounted at $MNT"
    else
        log "mounting $MNT ..."
        CONF="$CONF" sh "$BIN/mount-sshfs.sh" >&2 || { log "mount failed"; return 1; }
    fi
    # Keep the plugin alive while the mount exists; sshfs exits when the NAS
    # goes away, and the supervisor restarts this entry.
    while mounted; do sleep 30; done
    log "mount disappeared: $MNT"
    return 1
}

stop() {
    CONF="$CONF" sh "$BIN/umount-sshfs.sh" >/dev/null 2>&1
    return 0
}

status() {
    if mounted; then
        echo "running (mounted at $MNT)"
        return 0
    fi
    echo "stopped (not mounted)"
    return 3
}

case "${1:-$SVCD_ACTION}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  *) exit 64 ;;
esac
'@
Write-Lf "$dir\payload\bin\nas.sh" $nas
Assert-NoAbsolute "$dir\payload\bin\nas.sh"

$mount = @'
#!/bin/sh
# Mount the NAS over sshfs. Package binaries live in the same bin directory;
# the SSH key stays host state and must be set as NAS_KEY in nas.conf.
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
CONF=${CONF:-$ROOT/config/nas.conf}
[ -f "$CONF" ] && . "$CONF"
BIN=${NAS_BIN:-$BIN}

HOST=${NAS_HOST:-192.168.10.45}
NUSER=${NAS_USER:-root}
RPATH=${NAS_PATH:-/root/zte}
PORT=${NAS_PORT:-22}
MNT=${MNT:-${NAS_MNT:-/var/nas}}
KEY=${NAS_KEY:-}
[ -n "$KEY" ] || { echo "!! NAS_KEY is not set in $CONF"; exit 1; }
[ -f "$KEY" ] || { echo "!! key not found: $KEY"; exit 1; }
[ -d "$MNT" ] || mkdir -p "$MNT"
if mount | grep -q "on $MNT type fuse"; then echo "already mounted: $MNT"; exit 0; fi
exec "$BIN/sshfs" -f -o "ssh_command=$BIN/dbclient -i $KEY -y -p $PORT" \
    -o reconnect -o ServerAliveInterval=15 \
    "$NUSER@$HOST:$RPATH" "$MNT"
'@
Write-Lf "$dir\payload\bin\mount-sshfs.sh" $mount
Assert-NoAbsolute "$dir\payload\bin\mount-sshfs.sh"

$umount = @'
#!/bin/sh
# Unmount the NAS mounted by mount-sshfs.sh.
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
CONF=${CONF:-$ROOT/config/nas.conf}
[ -f "$CONF" ] && . "$CONF"
MNT=${MNT:-${NAS_MNT:-/var/nas}}
BIN=${NAS_BIN:-$BIN}
"$BIN/fusermount3" -u "$MNT" 2>/dev/null || umount "$MNT"
'@
Write-Lf "$dir\payload\bin\umount-sshfs.sh" $umount
Assert-NoAbsolute "$dir\payload\bin\umount-sshfs.sh"

$nasConf = @'
# NAS (sshfs) config — part of the nas plugin.
NAS_HOST=192.168.10.45
NAS_USER=root
NAS_PATH=/root/zte
NAS_PORT=22
NAS_MNT=/var/nas
# Required: path to the device SSH private key (host state).
# NAS_KEY=/path/to/id_ed25519
'@
Write-Lf "$dir\payload\config\nas.conf" $nasConf
Assert-NoAbsolute "$dir\payload\config\nas.conf"

# ---------------------------------------------------------------- wireguard
$dir = Join-Path $STAGE "wireguard"
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
foreach ($b in @('wg-tool', 'wireguard.ko')) {
    Copy-Into "$SRC\bin\$b" "$dir\payload\bin\$b"
}

$wg = Read-Lf "$SRC\autostart\10-wireguard"
$wg = $wg.Replace("TYPE=oneshot`n", "")
$wg = $wg.Replace(
    "WGDIR=/etc/sr/wg`nTOOLDIR=/etc/sr/bin`n[ -x `"`$TOOLDIR/wg-tool`" ] || TOOLDIR=/etc/sr/runtime",
    @'
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
CONF=${WG_CONF:-$ROOT/config/wg.conf}
[ -f "$CONF" ] && . "$CONF"
PATH="$BIN:/kmodule/bin:/bin:/sbin:/usr/bin:/usr/sbin"
WGDIR=${WG_DIR:-$ROOT/config/keys}
TOOLDIR=$BIN
'@.TrimEnd("`n"))
$wg = $wg.Replace(
    "    /etc/sr/sh/wg-start.sh",
    '    CONF="$CONF" WG_DIR="$WGDIR" sh "$BIN/wg-start.sh" >&2')
$wg = $wg.Replace("pkill -f 'wg-lan-fix.sh'", 'pkill -f "$BIN/wg-lan-fix.sh"')
$wg = $wg.Replace(
@"
start() {
    CONF=`"`$CONF`" WG_DIR=`"`$WGDIR`" sh `"`$BIN/wg-start.sh`" >&2
    return `$?
}
"@,
@"
start() {
    CONF=`"`$CONF`" WG_DIR=`"`$WGDIR`" sh `"`$BIN/wg-start.sh`" >&2 || return `$?
    # Keep the plugin running while the tunnel exists; stop removes wg0.
    while ip link show wg0 >/dev/null 2>&1; do sleep 30; done
    return 1
}
"@)
$wg = $wg + @'

case "${1:-$SVCD_ACTION}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  *) exit 64 ;;
esac
'@
Write-Lf "$dir\payload\bin\wireguard.sh" $wg
Assert-NoAbsolute "$dir\payload\bin\wireguard.sh"

$wgs = Read-Lf "$SRC\sh\wg-start.sh"
$wgs = $wgs.Replace(
    "PATH=/kmodule/bin:/bin:/sbin:/usr/bin:/usr/sbin",
    @'
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
PATH="$BIN:/kmodule/bin:/bin:/sbin:/usr/bin:/usr/sbin"
'@.TrimEnd("`n"))
$wgs = $wgs.Replace(
    @'
# 二进制在 UBI 的 overlay upper 里（/etc/sr/bin）：rootfs 只有 26.7MB，装不下它们。
# /etc/sr/bin 里是 data 组的程序（UBI 的 upper）；不可用时回退 /etc/sr/runtime。
TOOLDIR=/etc/sr/bin
[ -x "$TOOLDIR/wg-tool" ] || TOOLDIR=/etc/sr/runtime
LOG=/var/wg-start.log
'@.TrimEnd("`n"),
    @'
TOOLDIR=$BIN
'@.TrimEnd("`n"))
$wgs = $wgs.Replace(
    @'
# ---- 运行时配置（/etc/sr/wg/wg.conf，在 overlay 上，刷机不丢）----------------
# 这里只放缺省值：配置文件里的同名项优先，删掉那行也能跑。
CONF=${CONF:-/etc/sr/wg/wg.conf}
[ -f "$CONF" ] && . "$CONF"
'@.TrimEnd("`n"),
    @'
# ---- 运行时配置（包内 config/wg.conf）----------------
# 这里只放缺省值：配置文件里的同名项优先，删掉那行也能跑。
CONF=${CONF:-$ROOT/config/wg.conf}
[ -f "$CONF" ] && . "$CONF"
WGDIR=${WG_DIR:-$ROOT/config/keys}
'@.TrimEnd("`n"))
$wgs = $wgs.Replace("WGDIR=/etc/sr/wg`n", "")
$wgs = $wgs.Replace("    [ -x /etc/sr/bin/wg-tool ] && TOOLDIR=/etc/sr/bin`n", "")
$wgs = $wgs.Replace('（v2 的 /etc/sr/wg/wg.conf 里配的 WG_LAN_IF）', '(wg.conf 里配的 WG_LAN_IF)')
$wgs = $wgs.Replace('pgrep -f "wg-lan-fix.sh"', 'pgrep -f "$BIN/wg-lan-fix.sh"')
$wgs = $wgs.Replace('setsid /etc/sr/sh/wg-lan-fix.sh', 'setsid sh "$BIN/wg-lan-fix.sh"')
$wgs = $wgs.Replace('setsid sh "$BIN/wg-lan-fix.sh" >/dev/null 2>&1 &', 'setsid sh "$BIN/wg-lan-fix.sh" >&2 &')
$wgs = $wgs.Replace('log() { echo "$(date ''+%H:%M:%S'') $*" >> "$LOG"; }', 'log() { echo "$(date ''+%H:%M:%S'') $*" >&2; }')
Write-Lf "$dir\payload\bin\wg-start.sh" $wgs
Assert-NoAbsolute "$dir\payload\bin\wg-start.sh"

$wgf = Read-Lf "$SRC\sh\wg-lan-fix.sh"
$wgf = $wgf.Replace(
    @'
# ---- 运行时配置（/etc/sr/wg/wg.conf，在 overlay 上，刷机不丢）----------------
# 这里只放缺省值：配置文件里的同名项优先，删掉那行也能跑。
CONF=${CONF:-/etc/sr/wg/wg.conf}
[ -f "$CONF" ] && . "$CONF"
'@.TrimEnd("`n"),
    @'
BIN=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
ROOT=$(dirname "$BIN")
CONF=${CONF:-$ROOT/config/wg.conf}
[ -f "$CONF" ] && . "$CONF"
'@.TrimEnd("`n"))
$wgf = $wgf.Replace("LOG=/var/run/sr/wg-arp-sync.log`n", "")
$wgf = $wgf.Replace('log() { echo "$(date ''+%H:%M:%S'') $*" >> "$LOG"; }', 'log() { echo "$(date ''+%H:%M:%S'') $*" >&2; }')
$wgf = $wgf.Replace(
    @'
wg_tool() {
    for t in /etc/sr/bin/wg-tool /etc/sr/runtime/wg-tool; do
        [ -x "$t" ] && { "$t" "$@"; return $?; }
    done
    return 1
}
'@.TrimEnd("`n"),
    @'
wg_tool() {
    [ -x "$BIN/wg-tool" ] && { "$BIN/wg-tool" "$@"; return $?; }
    return 1
}
'@.TrimEnd("`n"))
$wgf = $wgf.Replace("mkdir -p /var/run/sr 2>/dev/null`n", "")
Write-Lf "$dir\payload\bin\wg-lan-fix.sh" $wgf
Assert-NoAbsolute "$dir\payload\bin\wg-lan-fix.sh"

$wgConf = @'
# WireGuard config — part of the wireguard plugin.
WG_PORT=51820
WG_NET=10.99.0.0/24
WG_ADDR=10.99.0.1
WG_PEER_IP=10.99.0.2
WG_LAN_IF=br0
WG_TABLE=200
# Device keys (server.key / pc.pub) are host state: default is
# <package>/config/keys; point WG_DIR at the firmware key directory to reuse it.
'@
Write-Lf "$dir\payload\config\wg.conf" $wgConf
Assert-NoAbsolute "$dir\payload\config\wg.conf"

# ---------------------------------------------------------------- manifests + bundle
$plugins = @(
    @{ id = "meow";      desc = "meow 透明代理网关";  config = "payload/config/meow/config.yaml"; url = "http://{host}:9090/ui/" },
    @{ id = "nas";       desc = "NAS 挂载（sshfs）";  config = "payload/config/nas.conf";         url = "" },
    @{ id = "wireguard"; desc = "WireGuard 隧道";     config = "payload/config/wg.conf";          url = "" }
)

foreach ($p in $plugins) {
    $dir = Join-Path $STAGE $p.id
    $entry = "payload/bin/$($p.id).sh"
    $lifecycle = [ordered]@{
        start  = [ordered]@{ path = $entry; args = @("start") }
        stop   = [ordered]@{ path = $entry; args = @("stop") }
        status = [ordered]@{ path = $entry; args = @("status") }
    }
    $manifest = [ordered]@{
        config       = $p.config
        capabilities = @()
        dependencies = @()
        interpreter  = [ordered]@{ name = "sh" }
        exec         = [ordered]@{ path = $entry; args = @(); ready = "exec" }
        lifecycle    = $lifecycle
    }
    if ($p.url) { $manifest["url"] = $p.url }
    Write-Lf (Join-Path $dir "manifest.json") ($manifest | ConvertTo-Json -Depth 8)

    $localOut = Join-Path $STAGE ("out\{0}-1.0.0-aarch64.spk" -f $p.id)
    $outFile = Join-Path $OUT ("{0}-1.0.0-aarch64.spk" -f $p.id)
    New-Item -ItemType Directory -Force (Split-Path $localOut) | Out-Null
    & $PLUGIN bundle $dir --key $KEY --id $p.id --version 1.0.0 --desc $p.desc --exec $entry --interpreter sh --compat sr6110 --platform aarch64-unknown-linux-musl -o $localOut
    if ($LASTEXITCODE -ne 0) { throw "bundle failed for $($p.id)" }
    Copy-Item -Force $localOut $outFile

    & node $CHECK $outFile --key $PUB
    if ($LASTEXITCODE -ne 0) { throw "check failed for $($p.id)" }
    Write-Host "OK $outFile"
}
