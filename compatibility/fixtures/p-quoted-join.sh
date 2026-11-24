# Quoted "$*" joins with the first character of IFS, not a literal space.
set -- aa bb cc
IFS=,
printf '[%s]\n' "$*"
IFS=
printf '[%s]\n' "$*"
