---
name: murmur-release
description: Cut and publish a new Murmur version from the PRs merged since the last release tag. Use when the owner asks to release, bump the version, write release notes, tag, publish, or ship an in-app update.
---

# Murmur release

Turn the work merged into `main` since the last tag into one versioned, signed release that the
in-app updater can deliver. A release is a deliberate event: merging a PR never updates anyone.

Read `AGENTS.md` first. The version root is `[workspace.package].version` in `Cargo.toml`;
`package.json` mirrors it.

Fast path: collect PRs -> choose the version -> write notes -> bump -> `pnpm check` and E2E ->
commit, tag, push -> review the draft -> publish -> verify `latest.json`.

## 0. Inputs

- The target version from the request (for example `0.2.0`). If the request does not name one,
  propose one in step 2 and ask before tagging.
- A clean `main` that is up to date, `gh` logged in, `pnpm`, and the dev models for E2E.
- The signing identity `Murmur Self-Signed` (see `scripts/make-signing-cert.sh`). Never change it:
  a new certificate resets every user's microphone and accessibility grants.

## 1. Collect what changed

```sh
git switch main && git pull --ff-only
LAST=$(git describe --tags --abbrev=0)
LAST_DATE=$(git log -1 --format=%cs "$LAST")
git log --oneline "$LAST"..HEAD
gh pr list --state merged --base main --limit 200 \
  --search "merged:>=$LAST_DATE" \
  --json number,title,mergedAt,url,labels
```

- `git log` catches direct commits; `gh pr list` names the PRs. Fast-forward merges do not create
  merge commits, so use both.
- Drop anything already in the last release (a PR merged on the tag's date can appear twice) and
  the release commit itself.
- Ignore anything that is not on `main`.

## 2. Choose the version

While the app is 0.x:

| Change | Bump |
|---|---|
| breaking change (`!` or `BREAKING CHANGE` in a title) | minor |
| `feat` | minor |
| `fix`, `perf`, `refactor` | patch |
| only `chore`, `docs`, `test`, `ci` | patch, or skip the release |

State the proposed version and the reason in one line. If the request named a version, use it.

## 3. Write the release notes

Group by user-visible outcome, not by commit type:

```markdown
## Features
- ... (#12)

## Fixes
- ... (#9)

## Performance
- ... (#2)

## Verification
- `pnpm check` passes.
- E2E: 74/74 dictation checks (and the annotation group when it was touched).
```

Rules:

- One line per PR, plain language, no internal ticket numbers.
- Never paste transcript text, local-only docs (the private section of `.gitignore`) or
  secrets.
- No em dash (U+2014); use a plain dash.
- Write the notes to a temp file such as `/tmp/murmur-notes.md`, not into the repo.

## 4. Bump and verify

Set the version in both places:

- `Cargo.toml`: the `[workspace.package].version` line (not the dependency `version` lines).
- `package.json`: the top-level `"version"`.

Then run the gates:

```sh
pnpm check
MURMUR_TEST_MODELS=~/.murmur-dev-models bash scripts/e2e.sh
# when the release touches annotations:
MURMUR_TEST_MODELS=~/.murmur-dev-models bash scripts/e2e.sh annotations
```

- `pnpm check` is the gate. When GitHub Actions cannot run, it is the only gate.
- `Cargo.lock` may change with the version; commit it.
- Optional smoke test of the real bundle: `CI=true APPLE_SIGNING_IDENTITY="Murmur Self-Signed"
  pnpm tauri build`, then quit the installed app and open
  `target/release/bundle/macos/Murmur.app`.

## 5. Commit and tag

```sh
git add Cargo.toml Cargo.lock package.json
git commit -m "chore(release): X.Y.Z" -m "intent(release): ..." -m "decision(release): ..."
git tag -a "vX.Y.Z" -m "Murmur X.Y.Z"
git push origin main --tags
```

The tag push starts `.github/workflows/release.yml`: it builds the DMG and the signed updater
artifacts and creates a **draft** release. A draft is invisible to the updater.

## 6. Publish

Wait for the workflow, then review the draft:

```sh
gh run list --workflow Release --limit 1
gh release view "vX.Y.Z" --json isDraft,isPrerelease,assets
```

The draft must hold `Murmur_X.Y.Z_aarch64.dmg`, `Murmur_X.Y.Z_aarch64.app.tar.gz`, its `.sig`, and
`latest.json`. Download and open the DMG once. Then publish:

```sh
gh release edit "vX.Y.Z" --draft=false
```

### When Actions cannot run

Build locally and create the release by hand. The tag already exists.

```sh
V=X.Y.Z
CI=true APPLE_SIGNING_IDENTITY="Murmur Self-Signed" \
  TAURI_SIGNING_PRIVATE_KEY="<path or key>" \
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD="..." \
  pnpm tauri build

# The local bundle is unversioned; the release asset and latest.json use the versioned name.
cp target/release/bundle/macos/Murmur.app.tar.gz \
   "target/release/bundle/macos/Murmur_${V}_aarch64.app.tar.gz"
cp target/release/bundle/macos/Murmur.app.tar.gz.sig \
   "target/release/bundle/macos/Murmur_${V}_aarch64.app.tar.gz.sig"

SIG=$(tr -d '\n' < "target/release/bundle/macos/Murmur_${V}_aarch64.app.tar.gz.sig")
jq -n --arg v "$V" --arg notes "$(cat /tmp/murmur-notes.md)" \
  --arg pub "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg sig "$SIG" \
  --arg url "https://github.com/naoya5/murmur/releases/download/v$V/Murmur_${V}_aarch64.app.tar.gz" \
  '{version:$v, notes:$notes, pub_date:$pub, platforms:{"darwin-aarch64":{signature:$sig,url:$url},"darwin-aarch64-app":{signature:$sig,url:$url}}}' \
  > target/release/bundle/latest.json

gh release create "v$V" --draft --title "Murmur $V" --notes-file /tmp/murmur-notes.md \
  "target/release/bundle/dmg/Murmur_${V}_aarch64.dmg" \
  "target/release/bundle/macos/Murmur_${V}_aarch64.app.tar.gz" \
  "target/release/bundle/macos/Murmur_${V}_aarch64.app.tar.gz.sig" \
  target/release/bundle/latest.json
gh release edit "v$V" --draft=false
```

The updater reads `releases/latest/download/latest.json`, and GitHub's "latest" ignores drafts and
prereleases.

## 7. Verify the update path

```sh
curl -sL https://github.com/naoya5/murmur/releases/latest/download/latest.json | jq -r .version
```

- The value must be `X.Y.Z`.
- Update from the previously installed app (Settings > About > check) and confirm the version in
  the About tab.
- If the app does not see it: the release is still a draft or a prerelease, `latest.json` is
  missing, the version is not greater than the installed one, or the signature does not match the
  public key in `src-tauri/tauri.conf.json`.

## 8. If it goes wrong

- Before publishing: `gh release delete "vX.Y.Z" --yes`, `git push --delete origin "vX.Y.Z"`,
  `git tag -d "vX.Y.Z"`, fix, then repeat from step 4.
- After publishing: never reuse the version. Ship the fix as the next patch.
- Before installing a build with a database migration, copy
  `~/Library/Application Support/io.github.naoya5.murmur/murmur.db` aside.

## Never

- Commit local-only docs (the private section of `.gitignore`).
- Commit keys, `.p12` files, or `TAURI_SIGNING_PRIVATE_KEY`.
- Change the signing identity or the updater public key.
- Bump the version without running `pnpm check` and the E2E.
- Publish a release whose DMG you have not opened once.
