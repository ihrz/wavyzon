# Wavyzon changelogs

---

This directory contains the changelogs of current and previous Wavyzon releases, archived for reference and historical tracking.

## Structure

Changelogs are organized by language (`en` for English, `fr` for French) and then by Wavyzon version number.

Each version directory contains:

* `CHANGELOG_<LANG>_<VERSION>.md` — the changelog in Markdown
* optionally `CHANGELOG_<LANG>_<VERSION>.pdf` — a PDF export of the same text

## Example

```text
en/
└── 0.1.0/
    └── CHANGELOG_EN_0.1.0.md

fr/
└── 0.1.0/
    └── CHANGELOG_FR_0.1.0.md
```

## Notes

* Version folders are named after their corresponding Wavyzon release (`version` in the root `Cargo.toml`, tagged `v<version>`).
* Wavyzon follows [Semantic Versioning](https://semver.org/). Until 1.0.0, minor versions may contain breaking changes; they are always called out in the changelog.
* The root of the repository contains `CHANGELOG.md` (English) and `CHANGELOG_FR.md` (French), which always reflect the latest release.
* Changes not yet released are collected by contributors in merge requests and written up in both languages at release time.
