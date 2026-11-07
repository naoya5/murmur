# Security policy

## Supported versions

Only the latest release gets security fixes. Murmur updates itself from GitHub Releases, so keep
automatic update checks on, or check for updates in **Settings > About**.

## Report a vulnerability

Please do not open a public issue, pull request or discussion for a security problem.

Report it privately through GitHub:
[**Security > Report a vulnerability**](https://github.com/naoya5/murmur/security/advisories/new).

Include:

- the Murmur version (**Settings > About**), the macOS version and the Mac model
- what an attacker can do, and what they need first (for example, local access or a malicious
  file)
- the steps to reproduce, or a proof of concept

Do not include real transcripts, recordings or screen pictures. Use test data.

Murmur is maintained by one person in spare time, so a reply can take a few days. After a fix is
released, the advisory is published with credit to you, unless you ask to stay anonymous.

## Security model

Murmur is a desktop app with no server and no account. These are the properties it is built to
keep; a way to break one of them is a vulnerability.

| Property | How |
|---|---|
| Audio, text and screen pictures stay on the Mac | Speech recognition and the language models run locally. The only network code is in the model downloader (`src-tauri/src/adapters/models.rs`) and the updater, and `scripts/guardrails.sh` checks this on every change. |
| Downloads cannot be swapped | Each model comes from a pinned Hugging Face revision and is verified with SHA-256 before use. |
| Updates cannot be forged | Update packages are signed. The app verifies the signature against the public key in `src-tauri/tauri.conf.json` before it installs an update. |
| The UI cannot reach more than it needs | The web views use the CSP `default-src 'self'`. Each window gets its own Tauri capability; the annotation overlay may call only `submit_ink_stroke`. |
| The language-model helper stays isolated | The sidecar (`crates/murmur-llm`) talks to the app only through JSON Lines on stdin and stdout, and opens no network sockets. |
| Logs do not leak words | Logs never contain transcript text. |
| Saved annotations are private | The annotations folder is readable only by your user account (folder `0700`, files `0600`). |

## Out of scope

- Problems that need an attacker who already controls your user account or has root.
- macOS "unidentified developer" warnings. The app is not notarized on purpose; see the README.
- Vulnerabilities in third-party models or libraries that Murmur does not make worse. Report those
  upstream, and tell us if Murmur needs to update.
