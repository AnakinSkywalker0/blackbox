#!/usr/bin/env bash
# Diagnoses a finished workflow run and comments on its pull request(s).
#
# Used by the blackbox GitHub Action (action.yml). Safe by design:
#   - it never exits non-zero, so it can't fail a build, and any problem posts nothing;
#   - the log is only ever fed to `bb explain` as data, never executed;
#   - one comment per workflow per PR, found by a marker and updated on re-runs.
#
# Inputs (environment): BB_BIN, GH_TOKEN, GITHUB_REPOSITORY, BB_RUN_ID, WORKFLOW_NAME,
# CONCLUSION, PR_NUMBERS (space separated), RUN_URL, MAX_LOG_KB.
#
# `--print` posts nothing: it says what it would do. For testing it also accepts
# BB_LOG_FILE (a log to use instead of fetching one) and BB_EXISTING_COMMENTS_FILE
# (a saved comments listing instead of asking GitHub).
set -u

PRINT=0
[ "${1:-}" = "--print" ] && PRINT=1

note() { echo "blackbox: $*"; }
warn() { if [ "$PRINT" = 1 ]; then echo "blackbox: warning: $*"; else echo "::warning::blackbox: $*"; fi; }

# GitHub calls go through here so --print can never write.
gh_read() { gh api "$@"; }

process_pr() {
  local pr="$1" tmp="$2" marker="$3" mode="$4" comments pick action id
  if [ -n "${BB_EXISTING_COMMENTS_FILE:-}" ]; then
    comments="$BB_EXISTING_COMMENTS_FILE"
  else
    comments="$tmp/comments-$pr.json"
    if ! gh_read "repos/$GITHUB_REPOSITORY/issues/$pr/comments?per_page=100" --paginate >"$comments" 2>"$tmp/gh.err"; then
      warn "couldn't list comments on PR #$pr ($(head -c 200 "$tmp/gh.err")), so nothing was posted there."
      return 0
    fi
  fi
  if ! pick="$("$BB_BIN" ci pick-comment --marker "$marker" --body-file "$tmp/body.md" <"$comments" 2>"$tmp/pick.err")"; then
    warn "couldn't read the comments on PR #$pr, so nothing was posted there."
    return 0
  fi
  action="${pick%% *}"
  id="${pick#* }"

  case "$mode:$action" in
    success:none) note "PR #$pr: no earlier failure comment to update."; return 0 ;;
    *:same)       note "PR #$pr: comment $id is already up to date."; return 0 ;;
  esac

  if [ "$PRINT" = 1 ]; then
    if [ "$action" = "none" ]; then
      echo "would CREATE a comment on PR #$pr:"
    else
      echo "would UPDATE comment $id on PR #$pr:"
    fi
    sed 's/^/  | /' "$tmp/body.md"
    return 0
  fi

  if [ "$action" = "none" ]; then
    if gh api -X POST "repos/$GITHUB_REPOSITORY/issues/$pr/comments" -F "body=@$tmp/body.md" >/dev/null 2>"$tmp/gh.err"; then
      note "commented on PR #$pr."
    else
      warn "couldn't comment on PR #$pr ($(head -c 200 "$tmp/gh.err")). Does the workflow have pull-requests: write?"
    fi
  else
    if gh api -X PATCH "repos/$GITHUB_REPOSITORY/issues/comments/$id" -F "body=@$tmp/body.md" >/dev/null 2>"$tmp/gh.err"; then
      note "updated the comment on PR #$pr."
    else
      warn "couldn't update the comment on PR #$pr ($(head -c 200 "$tmp/gh.err"))."
    fi
  fi
}

main() {
  local bb="${BB_BIN:-}"
  if [ -z "$bb" ] || [ ! -x "$bb" ]; then
    note "no usable bb, so nothing was posted."
    return 0
  fi
  local mode
  case "${CONCLUSION:-}" in
    failure | timed_out) mode=failure ;;
    success) mode=success ;;
    *) note "the run ended as '${CONCLUSION:-unknown}', so there is nothing to do."; return 0 ;;
  esac

  local prs="${PR_NUMBERS:-}" pr
  if [ -z "${prs//[[:space:]]/}" ]; then
    note "this run has no pull request (runs from forks and plain pushes don't), so there is nothing to comment on."
    return 0
  fi
  for pr in $prs; do
    case "$pr" in '' | *[!0-9]*) warn "ignoring a bad pull request number."; return 0 ;; esac
  done
  case "${GITHUB_REPOSITORY:-}" in */*) ;; *) warn "GITHUB_REPOSITORY isn't set."; return 0 ;; esac
  case "${BB_RUN_ID:-}" in '' | *[!0-9]*) [ -n "${BB_LOG_FILE:-}" ] || { warn "no run id."; return 0; } ;; esac

  local tmp
  tmp="$(mktemp -d)" || { warn "no temporary folder."; return 0; }
  trap 'rm -rf "$tmp"' RETURN

  # One comment per workflow: the marker names it. Letters, digits and dashes only.
  local key marker
  key="$(printf '%s' "${WORKFLOW_NAME:-workflow}" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9' '-' | cut -c1-40)"
  marker="<!-- blackbox-diagnosis:${key} -->"

  if [ "$mode" = success ]; then
    {
      echo "$marker"
      echo "### blackbox: this workflow passes now"
      echo
      echo "An earlier run of **this workflow** failed and was diagnosed here. The latest run passed.${RUN_URL:+ [Latest run]($RUN_URL).}"
    } >"$tmp/body.md"
  else
    local max_kb="${MAX_LOG_KB:-256}"
    case "$max_kb" in '' | *[!0-9]*) max_kb=256 ;; esac
    if [ -n "${BB_LOG_FILE:-}" ]; then
      tail -c "$((max_kb * 1024))" "$BB_LOG_FILE" >"$tmp/log.txt" 2>/dev/null
    else
      # Lines look like "job<TAB>step<TAB>timestamp message"; keep the message.
      gh run view "$BB_RUN_ID" --repo "$GITHUB_REPOSITORY" --log-failed 2>"$tmp/gh.err" \
        | sed -E $'s/^[^\t]*\t[^\t]*\t//' | tail -c "$((max_kb * 1024))" >"$tmp/log.txt"
    fi
    if [ ! -s "$tmp/log.txt" ]; then
      note "couldn't get a failure log for the run, so nothing was posted."
      return 0
    fi
    if ! "$bb" explain --markdown --title "${WORKFLOW_NAME:-workflow}" --run-url "${RUN_URL:-https://github.com/}" <"$tmp/log.txt" >"$tmp/md.txt" 2>"$tmp/bb.err"; then
      warn "bb couldn't process the log ($(head -c 200 "$tmp/bb.err")), so nothing was posted."
      return 0
    fi
    if [ ! -s "$tmp/md.txt" ]; then
      note "nothing in the log matched a cause blackbox knows, so nothing was posted."
      return 0
    fi
    { echo "$marker"; cat "$tmp/md.txt"; } >"$tmp/body.md"
  fi

  for pr in $prs; do
    process_pr "$pr" "$tmp" "$marker" "$mode"
  done
  return 0
}

main
exit 0
