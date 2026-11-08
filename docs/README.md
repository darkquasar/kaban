# Offline Kaban user documentation

This folder contains Mintlify-structured MDX source and a `docs.json` navigation
configuration. It is also readable directly in the checkout. There is no live
site, account setup, package installation, deployment or preview included.
The configuration uses a built-in theme and colors; no custom logo, remote image
or font asset is required. Mermaid diagrams are text, not generated bitmap files.

Start with [the introduction](index.mdx), then [quickstart](quickstart.mdx).
Navigation lists the agreed concept, guide, reference and operations pages.
Other lanes supply those pages; navigation entries are not placeholder files.

## Adapting later

Treat this folder as a candidate documentation root. In a separately authorized
publishing environment, the owner can select a compatible Mintlify toolchain,
adjust branding and deployment settings, and review navigation, MDX/Mermaid
rendering and links before publication. No renderer/version compatibility or
hosted-site behavior has been exercised by this offline delivery.

Within MDX, `.mdx` links target neighboring source pages so the checkout remains
readable. Links to `../` or `../../` guides outside this folder point to existing
repository documents, not hosted routes. A standalone site should copy or adapt
those guides and rewrite links deliberately; the JSON configuration does not
publish the rest of the repository or its release evidence.

## Scope and source of truth

Examples describe the implemented CLI and stdio MCP service, not an executed
tutorial. The workspace and four-target package version are `3.0.0`; see the
[releases page](https://github.com/darkquasar/kaban/releases) for available downloads.
The historical Linux prerelease is `3.0.0-alpha.1`.
[Release scope](developer/RELEASE.md) and [third-party limits](developer/THIRD_PARTY.md) remain
separate from rendering and publication readiness.

Current source and existing CONFIG, REGISTER, SYNC, REVIEW, INGEST and code-graph
guides are the implementation references. Older specs may describe reserved,
deferred or cancelled capabilities; their presence is not a delivery promise.
M6 two-store/personal memory is cancelled. HTTP/REST, embeddings and `ask`/workflow
orchestration are not working features in this release.
