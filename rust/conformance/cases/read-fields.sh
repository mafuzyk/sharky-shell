printf 'a b c\n' | { read x y; printf '%s|%s' "$x" "$y"; }
