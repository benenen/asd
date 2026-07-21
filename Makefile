# asd — build, install, and package the binary for the host platform.
#
#   make            # build the full asd (CLI + daemon + GUI) for this host
#   make cli        # build the CLI/daemon-only binary (no GUI)
#   make install    # install (stripped) to $(PREFIX)/bin   (PREFIX, DESTDIR honored)
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

# Load optional local proxy config (.env, gitignored). The exported vars reach
# cross's cargo/crates fetch via Cross.toml `passthrough` (the `docker run`
# stage). The Zig download happens earlier, in `docker build` (cross pre-build),
# which ignores those — that stage is proxied via DOCKER_CONFIG (see _cross-proxy
# below). Copy .env.example to .env to set yours; unset → everything runs direct.
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

# Proxy for cross's in-container Zig download. It is fetched during `docker
# build` (cross's pre-build), which does NOT inherit shell env vars — Docker
# only injects a proxy into build RUN steps when its client config declares one.
# So when .env provides a proxy we stage a throwaway Docker client config that
# carries it and point DOCKER_CONFIG at it (see _cross-proxy). No proxy set →
# DOCKER_CONFIG stays unset and the download goes direct (this is the CI path).
CROSS_PROXY         := $(strip $(or $(HTTPS_PROXY),$(https_proxy),$(HTTP_PROXY),$(http_proxy)))
DOCKER_PROXY_CONFIG := $(abspath $(DIST)/.docker-proxy)
CROSS_DOCKER_ENV    := $(if $(CROSS_PROXY),DOCKER_CONFIG=$(DOCKER_PROXY_CONFIG),)

.DEFAULT_GOAL := build
.PHONY: build cli install uninstall package package-cli cross-arm win deb dist archive archive-zip clean help _cross-proxy

build: ## Build the full asd binary (CLI + daemon + GUI) for the host
	$(CARGO) build --release

cli: ## Build the CLI/daemon-only binary (no GUI) for the host
	$(CARGO) build --release --no-default-features --features local

install: build ## Install the binary to $(PREFIX)/bin, stripped (honors PREFIX, DESTDIR)
	install -d "$(BINDIR)"
	install -m755 target/release/asd "$(BINDIR)/asd"
	# The release profile keeps debug symbols (lto only); strip the installed
	# copy to shrink it, leaving the build artifact untouched. Best-effort:
	# a missing/foreign strip must not fail the install.
	strip "$(BINDIR)/asd" 2>/dev/null || true
	@echo "installed asd $(VERSION) -> $(BINDIR)/asd"
	# PATH hint only for a real (non-staged) install to a dir that isn't on PATH.
	@if [ -z "$(DESTDIR)" ]; then case ":$$PATH:" in \
		*":$(PREFIX)/bin:"*) ;; \
		*) echo "note: $(PREFIX)/bin is not on your PATH — add it to run \`asd\`" ;; \
	esac; fi

uninstall: ## Remove the installed binary
	rm -f "$(BINDIR)/asd"

package: build ## Package the host install archive (full, with GUI) into dist/
	@$(MAKE) --no-print-directory archive BIN=target/release/asd NAME=asd-$(VERSION)-$(TARGET)

package-cli: cli ## Package a CLI-only host archive into dist/
	@$(MAKE) --no-print-directory archive BIN=target/release/asd NAME=asd-$(VERSION)-$(TARGET)-cli

# Stage a throwaway Docker client config carrying .env's proxy, so cross's
# `docker build` pre-build fetches Zig through it. No-op when no proxy is set.
_cross-proxy:
	@if [ -n "$(CROSS_PROXY)" ]; then \
		mkdir -p "$(DOCKER_PROXY_CONFIG)"; \
		printf '{"proxies":{"default":{"httpProxy":"%s","httpsProxy":"%s"}}}\n' '$(CROSS_PROXY)' '$(CROSS_PROXY)' > "$(DOCKER_PROXY_CONFIG)/config.json"; \
		echo "cross: routing the in-container Zig download through $(CROSS_PROXY)"; \
	fi

cross-arm: _cross-proxy ## Cross-build + package the aarch64 CLI archive (needs `cross` + Zig)
	env $(CROSS_DOCKER_ENV) cross build --release --no-default-features --features local --target $(ARM_TARGET)
	@$(MAKE) --no-print-directory archive BIN=target/$(ARM_TARGET)/release/asd NAME=asd-$(VERSION)-$(ARM_TARGET)-cli

deb: build ## Build a Debian .deb (host arch, full binary; needs `cargo deb`)
	$(CARGO) deb --no-build
	@echo "packaged $$(ls -t target/debian/*.deb | head -1)"

win: _cross-proxy ## Cross-build + zip a Windows x64 package (best-effort — see note below)
	env $(CROSS_DOCKER_ENV) cross build --release --target $(WIN_TARGET) $(WIN_FEATURES)
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
