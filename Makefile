# asd — build, install, and package the binary for the host platform.
#
#   make            # build the full asd (CLI + daemon + GUI) for this host
#   make cli        # build the CLI/daemon-only binary (no GUI)
#   make install    # install to $(PREFIX)/bin   (PREFIX, DESTDIR honored)
#   make package    # stage a tar.gz install archive for THIS platform in dist/
#   make cross-arm  # cross-build + package the aarch64 CLI archive (needs cross)
#   make dist       # package every buildable target
#   make clean

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

.DEFAULT_GOAL := build
.PHONY: build cli install uninstall package package-cli cross-arm dist archive clean help

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

dist: package cross-arm ## Package every buildable target's archive

# Stage $(BIN) + LICENSE/README into $(DIST)/$(NAME)/ and tar.gz it.
archive:
	@mkdir -p "$(DIST)/$(NAME)"
	cp "$(BIN)" "$(DIST)/$(NAME)/"
	[ -f LICENSE ] && cp LICENSE "$(DIST)/$(NAME)/" || true
	[ -f README.md ] && cp README.md "$(DIST)/$(NAME)/" || true
	tar -czf "$(DIST)/$(NAME).tar.gz" -C "$(DIST)" "$(NAME)"
	rm -rf "$(DIST)/$(NAME)"
	@echo "packaged $(DIST)/$(NAME).tar.gz"

clean: ## Remove build output and dist/
	$(CARGO) clean
	rm -rf "$(DIST)"

help: ## List targets
	@awk 'BEGIN{FS=":.*## "} /^[a-zA-Z_-]+:.*## /{printf "  \033[36m%-12s\033[0m %s\n",$$1,$$2}' $(MAKEFILE_LIST)
