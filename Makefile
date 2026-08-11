# org-semantic — build, test, and the documentation site.
#
# The Rust side is plain cargo; `make html` exports docs/manual.org to a themed
# single-page site for GitHub Pages.  The Emacs client under lisp/ is checked
# the way any package is: byte-compiled with warnings fatal, checkdoc, ERT.

.PHONY: all build test test-rust test-elisp lint lint-rust lint-elisp html clean

EMACS ?= emacs
ELISP := lisp/org-semantic.el test/org-semantic-tests.el

all: build

build:
	cargo build --release

test: test-rust test-elisp

test-rust:
	cargo test --release

# The tests that need the binary find it at target/release and skip if it is
# not there, so this is worth running on its own; `make test` builds it first.
test-elisp:
	$(EMACS) --batch --no-init-file -L lisp -l ert \
		-l test/org-semantic-tests.el -f ert-run-tests-batch-and-exit

lint: lint-rust lint-elisp

lint-rust:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

# Two checks, and the second needs the exit status faked: checkdoc reports
# through `display-warning' and returns nil either way, so a style complaint
# would otherwise pass a build.  The .elc files are removed again -- they are
# built to be checked, not kept, and a stale one shadows the source.
lint-elisp:
	$(EMACS) --batch --no-init-file -L lisp \
		--eval "(setq byte-compile-error-on-warn t)" \
		-f batch-byte-compile $(ELISP)
	@rm -f lisp/*.elc test/*.elc
	@for f in $(ELISP); do \
		$(EMACS) --batch --no-init-file -L lisp \
			--eval "(progn \
			          (require 'checkdoc) \
			          (defvar said 0) \
			          (advice-add 'display-warning :before \
			            (lambda (&rest _) (setq said (1+ said)))) \
			          (checkdoc-file \"$$f\") \
			          (kill-emacs (if (> said 0) 1 0)))" || exit 1; \
	done

# Emacs exports docs/manual.org to public/index.html.  htmlize gives the source
# blocks their syntax highlighting; without it they render as plain text.
#
# The output path is expanded before the buffer is opened: inside it,
# default-directory is docs/, so a relative path would land in docs/public/.
DOC_THEME_FILES := $(shell find docs/org-html-themes -type f)

html: public/index.html

public/index.html: docs/manual.org $(DOC_THEME_FILES)
	@mkdir -p public
	emacs --batch --no-init-file \
		--eval "(progn \
		          (require 'package) \
		          (setq package-user-dir (expand-file-name \".make-elpa\")) \
		          (add-to-list 'package-archives '(\"melpa\" . \"https://melpa.org/packages/\") t) \
		          (package-initialize) \
		          (unless (package-installed-p 'htmlize) \
		            (package-refresh-contents) \
		            (package-install 'htmlize)))" \
		--eval "(require 'htmlize)" \
		--eval "(require 'ox-html)" \
		--eval "(setq org-html-doctype \"html5\" \
		              org-html-validation-link nil \
		              org-export-with-broken-links t \
		              org-html-htmlize-output-type 'css)" \
		--eval "(let ((out (expand-file-name \"public/index.html\"))) \
		          (with-current-buffer (find-file-noselect \"docs/manual.org\") \
		            (org-export-to-file 'html out)))"
	cp -R docs/org-html-themes/src public/

clean:
	cargo clean
	rm -rf public .make-elpa
