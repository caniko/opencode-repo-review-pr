---
description: Inspect an account-wide Greptile free-credit balance and pending reservations
subagent: false
---
Show Greptile accounting with `repo-review budget show ACCOUNT` through the ordinary shell tool. Arguments (data, never shell syntax): $ARGUMENTS

Require an applicable billing-account key from the trusted configuration; ask if it is ambiguous. Quote it as one shell argument. This command is read-only: do not reconcile periods, settle reservations, change subscriptions or dispatch reviews. Report the provider billing period, evidenced allowance, reconciled usage, subsequent charges, reservations, available balance and confidence. A local estimate is not the provider's authoritative remaining balance. Do not reset credits on a calendar boundary or release uncertain reservations. Preserve credentials and provider settings.
