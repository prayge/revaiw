#!/usr/bin/env bash
# Statusline segment: the repo's /load-skills set as badges, coloured once that skill has ACTUALLY loaded this session
# (a Skill call or a typed /<skill>, recorded in ~/.claude/.skills-loaded/<session>.loaded), grey until then.
# Prints one fragment, no newline; silent in repos without .claude/skills/load-skills/SKILL.md.
# Add to your statusline command: $(bash <plugin>/statusline/skills-segment.sh <<<"$input") — /revaiw-statusline does it.
IN=$(cat)
eval "$(python3 -c "
import json,sys
d=json.loads(sys.stdin.read() or '{}')
print('SID=%r' % d.get('session_id',''))
print('CWD=%r' % ((d.get('workspace') or {}).get('project_dir') or d.get('cwd') or ''))
" <<<"$IN" 2>/dev/null)"
CWD="${CWD:-$PWD}"
LIST="$CWD/.claude/skills/load-skills/SKILL.md"
[ -f "$LIST" ] || exit 0

R="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.skills-loaded/$SID"
G=$'\033[38;5;108m' W=$'\033[38;5;255m' D=$'\033[38;5;244m' X=$'\033[0m'

# The set is the numbered list in the project's /load-skills skill, the one place it is written.
skillset=$(grep -oE '^[0-9]+\. `[^`]+`' "$LIST" 2>/dev/null | sed -E 's/^[0-9]+\. `([^`]+)`/\1/')
loaded=" $(cat "$R.loaded" 2>/dev/null | tr '\n' ' ') "
has() { case "$loaded" in *" ${1##*:} "*) return 0;; esac; return 1; }
sp=0 spall=0 short= badges=
for s in $skillset; do
    has "$s" || short=1
    case "$s" in
        superpowers:*) spall=$((spall+1)); has "$s" && sp=$((sp+1)); continue;;
        *) l=$(echo "$s" | tr a-z A-Z) c=$W;;
    esac
    has "$s" || c=$D
    badges+=" $c◆$l$X"
done
if [ "$spall" -gt 0 ]; then
    if [ "$sp" -eq "$spall" ]; then c=$G; else c=$D short=1; fi
    printf '%s⚡SP:%s/%s%s' "$c" "$sp" "$spall" "$X"
fi
printf '%s' "$badges"
[ -n "$short" ] && printf ' %s/load-skills%s' "$G" "$X"
exit 0
