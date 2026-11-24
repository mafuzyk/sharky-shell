f() { local v=inner; printf '%s\n' "$v"; }
v=outer
f
printf '%s\n' "$v"
