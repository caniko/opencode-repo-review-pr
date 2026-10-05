# Forge-aware reviews for OpenCode

`repo-review` plans, requests and tracks revision-bound repository reviews. It
uses Greptile's confirmed free allowance for eligible GitHub targets, then
roborev for overflow. Forgejo targets use roborev. The acceptance engine is the
published `canix-toolbelt = 0.2.0` review library.

This repository contains the Rust CLI and five installable OpenCode V2 commands.
Provider configuration and worker qualification are operator-owned. The default
configuration dispatches no reviews: Greptile is disabled and roborev is
unqualified.

## Install

From this checkout, with Rust/Cargo and Git available:

```sh
cargo install --locked --path .
repo-review doctor
repo-review install-commands
```

Commands install under `$XDG_CONFIG_HOME/opencode/commands`, falling back to
`~/.config/opencode/commands`. Use `--directory PATH` for a project or a different
OpenCode installation. Installation is idempotent and refuses to overwrite
custom definitions. It does not change permissions, models, providers or
sessions. There is no ambiguous `/review` command.

| OpenCode command | Purpose |
|---|---|
| `/repo-review-plan` | Resolve the target, comparison and provider before dispatch |
| `/repo-review-local` | Review committed changes, or explicitly selected tracked working-tree changes |
| `/repo-review-pr` | Request or resume an exact-comparison PR review |
| `/repo-review-status` | Read the last saved observation for a request |
| `/repo-review-budget` | Inspect account-wide credit reservations and reconciled usage |

Templates instruct the agent to use ordinary permission-controlled shell calls.
They contain no executable interpolation or model override. Review output and
command arguments are data, not instructions to execute.

## Select the comparison

```sh
repo-review plan --repo /path/to/checkout --base origin/trunk
repo-review local --repo /path/to/checkout --base origin/trunk
repo-review local --repo /path/to/checkout --working-tree
repo-review plan --pr https://github.com/caniko/example/pull/123
repo-review pr --pr https://github.com/caniko/example/pull/123
repo-review gate --pr https://github.com/caniko/example/pull/123
```

Local review defaults to committed changes from the merge base through `HEAD`.
Without `--base`, the CLI requires the selected remote's existing symbolic
default-branch reference. It does not guess `main` or `master` and does not fetch
to refresh that reference. `--working-tree` selects the binary tracked diff
against `HEAD`, including staged changes. Planning lists excluded untracked
files; add files to Git before requesting their review.

PRs use the forge's current head, base, source repository and branches. A checkout
must match that target and head. `--remote-only --pr URL` requests a private
snapshot without relying on a matching checkout. `--expected-head FULL_SHA`
pins the expected head. PR review rejects local base/working-tree flags.

An explicit PR, a trusted canonical/fork mapping, the selected `--remote`, or an
unambiguous `origin` establishes target identity. A GitHub mirror alone does not
make a Forgejo repository Greptile-eligible. Known hosts are `github.com`
(GitHub Cloud), `codefloe.com` and `codeberg.org` (Forgejo). Other Forgejo hosts
require an exact entry in trusted configuration. GitHub Enterprise is outside
the qualified Greptile scope.

Review work uses atomically prepared private Git snapshots. User refs, branches
and hooks are not modified. The worker's confinement and its treatment of
repository-supplied configuration require separate qualification.

## Configure providers and credits

Policy comes from `$XDG_CONFIG_HOME/repo-review/config.json`, falling back to
`~/.config/repo-review/config.json`. Select an explicit trusted file with
`--config PATH` or `REPO_REVIEW_CONFIG`. See
[`examples/config.json`](examples/config.json); its providers remain disabled.
The CLI does not discover policy from an untrusted reviewed repository.

Greptile's default repository scope is `github.com/caniko/*`. Enabling it also
requires a GitHub-login-to-billing-account mapping, a confirmed effective maximum
PR cost (1, 3 or 10 credits), and an evidenced current provider billing period
with zero paid overage. PR charges are attributed to the PR author. Linked
account/organization eligibility must be verified outside this CLI. Unknown
identity, tier, eligibility or balance routes to roborev.

```sh
repo-review budget show ACCOUNT
repo-review budget reconcile ACCOUNT --receipt /path/to/current-period.json
```

A period receipt has this shape; supply actual provider-evidenced timestamps,
usage and policy rather than copying these illustrative values:

```json
{
  "start": 1790812800,
  "end": 1793491200,
  "allowance": 50,
  "reconciled_usage": 12,
  "evidence": "operator-owned billing evidence reference",
  "no_overage": true,
  "included_requests": []
}
```

Account-wide file locks and write-ahead reservations prevent independent clients
from spending the last credit twice. The 50-credit monthly allowance is a
maximum, not an assumed current balance. Calendar rollover alone never resets
it. Uncertain charges remain reserved across rollover until reconciled.
`included_requests` names settled local charge receipts proven to be included
in provider usage, preventing double counting without silently freeing charges.

An ambiguous dispatch can be settled explicitly after provider investigation:

```sh
repo-review budget settle ACCOUNT REQUEST_ID --charged --evidence "provider receipt reference"
```

Omitting `--charged` records an evidenced non-charge. Settlement is accounting;
it does not authorize replaying a possibly accepted provider request.

Local Greptile reviews additionally require the integrity-qualified CLI **3.2.3**,
attributed OAuth identity, its existing credential-file reference and an account
qualification receipt. `cli_auth_digest` binds the OAuth method, subject and
email through `greptile_cli::auth_identity_digest`; normal token refresh does
not change that binding. API-key authentication cannot use this allowance.
The CLI does not log in or change credentials. Local Greptile reviews cost one
credit and remain disabled until their account/input contract is qualified.

Roborev requires a provisioned Unix socket, a supported approved adapter
(`opencode`, `codex`, or `claude-code`) and a worker qualification receipt. The
client directly uses the v0.71 daemon API; it does not autostart a daemon. It
requests a single non-agentic review with `panel: none`. The worker qualification
must cover approved wrappers, repository-config handling, isolation, resource
limits and absence of provider failover. A policy flag is a reference to that
evidence, not a qualification performed by this client.

Existing GitHub credentials come from `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth
token`. Forgejo credentials use instance-scoped environment references from
`forgejo_token_env`. Credentials are excluded from durable review receipts.

## Resume and assess the result

Every request saves its comparison, policy digest, provider route and credit
reservation before dispatch. Repeating the original review command resumes that
request. Lost roborev enqueue replies are reconciled by job/UUID and immutable
snapshot identity, without a second enqueue. A timeout, missing reply or generic
rate-limit response never authorizes switching providers. Only the trusted
exact-head Greptile credit-limit rejection records a roborev handover; resume
the same command to execute it.

Local/PR commands observe pending work for at most 600 seconds by default.
`--timeout-seconds 0` observes once; the maximum is 3,600 seconds. An ambiguous
dispatch returns blocked for explicit reconciliation/resumption rather than
pretending that it was accepted. Polling respects `next_poll_at` and cannot
allocate a new request when the comparison changes.

The CLI worker retains a private failure-category receipt when authentication,
version validation, execution or status collection fails. Returned review content
is preserved if the later status query fails, but cannot establish acceptance
without its run attestation. A failed worker is not replayed; investigate that
existing attempt and reconcile its possible charge.

```sh
repo-review status REQUEST_ID
repo-review disposition REQUEST_ID --finding FINDING_ID --reason "source-backed assessment" --evidence "test or code reference"
```

Status is the last saved observation. It is not fresh merge authorization.
Findings require exact-review dispositions and a fresh observation. Earlier
unresolved PR findings carry forward across head/base/policy changes; dispose
them against their original request and finding IDs.

| Exit | Review verdict |
|---|---|
| 0 | Ready for this review scope |
| 20 | Unresolved findings |
| 21 | Pending |
| 22 | Blocked or unavailable evidence |
| 23 | Stale comparison |

Execution completion, confidence scores and empty legacy output cannot establish
acceptance. The adapters require comparison correlation and complete canonical
findings. Local evidence remains advisory. `gate` additionally requires the
separate forge merge gates and returns 22 while they are blocked.

PR roborev jobs compare the full observed target SHA to the source SHA
(`base..head`), including when the target tip differs from the merge base. The
client checks that range before submission and against persisted result metadata.
Local committed reviews retain their merge-base comparison; working-tree reviews
retain the explicitly captured tracked diff. These local scopes remain advisory.
An existing submission recorded with a different range blocks resumption; preserve
its state and reconcile it rather than deleting its ledger to enqueue again.

This client still uses published Toolbelt 0.2.0 and its local Unix transport. The
newer `github-receipt-v1` producer APIs require a separately qualified registry
release before consumer migration; this comparison fix does not qualify that
transport or its production worker.

Roborev dispatch explicitly requests the `low` severity floor and rejects jobs
that used a narrower filter. Working-tree recovery reads the single-job detail
to verify the captured diff; roborev's job listing omits those bytes. Roborev
0.71 does not provide file-coverage attestation for dirty reviews, so their
completed findings are collectible but acceptance remains blocked. The client
does not infer completeness from execution success or from its own file count.

## Extra forge gate and explicit writes

GitHub requires CI plus `review-policy` produced by the configured dedicated
GitHub App. Shared GitHub Actions App ID 15368 is rejected. Production deployment
must separately provision the App, its installation credential, trusted policy
authority and strict native branch protection applying to administrators.

`publish-check --pr URL --details-url HTTPS_URL` uses only
`REVIEW_POLICY_TOKEN` to publish a review-only check and verifies the returned
App identity. `merge --pr URL --expected-head FULL_SHA` revalidates provider
evidence, CI, the App-bound gate and branch protection; it is a dry run unless
`--apply` is supplied. A merge attempt has a write-ahead receipt and an atomic
expected-head guard. Ambiguous merge replies require authoritative reconciliation
before retrying. These write surfaces are implemented but remain live-unqualified.

Forgejo `publish --pr URL` explicitly posts/updates an advisory summary using a
publisher-owned marker after rechecking the comparison. Producer authenticity
and expected-head merge enforcement remain unqualified; Forgejo merges stay
blocked. Neither review commands nor OpenCode templates authorize these writes.

Both initial publication commands require completed evidence and persist a
request/kind-bound write-ahead attempt before the forge mutation. They bind the
publisher and exact payload, then retain the validated response. Pending reviews
cannot consume the initial attempt; blocked outcomes cannot publish success.
An existing attempt or surviving lock anchor prevents another initial write,
including when content or the details URL changes.

Use the explicit recovery/update lifecycle for an existing check or comment:

```sh
repo-review reconcile-publication --pr URL --expected-head FULL_SHA \
  --request REQUEST_ID --publication-id OBJECT_ID
repo-review update-publication --pr URL --expected-head FULL_SHA --remote-only \
  --request REQUEST_ID --publication-id OBJECT_ID \
  --expected-publication-digest DIGEST --details-url HTTPS_URL
```

The first command uses authoritative forge reads and records adoption in a
companion private journal. `FULL_SHA` is the original request's frozen head, even
if the PR has since moved or closed. This historical object read does not inspect
the current PR or collect provider evidence. Its receipt reports
`observation_scope: historical-publication-only` and `merge_authorized: false`.
Obtain `OBJECT_ID` from the forge; use the returned
`payload_digest` as `DIGEST`. It must match the original request/comparison,
publisher, object and exact payload. Original attempt files remain untouched.
Legacy attempts without a pre-effect publisher/payload binding remain blocked for
operator disposition; recovery does not fabricate that missing evidence.

The second command freshly collects existing review evidence without dispatching
another review, validates the original policy/comparison and previews an update.
Add `--apply` only when separately authorizing a PATCH of that same object.
GitHub requires `--details-url`; omit it for Forgejo. The default is a preview.
Updates still require the current PR to match the frozen comparison. They journal
their unknown outcome before PATCH; lost replies must be adopted
by `reconcile-publication` before another update. Missing/deleted objects, changed
authorization or unexpected payloads stay blocked. Recovery/update never POST a
replacement. Run these operations on the existing coordination authority.

Each adopted lifecycle has a separate durable tombstone binding the original
attempt, publisher and object. It is fsynced before the companion journal and
before any PATCH. A missing journal behind that tombstone blocks reconciliation
and updates, even when the forge returns the original payload. Intact older
journals acquire this protection before their next PATCH; missing older state
cannot be reconstructed or qualified by this migration.

Never delete attempt, journal, tombstone or lock files to enable replay. Offline regressions
use synthetic forge responses; actual forge publication remains live-unqualified.

## Shared coordination and state

`--coordinator SSH_HOST` / `REPO_REVIEW_COORDINATOR` delegates PR planning,
request/collection, status and budget inspection to one authority through
versioned SSH/stdio RPC. The authority uses its own policy, state and credentials;
clients cannot inject configuration. PR RPC requires an explicit URL. Unsupported
operations must run on the authority. Unreachable coordination never falls back
to a new independent local allowance. Local checkout transfer and an automatically
provisioned remote service are outside this RPC contract.

State defaults to `$XDG_STATE_HOME/repo-review` or `~/.local/state/repo-review`.
Use `--state-dir PATH` / `REPO_REVIEW_STATE` for an explicit root. State directories
must be user-owned and private; receipt/lock files are mode 0600. Symlink traversal
and shared/writable receipt files are rejected. Keep one authority for all consumers
of a billing account. Do not delete state to resolve an ambiguous dispatch.

## Development and qualification

Native checks use the project's approved environment and its Cargo lock:

```sh
cargo test --locked --jobs 2
cargo clippy --locked --all-targets --jobs 2 -- -D warnings
treefmt --config-file "$PWD/treefmt.toml" --tree-root-file Cargo.toml --working-dir "$PWD" --walk filesystem src tests
```

Specify all treefmt scope flags: this checkout can sit beneath another repository
with a wrapped formatter. Offline tests cover forge routing, concurrent credit
allocation, rollover/reconciliation, fake CLI execution and accounting, daemon
lost-reply recovery, dispositions, App protection, atomic snapshots, RPC failures
and process-group cleanup. Synthetic fixtures do not qualify live providers.

Canix's `tests/roborev/client.py` additionally drives this compiled CLI against
the exact Harbor roborev daemon in private offline PID/network namespaces. It
tests lost enqueue replies, restart/resume without replay, severity/diff identity,
stale heads, findings/dispositions, tracked working-tree scope, failed execution
and offline forge routing. Its receipt binds both executable hashes and verifies
cleanup; the adapter and repository are synthetic.

The separate Canix Harbor roborev package, generated Home Manager units,
service-switch fixtures, exact-output adapter replay and worker confinement have
their own qualification records. Thething's roborev integration must remain
disabled until those gates pass. Provider account verification, App/protection
provisioning, production activation, enrollment and live canaries require the
separately authorized rollout.
