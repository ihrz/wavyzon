# Security policy

## Reporting a vulnerability

Please don't open a public issue for security problems. Report them privately instead:

- **GitLab (preferred):** open a [new issue](https://gitlab.com/ihrz/wavyzon/-/issues/new?issuable_template=Security) and tick **"This issue is confidential"**. Only project members can see it.
- **GitHub mirror:** use the [private advisory form](https://github.com/ihrz/wavyzon/security/advisories/new).

Include what you found, how to reproduce it, the Wavyzon version or commit, and what an attacker could do with it. You'll get a reply within a week, and we'll keep you updated until it's fixed. We're happy to credit you in the changelog once a fix is released, unless you'd rather stay anonymous.

## Supported versions

Only the latest release and the latest commit on `main` receive security fixes.

## Scope

Things we especially want to hear about:

- Authentication bypasses on the REST or WebSocket API (`WAVYZON_PASSWORD`).
- Server-side request forgery through the HTTP audio source, including ways around `WAVYZON_HTTP_ALLOW_PRIVATE=false` (redirects, DNS rebinding, IPv6 or encoded addresses).
- Path traversal out of `WAVYZON_LOCAL_ROOT` or `WAVYZON_DATA_DIR`.
- Crashes or unbounded memory use triggered by crafted audio files or streams.
- Problems in voice encryption (DAVE end-to-end encryption, transport encryption).

Plugins run inside the Wavyzon process with its full privileges. A vulnerability in a third-party plugin should be reported to that plugin's authors; a flaw in the plugin API itself that lets a plugin escape what it was granted belongs here.

## Keeping your node safe

- Set a long, random `WAVYZON_PASSWORD`. Every request is authorised with it.
- Don't expose Wavyzon directly to the internet unless you have to. Put it on a private network with your bot, or behind a firewall that only allows your bot's address.
- Leave `WAVYZON_HTTP_ALLOW_PRIVATE=false` unless you trust every client that can reach the node; otherwise anyone with the password can make it fetch addresses on your internal network.
- Only load plugins you trust (`WAVYZON_PLUGINS`).
- Never commit your `.env`. It holds your password and API credentials.
- Run the official Docker image or the binary as an unprivileged user (the image already does).
