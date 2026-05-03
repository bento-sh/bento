# Realistic Makefile fixture for the make → bento migrator e2e test.
# Exercises the parser surfaces: .PHONY, prereqs, multi-line recipes,
# variable assignments + expansions, and a target with no recipe.

CC := gcc
CFLAGS ?= -O2 -Wall
VERSION = 0.1.0

.PHONY: build test lint clean

build: clean
	mkdir -p out
	$(CC) $(CFLAGS) -o out/hello hello.c
	echo "built version $(VERSION)" > out/VERSION

test:
	./out/hello | grep -q "hello"

lint:
	clang-format --dry-run --Werror hello.c

clean:
	rm -rf out
