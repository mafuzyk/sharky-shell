sh -c 'printf o; printf e >&2' > "$DIFF_TMP/out" 2>&1; cat "$DIFF_TMP/out"
