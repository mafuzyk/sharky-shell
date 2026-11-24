# Correct unquoted IFS expansion: an unquoted parameter splits into fields on IFS.
IFS=,
v=a,b,c
set -- $v
printf 'count=%s\n' "$#"
printf '<%s>\n' "$@"
