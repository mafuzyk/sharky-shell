# pipefail is a Bash extension, so this case is judged by bash, not dash.
# The conditional suspends errexit so the pipefail status is observable;
# the final expansion then trips nounset under `set -u`.
set -euo pipefail
printf 'start\n'
if false | true; then
  printf 'ok\n'
else
  printf 'rc=%s\n' "$?"
fi
printf '%s\n' "$SHARKY_UNSET_SENTINEL"
