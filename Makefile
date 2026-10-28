PREFIX ?= /usr/local
DESTDIR ?=
RUBY ?= ruby
INSTALL ?= install

BINDIR := $(PREFIX)/bin
LIBDIR := $(PREFIX)/lib
SHARKY_LIBDIR := $(LIBDIR)/sharky
MANDIR := $(PREFIX)/share/man/man1
VERSION := $(shell $(RUBY) -e 'require_relative "lib/sharky/version"; print Sharky::VERSION')
DISTNAME := sharky-$(VERSION)
DISTFILE := dist/$(DISTNAME).tar.gz

.PHONY: install uninstall test dist

install:
	$(INSTALL) -d "$(DESTDIR)$(BINDIR)" "$(DESTDIR)$(LIBDIR)" "$(DESTDIR)$(SHARKY_LIBDIR)" "$(DESTDIR)$(MANDIR)"
	$(INSTALL) -m 0644 lib/sharky.rb "$(DESTDIR)$(LIBDIR)/sharky.rb"
	$(INSTALL) -m 0644 lib/sharky/*.rb "$(DESTDIR)$(SHARKY_LIBDIR)/"
	$(INSTALL) -m 0755 bin/sharky "$(DESTDIR)$(BINDIR)/sharky"
	$(INSTALL) -m 0644 man/sharky.1 "$(DESTDIR)$(MANDIR)/sharky.1"
	@printf 'installed sharky into %s\n' "$(DESTDIR)$(PREFIX)"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/sharky"
	rm -f "$(DESTDIR)$(LIBDIR)/sharky.rb"
	rm -rf "$(DESTDIR)$(SHARKY_LIBDIR)"
	rm -f "$(DESTDIR)$(MANDIR)/sharky.1"
	@printf 'uninstalled sharky from %s\n' "$(DESTDIR)$(PREFIX)"

test:
	$(RUBY) test/run.rb

dist:
	rm -rf "dist/$(DISTNAME)" "$(DISTFILE)"
	mkdir -p "dist/$(DISTNAME)"
	cp -R bin lib man packaging test bench .github "dist/$(DISTNAME)/"
	cp README.md LICENSE CHANGELOG.md CONTRIBUTING.md SECURITY.md Makefile .editorconfig .gitattributes "dist/$(DISTNAME)/"
	tar -czf "$(DISTFILE)" -C dist "$(DISTNAME)"
	@printf 'created %s\n' "$(DISTFILE)"
