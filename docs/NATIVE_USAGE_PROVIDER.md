# Native usage providers

Native provider commands are available as `dunkingsheep usage-provider codex`,
`pi`, and `opencode`. They print a single JSON object and exit nonzero when
the reading is unknown or errored, so they can be used as commands in a usage
config. Codex and Pi accept `--max-age-s`; OpenCode still runs `opencode stats`
and does not report plan quota. Claude's subscription probe remains separate.

