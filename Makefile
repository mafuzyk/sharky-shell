PREFIX ?= /usr/local
DESTDIR ?=
RUBY ?= ruby
INSTALL ?= install

BINDIR := $(PREFIX)/bin
LIBDIR := $(PREFIX)/lib
SHARKY_LIBDIR := $(LIBDIR)/sharky

.PHONY: install uninstall test

install:
	$(INSTALL) -d "$(DESTDIR)$(BINDIR)" "$(DESTDIR)$(LIBDIR)" "$(DESTDIR)$(SHARKY_LIBDIR)"
	$(INSTALL) -m 0644 lib/sharky.rb "$(DESTDIR)$(LIBDIR)/sharky.rb"
	$(INSTALL) -m 0644 lib/sharky/*.rb "$(DESTDIR)$(SHARKY_LIBDIR)/"
	$(INSTALL) -m 0755 bin/sharky "$(DESTDIR)$(BINDIR)/sharky"
	@printf 'installed sharky into %s\n' "$(DESTDIR)$(PREFIX)"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/sharky"
	rm -f "$(DESTDIR)$(LIBDIR)/sharky.rb"
	rm -rf "$(DESTDIR)$(SHARKY_LIBDIR)"
	@printf 'uninstalled sharky from %s\n' "$(DESTDIR)$(PREFIX)"

test:
	$(RUBY) test/run.rb
