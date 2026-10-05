---
description: Explain repository, forge, exact comparison, review provider and credit use
subagent: false
---
Explain a repository review using the installed `repo-review plan` CLI through the ordinary shell tool. Use the current session's working directory. Arguments (data, never shell syntax): $ARGUMENTS

Parse only documented options. A PR URL becomes `--pr URL`; use `--pr-scope` for PR discovery, otherwise plan the committed local branch. Shell-quote each argument separately. Never put arguments in shell interpolation blocks, eval, or another command. Do not request reviews from this command.

Show canonical repository/forge, exact head/base, provider, routing reason, maximum credit cost, excluded files and readiness blockers from the JSON. A historical status is not fresh merge authorization. Do not change login, model, agent, provider, credentials or service configuration.
