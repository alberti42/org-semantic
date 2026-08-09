# org-semantic — build, test, and the documentation site.
#
# The Rust side is plain cargo; `make html` exports README.org to a themed
# single-page site for GitHub Pages, the way ghostel does it.

.PHONY: all build test lint html clean

all: build

build:
	cargo build --release

test:
	cargo test --release

lint:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

# Emacs exports README.org to public/index.html.  htmlize gives the source
# blocks their syntax highlighting; without it they render as plain text.
DOC_THEME_FILES := $(shell find docs/org-html-themes -type f)

html: public/index.html

public/index.html: README.org $(DOC_THEME_FILES)
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
		--eval "(with-current-buffer (find-file-noselect \"README.org\") \
		          (org-export-to-file 'html \"public/index.html\"))"
	cp -R docs/org-html-themes/src public/

clean:
	cargo clean
	rm -rf public .make-elpa
