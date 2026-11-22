#!/bin/zsh
# Compare mootui's import speed with the Mac's own copy tools, reading from a
# real card with a cold cache every run.
#
#   sudo ./bench-copy.sh [SOURCE] [RUNS]
#
#   SOURCE  folder on the card (default: /Volumes/Untitled/DCIM)
#   RUNS    runs per tool (default: 3)
#
# sudo is only used for `purge` (drops the file cache between runs, so every
# run really reads the card). Every copy runs as you, not root. The card is
# only read; copies go to a temporary folder that is deleted at the end.

set -u
SRC=${1:-/Volumes/Untitled/DCIM}
RUNS=${2:-3}
HERE=${0:A:h}
MOOTUI=$HERE/target/release/mootui

if [[ $EUID -ne 0 || -z ${SUDO_USER:-} ]]; then
  echo "run with sudo: sudo $0 $*" >&2; exit 1
fi
[[ -d $SRC ]] || { echo "no such folder: $SRC" >&2; exit 1; }
[[ -x $MOOTUI ]] || { echo "build first: cargo build --release ($MOOTUI missing)" >&2; exit 1; }

USR=$SUDO_USER
UID_=$(id -u $USR)
as_user() { sudo -H -u $USR "$@"; }
# Finder needs the user's GUI session, not just their uid.
as_gui()  { launchctl asuser $UID_ sudo -H -u $USR "$@"; }

DEST=$(as_user mktemp -d /tmp/mootui-bench.XXXXXX)
trap 'rm -rf "$DEST"' EXIT

# Bytes of real files (AppleDouble ._ files on exFAT cards don't count).
BYTES=$(find "$SRC" -type f ! -name '._*' -print0 | xargs -0 stat -f%z | awk '{s+=$1} END {print s+0}')
NFILES=$(find "$SRC" -type f ! -name '._*' | wc -l | tr -d ' ')
(( BYTES > 0 )) || { echo "no files under $SRC" >&2; exit 1; }
MB=$(( BYTES / 1000000.0 ))

DEV=$(df "$SRC" | awk 'NR==2 {print $1}')
printf "source  %s  (%s files, %.0f MB, %s)\n" "$SRC" $NFILES $MB "$DEV"
printf "dest    %s\n" "$DEST"
printf "runs    %d per tool, file cache purged before each\n\n" $RUNS

now() { perl -MTime::HiRes=time -e 'printf "%.3f", time'; }
NAME=${SRC:t}
LOGS=$DEST/logs; as_user mkdir -p $LOGS

typeset -A total
run() { # label, then the command (in a string, run via as_user/as_gui)
  local label=$1 how=$2; shift 2
  local sum=0
  for i in $(seq 1 $RUNS); do
    rm -rf "$DEST/out"; as_user mkdir -p "$DEST/out"
    sync; purge
    local t0=$(now)
    $how "$@" > $LOGS/last.txt 2>&1; local rc=$?
    local t1=$(now)
    local n=$(find "$DEST/out" -type f ! -name '._*' \( -iname '*.jpg' -o -iname '*.jpeg' -o -iname '*.raf' -o -iname '*.mov' -o -iname '*.mp4' -o -iname '*.heic' -o -iname '*.dng' -o -iname '*.arw' -o -iname '*.cr3' -o -iname '*.nef' \) | wc -l | tr -d ' ')
    local secs=$(( t1 - t0 ))
    sum=$(( sum + secs ))
    local note=""
    (( rc != 0 )) && note="  rc=$rc (see below)"
    printf "%-20s run %d  %6.2f s  %6.1f MB/s  files=%s%s\n" "$label" $i $secs $(( MB / secs )) $n "$note"
    (( rc != 0 )) && sed 's/^/    /' $LOGS/last.txt | tail -5
  done
  total[$label]=$(( sum / RUNS ))
}

run "cp -R"    as_user cp -R "$SRC" "$DEST/out/"
run "ditto"    as_user ditto "$SRC" "$DEST/out/$NAME"
run "Finder"   as_gui  osascript -e "tell application \"Finder\" to duplicate (POSIX file \"$SRC\" as alias) to (POSIX file \"$DEST/out\" as alias)"
run "mootui"   as_user "$MOOTUI" --library "$DEST/out" --yes "$SRC"

echo "\naverage"
for label in "cp -R" "ditto" "Finder" "mootui"; do
  s=${total[$label]}
  printf "  %-10s %6.2f s  %6.1f MB/s\n" "$label" $s $(( MB / s ))
done
echo "\nmootui's time includes planning (EXIF dates, duplicate check) and verifying every copy."
