#!/bin/bash
# replay-diff.sh <old-binary> <new-binary> <recording>...: replay every recording through both
# binaries in each output form and compare. A refactor must leave the JSON identical; the text
# forms show where a wording changed. Exit 1 on any difference. OUT= keeps the outputs.
set -u
old=$1; new=$2; shift 2
out=${OUT:-$(mktemp -d)}; rc=0
for r in "$@"; do
    n=$(basename "$r")
    for form in "--json" "--once" "--once --flat --raw" "--plain --no-peaks"; do
        tag=$(echo "$form" | tr -d ' -')
        "$old" --replay "$r" $form > "$out/$n.$tag.old" 2>&1
        "$new" --replay "$r" $form > "$out/$n.$tag.new" 2>&1
        if cmp -s "$out/$n.$tag.old" "$out/$n.$tag.new"; then
            echo "$n $form: identical ($(wc -l < "$out/$n.$tag.new") lines)"
        else
            echo "$n $form: DIFFERS ($(diff "$out/$n.$tag.old" "$out/$n.$tag.new" | grep -c '^[<>]') lines)"
            diff "$out/$n.$tag.old" "$out/$n.$tag.new" | head -"${SHOW:-6}"
            rc=1
        fi
    done
done
exit $rc
