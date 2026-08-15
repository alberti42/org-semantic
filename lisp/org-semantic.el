;;; org-semantic.el --- Search org notes by meaning, or by word -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.4.1
;; Package-Requires: ((emacs "29.1"))
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; The Emacs side of org-semantic: it starts `org-semantic serve', keeps
;; one such process for every vault, and turns its JSON-RPC methods into
;; Lisp functions.  Nothing here draws a user interface -- no results
;; buffer, no as-you-type search.  That belongs in a layer above this
;; one, and this file is what it will be built on.
;;
;; Four rules the server sets, which everything here follows.
;;
;; One process serves every vault.  The model is loaded once and shared,
;; so a second vault costs a few megabytes.  `org-semantic--connection'
;; is therefore a single global, and every request names its vault.
;;
;; An index takes minutes and `jsonrpc-default-request-timeout' is ten
;; seconds.  Progress reports do not reset it.  Every `index' here is
;; asynchronous and carries `org-semantic-index-timeout'.
;;
;; A search sent during an index is answered from the version committed
;; before it, with `indexing' true in the result.  It is also slower,
;; because the query waits for the embedding batch in flight.
;;
;; A failure a client must act on carries `kind' in the JSON-RPC `data'
;; member.  Branch on `org-semantic-error-kind', never on the message.
;; No label means there is nothing to decide: show the message.

;;; Code:

(require 'cl-lib)
(require 'jsonrpc)
(require 'url-handlers)

(defconst org-semantic-version "0.4.1"
  "The release this package is from.

It moves whenever anything here ships, including a change to one
file.  It is not what the binary reports; the two are compared
through `org-semantic-minimum-binary-version'.")

(defconst org-semantic-minimum-binary-version "0.3.0"
  "The oldest binary this package knows how to talk to.

Raise it when the elisp needs something the server did not have: a
new method, a new field, or a changed reply shape.  Raise it also
when a release documents behaviour that only the newer binary
provides, and that the older one gets wrong without saying so.

A minimum, not an equality.  The two versions ship together and do
not change together, so an elisp-only release must not condemn a
binary that is still correct.  A newer binary is not reported: the
protocol only gains things.")


;;;; Settings

(defgroup org-semantic nil
  "Search a tree of org notes by meaning, or by word."
  :group 'outlines
  :prefix "org-semantic-")

(defcustom org-semantic-executable "org-semantic"
  "The org-semantic binary, by name on variable `exec-path' or as a path.

An absolute path here wins over everything: it is the one setting
that says outright which binary to run.  A bare name is looked for
in `org-semantic-install-directory' first and on variable
`exec-path' after."
  :type '(choice (string :tag "Name or path")
                 (file :tag "File" :must-match t)))

(defcustom org-semantic-install-directory
  (expand-file-name "org-semantic/" user-emacs-directory)
  "Where org-semantic keeps a binary of its own, and looks for one first.

`org-semantic-binary-install' puts one here.  Unpacking a release
here has the same effect, and needs no other setting.

Keep it outside the package manager's tree.  A straight or elpaca
rebuild repopulates the package directory, and would delete the
binary while a server runs from it.

It is searched before variable `exec-path', so a copy installed for
shell use cannot move Emacs onto a different build.  To run the one
on PATH, leave this directory empty or set
`org-semantic-executable' to an absolute path."
  :type 'directory)

(defcustom org-semantic-cache-home nil
  "Where the server downloads its models, or nil to inherit the environment.

The embedding model and the language classifier are the only files
written outside a vault.  A model is 128 MB for the small English
one, and up to 2.24 GB for the large multilingual ones.  They go
under `$XDG_CACHE_HOME', which is ~/.cache/fastembed and
~/.cache/org-semantic.  Set this to keep them elsewhere, on an
external disk or in a shared directory.

Nil, the default, sends nothing, and the server resolves the path
from its own environment.

The value is sent as ORG_SEMANTIC_CACHE_HOME, which replaces
`$XDG_CACHE_HOME' for org-semantic alone.  The layout below it does
not change, so moving a cache is a `mv' of those two directories.

It applies only to servers this Emacs starts.  A shell
`org-semantic index' reads its own environment.  If you use the
binary from a terminal, set the variable there too, or the model is
downloaded a second time into the default location."
  :type '(choice (const :tag "Inherit from the environment" nil)
                 (directory :tag "Directory")))

(defcustom org-semantic-model nil
  "Which embedding model to use, or nil to let the server choose.

Each model keeps its own semantic index, so a vault can have
several built side by side; nil means the one that is built, and
fails as an ambiguous choice if more than one is.  Names come from
`org-semantic status' -- `bge-small-en', `e5-small' and the larger
multilingual variants."
  :type '(choice (const :tag "Whichever is built" nil)
                 (string :tag "Model name")))

(defcustom org-semantic-index-mode "both"
  "Which indexes `org-semantic-reindex' builds.

The two are independent: `semantic' finds notes by meaning and
takes minutes to build, `lexical' finds them by word and takes
seconds."
  :type '(choice (const :tag "Both" "both")
                 (const :tag "Semantic only" "semantic")
                 (const :tag "Lexical only" "lexical")))

(defcustom org-semantic-conserve-memory nil
  "Whether an index may load a second copy of the model.

Off, the default, lets a long rebuild load its own weights, so that
searching and indexing run side by side.  This costs about 229 MB
on the small English model, and some gigabytes on the large
multilingual ones.  The process keeps that memory until it exits.

On, the two share one model and take turns.  A query that arrives
during an embedding batch then waits for it.

It is concurrency against memory, not speed against memory.  It
changes nothing while no index runs, and nothing at all for lexical
search, which uses no model."
  :type 'boolean)

(defcustom org-semantic-config nil
  "The indexing policy to send with every request, or nil to send none.

What a vault is indexed *as* -- its languages, its TODO keywords,
how large a passage may get -- is policy, and the server checks
the policy a client holds against the one an index was built
under.  Set this and a drifted setting fails a search with
`config-drift' instead of answering from passages split by rules
you no longer hold.  Leave it nil and the index is searched as it
stands, which is what the command line does.

The value is a plist serialised to JSON, so arrays must be
vectors:

  (:languages [\"en-US\" \"de-DE\"] :fold_diacritics :json-false)

Sending a policy is all-or-nothing: it is compared whole, so a
partial one reads as a change to everything it leaves out.  Copy
config.example.json and translate it, or leave this nil until
there is something to say."
  :type '(choice (const :tag "Send none" nil) (plist)))

(defcustom org-semantic-timeout 30
  "Seconds to wait for anything but an index.

Generous, not tight.  A warm search takes under ten milliseconds,
but the first search against a vault loads the model, which takes
0.12 s for the small English model and 1.6 s for the multilingual
ones."
  :type 'natnum)

(defcustom org-semantic-index-timeout 7200
  "Seconds to wait for an index.

A full semantic index of a thousand notes takes minutes, and a
large vault on a large model takes longer.  Only the first run
does: a later run touches the notes that changed.

The number is large because jsonrpc.el needs one.  Progress
reports say what is happening, and `org-semantic-cancel' stops a
run."
  :type 'natnum)


;;;; Which vault a buffer belongs to

;;;###autoload
(put 'org-semantic-vault-root 'safe-local-variable
     (lambda (v) (or (eq v t) (stringp v))))

(defcustom org-semantic-vault-root nil
  "Which vault the notes you search belong to.

Set here, globally, this is the whole of the setup for one vault --
a directory, and every buffer that says nothing else belongs to it,
including the ones that are nowhere in particular: `*scratch*', an
agenda.  Someone with several vaults leaves it nil, or names the one
to fall back on.

A vault says which it is in its own `.dir-locals.el', which is what
overrides this for the notes inside it:

  ((nil . ((org-semantic-vault-root . t))))

Value t means the directory holding that `.dir-locals.el' -- so it
is only meaningful there, having nothing to refer to when set
globally.  A string names the root instead: absolute, or relative
to the declaration, for a vault whose notes sit under a
subdirectory of the project that declares them.

Set globally, it may instead be a *function* of no arguments,
returning a directory or nil.  That is for the case where the answer
has to be worked out rather than written down -- notably a package
that already tracks which collection of notes is current:

  (setq org-semantic-vault-root
        (lambda () (and vulpea-vault-directory
                        (expand-file-name vulpea-vault-directory))))

It is called for every question about a vault, so keep it to a
variable lookup.  It is your code, so an error in it signals.
Return nil to say that no vault is open.

A function is legal only here, never in a `.dir-locals.el'.
`safe-local-variable' refuses one, because a directory you visit
must not run code.  A declaration says which directory, never how
to work it out.

The `.org-semantic' directory is not consulted.  It holds derived
data and may sit anywhere, so the same notes would be a vault on
one machine and not on another."
  ;; `t' and a relative directory are in the type because Emacs checks a
  ;; directory-local value against it and warns on a value it does not
  ;; admit.  Which values are legal where is the docstring's business.
  :type '(choice (const :tag "No vault unless one declares itself" nil)
                 (directory :tag "This vault, unless one declares itself")
                 (function :tag "Worked out by this function (globally only)")
                 (const :tag "The directory declaring it (.dir-locals.el only)" t)
                 (string :tag "Relative to the declaration (.dir-locals.el only)")))

(defun org-semantic-vault (&optional buffer)
  "Return the vault root BUFFER belongs to, or nil.

BUFFER defaults to the current one.  The answer is absolute and has
no trailing slash, which is how it is spelled on the wire.  The
server keys what it holds by that string.

Two ways to be a vault, in this order.  The buffer carries
`org-semantic-vault-root', which Emacs applied from the vault's
`.dir-locals.el'.  Failing that, the global value of the same
setting, which may be a function.

A function that signals is left to signal.  Nothing here falls back
to another answer.

Nothing here searches the filesystem for a vault."
  (let* ((buffer (or buffer (current-buffer)))
         ;; `local-variable-p' first: `buffer-local-value' cannot tell a
         ;; declaration from the global value, and would give the default
         ;; vault to every note in an undeclared one.
         (declared (and (local-variable-p 'org-semantic-vault-root buffer)
                        (buffer-local-value 'org-semantic-vault-root buffer)))
         ;; A function is not a declaration.  `safe-local-variable' refuses
         ;; one from a file; this covers a value marked safe by hand, which
         ;; the branch below would read as t.
         (declared (unless (functionp declared) declared))
         ;; `default-value', not the variable: reading it plainly answers
         ;; with the buffer-local binding, which is the value just refused.
         (global (default-value 'org-semantic-vault-root)))
    (cond
     ;; Both declared forms are relative to the nearest `.dir-locals.el',
     ;; which is the only one Emacs reads.  Resolving against the buffer's
     ;; own directory would answer notes/notes for a note in notes/.
     (declared
      (let* ((dir (with-current-buffer buffer default-directory))
             (home (locate-dominating-file dir ".dir-locals.el")))
        (cond
         ((stringp declared)
          (org-semantic-canonical-vault (expand-file-name declared (or home dir))))
         ;; With no file to have said t, there is no vault to name.
         (home (org-semantic-canonical-vault home)))))
     ;; Before the string case, because a function is not a string.  What
     ;; it returns is expanded, so it may answer `~/notes', and nil means
     ;; that no vault is open.
     ((functionp global)
      (let ((answer (funcall global)))
        (when (stringp answer)
          (org-semantic-canonical-vault (expand-file-name answer)))))
     ((stringp global)
      (org-semantic-canonical-vault (expand-file-name global))))))

(defun org-semantic-notes-root (vault)
  "Where VAULT's notes are: VAULT itself, or what its `vault.json' says.

A vault directory holds the index.  The notes are inside it unless
`.org-semantic/vault.json' names another directory, which is for
notes in a synced folder, or for several vaults that keep their
indexes together.

The server is the authority and answers this in `status'.  This
reads the one key instead, because the caller is `after-save-hook',
where a round trip would start the server for any org file saved
anywhere.  A file that is absent, unreadable or silent answers
VAULT, which is also the server's default."
  (let ((said (expand-file-name ".org-semantic/vault.json" vault)))
    (or (and (file-readable-p said)
             (ignore-errors
               (let ((notes (plist-get (with-temp-buffer
                                         (insert-file-contents said)
                                         (json-parse-buffer :object-type 'plist))
                                       :notes)))
                 (and (stringp notes)
                      ;; `expand-file-name' against the vault covers all three
                      ;; forms the server accepts: absolute, `~/...', and
                      ;; relative to the vault.
                      (org-semantic-canonical-vault (expand-file-name notes vault))))))
        vault)))

(defun org-semantic-vault-or-error (&optional buffer)
  "Return the vault BUFFER belongs to, or signal an error saying it has none.
BUFFER is as in `org-semantic-vault'."
  (or (org-semantic-vault buffer)
      ;; The message names the setting, because nothing was set: there
      ;; is no file to go and look at.
      (user-error
       (concat "No org-semantic vault for %s: set org-semantic-vault-root, "
               "or declare it in the vault's .dir-locals.el")
       (buffer-name (or buffer (current-buffer))))))

(defun org-semantic-canonical-vault (dir)
  "Return DIR as the server will be asked about it.

Resolved through `file-truename' and left without a trailing
slash, so that one vault reached two ways is one key.  A symlink,
or /tmp against /private/tmp, is the usual case.

Public because an integration needs it.  The server keys what it
holds by the string it was given, so a caller that names a vault of
its own must spell it the same way."
  (directory-file-name (file-truename (expand-file-name dir))))

(define-obsolete-function-alias 'org-semantic--canonical
  'org-semantic-canonical-vault "0.3.0")


;;;; Errors

(define-error 'org-semantic-error "org-semantic")

(defun org-semantic-error-message (err)
  "The sentence ERR carries, which is the one to show a user."
  (nth 1 err))

(defun org-semantic-error-kind (err)
  "What kind of failure ERR is, as a string, or nil if it is unlabelled.

Branch on this and never on the message.  The kinds, and what
each promises to carry in `org-semantic-error-data':

  no-index      target, remedy, and built/model where known
  model-missing target, model, remedy.  The index is here and its
                model is not downloaded, so the search refused
                instead of fetching it
  index-layout  target, found, expected, remedy
  index-corrupt target, chunks, vectors, remedy
  config-drift  target, changed (setting names), remedy
  unknown-model known
  ambiguous-model  built
  indexing      remedy (\"wait\")

`remedy' is the machine form, \"index\", \"reindex-full\" or
\"wait\", so a client never parses prose to find which call to
offer.  Nil means there is nothing to decide: show the message."
  (nth 2 err))

(defun org-semantic-error-data (err)
  "Everything ERR was labelled with, as a plist, or nil."
  (nth 3 err))

(defun org-semantic--fail (error-object)
  "Signal `org-semantic-error' for the JSON-RPC ERROR-OBJECT.
ERROR-OBJECT is a plist with `:code', `:message' and `:data'."
  (let ((data (plist-get error-object :data)))
    (signal 'org-semantic-error
            (list (or (plist-get error-object :message) "request failed")
                  (plist-get data :kind)
                  data
                  (plist-get error-object :code)))))

(defun org-semantic--rethrow (err)
  "Re-signal the `jsonrpc-error' ERR as an `org-semantic-error'."
  (let ((alist (cdr err)))
    (org-semantic--fail
     (list :code (alist-get 'jsonrpc-error-code alist)
           :message (alist-get 'jsonrpc-error-message alist)
           :data (alist-get 'jsonrpc-error-data alist)))))


;;;; The connection

(defvar org-semantic--connection nil
  "The one server, shared by every vault, or nil if none is running.")

(defvar org-semantic--server-version nil
  "The release the running process reports, from the handshake.

A different answer from the binary's own `--version' the moment a
new binary has been installed underneath a server that is still
running: the file on disk then no longer says what this process
is.")

(defvar org-semantic--starting nil
  "Non-nil while the handshake is in flight.

The server accepts nothing between the `initialize' request and
the `initialized' notification: anything else is a protocol error
and it exits.  The handshake is therefore synchronous, and this
stops a timer that fires inside it from starting a second server
or sending a request first.")

(defvar org-semantic--watchers (make-hash-table :test 'eql)
  "Progress callbacks, keyed by the request id they report under.")

(defvar org-semantic--runs (make-hash-table :test 'equal)
  "The id of the index in flight for each vault, so it can be cancelled.")

(defun org-semantic-running-p ()
  "Whether a server is running."
  (and org-semantic--connection
       (jsonrpc-running-p org-semantic--connection)
       t))

(defconst org-semantic--build-url
  "https://alberti42.github.io/org-semantic/#build-from-source"
  "The manual on building the binary yourself.

The answer for a platform with no published build, and for anyone
who prefers to compile what they run.")

(defun org-semantic--installed-binary ()
  "Return the binary under `org-semantic-install-directory', or nil.

`file-regular-p' as well as `file-executable-p': the second answers
t for a directory, which `make-process' would then try to run.

Symlinks are followed.  Link a development build in here to test
the installed path without installing anything."
  (let ((path (expand-file-name (if (eq system-type 'windows-nt)
                                    "org-semantic.exe"
                                  "org-semantic")
                                org-semantic-install-directory)))
    (and (file-regular-p path) (file-executable-p path) path)))

(defvar org-semantic--may-ask t
  "Whether a missing binary may be asked about rather than only reported.

Bound to nil where nobody is waiting on the answer, which is the
save hook's timer: a question raised from there would interrupt
whatever is being typed, and return on the next save.")

(defun org-semantic--binary ()
  "Return the org-semantic binary, or offer to get one and signal if not.

Three places, in order: an absolute `org-semantic-executable', the
install directory, then variable `exec-path'.  Finding none of them
raises a question; see `org-semantic--offer-binary'."
  (or (and (file-name-absolute-p org-semantic-executable)
           (file-executable-p org-semantic-executable)
           org-semantic-executable)
      (org-semantic--installed-binary)
      (executable-find org-semantic-executable)
      (org-semantic--offer-binary)))

(defun org-semantic--binary-prompt (asset)
  "What to ask when there is no binary.  ASSET is the download, or nil.

Nil offers the build alone: there is no asset for this platform."
  (concat
   "org-semantic needs its binary, and there is none installed.\n\n"
   (when asset
     (format "  [d] Download it — %s, checked against the release's own SHA256SUMS\n"
             asset))
   "  [b] Build it yourself — opens the manual; needs a Rust toolchain\n"
   "  [q] leave it\n\nChoice: "))

(defun org-semantic--offer-binary ()
  "Ask what to do about there being no binary, and do it.

Downloading returns the binary, so the call that provoked the
question carries on.  Anything else signals, since the caller
needs one.

The question is skipped in batch, over an active minibuffer, and
wherever `org-semantic--may-ask' is nil; the error is then the
whole answer, so it names the command instead.

`quit' is caught because this can run from a timer, where an
escaping \\`C-g' is \"Error running timer\"."
  (let* ((asset (org-semantic--release-asset))
         (choice
          (and org-semantic--may-ask
               (not noninteractive)
               (not (active-minibuffer-window))
               (let ((message-log-max nil))
                 (prog1 (condition-case nil
                            (read-char-choice
                             (org-semantic--binary-prompt asset)
                             (if asset '(?d ?b ?q) '(?b ?q)))
                          (quit ?q))
                   ;; The minibuffer exits on the answer but its last line
                   ;; stays in the echo area, where "Choice: d" reads as a
                   ;; question still waiting.
                   (message nil))))))
    (pcase choice
      (?d (org-semantic-binary-install)
          (org-semantic--installed-binary))
      (?b (browse-url org-semantic--build-url)
          (user-error "No org-semantic binary yet: the manual on building one is open in your browser"))
      (_ (user-error "No org-semantic binary: %s is neither in %s nor on exec-path.  %s"
                     org-semantic-executable org-semantic-install-directory
                     (if asset
                         "M-x org-semantic-binary-install downloads one"
                       (format "None is published for %s; see %s"
                               system-configuration org-semantic--build-url)))))))

(defconst org-semantic--release-url
  "https://github.com/alberti42/org-semantic/releases/download/v%s/%s"
  "Where a release asset is, given a version and an asset name.

The tag is `v' and the release version, which is this package's
own.  The release workflow refuses a tag that does not equal
`org-semantic-version', so the matching release always exists.")

(defun org-semantic--release-platform (&optional configuration)
  "The platform token this release names its binary for, or nil.

CONFIGURATION defaults to `system-configuration', which names the
platform Emacs itself was built for.  The tokens are the ones in the
release workflow's build matrix.

Nil is an answer, not a failure.  There is no Intel macOS build,
because ONNX Runtime publishes no `x86_64-apple-darwin'.  Nil is
also the answer for an Emacs built for x86_64 and running under
Rosetta, which reports itself as Intel."
  (let* ((c (or configuration system-configuration))
         (arm (string-match-p "\\`\\(aarch64\\|arm64\\)" c))
         (intel (string-match-p "\\`x86_64" c)))
    (cond ((string-match-p "darwin" c) (and arm "aarch64-macos"))
          ((string-match-p "linux" c)
           (cond (arm "aarch64-linux") (intel "x86_64-linux")))
          ((string-match-p "mingw\\|windows\\|msvc\\|cygwin" c)
           (and intel "x86_64-windows")))))

(defun org-semantic--release-asset (&optional configuration version)
  "The release archive built for CONFIGURATION, or nil if there is none.

VERSION defaults to `org-semantic-version'.  Every asset carries
it, so a file says which release it came from and a `SHA256SUMS'
line is unambiguous between releases.

The name says `bin' against `src', and not `cli': the same binary
is the server this package drives.

Each archive holds one binary at its top level, called
`org-semantic' or `org-semantic.exe'.

Nil when this platform has no build.  See
`org-semantic--release-platform'."
  (let ((platform (org-semantic--release-platform configuration)))
    (when platform
      (format "org-semantic-%s-bin-%s.%s"
              (or version org-semantic-version)
              platform
              (if (string-suffix-p "windows" platform) "zip" "tar.gz")))))

(defun org-semantic--verify-checksum (file sums asset)
  "Signal unless FILE hashes to what SUMS records for ASSET.

SUMS is the release's own `SHA256SUMS', published beside the
archives.  A mismatch is more often a truncated download than an
attack, and both get the same refusal."
  (let ((want (with-temp-buffer
                (insert-file-contents sums)
                (goto-char (point-min))
                (when (re-search-forward
                       (concat "^\\([0-9a-f]\\{64\\}\\) +\\*?"
                               (regexp-quote asset) "$")
                       nil t)
                  (match-string 1))))
        (got (with-temp-buffer
               (set-buffer-multibyte nil)
               (insert-file-contents-literally file)
               (secure-hash 'sha256 (current-buffer)))))
    (unless want
      (error "The release lists no checksum for %s" asset))
    (unless (equal want got)
      (error "Checksum mismatch for %s: expected %s, got %s" asset want got))))

;;;###autoload
(defun org-semantic-binary-install (&optional version)
  "Download the org-semantic binary for this platform and install it.

It goes in `org-semantic-install-directory', which
`org-semantic--binary' searches before variable `exec-path'.
Nothing needs configuring afterwards, and a binary installed for
shell use is not touched.

VERSION is the release to take, and defaults to
`org-semantic-version'.  The matching release is used, not the
newest one, so the binary that arrives is the one this elisp was
written against.

The archive is checked against the release's own `SHA256SUMS'
before it is unpacked, and the result is asked for its version
before this returns."
  (interactive)
  (let* ((version (or version org-semantic-version))
         (asset (or (org-semantic--release-asset nil version)
                    (user-error
                     "No org-semantic binary is published for %s%s"
                     system-configuration
                     (if (string-match-p "darwin" system-configuration)
                         " (there is no Intel macOS build; \
build it with `cargo install --git \
https://github.com/alberti42/org-semantic')"
                       ""))))
         (destination (expand-file-name
                       (if (eq system-type 'windows-nt)
                           "org-semantic.exe" "org-semantic")
                       org-semantic-install-directory))
         (staging (make-temp-file "org-semantic-install" t)))
    (unwind-protect
        (let ((archive (expand-file-name asset staging))
              (sums (expand-file-name "SHA256SUMS" staging))
              ;; The asset is named `.tar.gz', which is enough to corrupt
              ;; it: `url-copy-file' writes through `write-region', and
              ;; jka-compr claims the file by name and gzips the bytes a
              ;; second time.  The checksum then fails.
              ;;
              ;; No `require': the variable is preloaded, and requiring
              ;; `jka-compr' would load a file Emacs leaves until something
              ;; opens a compressed file.
              (jka-compr-inhibit t))
          (message "org-semantic: downloading %s %s..." asset version)
          (url-copy-file (format org-semantic--release-url version asset) archive t)
          (url-copy-file (format org-semantic--release-url version "SHA256SUMS") sums t)
          (org-semantic--verify-checksum archive sums asset)
          ;; `tar -xf' reads both formats: it detects the compression, and
          ;; the bsdtar on macOS and Windows also reads zip.  Only Windows
          ;; is published as a zip, so one command covers every asset.
          (unless (zerop (let ((default-directory (file-name-as-directory staging)))
                           (process-file "tar" nil nil nil "-xf" archive)))
            (error "Could not unpack %s" asset))
          (let ((unpacked (expand-file-name (file-name-nondirectory destination) staging)))
            (unless (file-regular-p unpacked)
              (error "%s did not contain %s" asset (file-name-nondirectory destination)))
            (make-directory org-semantic-install-directory t)
            (copy-file unpacked destination t)
            ;; The zip carries no mode bits, so the binary arrives without
            ;; the execute bit.  Set the modes whatever the archive said.
            (set-file-modes destination #o755)))
      (delete-directory staging t))
    (let ((org-semantic-executable destination))
      (let ((reported (org-semantic-binary-version)))
        (unless reported
          (error "Installed %s, but it will not report a version" destination))
        (org-semantic--check-version reported "the binary just installed")
        (message "org-semantic: installed %s in %s"
                 reported org-semantic-install-directory)
        reported))))

(defun org-semantic-binary-version ()
  "Return the version of the binary on disk, or nil if it will not say.

Ask this before starting a server.  `org-semantic--server-version'
answers for a process that is already running, which can be a
different release."
  (with-temp-buffer
    (when (zerop (process-file (org-semantic--binary) nil t nil "--version"))
      (string-trim (buffer-string)))))

(defun org-semantic--too-old-p (found)
  "Whether FOUND is a binary version older than this package can use.

Nil for a version at or above the minimum, and nil for nil.  A
binary that will not say what it is has not said it is too old."
  (and found (ignore-errors (version< found org-semantic-minimum-binary-version))))

(defun org-semantic--check-version (found where)
  "Warn when FOUND is too old for this package.  WHERE says what was asked.

Only the lower bound is checked.  A newer binary is not reported;
see `org-semantic-minimum-binary-version'."
  (when (org-semantic--too-old-p found)
    (display-warning
     'org-semantic
     (format "%s is org-semantic %s, but this package needs %s or newer; \
update the binary"
             where found org-semantic-minimum-binary-version))))

(defun org-semantic--handle-notification (_conn method params)
  "Handle METHOD with PARAMS, a notification from the server.
The only one it sends is `$/progress'."
  (when (eq method '$/progress)
    (let ((watcher (gethash (plist-get params :token) org-semantic--watchers)))
      (when watcher
        (funcall watcher (plist-get params :value))))))

(defun org-semantic--handle-request (_conn method _params)
  "Refuse METHOD: the server asks us nothing, so anything it asks is a mistake."
  (jsonrpc-error :code -32601
                 :message (format "org-semantic: no such client method: %s"
                                  method)))

(defun org-semantic--forget-connection ()
  "Forget the server and everything that was in flight on it."
  (setq org-semantic--connection nil
        org-semantic--server-version nil)
  (clrhash org-semantic--watchers)
  (clrhash org-semantic--runs))

(defun org-semantic--process-environment ()
  "`process-environment' for the server, honouring `org-semantic-cache-home'.

The path is expanded here, because the server does not expand it.
The variable becomes a `PathBuf' verbatim, so a literal \"~/cache\"
would create a directory named \"~\" in the server's working
directory and succeed.

Nil sends nothing.  An empty value would not mean \"inherit\" on
the far side: it would resolve the cache against the current
directory."
  (if org-semantic-cache-home
      (cons (concat "ORG_SEMANTIC_CACHE_HOME="
                    (expand-file-name org-semantic-cache-home))
            process-environment)
    process-environment))

(defun org-semantic--start ()
  "Start a server, complete its handshake, and return the connection."
  (let* ((binary (org-semantic--binary))
         (name "org-semantic")
         ;; jsonrpc.el creates the stderr buffer under this name and
         ;; expects `make-process' to use it.  A merged stream would put
         ;; bytes into the Content-Length framing.
         (stderr (format "*%s stderr*" name))
         connection)
    (org-semantic--check-version (org-semantic-binary-version) binary)
    (setq connection
          (make-instance
           'jsonrpc-process-connection
           :name name
           :notification-dispatcher #'org-semantic--handle-notification
           :request-dispatcher #'org-semantic--handle-request
           :on-shutdown (lambda (_conn) (org-semantic--forget-connection))
           ;; A zero-argument closure: older jsonrpc.el calls it with no
           ;; arguments, and the current one asks `func-arity' first.
           :process
           (lambda ()
             (let ((process-environment (org-semantic--process-environment)))
               (make-process :name name
                             :command (list binary "serve")
                             :connection-type 'pipe
                             :coding 'utf-8-emacs-unix
                             :stderr (get-buffer-create stderr)
                             :noquery t)))))
    ;; Synchronously, and before this connection is visible to anything
    ;; else.  `initialized' must be the next message the server reads, so
    ;; no request may go in front of it.
    (let ((org-semantic--starting t))
      (condition-case err
          (let ((info (jsonrpc-request
                       connection "initialize"
                       ;; Nothing is negotiated.  The handshake starts the
                       ;; session and reports the server's release.
                       (list :capabilities (make-hash-table :test 'equal))
                       :timeout org-semantic-timeout)))
            (jsonrpc-notify connection "initialized" :jsonrpc-omit)
            (setq org-semantic--server-version
                  (plist-get (plist-get info :serverInfo) :version))
            (org-semantic--check-version org-semantic--server-version
                                         "the running server"))
        ((jsonrpc-error error quit)
         (jsonrpc-shutdown connection 'cleanup)
         (signal (car err) (cdr err)))))
    connection))

(defun org-semantic-connection ()
  "Return a running server, starting one if there is none."
  (cond
   ((org-semantic-running-p) org-semantic--connection)
   (org-semantic--starting
    (error "The org-semantic server is still starting up"))
   (t (setq org-semantic--connection (org-semantic--start)))))

;;;###autoload
(defun org-semantic-quit (&optional hard)
  "Stop the server, letting an index in flight finish first.

`shutdown' and `exit' are two steps.  The first lets a run that is
still going answer under its own id.  With HARD, or a prefix
argument, only `exit' is sent: the process ends at once and
abandons any run.  That is safe, because an index is committed by
one rename, so an abandoned run leaves the previous index as it
was.

A hard quit still sends `exit' rather than deleting the process.
`exit' stops the server's reader, so it ends cleanly."
  (interactive "P")
  ;; `os-' again: read by the callbacks below, after this has returned.
  (let ((os-connection org-semantic--connection))
    (cond
     ((not (org-semantic-running-p)) (org-semantic--forget-connection))
     (hard (jsonrpc-notify os-connection "exit" :jsonrpc-omit)
           (jsonrpc-shutdown os-connection 'cleanup))
     (t
      ;; Asynchronously, and with an index's timeout: `shutdown' waits
      ;; for a run in flight, which is minutes.
      (jsonrpc-async-request
       os-connection "shutdown" :jsonrpc-omit
       :timeout org-semantic-index-timeout
       :success-fn (lambda (_result)
                     (jsonrpc-notify os-connection "exit" :jsonrpc-omit)
                     (jsonrpc-shutdown os-connection 'cleanup))
       :error-fn (lambda (_error) (jsonrpc-shutdown os-connection 'cleanup))
       :timeout-fn (lambda () (jsonrpc-shutdown os-connection 'cleanup)))))))

;;;###autoload
(defun org-semantic-restart ()
  "Stop the server and start a fresh one.

For a new binary, or a wedged process.  An index rebuilt behind
the server's back needs `org-semantic-reload' instead, which
costs no model load."
  (interactive)
  (org-semantic-quit 'hard)
  (org-semantic--forget-connection)
  (org-semantic-connection)
  (message "org-semantic %s" (or org-semantic--server-version "?")))


;;;; Requests

(defun org-semantic--params (&rest pairs)
  "Build a JSON object from PAIRS, dropping every key whose value is nil.

Dropped, not sent as null, so the server applies its own default.
A nil `config' would arrive as JSON null and fail to parse, where
an absent one means \"whatever the index was built under\".  A
boolean that must be false is therefore `:json-false'."
  (let (out)
    (while pairs
      (let ((key (pop pairs)) (value (pop pairs)))
        (when value (setq out (cons value (cons key out))))))
    (nreverse out)))

(defun org-semantic--bool (value)
  "Return VALUE as a JSON boolean, mapping nil to false rather than null."
  (if value t :json-false))

(defun org-semantic-true-p (value)
  "Whether VALUE, as JSON read it, is true.
JSON false arrives as `:json-false', which is not nil."
  (and value (not (eq value :json-false))))

(defun org-semantic--call (method params &optional timeout)
  "Send METHOD with PARAMS and wait TIMEOUT seconds for the reply.

Synchronous, so it blocks Emacs.  Use it for a search, which takes
milliseconds, and never for an index.  A failure arrives as a
labelled `org-semantic-error'."
  (condition-case err
      (jsonrpc-request (org-semantic-connection) method params
                       :timeout (or timeout org-semantic-timeout))
    (jsonrpc-error (org-semantic--rethrow err))))

(cl-defun org-semantic--call-async (method params
                                           &key success failure progress
                                           timeout)
  "Send METHOD with PARAMS and return the request id at once.

SUCCESS is called with the result, FAILURE with an
`org-semantic-error'-shaped error object; either may be nil, and
a failure nobody handles is reported through `display-warning'
rather than dropped.  PROGRESS, if given, is called with each
`$/progress' report until the reply arrives.  TIMEOUT defaults to
`org-semantic-timeout'.

The id is what `$/cancelRequest' names, so a caller that may want
to stop the work keeps it."
  ;; `os-' prefixes: a callback outlives the call, so what it closes over
  ;; must be lexical.  A name that anything has `defvar'-ed is dynamic
  ;; instead, and unbound again before the reply arrives.  `vault' and
  ;; `id' are names a note-taking configuration is likely to have taken,
  ;; and the failure is silent: the request is answered, and the
  ;; bookkeeping keys itself on whatever the global held.
  (let* ((connection (org-semantic-connection))
         (os-id nil)
         (os-forget (lambda () (remhash os-id org-semantic--watchers))))
    (setq os-id
          (car (jsonrpc-async-request
                connection method params
                :timeout (or timeout org-semantic-timeout)
                :success-fn (lambda (result)
                              (funcall os-forget)
                              (when success (funcall success result)))
                :error-fn (lambda (error-object)
                            (funcall os-forget)
                            (org-semantic--failed error-object failure))
                :timeout-fn (lambda ()
                              (funcall os-forget)
                              (org-semantic--failed
                               (list :message (format "%s timed out" method))
                               failure)))))
    ;; Registered after the request went out, which is the only order
    ;; available: jsonrpc.el assigns the id.  A report that arrives
    ;; before this line is dropped, which the contract allows.
    (when progress (puthash os-id progress org-semantic--watchers))
    os-id))

(defun org-semantic--failed (error-object failure)
  "Hand ERROR-OBJECT to FAILURE, or report it if FAILURE is nil."
  (if failure
      (funcall failure error-object)
    (display-warning 'org-semantic
                     (or (plist-get error-object :message) "request failed"))))


;;;; Searching

(cl-defun org-semantic-search (query &key vault k per-file merge-by-section
                                     mode model any config)
  "Search VAULT for QUERY and return the reply, waiting for it.

The reply is a plist: `:hits', a vector of hits, and `:indexing',
true when an index was running.  The list is then the version
committed before that index, and the search is slower.
`org-semantic-hits' unpacks the first, `org-semantic-true-p' reads
the second.

VAULT defaults to the current buffer's.  MODE is \"semantic\"
\(the default) or \"lexical\".  The two take the same request and
return the same shape, so a caller never branches on the reply.

K bounds how many notes may appear, PER-FILE how many passages any
one of them may contribute.  Set both: with PER-FILE at its
default, a vault kept in three large files answers a K of 50 with
nine hits.

MERGE-BY-SECTION folds a section that answered as several
passages into one hit.  ANY makes a lexical query match notes
carrying any of its terms rather than all.  MODEL and CONFIG
default to `org-semantic-model' and `org-semantic-config'.

An empty QUERY returns no hits and is not an error, so it is safe
to send on every keystroke.  Debouncing is the caller's business."
  (org-semantic--call
   "search"
   (org-semantic--params
    :vault (or vault (org-semantic-vault-or-error))
    :query query :k k :perFile per-file
    :mergeBySection (and merge-by-section t)
    :mode mode :model (or model org-semantic-model)
    :any (and any t)
    :config (or config org-semantic-config))))

(cl-defun org-semantic-search-async (query &key vault k per-file
                                           merge-by-section mode model any
                                           config success failure)
  "Search VAULT for QUERY without waiting, and call SUCCESS with the reply.

Arguments are as in `org-semantic-search'; FAILURE is called with
the error object if there is one.  Returns the request id.

The server supersedes no search: ten keystrokes get ten replies,
in order.  Keep one search in flight, hold the latest query, and
send it from the previous reply.  See `org-semantic-ui-driver',
which does this."
  (org-semantic--call-async
   "search"
   (org-semantic--params
    :vault (or vault (org-semantic-vault-or-error))
    :query query :k k :perFile per-file
    :mergeBySection (and merge-by-section t)
    :mode mode :model (or model org-semantic-model)
    :any (and any t)
    :config (or config org-semantic-config))
   :success success :failure failure))

(defun org-semantic-hits (reply)
  "The hits in REPLY, as a list."
  (append (plist-get reply :hits) nil))

(defun org-semantic-hit-file (hit)
  "The absolute file HIT is in."
  (plist-get hit :file))

(defun org-semantic-hit-line (hit)
  "The line to go to for HIT.

The heading that owns the passage, counted over the raw file, so
point lands on the section and org supplies the subtree and the
properties.  This and `org-semantic-hit-file' are the whole address
of a hit.  Do not use `:id': in a file of many notes, every hit can
carry the same one."
  (or (plist-get hit :headingLine) 1))

(defun org-semantic-hit-path (hit)
  "The path HIT is in, relative to the vault.
What to show; `org-semantic-hit-file' is what to open."
  (plist-get hit :path))

(defun org-semantic-hit-title (hit)
  "The name of the note HIT is in.

Its `#+title:' if it has one, and otherwise its filename without
the extension.  The server decides this.  It can be empty, so
treat it as a string that may be blank."
  (plist-get hit :title))

(defun org-semantic-hit-start-line (hit)
  "The first line of the passage HIT matched on."
  (plist-get hit :startLine))

(defun org-semantic-hit-end-line (hit)
  "The last line of the passage HIT matched on."
  (plist-get hit :endLine))

(defun org-semantic-hit-text (hit)
  "The passage HIT matched on, as the note's own lines.

Read from the note when the search was answered, not stored in the
index, so it is the text as it is now.  It is the lines
`org-semantic-hit-start-line' to `org-semantic-hit-end-line'
joined with newlines: its nth line is line START-LINE + n of the
note.  A client can therefore number the lines, jump to one, or
write one back.

Empty when the note has been cut shorter than the span.  The
caller must test for this: an empty string against a span of
several lines is not a passage of one blank line."
  (plist-get hit :text))

;;;###autoload
(defun org-semantic-visit-hit (hit &optional other-window)
  "Open HIT: its file, at its heading.  In OTHER-WINDOW if non-nil.

It goes to a line, and does not search for a heading: the recorded
text can be older than the note."
  (let ((buffer (find-file-noselect (org-semantic-hit-file hit))))
    (if other-window (pop-to-buffer buffer) (pop-to-buffer-same-window buffer))
    (goto-char (point-min))
    (forward-line (1- (org-semantic-hit-line hit)))
    (when (and (derived-mode-p 'org-mode)
               (fboundp 'org-fold-show-set-visibility))
      (org-fold-show-set-visibility 'canonical))
    (recenter 0)
    buffer))


;;;; Indexing

(cl-defun org-semantic-index (&key vault mode full rehash model config
                                   conserve-memory success failure progress)
  "Index VAULT, without waiting, and return the request id.

MODE is \"semantic\", \"lexical\" or \"both\", and defaults to
`org-semantic-index-mode'.  FULL rebuilds from scratch, which is
also how a changed policy is agreed to; REHASH re-reads every
note rather than trusting its timestamp.  MODEL, CONFIG and
CONSERVE-MEMORY default to the corresponding settings.

SUCCESS is called with what each index did, as numbers, and with
any `remarks': warnings that did not stop the run, which travel on
the reply because stderr does not reach us.  FAILURE is called
with the error object.  PROGRESS is called with each report; pass
`org-semantic-report-message' to say where the run has got to in
the echo area.

Only one index per vault runs at a time.  A second is refused with
kind `indexing', and is not queued, so coalesce on this side and
send again from the reply.  A different vault is not refused, but
rebuilding several at once costs about 665 MB each and is no
faster than one after another.

The reply ends the run, whatever happened.  Do not wait for a
final report: reports are thinned by a send-rate floor, and any of
them can be dropped."
  ;; `os-' as in `org-semantic--call-async': a callback reads these two
  ;; after this call has returned.
  (let* ((os-vault (or vault (org-semantic-vault-or-error)))
         (os-id nil)
         (os-release
          (lambda ()
            ;; Only if it is still ours: a reply that arrives after the
            ;; next run has started must not retire that one's entry.
            (when (equal os-id (gethash os-vault org-semantic--runs))
              (remhash os-vault org-semantic--runs)))))
    (setq os-id
          (org-semantic--call-async
           "index"
           (org-semantic--params
            :vault os-vault
            :mode (or mode org-semantic-index-mode)
            :full (and full t) :rehash (and rehash t)
            :model (or model org-semantic-model)
            :config (or config org-semantic-config)
            ;; Spelled out, since the point of sending it at all is to
            ;; say which of the two it is.
            :conserveMemory (org-semantic--bool
                             (or conserve-memory
                                 org-semantic-conserve-memory)))
           :timeout org-semantic-index-timeout
           :progress progress
           :success (lambda (result)
                      (funcall os-release)
                      (when success (funcall success result)))
           :failure (lambda (error-object)
                      (funcall os-release)
                      (org-semantic--failed error-object failure))))
    (puthash os-vault os-id org-semantic--runs)
    os-id))

(cl-defun org-semantic-download (&key model success failure progress)
  "Fetch MODEL's weights, without waiting, and return the request id.

MODEL is a name from `org-semantic-models' and is required.  A
download belongs to a model, not to a vault, and the
`model-missing' error carries the name in its `data'.

SUCCESS is called with `model' and `downloaded'.  The second is
nil when the weights were already there, so a client does not
report a download it did not make.  FAILURE is called with the
error object.  PROGRESS is called with each report, of which there
is one, giving the size before the wait.

Nothing else happens: no index is built and no vault is touched.
Search again afterwards.

A second fetch of the same model is refused with kind
`downloading', and is not queued.  A download cannot be cancelled,
and a large model takes minutes, so this uses
`org-semantic-index-timeout'."
  (org-semantic--call-async
   "download"
   (org-semantic--params :model (or model (error "Which model to download?")))
   :timeout org-semantic-index-timeout
   :progress progress
   :success success
   :failure (lambda (error-object) (org-semantic--failed error-object failure))))

(defun org-semantic-indexing-p (&optional vault)
  "The id of the index this client started for VAULT, or nil.

About what this client asked for.  A run started by another Emacs
or by a shell is not here.  `org-semantic-status' answers about the
vault itself."
  (gethash (or vault (org-semantic-vault-or-error)) org-semantic--runs))

;;;###autoload
(defun org-semantic-cancel (&optional vault)
  "Stop the index this client started for VAULT.

A run stops at a note boundary and writes nothing, so the index
already committed is unchanged.  The request carries the id it
answers under, so a cancellation for a run that has already
answered does nothing and cannot stop the next one.

A model download cannot be cancelled.  Killing the process is the
only answer there."
  (interactive)
  (let* ((vault (or vault (org-semantic-vault-or-error)))
         (id (gethash vault org-semantic--runs)))
    (if (not id)
        (message "org-semantic: no index of %s to stop"
                 (abbreviate-file-name vault))
      (jsonrpc-notify (org-semantic-connection) "$/cancelRequest"
                      (list :id id))
      (message "org-semantic: stopping the index of %s"
               (abbreviate-file-name vault)))))

(defun org-semantic-report-message (report)
  "Show REPORT, one `$/progress' value, in the echo area.

Built from the fields that are present, and not from a match on
the phase, so it needs no list of the phases that exist."
  (let* ((target (plist-get report :target))
         (phase (plist-get report :phase))
         (unit (plist-get report :unit))
         (done (plist-get report :done))
         (total (plist-get report :total))
         (bytes (plist-get report :bytes))
         (tokens (plist-get report :tokens))
         (of-tokens (plist-get report :ofTokens)))
    (message
     "org-semantic %s %s: %s%s" target phase
     (cond (total (format "%s/%s %s" done total unit))
           (bytes (format "%.0f MB" (/ bytes 1e6)))
           (t "..."))
     (if (and tokens of-tokens)
         (format " (%s/%s tokens)" tokens of-tokens)
       ""))))

(defun org-semantic--reindex-flags (arg)
  "Return (REHASH . FULL) for the raw prefix argument ARG.

Ordered by what each one costs:

  plain      trust every note's timestamp and size.  0.03 s when
             nothing changed.
  \\[universal-argument]        rehash: read and hash every note, and re-embed the
             ones whose content moved but whose stamp did not.
             0.09 s of reading on a thousand notes.  Use it after
             a timestamp-preserving restore, `rsync --times' or
             `touch -r'.
  \\[universal-argument] \\[universal-argument]    full: rebuild from scratch, which takes minutes.

Rehash is not a small full rebuild.  It re-reads every note and
still re-embeds only what differs, so it cannot pick up a changed
policy or a changed language set.  Use full for those.

FULL implies rehashing, so the two are never both sent."
  (let ((level (prefix-numeric-value arg)))
    (cond ((null arg) (cons nil nil))
          ((>= level 16) (cons nil t))
          (t (cons t nil)))))

;;;###autoload
(defun org-semantic-reindex (&optional arg)
  "Index the current buffer's vault, reporting progress in the echo area.

Plain, this is incremental: a note whose timestamp and size are
unchanged is not read.  ARG does more, and the prefixes are
ordered by cost: one `C-u' rehashes, two rebuild from scratch.
See `org-semantic--reindex-flags'."
  (interactive "P")
  ;; `os-' again: read by the callbacks below, after this has returned.
  (let* ((os-vault (org-semantic-vault-or-error))
         (flags (org-semantic--reindex-flags arg))
         (os-how (cond ((cdr flags) "full rebuild of")
                       ((car flags) "rehashing")
                       (t "indexing"))))
    (org-semantic-index
     :vault os-vault
     :rehash (car flags)
     :full (cdr flags)
     :progress #'org-semantic-report-message
     :success
     (lambda (result)
       (message "org-semantic: indexed %s%s"
                (abbreviate-file-name os-vault)
                (org-semantic--summarise result)))
     :failure
     (lambda (error-object)
       (message "org-semantic: %s"
                (or (plist-get error-object :message) "the index failed"))))
    (message "org-semantic: %s %s..." os-how (abbreviate-file-name os-vault))))

(defun org-semantic--summarise (result)
  "A short account of RESULT, what an index reported it did."
  (mapconcat
   (lambda (target)
     (let ((report (plist-get result (car target))))
       (if (null report)
           ""
         (format ", %s: %s files, %s chunks in %.1fs"
                 (cdr target)
                 (plist-get report :files)
                 (plist-get report :chunks)
                 (plist-get report :secs)))))
   '((:semantic . "semantic") (:lexical . "lexical"))
   ""))


;;;; Reindexing when a note is saved

(defcustom org-semantic-auto-reindex-delay 2.0
  "Seconds of quiet after a save before `org-semantic-auto-reindex-mode' runs.

Each save restarts the wait, so `save-some-buffers' over fifty
notes costs one run.  A run of one changed note takes about 70 ms,
so a longer delay gains nothing."
  :type 'number)

(defcustom org-semantic-auto-reindex-quietly t
  "Whether `org-semantic-auto-reindex-mode' keeps quiet about having worked.

An automatic reindex should not need attention, and a line in the
echo area after every save asks for it.

This applies to success only.  A vault with no index to refresh,
and a run that failed, are said once: an automatic feature that has
stopped working looks the same as one that is working."
  :type 'boolean)

(defvar org-semantic-auto-reindex--timers (make-hash-table :test 'equal)
  "The pending reindex of each vault, keyed by the vault.")

(defvar org-semantic-auto-reindex--said (make-hash-table :test 'equal)
  "What has been said about each vault, so it is not said again per save.")

(defun org-semantic-auto-reindex--say (os-vault what message)
  "Say MESSAGE about OS-VAULT once, and remember it as WHAT.

Latched per vault, because each condition holds until somebody acts
on it: a vault with no index still has none at the next save.
Unlatched, `save-some-buffers' over fifty notes says the same
sentence fifty times.

WHAT is compared, not the sentence, so a different condition still
speaks.  A run that works clears it."
  (unless (eq what (gethash os-vault org-semantic-auto-reindex--said))
    (puthash os-vault what org-semantic-auto-reindex--said)
    (message "%s" message)))

(defun org-semantic-auto-reindex--refreshable (vault)
  "Which of VAULT's indexes an automatic run may refresh, or nil.

Only the indexes that already exist.  Saving a note must not start
minutes of embedding, which is what the first automatic run in an
unindexed vault would be with `org-semantic-index-mode' at its
default of \"both\".  A vault with only the word index refreshes
that one, and a vault with nothing built is left to
`org-semantic-reindex'.

It costs one `status', which is milliseconds, and asks every time.
A negative answer must not be cached, or a new index would not be
noticed."
  (let* ((status (org-semantic-status vault))
         (built-semantic (> (length (append (plist-get status :semantic) nil)) 0))
         (built-lexical (org-semantic-true-p (plist-get status :lexical)))
         (semantic (and (member org-semantic-index-mode '("both" "semantic"))
                        built-semantic))
         (lexical (and (member org-semantic-index-mode '("both" "lexical"))
                       built-lexical)))
    (cond ((and semantic lexical) "both")
          (semantic "semantic")
          (lexical "lexical"))))

(defun org-semantic-auto-reindex--start (os-vault mode)
  "Reindex MODE of OS-VAULT, saying as little as it is allowed to."
  (org-semantic-index
   :vault os-vault
   :mode mode
   :success
   (lambda (result)
     (remhash os-vault org-semantic-auto-reindex--said)
     (unless org-semantic-auto-reindex-quietly
       (message "org-semantic: indexed %s%s"
                (abbreviate-file-name os-vault)
                (org-semantic--summarise result))))
   :failure
   (lambda (error-object)
     (org-semantic-auto-reindex--say
      os-vault 'failed
      (format "org-semantic: %s"
              (or (plist-get error-object :message) "the index failed"))))))

(defun org-semantic-auto-reindex--run (os-vault)
  "Reindex OS-VAULT now, or find out why not.

A run may be in flight, and the server refuses a second one per
vault.  The answer is to wait and ask again, which also folds
every save made during that run into the run that follows."
  (remhash os-vault org-semantic-auto-reindex--timers)
  (condition-case error
      ;; A missing binary is reported below rather than asked about: this
      ;; fires a delay after a save, into whatever is being typed by then.
      (let ((org-semantic--may-ask nil))
        (if (org-semantic-indexing-p os-vault)
            (org-semantic-auto-reindex--arm os-vault)
          (let ((mode (org-semantic-auto-reindex--refreshable os-vault)))
            (if mode
                (org-semantic-auto-reindex--start os-vault mode)
              (org-semantic-auto-reindex--say
               os-vault 'no-index
               (format (concat "org-semantic: %s has no index to keep up to "
                               "date -- M-x org-semantic-reindex builds one")
                       (abbreviate-file-name os-vault)))))))
    ;; A server that will not start, or a binary that is not there: said
    ;; once, and never signalled.  This runs from a timer, and an error
    ;; there would also stop later saves from trying.
    (error (org-semantic-auto-reindex--say
            os-vault 'broken
            (format "org-semantic: %s" (error-message-string error))))))

(defun org-semantic-auto-reindex--arm (vault)
  "Reindex VAULT once saving has stopped for `org-semantic-auto-reindex-delay'."
  (let ((pending (gethash vault org-semantic-auto-reindex--timers)))
    (when (timerp pending) (cancel-timer pending)))
  (puthash vault
           (run-with-timer org-semantic-auto-reindex-delay nil
                           #'org-semantic-auto-reindex--run vault)
           org-semantic-auto-reindex--timers))

(defun org-semantic-auto-reindex--on-save ()
  "Arm a reindex, if what was just saved is a note in a vault.

On `after-save-hook', which runs for every save in Emacs, so the
cheap questions come first.  The vault is worked out each time and
not cached: it costs a few file-name operations, and a cache would
answer for the vault this buffer belonged to when it was opened.

Containment is the last question, and it is necessary.  With
`org-semantic-vault-root' set globally, every buffer resolves to
that vault, which is what makes a search from `*scratch*' work.
Without this test, saving a README in a code repository would
reindex your notes and report success.

The test is containment in the notes, which are not always in the
vault: a vault directory can hold nothing but the index.  Testing
the vault instead would do nothing for those vaults."
  (let ((file (buffer-file-name)))
    (when (and file (string-suffix-p ".org" file))
      (let ((vault (org-semantic-vault)))
        (when (and vault (file-in-directory-p file (org-semantic-notes-root vault)))
          (org-semantic-auto-reindex--arm vault))))))

;;;###autoload
(define-minor-mode org-semantic-auto-reindex-mode
  "Keep a vault's indexes up to date as its notes are saved.

Incremental, so a run costs about what one changed note costs.  A
note whose timestamp and size are unchanged is not read, and a note
that changed re-embeds only the passages whose text moved.
`org-semantic-auto-reindex-delay' is how long saving must stop for,
and `org-semantic-auto-reindex-quietly' whether a run that worked
says so.

It does not build an index that does not exist.  That takes minutes
of embedding, so it says once which vault needs
`org-semantic-reindex'.

It does not see notes changed outside Emacs: a sync, a `git pull',
a rename in Dired.  Nothing here watches the filesystem.  A package
that does watch it can call `org-semantic-auto-reindex-touch', and
`org-semantic-reindex' catches up at any time."
  :global t
  :group 'org-semantic
  (clrhash org-semantic-auto-reindex--said)
  (if org-semantic-auto-reindex-mode
      (add-hook 'after-save-hook #'org-semantic-auto-reindex--on-save)
    (remove-hook 'after-save-hook #'org-semantic-auto-reindex--on-save)
    (maphash (lambda (_vault timer) (when (timerp timer) (cancel-timer timer)))
             org-semantic-auto-reindex--timers)
    (clrhash org-semantic-auto-reindex--timers)))

;;;###autoload
(defun org-semantic-auto-reindex-touch (&optional vault)
  "Arm a reindex of VAULT, as saving one of its notes would.

For the changes a save cannot report: a note renamed or deleted in
Dired, a `git pull', a folder arriving from a sync.  Call it from
whatever watches the tree.

It does not take the file that changed.  A run is a vault-wide
incremental scan, so it is enough to know that something changed:
a rename is caught by the arrival of the new name, because the same
scan finds the old one gone.  It is therefore cheap to over-call.
Fifty touches inside `org-semantic-auto-reindex-delay' are one run,
as fifty saves are.

VAULT is a vault root, spelled as `org-semantic-vault' spells it.
It defaults to the current buffer's vault, which is rarely what a
caller wants, because a watcher's callback runs in whatever buffer
is current.  Nil, and no vault, does nothing.

It is independent of `org-semantic-auto-reindex-mode' and is not
gated on it.  That mode is one trigger, `after-save-hook', and a
configuration whose watcher already reports saves does not want it.
The two share `org-semantic-auto-reindex-delay',
`org-semantic-auto-reindex-quietly', and the rule that an index
which does not exist is not built."
  (when-let* ((vault (or vault (org-semantic-vault))))
    (org-semantic-auto-reindex--arm vault)))

;;;; What the server holds

(defun org-semantic-status (&optional vault)
  "Return what VAULT has: its built indexes, and whether one is being built.

Every field is about that vault: which models have a semantic
index, whether a lexical one exists, whether the index is resident
here, and whether an index is running.  The server's release comes
from the handshake, not from here.

Each entry in `:semantic' carries `:cached', which says whether the
model that built that index is still downloaded on this machine.
An index outlives its model, and a search refuses when the model is
gone, so ask this before searching."
  (org-semantic--call
   "status"
   (org-semantic--params :vault (or vault (org-semantic-vault-or-error)))))

;;;###autoload
(defun org-semantic-show-status (&optional vault)
  "Say in the echo area what VAULT has."
  (interactive)
  (let* ((vault (or vault (org-semantic-vault-or-error)))
         (status (org-semantic-status vault))
         (models (append (plist-get status :semantic) nil)))
    (message "%s%s: semantic [%s], lexical %s, %s%s"
             (abbreviate-file-name vault)
             ;; Only when they differ.  The vault directory then does not
             ;; say which notes are indexed.
             (let ((notes (plist-get status :notes)))
               (if (and notes (not (equal notes vault)))
                   (format " (notes in %s)" (abbreviate-file-name notes))
                 ""))
             ;; A model whose weights are gone is named as such.  The index
             ;; is there and cannot be searched, which otherwise looks the
             ;; same as a working one until a search refuses.
             (mapconcat (lambda (m)
                          (if (org-semantic-true-p (plist-get m :cached))
                              (plist-get m :name)
                            (concat (plist-get m :name) " (not downloaded)")))
                        models " ")
             (if (org-semantic-true-p (plist-get status :lexical)) "yes" "no")
             (if (org-semantic-true-p (plist-get status :loaded))
                 "resident" "not loaded")
             (if (org-semantic-true-p (plist-get status :indexing))
                 ", indexing" ""))))

(defun org-semantic-memory ()
  "Return what the process is holding, in bytes.

`rss' is the total.  `vaults' and `models' are the parts that can
be counted exactly.  There is no figure for the ONNX runtime,
which cannot be asked about from inside the process, and nothing
is derived: a caller that wants the remainder subtracts."
  (org-semantic--call "memory" :jsonrpc-omit))

;;;###autoload
(defun org-semantic-show-memory ()
  "Say in the echo area what the process is holding."
  (interactive)
  (let ((memory (org-semantic-memory)))
    (message "org-semantic: %.0f MB resident, %s vault(s), %s model(s)"
             (/ (plist-get memory :rss) 1e6)
             (length (plist-get memory :vaults))
             (length (plist-get memory :models)))))

;;;###autoload
(defun org-semantic-reload ()
  "Make the server forget the indexes it has cached.

For an index rebuilt outside this session, by a shell run or
another Emacs.  An index the server built itself needs none of
this: it adopts what it wrote."
  (interactive)
  (org-semantic--call "reload" :jsonrpc-omit)
  (message "org-semantic: cached indexes dropped"))

;;;###autoload
(defun org-semantic-close (&optional vault)
  "Tell the server we are finished with VAULT.

Its chunk table and vectors are dropped, and the model with them
if no other vault is using it.  Worth sending when the last
buffer visiting a vault is gone.

Expect a ceiling, not a refund.  The memory returns to the system
on the allocator's schedule, so do not wait for the number to fall.
What this buys is that N vaults on one model cost about 262 MB,
and not 255 MB plus 143 MB for each vault after the first.

Returns how many entries were dropped, and says so only when called
as a command.  A caller that sends this knows a vault has been
left, which is not an occasion for a line in the echo area."
  (interactive)
  (let* ((vault (or vault (org-semantic-vault-or-error)))
         (result (and (org-semantic-running-p)
                      (org-semantic--call
                       "close" (org-semantic--params :vault vault))))
         (dropped (or (plist-get result :dropped) 0)))
    (when (called-interactively-p 'any)
      (message "org-semantic: closed %s (%s entry/entries dropped)"
               (abbreviate-file-name vault) dropped))
    dropped))

(provide 'org-semantic)
;;; org-semantic.el ends here
