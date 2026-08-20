# AGENTS.md

## Project identity

This repository is an **unofficial Rust implementation** of
[Cordis](https://github.com/cordisjs/cordis), the JavaScript framework for
plugin-based applications.

- It is a from-scratch port. It is not affiliated with or endorsed by the
  official Cordis project.
- Design rationale and story cards live under `docs/stories/` (Chinese);
  the plugin ABI protocol is documented in `docs/abi.md` (English) and
  `docs/abi_cn.md` (中文).
- User-facing documentation should be provided in both Chinese and English:
  the default file (`README.md`, `docs/X.md`) is English, and the Chinese
  translation lives next to it with a `_cn` suffix (`README_cn.md`,
  `docs/X_cn.md`).

## Commit conventions

All commit messages MUST:

1. Be written in **English**.
2. Follow the [Conventional Commits](https://www.conventionalcommits.org/)
   specification: `type(scope): summary`.
   - Allowed types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`,
     `ci`, `build`, `perf`, `style`.
3. **NOT contain story-card identifiers** such as `E9`, `B13`, `H2` (the
   internal planning IDs in `docs/stories/`). Describe what changed, not
   which card it implements.

Examples:

```text
feat(sdk): bridge the context surface to so plugins
fix(loader): reject plugins with an unsupported abi version
docs: add bilingual abi protocol documentation
test(loader): cover the context bridge end to end
```

## Quality gate

Run `./scripts/quality.sh` (fmt, clippy, test, doc) before committing.
