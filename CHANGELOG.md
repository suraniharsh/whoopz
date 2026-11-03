# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `whoops shell` starts your shell (bash or zsh) with Whoopz watching it.
- `whoops` undoes the last command that changed files: deletes (including `rm -rf`), moves
  and renames, overwrites (`>`, `cp` onto a file, `sed -i`), truncates, and new files and
  folders.
- `whoops list` shows what each command changed, and `whoops <n>` undoes a specific one.
- Undo never overwrites anything: if something new is in the way, that step is skipped and
  reported.
