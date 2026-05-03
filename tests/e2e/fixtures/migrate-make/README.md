# migrate-make fixture

Fixture for the `bento migrate make` e2e tests. The integration
test (composed across all four migrators by the parent task) runs the
migrator against this directory and asserts the emitted `dish.toml`,
`bento.toml`, and `bentos/prod.toml` look right.

## What's exercised

The `Makefile` here intentionally covers the parser surfaces:

- `.PHONY: build test lint clean` declaration — should land in an
  `Inferred` note.
- Variable assignments (`CC := gcc`, `CFLAGS ?= …`, `VERSION = 0.1.0`)
  — should be skipped without producing tasks.
- Variable expansions (`$(CC)`, `$(CFLAGS)`, `$(VERSION)`) inside a
  recipe — should pass through verbatim with one `Inferred` note.
- A target with prerequisites (`build: clean`) — should produce an
  `Inferred` note about intra-task dependencies.
- A multi-line recipe (`build`) — recipe lines should join with ` && `
  in the emitted `run` field.
- Single-line recipes (`test`, `lint`, `clean`) — straight passthrough.

This fixture is fed *into* the migrator; it doesn't itself need to
build anything. There is no `hello.c` because the migrator never runs
the recipes — it only reads the Makefile.
