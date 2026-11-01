#!/usr/bin/env bash
# Converts the hand-staged CC0 sounds into the embedded OGG Vorbis set.
#
# Usage: scripts/convert-sounds.sh <staging-dir>
#
# <staging-dir> holds the source files (<name>.<any extension>) and a
# `sources.txt` with one line per sound:
#
#   <name> | <url> | <title> | <uploader> | <license> | <start s> | <end s>
#
# start and end are optional (the window of the source to keep, before the
# silence trim and the 6 s cap). Lines starting with # and blank lines are
# skipped. A line is accepted only if its license is CC0, its url, title and
# uploader are not empty and its name matches ^[a-z0-9-]+$. A file without an
# accepted line, or a line without a file, is reported on stderr and skipped.
#
# The script reads only the staging dir and writes only under
# crates/app/assets/sounds/ (the .ogg files and SOURCES.md). It uses no temp
# files. Running it twice gives byte-identical output.
#
# Needs ffmpeg, ffprobe and oggenc (macOS: brew install ffmpeg vorbis-tools;
# Debian/Ubuntu: apt install ffmpeg vorbis-tools).

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <staging-dir>" >&2
    exit 2
fi

staging=$1
if [[ ! -d $staging ]]; then
    echo "error: staging dir not found: $staging" >&2
    exit 2
fi
sources=$staging/sources.txt
if [[ ! -f $sources ]]; then
    echo "error: $sources not found" >&2
    exit 2
fi

for tool in ffmpeg ffprobe oggenc; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: $tool not found. Install it (macOS: brew install ffmpeg vorbis-tools; Debian/Ubuntu: apt install ffmpeg vorbis-tools)" >&2
        exit 1
    fi
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out_dir=$repo_root/crates/app/assets/sounds
mkdir -p "$out_dir"

MAX_SECONDS=6
MAX_RATE=48000
FADE_SECONDS=0.02
PEAK_DB=-1

trim() {
    local s=$1
    s="${s#"${s%%[![:space:]]*}"}"
    s="${s%"${s##*[![:space:]]}"}"
    printf '%s' "$s"
}

is_number() {
    [[ $1 =~ ^[0-9]+(\.[0-9]+)?$ ]]
}

# Newline-joined list of the accepted names (bash 3.2 has no associative arrays).
accepted=""
table=""
index=0

while IFS='|' read -r name url title uploader license start end || [[ -n ${name-} ]]; do
    name=$(trim "$name")
    if [[ -z $name || $name == \#* ]]; then
        continue
    fi
    url=$(trim "${url-}")
    title=$(trim "${title-}")
    uploader=$(trim "${uploader-}")
    license=$(trim "${license-}")
    start=$(trim "${start-}")
    end=$(trim "${end-}")

    if [[ ! $name =~ ^[a-z0-9-]+$ ]]; then
        echo "skipped: invalid name '$name'" >&2
        continue
    fi
    if [[ $license != CC0 ]]; then
        echo "skipped: $name: license is '$license', not CC0" >&2
        continue
    fi
    if [[ -z $url || -z $title || -z $uploader ]]; then
        echo "skipped: $name: url, title and uploader must not be empty" >&2
        continue
    fi
    if [[ -n $start ]] && ! is_number "$start"; then
        echo "skipped: $name: start '$start' is not a number" >&2
        continue
    fi
    if [[ -n $end ]] && ! is_number "$end"; then
        echo "skipped: $name: end '$end' is not a number" >&2
        continue
    fi
    if [[ -n $start && -n $end ]] && ! awk -v s="$start" -v e="$end" 'BEGIN { exit !(e > s) }'; then
        echo "skipped: $name: end must be greater than start" >&2
        continue
    fi
    if printf '%s\n' "$accepted" | grep -Fxq -- "$name"; then
        echo "skipped: $name: duplicate line" >&2
        continue
    fi

    input=""
    for candidate in "$staging/$name".*; do
        if [[ -f $candidate ]]; then
            input=$candidate
            break
        fi
    done
    if [[ -z $input ]]; then
        echo "skipped: $name: no file named $name.* in $staging" >&2
        continue
    fi

    index=$((index + 1))

    rate=$(ffprobe -v error -select_streams a:0 -show_entries stream=sample_rate \
        -of csv=p=0 "$input" </dev/null)
    rate=$(trim "$rate")
    if ! [[ $rate =~ ^[0-9]+$ ]]; then
        echo "error: $name: cannot read the sample rate of $input" >&2
        exit 1
    fi
    if ((rate > MAX_RATE)); then
        rate=$MAX_RATE
    fi

    # Input seeking, before -i, for the optional window.
    inopts=()
    if [[ -n $start ]]; then
        inopts+=(-ss "$start")
    fi
    if [[ -n $end ]]; then
        window=$(awk -v s="${start:-0}" -v e="$end" 'BEGIN { printf "%.6f", e - s }')
        inopts+=(-t "$window")
    fi

    chain="silenceremove=start_periods=1:start_threshold=-50dB,areverse,silenceremove=start_periods=1:start_threshold=-50dB,areverse,atrim=end=$MAX_SECONDS"

    # Pass 1: the exact frame count (raw f32le piped into wc) and max_volume
    # (volumedetect, on stderr). ffmpeg's stderr and wc's count both land in
    # the captured text; the count is the only line made of digits.
    analysis=$({
        ffmpeg -hide_banner -nostdin -loglevel info ${inopts[@]+"${inopts[@]}"} -i "$input" \
            -vn -ac 1 -ar "$rate" -af "$chain,volumedetect" -f f32le - 2>&3 | wc -c
    } 3>&1)
    max_volume=$(printf '%s\n' "$analysis" | sed -n 's/.*max_volume: \(-\{0,1\}[0-9.]*\) dB.*/\1/p' | tail -n 1)
    bytes=$(printf '%s\n' "$analysis" | grep -E '^ *[0-9]+ *$' | tail -n 1 | tr -d ' ')
    if [[ -z $max_volume || -z $bytes ]]; then
        echo "error: $name: the analysis pass did not report max_volume and a size" >&2
        exit 1
    fi
    if ((bytes < 4)); then
        echo "error: $name: nothing left after the silence trim" >&2
        exit 1
    fi
    gain=$(awk -v m="$max_volume" -v p="$PEAK_DB" 'BEGIN { printf "%.2f", p - m }')
    duration=$(awk -v b="$bytes" -v r="$rate" 'BEGIN { printf "%.6f", b / 4 / r }')
    fade_start=$(awk -v d="$duration" -v f="$FADE_SECONDS" 'BEGIN { s = d - f; if (s < 0) s = 0; printf "%.6f", s }')

    # Pass 2: normalize to the peak, fade out the tail, encode.
    ffmpeg -hide_banner -nostdin -loglevel error ${inopts[@]+"${inopts[@]}"} -i "$input" \
        -vn -ac 1 -ar "$rate" \
        -af "$chain,volume=${gain}dB,afade=t=out:st=$fade_start:d=$FADE_SECONDS" \
        -f s16le - |
        oggenc -Q -r -B 16 -C 1 -R "$rate" -q 4 --serial "$index" \
            -o "$out_dir/$name.ogg" -

    echo "converted: $name ($rate Hz, ${duration}s, gain ${gain} dB)" >&2

    accepted+="$name"$'\n'
    table+="| \`builtin:$name\` | ${title//|/\\|} | ${uploader//|/\\|} | $url | CC0 1.0 |"$'\n'
done <"$sources"

if [[ -z $accepted ]]; then
    echo "error: no sound was accepted" >&2
    exit 1
fi

# Files in the staging dir without an accepted line are reported, never embedded.
for file in "$staging"/*; do
    [[ -f $file ]] || continue
    base=$(basename "$file")
    [[ $base == sources.txt ]] && continue
    stem=${base%.*}
    if ! printf '%s' "$accepted" | grep -Fxq -- "$stem"; then
        echo "skipped: $base: no accepted line in sources.txt" >&2
    fi
done

# The set is exactly the accepted lines.
for file in "$out_dir"/*.ogg; do
    [[ -f $file ]] || continue
    stem=$(basename "$file" .ogg)
    if ! printf '%s' "$accepted" | grep -Fxq -- "$stem"; then
        rm -f -- "$file"
        echo "removed: $stem.ogg (not in the accepted set)" >&2
    fi
done

{
    cat <<'EOF'
# Built-in sound credits

The sounds in this folder are embedded in Sound-Babound. All of them are
released under CC0 1.0 (public domain dedication,
<https://creativecommons.org/publicdomain/zero/1.0/>), so no attribution is
required; the credits are kept here anyway.

This file is generated by `scripts/convert-sounds.sh`. Do not edit it by hand.
`builtin:demo` is not listed because it is generated by `gen_demo_wav`.

| Name | Title | Uploader | Source | License |
| --- | --- | --- | --- | --- |
EOF
    printf '%s' "$table"
} >"$out_dir/SOURCES.md"

echo "done: $index sounds in $out_dir" >&2
