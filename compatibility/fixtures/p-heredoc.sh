# Builtin-only heredoc so the fixed PATH needs no external commands.
v=world
while IFS= read -r line; do
  printf '%s\n' "$line"
done <<EOF
hello $v
EOF
