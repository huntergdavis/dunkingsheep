# Native usage providers

Native provider commands are available as `dunkingsheep usage-provider codex`,
`pi`, and `opencode`. They print a single JSON object and exit nonzero when
the reading is unknown or errored, so they can be used as commands in a usage
config. Codex and Pi accept `--max-age-s`; OpenCode still runs `opencode stats`
and does not report plan quota. Claude's subscription probe remains separate.

`dunkingsheep usage-provider claude` reads the Claude Code OAuth credential file
and queries the existing subscription usage endpoint. It does not use API-key
billing data. Missing, expired, malformed, timed-out or failed readings return
an error rather than a quota estimate; credentials and response bodies are not
included in error output. The separate project-budget integration is unchanged.
