# asd — build, install, and package the binary for the host platform.
#
#   make            # build the full asd (CLI + daemon + GUI) for this host
#   make cli        # build the CLI/daemon-only binary (no GUI)
#   make install    # install to $(PREFIX)/bin   (PREFIX, DESTDIR honored)
#   make package    # stage a tar.gz install archive for THIS platform in dist/
#   make cross-arm  # cross-build + package the aarch64 CLI archive (needs cross)
#   make deb        # build a Debian .deb for this host (needs `cargo deb`)
#   make win        # cross-build + zip a Windows x64 package (see note)
#   make dist       # package every buildable Linux target
#   make clean
#
# Note on `make win`: cross-compiling from Linux is best-effort. The GUI links
# wgpu, which does not cross-compile to Windows from Linux reliably (it targets
# MSVC natively). For a dependable Windows build, run `cargo build --release`
# on Windows, or build gui-only: `make win WIN_FEATURES="--no-default-features
# --features gui"`. Both need `cross` + Docker + Zig in the container.
#
# Behind a proxy? Copy .env.example to .env and set your proxy there — `make`
# loads it and forwards it into the cross container's Zig download.

# Load optional local proxy config (.env, gitignored) and export it so cross
# build containers can reach ziglang.org for the Zig download. Cross.toml passes
# these through into the container. Copy .env.example to .env to set yours.
-include .env
export HTTP_PROXY HTTPS_PROXY http_proxy https_proxy

CARGO   ?= cargo
PREFIX  ?= /usr/local
DESTDIR ?=
BINDIR  := $(DESTDIR)$(PREFIX)/bin
DIST    ?= dist

# Host target triple (e.g. x86_64-unknown-linux-gnu, aarch64-apple-darwin) and
# the workspace version — used to name "the install package for this platform".
TARGET  := $(shell rustc -vV | awk '/^host/{print $$2}')
VERSION := $(shell awk -F'"' '/^\[workspace\.package\]/{f=1} f&&/^version/{print $$2; exit}' Cargo.toml)

# aarch64 Linux ships CLI-only: wgpu (the GUI) can't be cross-compiled cheaply.
ARM_TARGET := aarch64-unknown-linux-gnu
# Windows x64. Feature set is overridable, e.g.
#   make win WIN_FEATURES="--no-default-features --features gui"
WIN_TARGET   := x86_64-pc-windows-gnu
WIN_FEATURES ?=

.DEFAULT_GOAL := build
.PHONY: build cli install uninstall package package-cli cross-arm win deb dist archive archive-zip clean help

build: ## Build the full asd binary (CLI + daemon + GUI) for the host
	$(CARGO) build --release

cli: ## Build the CLI/daemon-only binary (no GUI) for the host
	$(CARGO) build --release --no-default-features --features local

install: build ## Install the binary to $(PREFIX)/bin (honors PREFIX, DESTDIR)
	install -d "$(BINDIR)"
	install -m755 target/release/asd "$(BINDIR)/asd"
	@echo "installed $(BINDIR)/asd"

uninstall: ## Remove the installed binary
	rm -f "$(BINDIR)/asd"

package: build ## Package the host install archive (full, with GUI) into dist/
	@$(MAKE) --no-print-directory archive BIN=target/release/asd NAME=asd-$(VERSION)-$(TARGET)

package-cli: cli ## Package a CLI-only host archive into dist/
	@$(MAKE) --no-print-directory archive BIN=target/release/asd NAME=asd-$(VERSION)-$(TARGET)-cli

cross-arm: ## Cross-build + package the aarch64 CLI archive (needs `cross` + Zig)
	cross build --release --no-default-features --features local --target $(ARM_TARGET)
	@$(MAKE) --no-print-directory archive BIN=target/$(ARM_TARGET)/release/asd NAME=asd-$(VERSION)-$(ARM_TARGET)-cli

deb: build ## Build a Debian .deb (host arch, full binary; needs `cargo deb`)
	$(CARGO) deb --no-build
	@echo "packaged $$(ls -t target/debian/*.deb | head -1)"

win: ## Cross-build + zip a Windows x64 package (best-effort — see note below)
	cross build --release --target $(WIN_TARGET) $(WIN_FEATURES)
	@$(MAKE) --no-print-directory archive-zip BIN=target/$(WIN_TARGET)/release/asd.exe NAME=asd-$(VERSION)-$(WIN_TARGET)

dist: package cross-arm ## Package every buildable Linux target's archive

# Stage $(BIN) + LICENSE/README into $(DIST)/$(NAME)/ and tar.gz it.
archive:
	@mkdir -p "$(DIST)/$(NAME)"
	cp "$(BIN)" "$(DIST)/$(NAME)/"
	[ -f LICENSE ] && cp LICENSE "$(DIST)/$(NAME)/" || true
	[ -f README.md ] && cp README.md "$(DIST)/$(NAME)/" || true
	tar -czf "$(DIST)/$(NAME).tar.gz" -C "$(DIST)" "$(NAME)"
	rm -rf "$(DIST)/$(NAME)"
	@echo "packaged $(DIST)/$(NAME).tar.gz"

# Same, but a .zip (Windows convention).
archive-zip:
	@mkdir -p "$(DIST)/$(NAME)"
	cp "$(BIN)" "$(DIST)/$(NAME)/"
	[ -f LICENSE ] && cp LICENSE "$(DIST)/$(NAME)/" || true
	[ -f README.md ] && cp README.md "$(DIST)/$(NAME)/" || true
	cd "$(DIST)" && zip -qr "$(NAME).zip" "$(NAME)"
	rm -rf "$(DIST)/$(NAME)"
	@echo "packaged $(DIST)/$(NAME).zip"

clean: ## Remove build output and dist/
	$(CARGO) clean
	rm -rf "$(DIST)"

help: ## List targets
	@awk 'BEGIN{FS=":.*## "} /^[a-zA-Z_-]+:.*## /{printf "  \033[36m%-12s\033[0m %s\n",$$1,$$2}' $(MAKEFILE_LIST)
