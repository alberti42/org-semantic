;;; org-semantic-tests.el --- tests for the org-semantic client -*- lexical-binding: t; -*-

;; SPDX-License-Identifier: MIT

;;; Commentary:

;; Two kinds of test, and the division is the same one the Rust suite
;; makes.  Most of these need no server at all: what a vault is, what
;; goes on the wire, how a labelled error is read.  The rest drive the
;; real binary as a client does, and use the *lexical* index to do it --
;; which needs no embedding model, so the whole file runs offline in a
;; second or two, given a built binary and the cached classifier.
;;
;; Run them with:
;;
;;   make test-elisp
;;
;; What is deliberately not here: the semantic path, which would mean a
;; model download, and the concurrency the server already tests on its
;; own side.  Both were driven by hand instead -- see the manual.

;;; Code:

(require 'ert)
;; For `custom-variable-type' and `widget-convert', used by the test that
;; the setting's type admits what a vault declares.
(require 'cus-edit)
(require 'wid-edit)
(require 'org-semantic)

(defconst org-semantic-tests--root
  (file-name-directory
   (directory-file-name
    (file-name-directory (or load-file-name buffer-file-name))))
  "The repository, found from this file rather than from where Emacs was run.")

(defun org-semantic-tests--binary ()
  "The binary to test against, or nil if there is none built.

Note that a plain file name is not enough to look for: an empty
one, and any directory, answer `file-executable-p' with t."
  (seq-find (lambda (path)
              (and (stringp path) (not (string-empty-p path))
                   (file-regular-p path) (file-executable-p path)))
            (list (getenv "ORG_SEMANTIC")
                  (expand-file-name "target/release/org-semantic"
                                    org-semantic-tests--root)
                  (expand-file-name "target/debug/org-semantic"
                                    org-semantic-tests--root))))

(defmacro org-semantic-tests--with-vault (dir &rest body)
  "Run BODY with DIR bound to a temporary vault of three notes."
  (declare (indent 1))
  `(let ((,dir (make-temp-file "org-semantic-test" t)))
     (unwind-protect
         (progn
           (dolist (note '(("pumps.org" . "The turbo pump was baked out.")
                           ("atoms.org" . "Atom number in the science chamber.")
                           ("laser.org" . "The laser was relocked at noon.")))
             (with-temp-file (expand-file-name (car note) ,dir)
               (insert "#+title: " (file-name-base (car note)) "\n\n")
               (insert "* A heading\n" (cdr note) "\n")))
           ,@body)
       (delete-directory ,dir t))))


;;;; What goes on the wire

(ert-deftest a-parameter-nobody-set-is-not-sent ()
  "Nil means \"the server's own default\", which is silence, not null.

A nil `config' sent as JSON null fails to parse on the far side,
where an absent one means \"whatever the index was built under\"."
  (should (equal (org-semantic--params :vault "/v" :query "q" :k nil :model nil)
                 '(:vault "/v" :query "q")))
  (should (equal (org-semantic--params :a nil) nil))
  ;; Order is kept, which only matters for reading the events buffer.
  (should (equal (org-semantic--params :a 1 :b 2 :c 3) '(:a 1 :b 2 :c 3))))

(ert-deftest false-is-said-out-loud-and-nil-is-not-false ()
  "JSON has three things Lisp spells nil, so neither direction may guess."
  (should (eq (org-semantic--bool nil) :json-false))
  (should (eq (org-semantic--bool t) t))
  ;; And coming back: `false' arrives as a keyword, which is not nil.
  (should-not (org-semantic-true-p :json-false))
  (should-not (org-semantic-true-p nil))
  (should (org-semantic-true-p t)))


;;;; What a prefix argument asks for

(ert-deftest the-prefixes-are-ordered-by-what-they-cost ()
  "One `C-u' rehashes, two rebuild, and the two flags never travel together.

The order is the point: rehashing is 0.09 s of reading, a rebuild
is minutes, so a second `C-u' has to be the expensive one."
  (should (equal (org-semantic--reindex-flags nil) '(nil . nil)))
  (should (equal (org-semantic--reindex-flags '(4)) '(t . nil)))
  (should (equal (org-semantic--reindex-flags '(16)) '(nil . t)))
  ;; Any deeper, and any plain number, still resolve to one of the two.
  (should (equal (org-semantic--reindex-flags '(64)) '(nil . t)))
  (should (equal (org-semantic--reindex-flags 3) '(t . nil)))
  (should (equal (org-semantic--reindex-flags '-) '(t . nil))))


;;;; The policy, as the manual writes it

(ert-deftest the-documented-policy-is-exactly-the-defaults ()
  "The plist in the manual must serialise to `config.example.json'.

Read out of the manual rather than restated here, because a
drifted *document* is the failure -- someone copies it, sends a
policy that is not the defaults, and every search reports drift
against an index built without one.  The Rust side guards the JSON
example the same way.

It also catches the Lisp trap: JSON's array, false and null all
spell themselves nil in Emacs, and a list where a vector belongs
serialises to something the server will not parse."
  (let ((manual (expand-file-name "docs/manual.org" org-semantic-tests--root)))
    (unless (file-readable-p manual) (ert-skip "no manual"))
    (with-temp-buffer
      (insert-file-contents manual)
      (goto-char (point-min))
      (should (re-search-forward "^#\\+name: config-plist\n#\\+begin_src emacs-lisp\n" nil t))
      (let* ((start (point))
             (end (progn (should (re-search-forward "^#\\+end_src" nil t))
                         (match-beginning 0)))
             (form (car (read-from-string (buffer-substring start end))))
             ;; (setq org-semantic-config 'PLIST) -- take the PLIST.
             (plist (cadr (nth 2 form)))
             (encoded (json-serialize plist :false-object :json-false :null-object nil))
             (parse (lambda (s) (json-parse-string s :object-type 'alist :array-type 'list))))
        (should (eq (nth 0 form) 'setq))
        (should (eq (nth 1 form) 'org-semantic-config))
        (should (equal (funcall parse encoded)
                       (funcall parse
                                (with-temp-buffer
                                  (insert-file-contents
                                   (expand-file-name "config.example.json"
                                                     org-semantic-tests--root))
                                  (buffer-string)))))))))


;;;; What can be reached before the package is loaded

(defun org-semantic-tests--autoloaded ()
  "Every name carrying an autoload cookie, read out of the sources.

Read from the files rather than from `autoloadp', because what goes
wrong is the cookie drifting off the definition it belonged to --
and once that has happened the symbol is not autoloaded at all,
which is precisely what cannot be observed from inside a session
that has already loaded everything."
  (let (out)
    (dolist (file '("org-semantic.el" "org-semantic-ui.el" "org-semantic-results.el"))
      (let ((path (expand-file-name (concat "lisp/" file) org-semantic-tests--root)))
        (when (file-readable-p path)
          (with-temp-buffer
            (insert-file-contents path)
            (goto-char (point-min))
            (while (re-search-forward "^;;;###autoload\n(\\(?:cl-\\)?def[a-z]* \\([^ ()\n]+\\)" nil t)
              (push (intern (match-string 1)) out))))))
    out))

(ert-deftest every-entry-point-is-autoloaded ()
  "A command a user types must not need the package loaded first.

**The failure is silent until a restart.**  Inserting a helper
between a cookie and the function it belonged to moves the cookie
onto the helper, and nothing complains: the command still works for
the rest of that session, because everything is already loaded.  It
is the next fresh Emacs that cannot find it -- and `use-package'
`:bind' makes it worse by autoloading the command from the
package's *main* file, so the error names a file the command was
never in.

That is not hypothetical; it happened to `org-semantic-find' while
its prefix handling was being rewritten."
  (let ((autoloaded (org-semantic-tests--autoloaded)))
    ;; Found something at all, or this passes by checking nothing.
    (should (> (length autoloaded) 5))
    (dolist (command '(org-semantic-find
                       org-semantic-find-at-point
                       org-semantic-reindex
                       org-semantic-cancel
                       org-semantic-show-status
                       org-semantic-visit-hit))
      (should (memq command autoloaded)))
    ;; And nothing private is: an autoload for a `--' name is a cookie that
    ;; has slid off whatever it was written for.
    (dolist (name autoloaded)
      (should-not (string-match-p "--" (symbol-name name))))))


;;;; Which binary gets run

(ert-deftest every-published-platform-maps-to-its-own-asset ()
  "And the names are built the way the release workflow builds them.

The workflow composes each asset from a matrix entry -- a `platform'
token and an `ext' -- and this composes the same name from
`system-configuration'.  Two places spelling one string, so the
tokens and the extensions are read out of the workflow rather than
restated here: a name that drifts is a 404 at the one moment a user
has no binary to fall back on.

The version is part of every name, so a downloaded file says which
release it came from and a `SHA256SUMS' line cannot be mistaken for
another release's."
  (let ((matrix
         (with-temp-buffer
           (insert-file-contents
            (expand-file-name ".github/workflows/release.yml"
                              org-semantic-tests--root))
           (goto-char (point-min))
           (let (out)
             (while (re-search-forward
                     "^ *platform: +\\([^ \n]+\\)\n *ext: +\\([^ \n]+\\)$" nil t)
               (push (cons (match-string 1) (match-string 2)) out))
             (nreverse out)))))
    (should (= 4 (length matrix)))
    (dolist (case '(("aarch64-apple-darwin21.6.0" . "aarch64-macos")
                    ("x86_64-pc-linux-gnu"        . "x86_64-linux")
                    ("aarch64-unknown-linux-gnu"  . "aarch64-linux")
                    ("x86_64-w64-mingw32"         . "x86_64-windows")))
      (let* ((platform (cdr case))
             (ext (cdr (assoc platform matrix))))
        (should ext)
        (should (equal (org-semantic--release-platform (car case)) platform))
        (should (equal (org-semantic--release-asset (car case) "9.9.9")
                       (format "org-semantic-9.9.9-bin-%s.%s" platform ext)))))
    ;; And the default version is this package's, since that is the release
    ;; whose binary it was written against.
    (should (string-match-p (regexp-quote (concat "org-semantic-" org-semantic-version "-bin-"))
                            (org-semantic--release-asset "aarch64-apple-darwin21.6.0")))))

(ert-deftest a-platform-with-no-build-is-refused-rather-than-guessed ()
  "Intel macOS above all, which is the one a reader will meet.

There is no such asset -- ONNX Runtime publishes no
`x86_64-apple-darwin' -- so answering with the arm64 one would
install a binary that cannot run, and answering with anything at
all would 404.  Nil is the answer, and `org-semantic-install' turns
it into a sentence naming the reason."
  (dolist (c '("x86_64-apple-darwin21.6.0" "powerpc-apple-darwin"
               "aarch64-w64-mingw32" "sparc-sun-solaris2.11"))
    (should-not (org-semantic--release-platform c))
    (should-not (org-semantic--release-asset c))))

(ert-deftest a-downloaded-archive-is-checked-against-the-release-s-own-sums ()
  "A truncated download and a tampered one want the same refusal.

The hash is over the bytes on disk, so this also pins that the file
is read literally: reading an archive as text would decode it and
hash something the release never published.

`jka-compr-inhibit' is bound for the same reason `org-semantic-install'
binds it -- writing to a name ending `.tar.gz' otherwise gzips what
is written, which is how that hazard was noticed here in the first
place."
  (let* ((dir (make-temp-file "org-semantic-sums" t))
         (archive (expand-file-name "org-semantic-aarch64-macos.tar.gz" dir))
         (sums (expand-file-name "SHA256SUMS" dir)))
    (unwind-protect
        (let ((coding-system-for-write 'binary)
              (jka-compr-inhibit t))
          (write-region "\xff\xd8\xff not really a tarball" nil archive)
          (let ((digest (with-temp-buffer
                          (set-buffer-multibyte nil)
                          (insert-file-contents-literally archive)
                          (secure-hash 'sha256 (current-buffer)))))
            (write-region (format "%s  org-semantic-aarch64-macos.tar.gz\n" digest)
                          nil sums)
            (should-not (org-semantic--verify-checksum
                         archive sums "org-semantic-aarch64-macos.tar.gz"))
            ;; An asset the release says nothing about is refused too, rather
            ;; than passing for want of anything to compare against.
            (should-error (org-semantic--verify-checksum archive sums "other.tar.gz"))
            (write-region (format "%s  org-semantic-aarch64-macos.tar.gz\n"
                                  (make-string 64 ?0))
                          nil sums)
            (should-error (org-semantic--verify-checksum
                           archive sums "org-semantic-aarch64-macos.tar.gz"))))
      (delete-directory dir t))))

(ert-deftest a-binary-in-the-install-directory-needs-no-configuration ()
  "Unpack a release there and it is found, with nothing set.

And a *directory* of that name is not mistaken for it:
`file-executable-p' answers t for a directory, so the guard is
`file-regular-p', and getting this wrong hands a directory to
`make-process' as the program to run."
  (org-semantic-tests--with-vault dir
    (let* ((org-semantic-install-directory dir)
           (org-semantic-executable "org-semantic")
           (path (expand-file-name "org-semantic" dir)))
      ;; Nothing there yet, and nothing on exec-path under this name.
      (let ((exec-path nil))
        (should-error (org-semantic--binary) :type 'user-error))
      ;; A directory is not a binary.
      (make-directory path)
      (let ((exec-path nil))
        (should-error (org-semantic--binary) :type 'user-error))
      (delete-directory path)
      ;; A file that is there and executable is.
      (write-region "#!/bin/sh\n" nil path nil 'silent)
      (set-file-modes path #o755)
      (should (equal (org-semantic--binary) path)))))

(ert-deftest an-absolute-executable-outranks-the-install-directory ()
  "The one setting that says outright which binary to run wins.

The install directory is searched before variable `exec-path' so a
`cargo install' for shell use cannot quietly redirect Emacs; an
absolute `org-semantic-executable' is the way to override that on
purpose, so it has to come first."
  (org-semantic-tests--with-vault dir
    (let* ((installed (expand-file-name "org-semantic" dir))
           (chosen (expand-file-name "elsewhere" dir)))
      (dolist (path (list installed chosen))
        (write-region "#!/bin/sh\n" nil path nil 'silent)
        (set-file-modes path #o755))
      (let ((org-semantic-install-directory dir)
            (org-semantic-executable chosen))
        (should (equal (org-semantic--binary) chosen))))))


;;;; Which binary this package will work with

(ert-deftest a-binary-is-old-enough-or-it-is-not ()
  "A minimum, not an equality -- and the direction is the whole point.

The release version moves whenever anything ships, an elisp-only
change included, so comparing the binary against it warned that
one of them was stale every time nothing was.  What matters is
only whether the server predates something this file needs.

Inverting the comparison fails nothing and looks like nothing,
which is why it is pinned here rather than left to the one call
site."
  (let ((org-semantic-minimum-binary-version "0.2.0"))
    (should (org-semantic--too-old-p "0.1.9"))
    (should (org-semantic--too-old-p "0.1.99"))
    (should-not (org-semantic--too-old-p "0.2.0"))
    ;; Newer is silent, not suspicious: the protocol only gains things.
    (should-not (org-semantic--too-old-p "0.3.0"))
    (should-not (org-semantic--too-old-p "1.0.0"))
    ;; A binary that will not say what it is has not said it is too old.
    (should-not (org-semantic--too-old-p nil))
    (should-not (org-semantic--too-old-p "not a version"))))

(ert-deftest an-elisp-only-release-does-not-condemn-the-binary ()
  "The regression this split exists for, stated as the case that bit.

Ship 0.2.1 of the package against a binary still reporting 0.1.0,
with the minimum left where it was: nothing is wrong, so nothing
is said."
  (let ((org-semantic-version "0.2.1")
        (org-semantic-minimum-binary-version "0.1.0"))
    (should-not (org-semantic--too-old-p "0.1.0"))))


;;;; Where the server downloads its models

(ert-deftest a-cache-directory-is-expanded-before-it-is-handed-over ()
  "And nil sends nothing, which is not the same as sending nothing useful.

The server turns the variable into a path verbatim, so a literal
\"~/cache\" would make a directory *named* \"~\" wherever the
process happened to start and download gigabytes into it, without
failing."
  (let ((org-semantic-cache-home nil))
    (should-not (seq-find (lambda (e) (string-prefix-p "ORG_SEMANTIC_CACHE_HOME=" e))
                          (org-semantic--process-environment))))
  (let ((org-semantic-cache-home "~/nowhere/cache"))
    (should (member (concat "ORG_SEMANTIC_CACHE_HOME="
                            (expand-file-name "~/nowhere/cache"))
                    (org-semantic--process-environment)))
    (should-not (seq-find (lambda (e) (string-match-p "\\`ORG_SEMANTIC_CACHE_HOME=~" e))
                          (org-semantic--process-environment)))))

(ert-deftest the-cache-directory-reaches-the-process-we-start ()
  "Asked of the binary, since a list of strings proves only half of it.

`models' names the directory it would download into, so running
it under the environment we build is the whole path end to end:
the defcustom, the expansion, the child process, and the server's
own resolution.  It downloads nothing."
  (let ((binary (org-semantic-tests--binary)))
    (unless binary (ert-skip "no org-semantic binary built"))
    (org-semantic-tests--with-vault dir
      (let* ((org-semantic-cache-home dir)
             (process-environment (org-semantic--process-environment))
             (said (with-temp-buffer
                     (should (zerop (process-file binary nil t nil "models")))
                     (buffer-string))))
        (should (string-search (expand-file-name "fastembed" dir) said))
        (should (string-search (expand-file-name "org-semantic" dir) said))))))


;;;; Which vault a buffer belongs to

(ert-deftest a-vault-declares-itself-and-a-note-in-it-carries-that ()
  "The vault's own `.dir-locals.el' is what a note's buffer arrives holding.

Emacs applies them when the file is opened, so this costs nothing per
buffer and works before anything has been indexed -- which is where
every vault starts."
  (org-semantic-tests--with-vault dir
    (with-temp-file (expand-file-name ".dir-locals.el" dir)
      (insert "((nil . ((org-semantic-vault-root . t))))\n"))
    (should-not (file-exists-p (expand-file-name ".org-semantic" dir)))
    (let ((buffer (find-file-noselect (expand-file-name "pumps.org" dir))))
      (unwind-protect
          (with-current-buffer buffer
            (should (local-variable-p 'org-semantic-vault-root))
            (should (equal (org-semantic-vault) (org-semantic-canonical-vault dir))))
        (kill-buffer buffer)))))

(ert-deftest a-declaration-may-name-a-directory-under-the-project ()
  "A string names the root, for notes that sit below what declares them."
  (org-semantic-tests--with-vault dir
    (make-directory (expand-file-name "notes" dir))
    (with-temp-file (expand-file-name ".dir-locals.el" dir)
      (insert "((nil . ((org-semantic-vault-root . \"notes\"))))\n"))
    (let ((buffer (find-file-noselect (expand-file-name "notes/pumps.org" dir))))
      (unwind-protect
          (with-current-buffer buffer
            (should (equal (org-semantic-vault)
                           (org-semantic-canonical-vault
                            (expand-file-name "notes" dir)))))
        (kill-buffer buffer)))))

(ert-deftest one-vault-needs-no-declaration-and-answers-from-anywhere ()
  "The global setting is the one-vault setup, and the somewhere-else answer.

A buffer that is nowhere in particular -- *scratch*, an agenda -- has
had no directory-local variables applied to it and never will, so
without this a search from one has no vault at all."
  (org-semantic-tests--with-vault dir
    (let ((org-semantic-vault-root dir))
      (with-temp-buffer
        (setq default-directory temporary-file-directory)
        (should (equal (org-semantic-vault) (org-semantic-canonical-vault dir)))))))

(ert-deftest a-vault-that-has-to-be-worked-out-is-a-function ()
  "The global value answers for every buffer that declares nothing.

A fixed directory is the wrong answer when something else already
tracks which collection of notes is current -- vulpea switching
vaults, and anything like it.  Before this, such a package had to
advise `org-semantic-vault' to say so, which is why the function
value exists: the setting can now express the answer instead."
  (org-semantic-tests--with-vault dir
    (let ((org-semantic-vault-root (lambda () dir)))
      (should (equal (org-semantic-canonical-vault dir) (org-semantic-vault))))
    ;; `~/notes' is answerable, which is what a caller will write.  This
    ;; guards the outcome and not one call: `file-truename', inside
    ;; `org-semantic-canonical-vault', expands `~' as well -- so it fails only
    ;; if the answer is taken verbatim, which is the thing that would break.
    (let* ((home (file-name-directory (directory-file-name dir)))
           (leaf (file-name-nondirectory (directory-file-name dir)))
           (org-semantic-vault-root (lambda () (concat "~/" leaf)))
           (process-environment (cons (concat "HOME=" (directory-file-name home))
                                      process-environment)))
      (should (equal (org-semantic-canonical-vault dir) (org-semantic-vault))))
    ;; nil is a real answer -- nothing open -- and not a failure to fall back
    ;; from.  A client with no vault says so; it does not guess.
    (let ((org-semantic-vault-root #'ignore))
      (should-not (org-semantic-vault)))))

(ert-deftest a-declaration-may-not-be-a-function ()
  "A vault says which directory it is, never how to work one out.

`safe-local-variable' is the first refusal, and the important one: a
directory you merely visit could otherwise run whatever it liked.
This also covers the value being marked safe by hand, where the
declared branch would read a function as t -- since it is neither a
string nor nil -- and answer with the directory holding the
`.dir-locals.el', which is a vault nobody named."
  ;; The predicate admits what a vault may write, and nothing else.
  (let ((safe (get 'org-semantic-vault-root 'safe-local-variable)))
    (should (funcall safe t))
    (should (funcall safe "notes"))
    (should-not (funcall safe (lambda () "/tmp")))
    (should-not (funcall safe 'ignore)))
  ;; And a declaration that is a function is ignored rather than obeyed: the
  ;; global value answers instead, which here is another vault entirely.
  (org-semantic-tests--with-vault dir
    (let ((buffer (find-file-noselect (expand-file-name "pumps.org" dir)))
          (before (default-value 'org-semantic-vault-root)))
      (unwind-protect
          (with-current-buffer buffer
            (setq-local org-semantic-vault-root (lambda () "/never/asked"))
            ;; `setq-default', not `let': a `let' here would rebind the
            ;; buffer-local binding -- the declaration itself -- so the
            ;; function under test would never see one, and the test would
            ;; pass against code that obeys a declared function.
            (setq-default org-semantic-vault-root "/elsewhere/notes")
            (should (equal (org-semantic-canonical-vault "/elsewhere/notes")
                           (org-semantic-vault))))
        (setq-default org-semantic-vault-root before)
        (kill-buffer buffer)))))

(ert-deftest only-a-declaration-carries-a-declaration-s-meaning ()
  "A value of t means \"the directory that said so\", and globally nothing did.

That value is what the manual shows in a vault's `.dir-locals.el', so it is
what someone reaching for the global setting is most likely to write
by mistake.  Read with `buffer-local-value' alone -- which cannot
tell a local binding from the global value -- it would then take the
*declared* meaning and name whichever directory happens to hold a
`.dir-locals.el' above the buffer: somebody's project root becomes
their vault, and searching it fails as an unindexed vault rather
than as the setting it is.

The same reading is what makes the guard invisible for an ordinary
absolute directory, where both routes expand to the same answer.
This is the case that tells them apart."
  (org-semantic-tests--with-vault dir
    ;; A project that says something else entirely, as most do.
    (with-temp-file (expand-file-name ".dir-locals.el" dir)
      (insert "((nil . ((indent-tabs-mode . nil))))\n"))
    (let ((buffer (find-file-noselect (expand-file-name "pumps.org" dir)))
          (before (default-value 'org-semantic-vault-root)))
      (unwind-protect
          (progn
            ;; `setq-default' and not `let': a `let' binds whichever
            ;; binding is current, and in a buffer carrying a declaration
            ;; that is the declaration itself.
            (setq-default org-semantic-vault-root t)
            (with-current-buffer buffer
              (should-not (org-semantic-vault))))
        (setq-default org-semantic-vault-root before)
        (kill-buffer buffer)))))

(ert-deftest an-index-on-disk-is-not-what-makes-a-vault ()
  "`.org-semantic' is derived data, and its location is not promised.

It was the fallback once, which meant a vault was discoverable only
after it had been indexed -- and would stop being discoverable at all
once that directory is allowed to live somewhere else, silently
answering with the default vault instead of with none."
  (org-semantic-tests--with-vault dir
    (make-directory (expand-file-name ".org-semantic/semantic" dir) t)
    (let ((org-semantic-vault-root nil)
          (buffer (find-file-noselect (expand-file-name "pumps.org" dir))))
      (unwind-protect
          (with-current-buffer buffer
            (should-not (org-semantic-vault))
            (should-error (org-semantic-vault-or-error) :type 'user-error))
        (kill-buffer buffer)))))

(ert-deftest somewhere-that-is-not-a-vault-is-not-one ()
  "Nothing declared and no default means no vault, not a guess."
  (org-semantic-tests--with-vault dir
    (let ((org-semantic-vault-root nil))
      (with-temp-buffer
        (setq default-directory (file-name-as-directory dir))
        (should-not (org-semantic-vault))
        (should-error (org-semantic-vault-or-error) :type 'user-error)))))

(ert-deftest the-type-admits-every-value-a-vault-may-declare ()
  "Emacs checks a directory-local value against the customize type.

So a type describing only what makes sense *globally* -- a directory, or
nothing -- warns about the value t that the docstring tells a vault to
write, and it warns on merely visiting a note in it.  `safe-local-variable' passing
is not enough: that decides whether the value is applied, this decides
whether Emacs complains while applying it."
  (let ((type (widget-convert (custom-variable-type 'org-semantic-vault-root))))
    (dolist (value '(nil t "/abs/notes" "notes" "~/notes"))
      (should (widget-apply type :match value))))
  ;; And the predicate that makes it safe to set from a vault admits the same
  ;; two shapes, or the value would be refused before the type is consulted.
  (let ((safe (get 'org-semantic-vault-root 'safe-local-variable)))
    (should (functionp safe))
    (should (funcall safe t))
    (should (funcall safe "notes"))
    (should-not (funcall safe 42))))

(ert-deftest the-spelling-of-a-vault-is-a-public-question ()
  "An integration naming a vault of its own has to spell it as we do.

The server keys everything it holds by the string it was given, so a
`close' or a `status' for the same vault spelled another way finds
nothing and says so cheerfully -- zero entries dropped, no index here.
This was a private function, and the one integration that needed it --
vulpea, closing the vault being switched away from -- had to carry a copy
that would fall out of step the moment ours changed."
  (should (fboundp 'org-semantic-canonical-vault))
  (org-semantic-tests--with-vault dir
    (let ((canonical (org-semantic-canonical-vault dir)))
      ;; What `org-semantic-vault' answers is what this answers.
      (let ((org-semantic-vault-root dir))
        (with-temp-buffer
          (setq default-directory temporary-file-directory)
          (should (equal (org-semantic-vault) canonical))))
      ;; No trailing slash, and a truename: the two ways of reaching one
      ;; directory are one key.
      (should (equal canonical (org-semantic-canonical-vault
                                (file-name-as-directory dir))))
      (should-not (string-suffix-p "/" canonical)))))

(ert-deftest a-vault-is-spelled-one-way-however-it-was-reached ()
  "The server keys what it holds on this string, so `close' has to match."
  (org-semantic-tests--with-vault dir
    (let ((canonical (org-semantic-canonical-vault dir)))
      (should (equal canonical (org-semantic-canonical-vault
                                (file-name-as-directory dir))))
      (should (equal canonical (org-semantic-canonical-vault
                                (expand-file-name "sub/.." dir))))
      (should-not (string-suffix-p "/" canonical)))))


;;;; Reindexing when a note is saved

(defmacro org-semantic-tests--saving (&rest body)
  "Run BODY with the timers and the server replaced by records of them.

`org-semantic-tests--armed' collects what would have been armed and
`--armed-args' the (SECONDS . ARGS) each was armed with,
`--cancelled' what would have been cancelled, `--indexed' the
arguments of each `org-semantic-index', and `--said' every message.
Nothing waits and nothing is sent."
  (declare (indent 0))
  `(let ((org-semantic-tests--armed nil)
         (org-semantic-tests--armed-args nil)
         (org-semantic-tests--cancelled nil)
         (org-semantic-tests--indexed nil)
         (org-semantic-tests--said nil))
     (cl-letf (((symbol-function 'run-with-timer)
                (lambda (secs _repeat fn &rest args)
                  (let ((timer (list 'fake-timer fn args)))
                    (push (cons secs args) org-semantic-tests--armed-args)
                    (push timer org-semantic-tests--armed)
                    timer)))
               ((symbol-function 'cancel-timer)
                (lambda (timer) (push timer org-semantic-tests--cancelled)))
               ((symbol-function 'timerp)
                (lambda (thing) (eq (car-safe thing) 'fake-timer)))
               ((symbol-function 'org-semantic-index)
                (lambda (&rest args) (push args org-semantic-tests--indexed) "id"))
               ((symbol-function 'org-semantic-indexing-p) #'ignore)
               ((symbol-function 'message)
                (lambda (format &rest args)
                  (when format
                    (push (apply #'format format args)
                          org-semantic-tests--said)))))
       (clrhash org-semantic-auto-reindex--timers)
       (clrhash org-semantic-auto-reindex--said)
       ,@body)))

(ert-deftest saving-a-note-arms-one-reindex-however-many-saves ()
  "Each save restarts the wait, so a batch of them costs one run.

`save-some-buffers' over a vault is the case: fifty notes written in
a second, and fifty reindexes of the same vault would be refused one
after another by a server that runs one per vault."
  (org-semantic-tests--with-vault dir
    (let ((org-semantic-vault-root dir))
      (org-semantic-tests--saving
        (let ((buffer (find-file-noselect (expand-file-name "pumps.org" dir))))
          (unwind-protect
              (with-current-buffer buffer
                (dotimes (_ 3) (org-semantic-auto-reindex--on-save)))
            (kill-buffer buffer)))
        (should (= 3 (length org-semantic-tests--armed)))
        ;; Each armed for the configured wait, which is the whole of the
        ;; debounce: a number written here instead and the setting does nothing.
        (should (equal (list org-semantic-auto-reindex-delay)
                       (delete-dups (mapcar #'car org-semantic-tests--armed-args))))
        ;; Two of the three were cancelled by the save that followed, so one
        ;; run is pending -- and it is the last one armed.
        (should (= 2 (length org-semantic-tests--cancelled)))
        (should (= 1 (hash-table-count org-semantic-auto-reindex--timers)))
        (should (equal (gethash (org-semantic-canonical-vault dir)
                                org-semantic-auto-reindex--timers)
                       (car org-semantic-tests--armed)))))))

(ert-deftest a-vault-may-keep-its-notes-somewhere-else ()
  "The notes are the vault, unless its `vault.json' says otherwise.

A vault directory is where the *index* lives, so saving a note asks
whether the file is in the **notes** -- and reindexes the vault, which
is what the server is keyed by.  Comparing against the vault instead
would make the mode silently do nothing for exactly the vaults that
keep their notes elsewhere.

Read here rather than asked of the server: this runs on
`after-save-hook', where a round trip would start the process for any
org file saved anywhere."
  (org-semantic-tests--with-vault notes
    (let ((state (make-temp-file "org-semantic-state" t)))
      (unwind-protect
          (progn
            (make-directory (expand-file-name ".org-semantic" state))
            (with-temp-file (expand-file-name ".org-semantic/vault.json" state)
              (insert (json-serialize `(:version 1 :notes ,notes))))
            (should (equal (org-semantic-notes-root state)
                           (org-semantic-canonical-vault notes)))
            ;; Anything absent, unreadable or silent is the vault itself.
            (should (equal (org-semantic-notes-root notes)
                           notes))
            (with-temp-file (expand-file-name ".org-semantic/vault.json" state)
              (insert "{ this is not json"))
            (should (equal (org-semantic-notes-root state) state))
            (with-temp-file (expand-file-name ".org-semantic/vault.json" state)
              (insert "{\"version\": 1}"))
            (should (equal (org-semantic-notes-root state) state))

            ;; And a note in those notes arms a reindex of the vault holding
            ;; the index, which is the path the server knows it by.
            (with-temp-file (expand-file-name ".org-semantic/vault.json" state)
              (insert (json-serialize `(:notes ,notes))))
            (let ((org-semantic-vault-root state))
              (org-semantic-tests--saving
                (let ((buffer (find-file-noselect
                               (expand-file-name "pumps.org" notes))))
                  (unwind-protect
                      (with-current-buffer buffer
                        ;; The declaration is what the buffer must resolve to,
                        ;; as a vault of its own would declare itself.
                        (setq-local org-semantic-vault-root state)
                        (org-semantic-auto-reindex--on-save))
                    (kill-buffer buffer)))
                (should (= 1 (length org-semantic-tests--armed)))
                (should (gethash (org-semantic-canonical-vault state)
                                 org-semantic-auto-reindex--timers)))))
        (delete-directory state t)))))

(ert-deftest status-names-the-notes-only-when-they-are-elsewhere ()
  "The one place a user sees where a vault's notes are.

Both halves matter because it is a condition, and a condition inverts
without failing: silent when they differ leaves the split invisible
from inside Emacs -- the reply carries `notes' and nothing shows it --
and spoken when they do not adds a clause to every ordinary vault
saying only that the notes are where you asked for them."
  (let ((said nil))
    (cl-letf (((symbol-function 'message)
               (lambda (format &rest args)
                 (when format (push (apply #'format format args) said)))))
      ;; The ordinary vault: nothing to say.
      (cl-letf (((symbol-function 'org-semantic-status)
                 (lambda (&rest _) `(:notes "/vault" :semantic [] :lexical
                                     :json-false :loaded :json-false
                                     :indexing :json-false))))
        (org-semantic-show-status "/vault"))
      (should-not (string-match-p "notes in" (car said)))
      ;; And one that keeps its notes elsewhere: named, so `M-x
      ;; org-semantic-show-status' answers "which notes is this index of?".
      (cl-letf (((symbol-function 'org-semantic-status)
                 (lambda (&rest _) `(:notes "/elsewhere/org" :semantic []
                                     :lexical t :loaded :json-false
                                     :indexing :json-false))))
        (org-semantic-show-status "/state/notes"))
      (should (string-match-p "notes in /elsewhere/org" (car said)))
      (should (string-match-p "/state/notes" (car said))))))

(ert-deftest a-save-that-is-not-a-note-in-the-vault-arms-nothing ()
  "Three questions, cheapest first, and containment is the one that bites.

`after-save-hook' runs for every save in Emacs.  With a global
`org-semantic-vault-root' every buffer resolves to that vault -- which
is what makes a search from `*scratch*' work -- so a README.org in a
code repository would otherwise reindex the notes and report success."
  (org-semantic-tests--with-vault dir
    (let* ((elsewhere (make-temp-file "org-semantic-outside" t))
           (org-semantic-vault-root dir))
      (unwind-protect
          (org-semantic-tests--saving
            ;; A note, but not one of this vault's.
            (with-temp-file (expand-file-name "README.org" elsewhere) (insert "hi"))
            (let ((buffer (find-file-noselect
                           (expand-file-name "README.org" elsewhere))))
              (unwind-protect
                  (with-current-buffer buffer
                    (org-semantic-auto-reindex--on-save))
                (kill-buffer buffer)))
            ;; And a file in the vault that the indexer would not index.
            (with-temp-file (expand-file-name "notes.txt" dir) (insert "hi"))
            (let ((buffer (find-file-noselect (expand-file-name "notes.txt" dir))))
              (unwind-protect
                  (with-current-buffer buffer
                    (org-semantic-auto-reindex--on-save))
                (kill-buffer buffer)))
            ;; And a buffer visiting nothing at all.
            (with-temp-buffer
              (setq default-directory (file-name-as-directory dir))
              (org-semantic-auto-reindex--on-save))
            (should-not org-semantic-tests--armed))
        (delete-directory elsewhere t)))))

(ert-deftest an-automatic-run-refreshes-what-exists-and-builds-nothing ()
  "A save must not start minutes of embedding nobody asked for.

`org-semantic-index-mode' defaults to \"both\", so a vault with only
the word index would have every save kick off a first semantic build
-- with the echo area silenced, from a keystroke about saving a file.
So the run is narrowed to what the vault already has, and a vault
with nothing built is told about `org-semantic-reindex' instead."
  (org-semantic-tests--with-vault dir
    (let ((org-semantic-index-mode "both")
          (vault (org-semantic-canonical-vault dir)))
      ;; The word index alone: refresh that, and say nothing.
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-status)
                   (lambda (&rest _) '(:semantic [] :lexical t))))
          (org-semantic-auto-reindex--run vault))
        (should (= 1 (length org-semantic-tests--indexed)))
        (should (equal (plist-get (car org-semantic-tests--indexed) :mode) "lexical"))
        (should-not org-semantic-tests--said))
      ;; Both: both.
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-status)
                   (lambda (&rest _) '(:semantic [(:name "e5-small" :cached t)]
                                       :lexical t))))
          (org-semantic-auto-reindex--run vault))
        (should (equal (plist-get (car org-semantic-tests--indexed) :mode) "both")))
      ;; Nothing built: nothing started, and said once rather than per save.
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-status)
                   (lambda (&rest _) '(:semantic [] :lexical :json-false))))
          (org-semantic-auto-reindex--run vault)
          (org-semantic-auto-reindex--run vault))
        (should-not org-semantic-tests--indexed)
        (should (= 1 (length org-semantic-tests--said)))
        ;; Naming what the reader can press, not a key of some other buffer.
        (should (string-match-p "org-semantic-reindex" (car org-semantic-tests--said)))))))

(ert-deftest a-run-of-its-own-is-waited-for-rather-than-refused ()
  "The server runs one index per vault and refuses the second.

So a save landing during a run must not send one: it re-arms, which
also folds every save made while that run was going into the single
run that follows it."
  (org-semantic-tests--with-vault dir
    (let ((vault (org-semantic-canonical-vault dir)))
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-indexing-p) (lambda (&rest _) "id"))
                  ((symbol-function 'org-semantic-status)
                   (lambda (&rest _) (error "Nobody should have asked"))))
          (org-semantic-auto-reindex--run vault))
        (should-not org-semantic-tests--indexed)
        (should (= 1 (length org-semantic-tests--armed)))))))

(ert-deftest a-failure-speaks-once-and-a-success-only-if-asked ()
  "Quiet is about success, and a failure is not success.

An automatic feature that has stopped working looks exactly like one
that is working, and there is no keystroke that came back empty to
make anybody suspicious -- so a failure is said, and latched, since
the condition holds until somebody acts on it."
  (org-semantic-tests--with-vault dir
    (let* ((vault (org-semantic-canonical-vault dir))
           (fail (lambda (&rest args)
                   (funcall (plist-get args :failure)
                            '(:message "the policy has changed"))
                   "id"))
           (win (lambda (&rest args)
                  (funcall (plist-get args :success) '(:lexical (:files 1 :chunks 2 :secs 0.1)))
                  "id")))
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-index) fail))
          (org-semantic-auto-reindex--start vault "lexical")
          (org-semantic-auto-reindex--start vault "lexical"))
        (should (= 1 (length org-semantic-tests--said)))
        (should (string-match-p "the policy has changed" (car org-semantic-tests--said))))
      ;; Quiet by default, and a run that works clears what was said, so the
      ;; next failure is heard.
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-index) win))
          (let ((org-semantic-auto-reindex-quietly t))
            (org-semantic-auto-reindex--start vault "lexical"))
          (should-not org-semantic-tests--said)
          (let ((org-semantic-auto-reindex-quietly nil))
            (org-semantic-auto-reindex--start vault "lexical"))
          (should (= 1 (length org-semantic-tests--said)))))
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-index) fail))
          (org-semantic-auto-reindex--start vault "lexical"))
        (cl-letf (((symbol-function 'org-semantic-index) win))
          (org-semantic-auto-reindex--start vault "lexical"))
        (should-not (gethash vault org-semantic-auto-reindex--said))))))

(ert-deftest a-server-that-will-not-start-does-not-break-every-save ()
  "The run is on a timer, where an error is a backtrace nobody asked for.

A missing binary, a server that dies on startup: raised from a timer
that has nothing to do with the command the user just gave, and it
would also stop later saves from trying.  Said once per vault instead
-- never silently, since a mode that has quietly stopped keeping the
index up to date looks exactly like one that is working."
  (org-semantic-tests--with-vault dir
    (let ((vault (org-semantic-canonical-vault dir)))
      (org-semantic-tests--saving
        (cl-letf (((symbol-function 'org-semantic-status)
                   (lambda (&rest _) (error "No such file or directory: org-semantic"))))
          ;; Twice, because the latch is what keeps a broken setup from saying
          ;; it on every save.
          (org-semantic-auto-reindex--run vault)
          (org-semantic-auto-reindex--run vault))
        (should-not org-semantic-tests--indexed)
        (should (= 1 (length org-semantic-tests--said)))
        (should (string-match-p "No such file" (car org-semantic-tests--said)))
        ;; And nothing is left pending, so the next save arms afresh.
        (should (= 0 (hash-table-count org-semantic-auto-reindex--timers)))))))

(ert-deftest turning-the-mode-off-drops-what-was-pending ()
  "A wait that outlives the mode would index a vault nobody asked about."
  (org-semantic-tests--with-vault dir
    (let ((org-semantic-vault-root dir))
      (org-semantic-tests--saving
        (org-semantic-auto-reindex--arm (org-semantic-canonical-vault dir))
        (should (= 1 (hash-table-count org-semantic-auto-reindex--timers)))
        (org-semantic-auto-reindex-mode 1)
        (should (memq #'org-semantic-auto-reindex--on-save after-save-hook))
        (org-semantic-auto-reindex-mode -1)
        (should-not (memq #'org-semantic-auto-reindex--on-save after-save-hook))
        (should (= 1 (length org-semantic-tests--cancelled)))
        (should (= 0 (hash-table-count org-semantic-auto-reindex--timers)))))))

(ert-deftest something-else-watching-the-tree-can-arm-a-run ()
  "A save is not the only way a note changes, and `touch' is the other way in.

A rename in Dired, a `git pull', a folder arriving from a sync: no
`after-save-hook' runs for any of them, and nothing here watches the
filesystem.  What this must get right is that it takes a *vault* --
never a file -- since a run is a vault-wide incremental scan, and
that it goes through the same debounce a save does, so a watcher
firing per file costs one run."
  (org-semantic-tests--with-vault dir
    (let ((vault (org-semantic-canonical-vault dir)))
      (org-semantic-tests--saving
        (dotimes (_ 3) (org-semantic-auto-reindex-touch vault))
        (should (= 3 (length org-semantic-tests--armed)))
        ;; The debounce is the save's own, not a second one: two of the three
        ;; were cancelled by the touch that followed.
        (should (= 2 (length org-semantic-tests--cancelled)))
        (should (= 1 (hash-table-count org-semantic-auto-reindex--timers)))
        ;; Keyed on the vault it was handed, and the run will be given that
        ;; same string -- a file name here would reach the server as a vault.
        (should (timerp (gethash vault org-semantic-auto-reindex--timers)))
        (should (equal (list vault)
                       (nth 2 (gethash vault org-semantic-auto-reindex--timers))))
        ;; And what it arms is the mode's own run, which is where the manners
        ;; come from: the quiet, and the refusal to build a first index.  A
        ;; second path to the server would have to repeat both.
        (should (eq #'org-semantic-auto-reindex--run
                    (nth 1 (gethash vault org-semantic-auto-reindex--timers))))))))

(ert-deftest a-touch-does-not-need-the-mode ()
  "The mode is one trigger and not the policy.

A configuration whose file watcher already reports saves wants the
touch and does not want `after-save-hook' -- so gating the touch on
the mode would make it demand the very hook it makes redundant.  The
two share a debounce, so both together still cost one run."
  (org-semantic-tests--with-vault dir
    (let ((vault (org-semantic-canonical-vault dir)))
      (should-not org-semantic-auto-reindex-mode)
      (org-semantic-tests--saving
        (org-semantic-auto-reindex-touch vault)
        (should (= 1 (length org-semantic-tests--armed)))
        (should (= 1 (hash-table-count org-semantic-auto-reindex--timers)))
        ;; The save hook is not switched on behind anybody's back either.
        (should-not (memq #'org-semantic-auto-reindex--on-save after-save-hook))
        ;; A save arriving on top of it is the same pending run, not a second.
        (let ((buffer (find-file-noselect (expand-file-name "pumps.org" dir))))
          (unwind-protect
              (with-current-buffer buffer
                (let ((org-semantic-vault-root dir))
                  (org-semantic-auto-reindex--on-save)))
            (kill-buffer buffer)))
        (should (= 1 (hash-table-count org-semantic-auto-reindex--timers)))
        (should (= 1 (length org-semantic-tests--cancelled)))))))

(ert-deftest a-touch-that-names-no-vault-is-a-silent-no-op ()
  "It falls back to the current buffer, which is rarely a watcher's own.

A file notification callback runs in whatever buffer happened to be
current, so the fallback answers nil about as often as it answers
usefully.  Nil is nothing to reindex -- not an error to raise inside
somebody else's callback, and not a message to say about a vault
nobody named."
  (org-semantic-tests--saving
    (with-temp-buffer
      (let ((org-semantic-vault-root nil))
        (should-not (org-semantic-auto-reindex-touch))))
    (should-not org-semantic-tests--armed)
    (should-not org-semantic-tests--said)))

;;;; Errors carry a label, and the label is what to branch on

(ert-deftest a-failure-worth-acting-on-arrives-labelled ()
  "`kind' is the branch and `data' is what it promised to carry."
  (let ((err (should-error
              (org-semantic--fail
               '(:code -32000 :message "no semantic index"
                       :data (:kind "no-index" :target "semantic"
                                    :remedy "index")))
              :type 'org-semantic-error)))
    (should (equal (org-semantic-error-message err) "no semantic index"))
    (should (equal (org-semantic-error-kind err) "no-index"))
    (should (equal (plist-get (org-semantic-error-data err) :remedy) "index"))))

(ert-deftest an-error-with-nothing-to-decide-carries-no-label ()
  "Absence of `data' is itself the signal: show it, do not branch on it."
  (let ((err (should-error
              (org-semantic--fail '(:code -32000 :message "unknown method"))
              :type 'org-semantic-error)))
    (should-not (org-semantic-error-kind err))
    (should-not (org-semantic-error-data err))))

(ert-deftest a-label-survives-the-trip-through-jsonrpc ()
  "The `data' member arrives in an alist sitting behind a bare string.

Built with `list' rather than quoted, so that the byte compiler
does not read the fixture as a call to `jsonrpc-error'."
  (let ((err (should-error
              (org-semantic--rethrow
               (list 'jsonrpc-error "request id=3 failed:"
                     (cons 'jsonrpc-error-code -32000)
                     (cons 'jsonrpc-error-message "the policy changed")
                     (cons 'jsonrpc-error-data
                           '(:kind "config-drift"
                                   :changed ["todo_keywords"]))))
              :type 'org-semantic-error)))
    (should (equal (org-semantic-error-kind err) "config-drift"))
    (should (equal (org-semantic-error-message err) "the policy changed"))))


;;;; Reading a reply

(ert-deftest hits-are-a-list-whatever-json-made-of-them ()
  "JSON arrays arrive as vectors; a caller should not have to know."
  (should (equal (org-semantic-hits '(:hits [(:path "a.org") (:path "b.org")]))
                 '((:path "a.org") (:path "b.org"))))
  (should-not (org-semantic-hits '(:hits []))))

(ert-deftest a-hit-above-every-heading-is-still-addressable ()
  "Text before the first heading reports line 1, and must not report none."
  (should (= (org-semantic-hit-line '(:file "/v/a.org")) 1))
  (should (= (org-semantic-hit-line '(:file "/v/a.org" :headingLine 42)) 42)))


;;;; Against the real binary, over the lexical index

(defmacro org-semantic-tests--with-server (&rest body)
  "Run BODY against a server of its own, or skip if there is no binary."
  (declare (indent 0))
  `(let ((binary (org-semantic-tests--binary)))
     (unless binary (ert-skip "no org-semantic binary built"))
     (let ((org-semantic-executable binary)
           (org-semantic--connection nil)
           (org-semantic--server-version nil))
       (unwind-protect (progn ,@body)
         (when (org-semantic-running-p) (org-semantic-quit 'hard))))))

(defun org-semantic-tests--wait (seconds predicate)
  "Run the event loop for up to SECONDS, or until PREDICATE returns non-nil."
  (let ((deadline (+ (float-time) seconds)))
    (while (and (< (float-time) deadline) (not (funcall predicate)))
      (accept-process-output nil 0.05))
    (funcall predicate)))

(ert-deftest the-handshake-says-which-binary-answered ()
  "Asked at the one moment a client is certain to be listening."
  (org-semantic-tests--with-server
    (org-semantic-connection)
    (should (org-semantic-running-p))
    (should (equal org-semantic--server-version
                   (org-semantic-binary-version)))))

(ert-deftest a-vault-with-no-index-says-so-with-a-remedy ()
  "And the remedy is the machine form, so nothing parses prose to act."
  (org-semantic-tests--with-server
    (org-semantic-tests--with-vault dir
      (let ((status (org-semantic-status dir)))
        (should-not (org-semantic-true-p (plist-get status :lexical)))
        (should (equal (plist-get status :semantic) [])))
      (let ((err (should-error (org-semantic-search "pump" :vault dir
                                                    :mode "lexical")
                               :type 'org-semantic-error)))
        (should (equal (org-semantic-error-kind err) "no-index"))
        (should (equal (plist-get (org-semantic-error-data err) :remedy)
                       "index"))))))

(ert-deftest an-index-reports-itself-and-then-answers ()
  "The reply ends the run; the reports are what happened on the way."
  (org-semantic-tests--with-server
    (org-semantic-tests--with-vault dir
      (let ((outcome nil) (phases '()))
        (org-semantic-index
         :vault dir :mode "lexical"
         :progress (lambda (report)
                     (let ((phase (plist-get report :phase)))
                       (unless (member phase phases) (push phase phases))))
         :success (lambda (result) (setq outcome (list 'ok result)))
         :failure (lambda (error) (setq outcome (list 'failed error))))
        (should (org-semantic-indexing-p dir))
        (should (org-semantic-tests--wait 120 (lambda () outcome)))
        (should (eq (car outcome) 'ok))
        ;; Every phase reports its own unit, and a scan comes before a
        ;; chunking pass -- so `push' leaves them in this order.
        (should (equal phases '("chunk" "scan")))
        (let ((report (plist-get (cadr outcome) :lexical)))
          (should (= (plist-get report :files) 3))
          (should (> (plist-get report :chunks) 0)))
        ;; And the run is no longer ours to cancel.
        (should-not (org-semantic-indexing-p dir))))))

(ert-deftest a-word-index-finds-notes-by-word ()
  "The end of it: index, search, and read the address of a hit."
  (org-semantic-tests--with-server
    (org-semantic-tests--with-vault dir
      (let ((done nil))
        (org-semantic-index :vault dir :mode "lexical"
                            :success (lambda (_) (setq done t))
                            :failure (lambda (e) (setq done (list 'failed e))))
        (should (eq (org-semantic-tests--wait 120 (lambda () done)) t)))
      (let* ((reply (org-semantic-search "turbo" :vault dir :mode "lexical"))
             (hits (org-semantic-hits reply)))
        (should (= (length hits) 1))
        (should (equal (file-name-nondirectory
                        (org-semantic-hit-file (car hits)))
                       "pumps.org"))
        ;; A note whose title is its filename headlines with it, and the
        ;; heading owning the passage is what a client jumps to.
        (should (= (org-semantic-hit-line (car hits)) 3))
        (should-not (org-semantic-true-p (plist-get reply :indexing)))
        ;; Nothing normalises a BM25 score, so there is no sigma for one.
        (should-not (plist-get (car hits) :z)))
      ;; An empty query is not an error: an editor may send one per keystroke.
      (should-not (org-semantic-hits
                   (org-semantic-search "" :vault dir :mode "lexical")))
      (should-not (org-semantic-hits
                   (org-semantic-search "helium" :vault dir :mode "lexical")))
      ;; And a vault can be handed back.
      (org-semantic-close dir))))

(ert-deftest closing-a-vault-says-nothing-unless-it-was-asked-for ()
  "A command reports; a function returns.

The caller with a reason to send `close' is one that knows a vault has
been left -- a vault switch, the last buffer of one being killed -- and
neither is an occasion for a line in the echo area.  Announcing it
anyway put \"closed ~/org/Private (0 entry/entries dropped)\" in front
of somebody on every switch, for a vault the server had never held."
  (let ((said nil))
    (cl-letf (((symbol-function 'org-semantic-running-p) (lambda () t))
              ((symbol-function 'org-semantic--call)
               (lambda (&rest _) '(:dropped 2)))
              ((symbol-function 'message)
               (lambda (format &rest args)
                 (when format (push (apply #'format format args) said)))))
      ;; From Lisp: the number, and silence.
      (should (equal 2 (org-semantic-close "/vault")))
      (should-not said)
      ;; As a command: the same number, and it says so.
      (should (equal 2 (funcall-interactively #'org-semantic-close "/vault")))
      (should (= 1 (length said)))
      (should (string-match-p "closed" (car said))))))

(provide 'org-semantic-tests)
;;; org-semantic-tests.el ends here
