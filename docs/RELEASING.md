# Releasing

A release is a tag on main, built in CI and drafted until a board has run it (ADR 12):

```sh
make release VERSION=x.y.z   # bump on release/vx.y.z, open its PR; check gates it
                             # merge it, then on main:
make tag                     # tag origin/main as its version, push the tag alone
```

The tag drafts the release (`.github/workflows/release.yml`). Run the draft's binary on a board,
then `gh release edit vx.y.z --draft=false`. Verify a published binary: signed by the release
workflow, on its tag, not by any workflow here.

```sh
gh attestation verify <binary> -R fabbrito/magellan-device \
  --signer-workflow fabbrito/magellan-device/.github/workflows/release.yml \
  --source-ref refs/tags/vx.y.z
```

Repository settings are the maintainer's, by hand — no workflow holds them. The rulesets are
`.github/rulesets/`: `main` takes no push but a merged PR with `check` green, and no bypass; a `v*`
tag is never moved or deleted, and only an admin creates one. In order — `selected` comes before its
list:

```sh
r=repos/fabbrito/magellan-device
for f in .github/rulesets/*.json; do gh api -X POST $r/rulesets --input "$f"; done
gh api -X PUT $r/actions/permissions -F enabled=true -f allowed_actions=selected \
  -F sha_pinning_required=true
gh api -X PUT $r/actions/permissions/selected-actions --input - <<<'{
  "github_owned_allowed": true, "verified_allowed": false,
  "patterns_allowed": ["jdx/mise-action@*", "Swatinem/rust-cache@*"]}'
gh api -X PUT $r/actions/permissions/fork-pr-contributor-approval \
  -f approval_policy=all_external_contributors
gh api -X PUT $r/immutable-releases
gh api -X PUT $r/private-vulnerability-reporting
gh api -X PUT $r/vulnerability-alerts
```

`GITHUB_TOKEN` stays read-only by default, and Actions may not approve PRs. A ruleset change is
another run of its file through `gh api -X PUT $r/rulesets/<id>`.
