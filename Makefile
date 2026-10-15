# Builds and installs LOST from source. For packages, see the README.
#
#   make                       build (release)
#   sudo make install          into /usr/local (PREFIX=/usr for elsewhere)
#   sudo make uninstall
#   make tarball               lost-VERSION-ARCH-linux.tar.gz in target/
#   make deb                   a .deb, with cargo-deb (cargo install cargo-deb)
#   make appimage              an AppImage, with appimagetool (dist/appimage.sh)
#   make spider                lost-spider, the crawler (not built by default)

PREFIX ?= /usr/local
DESTDIR ?=
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
ARCH := $(shell uname -m)
NAME := lost-$(VERSION)-$(ARCH)-linux

.PHONY: all build install uninstall tarball deb appimage spider clean

all: build

build:
	cargo build --release

install:
	PREFIX="$(PREFIX)" DESTDIR="$(DESTDIR)" sh dist/install.sh

uninstall:
	PREFIX="$(PREFIX)" DESTDIR="$(DESTDIR)" sh dist/install.sh --uninstall

tarball: build
	rm -rf target/$(NAME)
	mkdir -p target/$(NAME)/dist target/$(NAME)/res/images
	cp target/release/lost README.md LICENSE target/$(NAME)/
	cp -r dist/icons dist/install.sh dist/lost.desktop dist/lost.service dist/lost.toml target/$(NAME)/dist/
	cp res/images/lost.svg target/$(NAME)/res/images/
	tar -C target -czf target/$(NAME).tar.gz $(NAME)
	@echo "target/$(NAME).tar.gz"

deb:
	cargo deb

appimage: build
	sh dist/appimage.sh

spider:
	cargo build --release --no-default-features --features spider --bin lost-spider
	@echo "target/release/lost-spider"

clean:
	cargo clean
