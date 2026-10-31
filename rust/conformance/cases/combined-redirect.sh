sh -c 'printf "o\n"; printf "e\n" >&2' &> "$DIFF_TMP/combined"
cat "$DIFF_TMP/combined"
