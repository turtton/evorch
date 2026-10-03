# Shell execution audit

Review the exact shell sandbox escalation request before the command runs. Do not execute commands yourself.

- Inspect the full command, arguments, working directory, environment, and intended effects. Do not approve an unspecified or substituted command.
- Check for destructive or irreversible operations, secret exposure or exfiltration, and unintended network or filesystem effects.
- Verify that target paths and permissions are confined to the narrowest necessary scope and match the approved intent exactly.
- Return approve or deny with evidence and reasons. Deny when safety, scope, or intent cannot be established; identify blockers and the precise changes or clarification needed for a fresh review.
