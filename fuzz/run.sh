#!/usr/bin/env bash
# Bounded parser fuzzing. Replays every saved regression input, then fuzzes
# each target for SECONDS (default 60) with its dictionary and seeds.
# Corpora and crash artifacts go under OUT (default .agent/scratch/fuzz),
# never into the tracked seeds/ or regressions/ directories.
#
#   fuzz/run.sh [SECONDS] [TARGET...]
set -euo pipefail
cd "$(dirname "$0")"
SECONDS_PER_TARGET=${1:-60}
shift || true
OUT=${OUT:-../.agent/scratch/fuzz}
ALL=(tracker_response metainfo magnet_and_tracker_url peer_wire bencode persist_records engine_session udp_tracker_response mse_handshake pex_and_dht)
TARGETS=("${@:-${ALL[@]}}")
declare -A DICT=(
    [tracker_response]=tracker_response [metainfo]=metainfo
    [magnet_and_tracker_url]=magnet [peer_wire]=peer_wire
    [bencode]=bencode [persist_records]=persist [engine_session]=
    [udp_tracker_response]=
    [mse_handshake]= [pex_and_dht]=bencode
)

cargo +nightly fuzz build
BIN=target/x86_64-unknown-linux-gnu/release
status=0
for t in "${TARGETS[@]}"; do
    mkdir -p "$OUT/corpus/$t" "$OUT/artifacts/$t"
    if compgen -G "regressions/$t/*" > /dev/null; then
        "$BIN/$t" -runs=1 regressions/$t/* > "$OUT/$t.replay.log" 2>&1 \
            || { echo "$t: regression replay failed, see $OUT/$t.replay.log"; status=1; }
    fi
done
pids=()
for t in "${TARGETS[@]}"; do
    "$BIN/$t" -max_total_time="$SECONDS_PER_TARGET" -rss_limit_mb=2048 \
        -max_len=65536 -timeout=10 ${DICT[$t]:+-dict="dicts/${DICT[$t]}.dict"} \
        -artifact_prefix="$OUT/artifacts/$t/" "$OUT/corpus/$t" $( [ -d "seeds/$t" ] && echo "seeds/$t" ) \
        > "$OUT/$t.log" 2>&1 &
    pids+=($!)
done
for i in "${!pids[@]}"; do
    t=${TARGETS[$i]}
    if wait "${pids[$i]}"; then
        echo "$t: $(grep -E '^Done' "$OUT/$t.log" | tail -1)"
    else
        echo "$t: FAILED, artifacts in $OUT/artifacts/$t, log $OUT/$t.log"
        status=1
    fi
done
exit $status
