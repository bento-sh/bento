# migrate-make fixture

Fixture for the `bento migrate make` e2e tests. The test runs the
migrator against this directory, then runs `bento build` on the
result — the migrated workspace has to actually *work*, not just
parse.

## What's exercised

The `Makefile` covers the Make-only syntax a naive recipe copier gets
wrong, all of which `run = "make <target>"` handles for free:

- Variable assignments (`CC := gcc`, `CFLAGS ?= …`, `VERSION = 0.1.0`)
  — must not become tasks.
- Variable expansions (`$(CC)`, `$(CFLAGS)`, `$(VERSION)`) inside a
  recipe — shell would read `$(CC)` as command substitution.
- A target with prerequisites (`build: clean`) — Make orders these;
  bento tasks don't.
- A multi-line recipe (`build`).
- `.PHONY: build test lint clean` — a directive, not a target.

`hello.c` is here so `make build` genuinely compiles: the e2e test
builds the migrated workspace and asserts the dish has no adapter
(`bento install` is a no-op) rather than a `node-npm` placeholder that
fails with `npm error enoent Could not read package.json`.
