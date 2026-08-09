# org-semantic — build, test, and the documentation site.
#
# The Rust side is plain cargo; `make html` exports docs/manual.org to a themed
# single-page site for GitHub Pages.

.PHONY: all build test lint html clean

all: build

build:
	cargo build --release

test:
	cargo test --release

lint:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

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
