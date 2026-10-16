---
description: Offer revaiw's statusline segments (review state, loaded skills) for your existing statusline
---
Offer revaiw's statusline segments. They only add to the user's statusline; they never replace it.

1. Show what the review segment looks like: run `echo '{}' | bash "${CLAUDE_PLUGIN_ROOT}/statusline/rev-segment.sh"`
   from the repo root and show the output (or describe `◆REV idle` / `◆REV ▶ branch (sonnet) · 1 queued` / `◆REV off`).
2. Ask whether they want it, and separately whether they want the loaded-skills badges (`skills-segment.sh`, only
   useful in repos with a `.claude/skills/load-skills/SKILL.md` list).
3. If yes: read their `statusLine` setting (`~/.claude/settings.json`, or the project's). If they have a command,
   append `$(bash "<absolute plugin root>/statusline/rev-segment.sh" <<<"$input")` to what it prints, reading stdin
   once into `input` first if it does not already. If they have none, create `~/.claude/statusline.sh` that reads stdin
   into `input` and prints the chosen segments, and point `statusLine` at it. Use the absolute plugin root
   (`${CLAUDE_PLUGIN_ROOT}` expanded now), since the statusline does not get that variable.
4. Show the final diff of their settings before saving, and save only on a yes.
