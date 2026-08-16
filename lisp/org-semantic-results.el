;;; org-semantic-results.el --- A buffer of org-semantic hits -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.4.1
;; Package-Requires: ((emacs "29.1"))
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; `org-semantic-find' searches the current buffer's vault and shows what
;; came back in a buffer you can walk with `n' and `p' and open with
;; RET, which is the shape `grep' and `occur' established.  It is wired
;; into `next-error', so `M-g M-n' works from anywhere.
;;
;; A hit is a passage: several lines of prose, shown as the note's own
;; lines.  Four rules follow from that, and each is easy to undo.
;;
;; The passage is the note's lines, in order and unaltered.  The server
;; sends lines `startLine' to `endLine' joined with newlines, so the nth
;; line of the text is line startLine + n of the note.  That equality is
;; what lets each line carry its number and be jumped to on its own.  It
;; is checked at render time, because the server sends an empty string
;; when the note is shorter than the span.
;;
;; The content is therefore inserted verbatim.  No filling, no
;; truncation, no indentation inside it, and no `display' property over
;; it.  Everything drawn goes in the gutter, which is the few columns at
;; the start of each passage line, and which also carries the provenance
;; properties.
;;
;; A hit is addressed by file and line.  Not by its `:ID:', which is the
;; same for every hit in a large note, and not by its heading text, which
;; can be older than the note.
;;
;; The same line can be shown twice.  Consecutive passages of one section
;; overlap by a paragraph, and a long paragraph yields several passages
;; that all name the whole of it.  A claim map decides which drawing of a
;; line owns it.  The others are dimmed, and a passage with nothing left
;; to claim is dropped.

;;; Code:

(require 'cl-lib)
(require 'button)
;; Preloaded, so it costs nothing at runtime.  Without it the compiler
;; does not know the `occur-' names the gutter is shaped for.
(require 'replace)
(require 'org-semantic)
(require 'org-semantic-ui)

;; Org is loaded on demand: a passage is fontified with it, and nothing
;; else here needs it.  These two are therefore declared, not required,
;; so a results buffer opens in an Emacs that has never visited a note.
(defvar org-link-bracket-re)
(defvar org-link-descriptive)


;;;; Settings

(defgroup org-semantic-results nil
  "The buffer org-semantic shows its hits in."
  :group 'org-semantic
  :prefix "org-semantic-results-")

(defcustom org-semantic-results-passage-lines 12
  "How many lines of a passage to show before folding the rest away.

A passage runs to the length of a section, which can be long.  The
rest is hidden rather than dropped, and `TAB' unfolds it.  Nil
shows every line."
  :type '(choice (const :tag "All of them" nil) natnum))

(defcustom org-semantic-results-line-numbers nil
  "Whether to number a passage's lines with their numbers in the note.

Off, the gutter is a plain indent.  On, it shows each line's number
in its note.  The gutter records the file and the number either
way, so this changes only what is drawn."
  :type 'boolean)

(defcustom org-semantic-results-ranking "semantic"
  "Which ranking `org-semantic-find' asks for unless told otherwise.

\"semantic\" finds notes by meaning and needs the embedding index.
\"lexical\" finds them by word, from an index that builds in
seconds.  \"ask\" settles it for each search, which a single prefix
argument also does.  Set \"ask\" if you have no usual answer.

It is the `mode' the server is asked for, and is called `ranking'
here so as not to read as a setting for
`org-semantic-results-mode'."
  :type '(choice (const :tag "By meaning" "semantic")
                 (const :tag "By word" "lexical")
                 (const :tag "Ask each time" "ask")))

(defcustom org-semantic-results-connector 'and
  "How the terms of a word search are joined: `and' or `or'.

`and' answers with the notes carrying every term, `or' with those
carrying any of them.  This is the default for a search.  In the
results buffer, `l' changes it for the search on screen.

It applies to a word search only.  An embedding has no terms to
join, so the semantic ranking ignores it and the key refuses.

A query can also write `AND', `OR', `NOT' and parentheses, so this
is the default and not the only way to say it."
  :type '(choice (const :tag "All terms (AND)" and)
                 (const :tag "Any term (OR)" or)))

(defcustom org-semantic-results-fontify t
  "Whether to show a passage with org's own faces on it.

A passage is org text, and emphasis, verbatim, headings and block
markers all help to read it.  It is fontified by inserting it into
a hidden buffer in `org-mode' and copying the faces back out, which
is the method `magit' uses for diffs.

Only `face' is copied, and the characters are never touched.  The
nth line of a passage is line `startLine' + n of the note, and a
rendering that replaced or moved text would break that.  Org's
`keymap', `invisible' and `display' properties are therefore left
behind.

A link is the exception.  When `org-link-descriptive' is on, the
brackets are made invisible under a spec of our own.  See
`org-semantic-results--hide-link-syntax'.

It costs about 0.8 ms a passage, against 0.1 ms unfontified, and
needs org loaded.  Where org is absent, it does nothing.

A block is always whole in a passage: its `#+begin_' line is inside
the span, and a blank line inside it does not end a paragraph.  A
passage therefore never shows one marker without the other."
  :type 'boolean)

(defcustom org-semantic-results-display-action
  '(display-buffer-reuse-mode-window)
  "How the results buffer asks to be shown, as a `display-buffer' ACTION.

A default, never a decision.  `display-buffer-alist' is a user
option and is consulted before the ACTION a caller passes, so
anything set there wins over this.  This package therefore adds no
entry to `display-buffer-alist' itself.

The default gives a behaviour and not a layout: reuse a window that
already shows results, so a second search does not open another
one.  Where that window goes, and how large it is, is the user's
choice.  With no window to reuse, Emacs shows the buffer as it
shows any other.

For a results panel down the right-hand side, put this in your
configuration rather than here:

  (add-to-list \\='display-buffer-alist
               \\='((derived-mode . org-semantic-results-mode)
                 (display-buffer-reuse-mode-window
                  display-buffer-in-direction
                  display-buffer-use-some-window)
                 (direction . right)
                 (window-width . 0.5)))

The order of those functions matters: `display-buffer-use-some-window'
falls back to `get-largest-window' and almost always succeeds, so
anything after it is unreachable.  Put it last."
  :type 'sexp)

(defcustom org-semantic-results-reveal-function
  #'org-semantic-results-reveal-in-dired
  "How to show the directory part of a hit's address.

Called with two arguments, both absolute: the DIRECTORY the note
is in, and the FILE itself.  The second is for putting point on
the note once the directory is shown; a function with no use for
it may ignore it.

Dired is only the default because it is what Emacs has.  To use
something else:

  (setq org-semantic-results-reveal-function
        (lambda (directory _file) (my-file-manager directory)))"
  :type 'function)


;;;; Faces

(defface org-semantic-results-header '((t :inherit bold))
  "Face for the lines at the top of a results buffer.")

(defface org-semantic-results-file
  '((t :inherit font-lock-function-name-face :weight bold))
  "Face for the line naming a note.")

(defface org-semantic-results-heading '((t :inherit default))
  "Face for a hit's outline path.")

(defface org-semantic-results-score '((t :inherit bold))
  "Face for how well a hit matched.

Not `shadow', which makes the head of a block dimmer than its body
and leaves no visible seam between entries.

A weight and not a colour, because the address beside it is already
a link.  `bold' and not `semi-bold': a font without a semi-bold
face falls back to normal, and says nothing about it.")

(defface org-semantic-results-location '((t :inherit shadow))
  "Face for the separators between the parts of a hit's address.")

(defface org-semantic-results-link '((t :inherit link))
  "Face for the parts of a hit's address that go somewhere.

Inherits `link', so they look like every other link in Emacs.  Each
part of the address goes somewhere different, and nothing else says
so.")

(defface org-semantic-results-annotation '((t :inherit shadow))
  "Face for a hit's TODO keyword, priority and tags.")

(defface org-semantic-results-gutter '((t :inherit shadow))
  "Face for the few columns at the start of a passage line.

`shadow', and not `line-number', which many themes give a
background.  The gutter is blank unless
`org-semantic-results-line-numbers' is on, and a background then
paints a grey block four columns wide.  `shadow' colours the digits
when there are digits, and is invisible when there are none.")

(defface org-semantic-results-duplicate '((t :inherit shadow))
  "Face for a passage line already shown, in full, further up.")

(defface org-semantic-results-stale '((t :inherit warning))
  "Face for a passage the note no longer has room for.")

(defface org-semantic-results-elision '((t :inherit shadow :slant italic))
  "Face for the marker standing in for a folded passage tail.")


;;;; State

(defvar-local org-semantic-results--vault nil
  "The vault being searched, spelled as the server keys it.")

(defvar-local org-semantic-results--query nil
  "The query the hits on show answered.")

(defvar-local org-semantic-results--mode "semantic"
  "Which ranking this buffer will ask for next, \"semantic\" or \"lexical\".

What the buffer wants.  `M-s' and `M-l' change it, and a one-off
search does not.  See `org-semantic-results--asked-mode' for what
is on screen.")

(defvar-local org-semantic-results--asked-mode nil
  "The ranking that produced what is drawn, or nil before any reply.

Usually the same as `org-semantic-results--mode', and not always:
the offer that answers a refusal searches by word once, without
changing what the buffer wants.  The header says which ranking
produced the results on screen.")

(defvar-local org-semantic-results--k nil
  "How many notes may appear, or nil for the server's default.")

(defvar-local org-semantic-results--per-file nil
  "How many passages one note may contribute, or nil for the default.")

(defvar-local org-semantic-results--merge nil
  "Whether a section divided into several passages answers as one hit.")

(defvar-local org-semantic-results--fetching nil
  "(MODEL . BYTES) while a download this buffer started is running.

A search sent while the fetch is in flight is refused with
`model-missing' again, so the buffer says it is waiting instead of
asking a question that is already answered.  The wait ends by
itself: the download's own reply re-runs the search.

Only a fetch this buffer started.  One started by another Emacs or
by a shell sends us nothing when it lands.")

(defvar-local org-semantic-results--connector nil
  "How this buffer joins the terms of a word query, or nil for the default.
`and' or `or'; see `org-semantic-results-connector'.")

(defvar-local org-semantic-results--model nil
  "Which model to search, or nil for `org-semantic-model'.")

(defvar-local org-semantic-results--policy t
  "Whether to send `org-semantic-config' with a search.
Set to nil when the user chooses to search an index built under a
policy that has since drifted, which is a decision they make once.")

(defvar-local org-semantic-results--driver nil
  "This buffer's one-search-in-flight driver.")

(defvar-local org-semantic-results--started nil
  "When the search in flight was sent, from `float-time'.")

(defvar-local org-semantic-results--hits nil
  "The hits last drawn, in the order they were drawn.")

(defvar-local org-semantic-results--indexing nil
  "Whether the last reply said an index was running.")

(defvar-local org-semantic-results--latched nil
  "The failure kinds already said in full for the search in flight.
Cleared by `org-semantic-results--search'.  See
`org-semantic-results--latching' for which kinds these are and why
they are said once.")


;;;; A drawn unit

(cl-defstruct (org-semantic-results--item
               (:constructor org-semantic-results--item-create)
               (:copier nil))
  "One block in a results buffer: a hit, and everything drawn for it.

A fresh structure per block even when two of them describe the
same note, because navigation walks the property holding it and
two neighbouring blocks sharing one value would read as a single
block."
  (hit nil :documentation "The hit this block was drawn from.")
  (file nil :documentation "The note it is in, absolutely.")
  (line nil :documentation "The line to go to for the block as a whole.")
  (elided nil :documentation "How many lines of the passage are folded away."))


;;;; The mode

(defvar-keymap org-semantic-results-mode-map
  :doc "Keymap for `org-semantic-results-mode'."
  :parent special-mode-map
  "RET"       #'org-semantic-results-goto
  "o"         #'org-semantic-results-goto-other-window
  "C-o"       #'org-semantic-results-display
  "n"         #'org-semantic-results-next
  "p"         #'org-semantic-results-previous
  "M-n"       #'org-semantic-results-next-note
  "M-p"       #'org-semantic-results-previous-note
  "TAB"       #'org-semantic-results-toggle-passage
  "s"         #'org-semantic-results-set-query
  ;; Named after the rankings, as the header and the setting are:
  ;; `s'emantic and `l'exical.  `m' for meaning and `w' for word would
  ;; name the gloss, and give the keys and the screen two vocabularies.
  ;;
  ;; Meta and not control: the query prompt takes the same two keys, and
  ;; `C-s' is worth more as isearch over a list of passages.  This
  ;; shadows the `search-map' prefix, which is of little use here.
  "M-s"       #'org-semantic-results-rank-by-meaning
  "M-l"       #'org-semantic-results-rank-by-word
  "l"         #'org-semantic-results-toggle-connector
  "k"         #'org-semantic-results-more-notes
  "K"         #'org-semantic-results-fewer-notes
  "+"         #'org-semantic-results-more-passages
  "-"         #'org-semantic-results-fewer-passages
  ;; Each cap has a nudge and a set, and the set is the same key: `=' shares
  ;; its place on the keyboard with `+', and `C-k' its letter with `k'.
  "C-k"       #'org-semantic-results-set-notes
  "="         #'org-semantic-results-set-passages
  "R"         #'org-semantic-results-reindex
  ;; Shadows `revert-buffer' from `special-mode-map' because `C-h m'
  ;; shows the command's own docstring, and the inherited one describes
  ;; replacing the buffer's text with a file's.
  "g"         #'org-semantic-results-revert
  "f"         #'next-error-follow-minor-mode
  ;; Also under the name `occur' and `grep' give it.
  "C-c C-f"   #'next-error-follow-minor-mode)

(defvar org-semantic-results-passage-map
  (let ((map (make-sparse-keymap)))
    (define-key map [mouse-2] #'org-semantic-results-mouse-goto)
    map)
  "Keymap put on every line a hit was drawn on.")

;; The docstring below names keys by command, so they render as the key
;; and cannot go stale after a rebind.  It does so only inside the lists:
;; a form 40 characters wide that renders as one character wraps the
;; source at a width the reader never sees, which leaves a paragraph
;; ragged.  The flowing paragraphs therefore name no keys.
;;
;; `\<...>' sits on the line between the summary and the body.  It
;; renders as nothing, so that line becomes the blank line that belongs
;; there.
(define-derived-mode org-semantic-results-mode special-mode "org-semantic"
  "Major mode for a list of org-semantic hits.
\\<org-semantic-results-mode-map>
Hits are grouped by note, and within a note by section.  A passage is
shown as the note's own lines, so each line here is that line there.

Going to a hit:

  \\[org-semantic-results-goto]  the line under point, in its note
  \\[org-semantic-results-goto-other-window]  the same, in another window
  \\[org-semantic-results-display]  show it without leaving this buffer

Point lands on the line you were reading rather than at the top of its
section, which may be hundreds of lines above.  Every part of a hit's
address is a link too: the directory opens in Dired, the note at its
top, the section at its heading, and each line number where it says.

Moving about:

  \\[org-semantic-results-next] \\[org-semantic-results-previous]  passage by passage
  \\[org-semantic-results-next-note] \\[org-semantic-results-previous-note]  note by note
  \\[org-semantic-results-toggle-passage]  unfold a passage that was cut short

A passage runs to the length of its section and is cut at
`org-semantic-results-passage-lines' lines.  Moving by note skips
whatever is left of this one, sections and all.

Asking something else:

  \\[org-semantic-results-set-query]  edit the query
  \\[org-semantic-results-rank-by-meaning]  semantic: rank by meaning
  \\[org-semantic-results-rank-by-word]  lexical: rank by word
  \\[org-semantic-results-toggle-connector]  toggle the logical connector between AND and OR

The two rankings are separate indexes, not two orderings of one.  The
connector is a word search only, so it refuses on the other.

How much comes back:

  \\[org-semantic-results-more-notes] \\[org-semantic-results-fewer-notes]  more, fewer notes
  \\[org-semantic-results-set-notes]  that many notes exactly
  \\[org-semantic-results-more-passages] \\[org-semantic-results-fewer-passages]  more, fewer passages per note
  \\[org-semantic-results-set-passages]  that many passages exactly

The first pair widens the list, the second deepens the notes already in
it.  The header states both, and neither reaches nothing.

Keeping up to date:

  \\[org-semantic-results-revert]  ask again -- no note is read or written
  \\[org-semantic-results-reindex]  index the vault first, then ask again

Two prefix arguments rebuild the index from scratch.  And `next-error'
works from anywhere, so these hits can be walked from any buffer.

Reading without pressing anything:

  \\[next-error-follow-minor-mode]  follow point: preview each passage as you reach it

A minor mode, so it stays on until turned off.  With it, moving by
passage or by note shows that passage in its note as point arrives,
without selecting the window -- so the list stays where the typing
goes.  It answers to the key `occur' and `grep' use for it as well,
which the table below spells out.

To have it on in every results buffer, put it on this mode's hook:

  (add-hook \\='org-semantic-results-mode-hook
            \\='next-error-follow-minor-mode)

\\{org-semantic-results-mode-map}"
  (setq-local revert-buffer-function #'org-semantic-results--revert)
  ;; Wrapped, not truncated, with `wrap-prefix' putting the continuation
  ;; under the gutter.  A note's paragraph can be one long line, and
  ;; truncating it would hide the words that matched.  A file line stays
  ;; one logical line, which is what the numbers and the properties are
  ;; attached to.
  (setq-local truncate-lines nil)
  (setq-local word-wrap t)
  (setq next-error-function #'org-semantic-results--next-error)
  (setq next-error-last-buffer (current-buffer))
  ;; The symbol alone, not `(symbol . t)': the cons asks Emacs to draw its
  ;; own `...' at the end of the last visible line, which repeats what
  ;; `⋯ 3 lines' already says.
  (add-to-invisibility-spec 'org-semantic-results)
  ;; A second spec: `TAB' folds the tail of a passage away and back, and
  ;; must not take a link's brackets with it.
  (add-to-invisibility-spec 'org-semantic-results-link)
  (add-hook 'kill-buffer-hook #'org-semantic-results--abandon nil t))

(defun org-semantic-results--abandon ()
  "Stop caring about the search this buffer asked for."
  (when org-semantic-results--driver
    (org-semantic-ui-driver-abandon org-semantic-results--driver)))


;;;; Asking

(defun org-semantic--find-prompts (arg)
  "Return (RANKING . LIMITS): what a raw prefix ARG asks to be asked.

Ordered by how often each is wanted, as
`org-semantic--reindex-flags' is:

  plain      neither; the settings decide.
  \\[universal-argument]        the ranking, and only that.
  \\[universal-argument] \\[universal-argument]    the ranking and the limits.

A function of its own so that a test can hold the mapping: an
interactive spec cannot be checked."
  (let ((level (prefix-numeric-value arg)))
    (cond ((null arg) (cons nil nil))
          ((>= level 16) (cons t t))
          (t (cons t nil)))))

(defconst org-semantic--rankings
  '(("semantic" . "by meaning, over the embedding index")
    ("lexical"  . "by word, over the BM25 index"))
  "The two rankings, and what each one is.

Each names its own index.  `semantic' and `lexical' can read as two
orderings of one result set, which they are not: they are separate
indexes, built and searched separately, and never merged.")

(defun org-semantic--ranking-annotation (candidate)
  "What CANDIDATE means, for the ranking prompt's right-hand column."
  (let ((what (cdr (assoc candidate org-semantic--rankings))))
    (and what (concat "  " what))))

(defun org-semantic--read-ranking ()
  "Ask which ranking to use, offering both and saying what each is.

The setting is the default, and is not put into the minibuffer as
input.  A completion UI filters the candidates by the input, so
\"semantic\" as input offers one of the two rankings, and never the
one being reached for.

The annotation rides the table, and not
`completion-extra-properties'.  A table carries its metadata
wherever it is passed; the variable is global state that a
front-end can rebind."
  (completing-read
   "Rank by: "
   (lambda (string predicate action)
     (if (eq action 'metadata)
         '(metadata (annotation-function . org-semantic--ranking-annotation))
       (complete-with-action action org-semantic--rankings string predicate)))
   nil t nil nil
   ;; Never "ask" itself: it is not one of the rankings, so offering it as
   ;; the default would put a non-candidate in front of the one key that
   ;; takes it.
   (if (equal org-semantic-results-ranking "ask") "semantic" org-semantic-results-ranking)))

;;;###autoload
(defun org-semantic-find (query &optional arg mode)
  "Search the current buffer's vault for QUERY and show what comes back.

MODE is the ranking to use, and defaults to
`org-semantic-results-ranking'.  Called interactively it is whatever
the query prompt was left on: the prompt names the ranking, and
\\`M-s' and \\`M-l' change it while the query is being typed.

One ranking is used, never both.  `semantic' finds notes by
meaning and `lexical' by word, and the two are ranked separately,
because a score from one has no meaning beside a score from the
other.  In the results buffer, `M-s' and `M-l' ask again with the
other ranking.

With one prefix ARG, ask which ranking.  With two, ask about the
length of the list as well.  See `org-semantic--find-prompts'.

A query may carry predicates, which the server reads out of it:
`tag:x', `dir:x', `todo:x' and `lang:x', each of which both
rankings honour, and each of which negates with a leading `-'.
The rest of the query is free text."
  (interactive
   (let* ((asks (org-semantic--find-prompts current-prefix-arg))
          (start (if (or (car asks) (equal org-semantic-results-ranking "ask"))
                     (org-semantic--read-ranking)
                   org-semantic-results-ranking))
          (asked (org-semantic-results--read-query start)))
     (list (car asked) current-prefix-arg (cdr asked))))
  (let* ((asks (org-semantic--find-prompts arg))
         (vault (org-semantic-vault-or-error))
         (mode (or mode org-semantic-results-ranking))
         (k (and (cdr asks) (read-number "Notes at most: " 8)))
         (per-file (and (cdr asks) (read-number "Passages per note at most: " 3)))
         (buffer (org-semantic-results--buffer vault)))
    (with-current-buffer buffer
      (setq org-semantic-results--vault vault
            org-semantic-results--query query
            org-semantic-results--mode mode
            org-semantic-results--k k
            org-semantic-results--per-file per-file)
      (org-semantic-results--search))
    (pop-to-buffer buffer org-semantic-results-display-action)))

;;;###autoload
(defun org-semantic-find-at-point (&optional arg)
  "Search for the region, or the thing at point.  ARG is as in `org-semantic-find'."
  (interactive "P")
  (let* ((thing (if (use-region-p)
                    (buffer-substring-no-properties (region-beginning) (region-end))
                  (thing-at-point 'symbol t)))
         ;; A suggestion, so it goes in as the default and not as text
         ;; already typed: RET takes it, `M-n' fetches it for editing, and
         ;; anything else replaces it.  `read-string' says that
         ;; INITIAL-INPUT "has been superseded by DEFAULT-VALUE and should
         ;; normally be nil in new code".
         (start (if (equal org-semantic-results-ranking "ask")
                    (org-semantic--read-ranking)
                  org-semantic-results-ranking))
         (asked (org-semantic-results--read-query start nil thing)))
    (org-semantic-find (car asked) arg (cdr asked))))

(defun org-semantic-results--read-query (mode &optional initial default)
  "Read a query to rank by MODE, and return it as (QUERY . MODE).

The prompt names the ranking, because which index answers is half
of what a query means.  \\`M-s' and \\`M-l' change it while the
query is being typed, and carry the text across, so choosing the
ranking never costs the query.

INITIAL is text to edit; DEFAULT is offered instead, for
`org-semantic-find-at-point', where the thing at point is a
suggestion rather than something the user typed."
  (let ((switch nil)
        (map (make-sparse-keymap)))
    (set-keymap-parent map minibuffer-local-map)
    (define-key map (kbd "M-s")
                (lambda () (interactive) (setq switch "semantic") (exit-minibuffer)))
    (define-key map (kbd "M-l")
                (lambda () (interactive) (setq switch "lexical") (exit-minibuffer)))
    (let ((text (read-from-minibuffer
                 ;; The ranking's own name, as the header, the setting and
                 ;; the keys use it.  "Search notes semantically for" gives
                 ;; each ranking a second word, and one word per thing is
                 ;; worth more than the grammar.
                 (format "%s search for%s: "
                         (capitalize mode)
                         (if default (format " (default %s)" default) ""))
                 initial map nil 'org-semantic-search-history default)))
      (if switch
          ;; What was typed becomes the text to go on editing: the ranking
          ;; changed, the query did not.
          (org-semantic-results--read-query switch text default)
        (cons text mode)))))

(defvar org-semantic-search-history nil
  "Queries searched for, most recent first.

`M-p' and `M-n' walk it in the minibuffer, and `savehist-mode'
carries it between sessions without any configuration:
`savehist-minibuffer-hook' records whichever history variable each
minibuffer used, so nobody has to add this one to
`savehist-additional-variables'.

A `defvar' and not a `defcustom': a history is data the package
accumulates, not a setting anyone chooses.")

(defun org-semantic-results--buffer (vault)
  "The results buffer for VAULT, made if there is not one yet."
  (let ((buffer (get-buffer-create
                 (format "*org-semantic: %s*"
                         (file-name-nondirectory vault)))))
    (with-current-buffer buffer
      (unless (derived-mode-p 'org-semantic-results-mode)
        (org-semantic-results-mode))
      (setq default-directory (file-name-as-directory vault))
      buffer)))

(defun org-semantic-results--search (&optional mode)
  "Ask again for what this buffer is set to want.

MODE asks in that ranking once, and does not change what the buffer
wants.  It is for the offer that answers a refusal, where a vault
without its semantic index can still answer by word.  Taking that
offer must not redefine every later query in the buffer.

The header shows `org-semantic-results--asked-mode', which is the
ranking that produced what is on screen, and not what the buffer
will ask next.  Binding the buffer's own mode around the request
does not work: the reply is rendered after the binding is gone, so
the header would say \"semantic\" over results found by word."
  (unless org-semantic-results--driver
    (let ((buffer (current-buffer)))
      (setq org-semantic-results--driver
            (org-semantic-ui-driver-create
             :on-reply
             (lambda (reply)
               (when (buffer-live-p buffer)
                 (with-current-buffer buffer
                   (org-semantic-results--render reply))))
             :on-error
             (lambda (error-object)
               (when (buffer-live-p buffer)
                 (with-current-buffer buffer
                   (org-semantic-results--render-error error-object))))
             :on-waiting
             (lambda (_vault)
               (when (buffer-live-p buffer)
                 (with-current-buffer buffer
                   (org-semantic-results--render-waiting))))))))
  (setq org-semantic-results--asked-mode (or mode org-semantic-results--mode))
  ;; A new search is a new question.  The latch stops one reply being
  ;; asked about twice.  Left uncleared, it stops every later search in
  ;; the buffer from offering anything.
  (setq org-semantic-results--latched nil)
  (setq org-semantic-results--started (float-time))
  (setq mode-line-process " [searching]")
  (force-mode-line-update)
  ;; Only when there is nothing to look at yet.  A buffer that already
  ;; shows hits keeps them until the new ones arrive.
  (when (= (buffer-size) 0)
    (let ((inhibit-read-only t))
      (org-semantic-results--insert-header nil nil)
      (insert (propertize "  Searching...\n"
                          'face 'org-semantic-results-location
                          'read-only t))))
  (org-semantic-ui-ask org-semantic-results--driver
                       (org-semantic-results--params)))

(defun org-semantic-results--joined ()
  "How this buffer joins the terms of a word query, `and' or `or'."
  (or org-semantic-results--connector org-semantic-results-connector))

(defun org-semantic-results--params ()
  "What this buffer wants, as parameters for a search."
  (list :query (or org-semantic-results--query "")
        :vault org-semantic-results--vault
        :k org-semantic-results--k
        :per-file org-semantic-results--per-file
        :merge-by-section org-semantic-results--merge
        :mode org-semantic-results--asked-mode
        :model (or org-semantic-results--model org-semantic-model)
        ;; `any' is the server's spelling; this is the one place the two meet.
        :any (eq (org-semantic-results--joined) 'or)
        ;; Absent when waived, and the driver takes that literally, which
        ;; is how an index under a drifted policy is searched.
        :config (and org-semantic-results--policy org-semantic-config)))


;;;; Grouping, and the lines a drawing owns

(defun org-semantic-results--group (hits)
  "Arrange HITS as ((FILE . ((LINE . HITS) ...)) ...), in the order drawn.

Grouped on the heading's line, and never on its text.  The server
groups on the text, so two sections of one note whose outline paths
spell the same arrive as one group with two heading lines.  A
note's groups do not arrive together either, because they are
ranked against every other note's, so a note is gathered here and
not assumed to be contiguous.

Order is first appearance, which is the server's ranking: the best
note first, and within it the best section."
  (let ((files nil))
    (dolist (hit hits)
      (let* ((file (org-semantic-hit-file hit))
             (line (org-semantic-hit-line hit))
             (entry (assoc file files)))
        (unless entry
          (setq entry (list file))
          (setq files (append files (list entry))))
        (let ((section (assoc line (cdr entry))))
          (unless section
            (setq section (list line))
            (setcdr entry (append (cdr entry) (list section))))
          (setcdr section (append (cdr section) (list hit))))))
    ;; Within a section, in the order the note has them.  They arrive
    ;; ranked, which is right for choosing which sections to show and
    ;; wrong for reading one: the passages of a section are pieces of one
    ;; text, and tied scores would otherwise decide the order.  It also
    ;; settles the overlap, because the earlier passage then owns the
    ;; shared paragraph and the repeat is what gets dimmed.
    (dolist (file files)
      (dolist (section (cdr file))
        (setcdr section
                (sort (cdr section)
                      (lambda (a b)
                        (< (or (org-semantic-hit-start-line a) 0)
                           (or (org-semantic-hit-start-line b) 0)))))))
    files))

(defun org-semantic-results--claim (file start end claimed)
  "Say which of lines START to END of FILE this drawing owns.

CLAIMED is a hash table of every line already drawn in full
further up, and is added to.  Returns a list as long as the
passage, one element per line, non-nil where this drawing is the
first to show that line.

The same line can be drawn twice.  Consecutive passages of one
section begin with the last paragraph of the passage before, and a
long paragraph is cut into pieces that all name the whole
paragraph, so several passages can carry identical text.  The first
drawing owns the line and the rest are dimmed."
  (let ((owned nil))
    (dotimes (offset (1+ (- end start)))
      (let ((key (cons file (+ start offset))))
        (push (not (gethash key claimed)) owned)
        (puthash key t claimed)))
    (nreverse owned)))


;;;; Drawing

(defun org-semantic-results--render (reply)
  "Draw REPLY over the whole buffer."
  (let* ((hits (org-semantic-hits reply))
         (inhibit-read-only t)
         (claimed (make-hash-table :test 'equal))
         (elapsed (and org-semantic-results--started
                       (- (float-time) org-semantic-results--started)))
         (drawn nil)
         (dropped 0))
    (setq org-semantic-results--indexing
          (org-semantic-true-p (plist-get reply :indexing)))
    (erase-buffer)
    (org-semantic-results--insert-header hits elapsed t)
    (dolist (file (org-semantic-results--group hits))
      (let ((blocks nil))
        ;; Drawn into a string first: the note's own line says how many
        ;; passages it contributes, and that is not known until the claim
        ;; map has answered.
        (dolist (section (cdr file))
          (let ((first t))
            (dolist (hit (cdr section))
              (let ((block (org-semantic-results--block hit first claimed)))
                (if (null block)
                    (setq dropped (1+ dropped))
                  (setq first nil)
                  (push hit drawn)
                  (push block blocks))))))
        (when blocks
          (setq blocks (nreverse blocks))
          (org-semantic-results--insert-file
           (car file) (org-semantic-results--note-name file) (length blocks))
          (dolist (block blocks) (insert block)))))
    (setq org-semantic-results--hits (nreverse drawn))
    (when (> dropped 0)
      (insert (propertize
               (format "\n%d passage%s left out: every line of it was shown above.\n"
                       dropped (if (= dropped 1) "" "s"))
               'face 'org-semantic-results-elision
               'read-only t 'front-sticky t)))
    (setq mode-line-process (and org-semantic-results--indexing " [indexing]"))
    (force-mode-line-update)
    (goto-char (point-min))
    (org-semantic-results--first-item)))

(defun org-semantic-results--insert-header (hits elapsed &optional counts)
  "Insert the lines at the top of the buffer, describing HITS and ELAPSED.

COUNTS asks for the third line, which says how much came back.  It
is left off when nothing came back and the reason follows instead:
\"0 notes, 0 passages\" above an explanation of why there is no
index reads as an answer."
  (let* ((notes (length (org-semantic-results--group hits)))
         (facts (delq nil
                      (list (format "k=%s notes" (or org-semantic-results--k 8))
                            (format "%s passages per note"
                                    (or org-semantic-results--per-file 3))
                            (and org-semantic-results--merge "merged by section")
                            (and (equal org-semantic-results--asked-mode "lexical")
                                 (eq (org-semantic-results--joined) 'or)
                                 "any term (OR)")))))
    (insert (propertize
             (format "org-semantic: %s search%s\n"
                     (or org-semantic-results--asked-mode
                         org-semantic-results--mode)
                     ;; No dangling "for" when there is nothing to name.
                     (if (and org-semantic-results--query
                              (not (string-empty-p org-semantic-results--query)))
                         (format " for %S" org-semantic-results--query)
                       ""))
             'face 'org-semantic-results-header
             'org-semantic-header t 'read-only t 'front-sticky t))
    (insert (propertize
             (format "%s  ·  %s\n"
                     (abbreviate-file-name (or org-semantic-results--vault ""))
                     (mapconcat #'identity facts "  ·  "))
             'face 'org-semantic-results-location
             'org-semantic-header t 'read-only t))
    (if counts
        (insert (propertize
                 (format "%d note%s, %d passage%s%s%s\n\n"
                         notes (if (= notes 1) "" "s")
                         (length hits) (if (= (length hits) 1) "" "s")
                         (if elapsed (format " in %.2f s" elapsed) "")
                         (if org-semantic-results--indexing
                             "  ·  indexing: this list is one version behind"
                           ""))
                 'face 'org-semantic-results-location
                 'org-semantic-header t 'read-only t))
      (insert (propertize "\n" 'org-semantic-header t 'read-only t)))))

(defun org-semantic-results--note-name (group)
  "What to call the note GROUP is the hits of.

Its `#+title:', or its filename without the extension.  The server
makes that substitution, so this is the title it sends, and the
fallback here is for a reply that carries none.

The path is not repeated here.  The address line below names the
directory and the file as separate links, and that is also what
tells two notes apart when they share one title."
  (let ((title (org-semantic-hit-title (cadr (cadr group)))))
    (if (and title (not (string-empty-p title)))
        title
      (file-name-base (car group)))))

(defun org-semantic-results--insert-file (file name passages)
  "Insert the line naming FILE as NAME, which contributed PASSAGES of them."
  (insert (propertize
           (format "%s  ·  %d passage%s\n"
                   name
                   passages (if (= passages 1) "" "s"))
           'face 'org-semantic-results-file
           'org-semantic-file file
           'org-semantic-group 'file
           'read-only t)))

(defvar org-semantic-results--fontifier nil
  "A hidden `org-mode' buffer, kept because starting one is not free.")

(defun org-semantic-results--fontifier ()
  "The buffer passages are fontified in, made if there is not one yet."
  (or (and (buffer-live-p org-semantic-results--fontifier)
           org-semantic-results--fontifier)
      (setq org-semantic-results--fontifier
            (with-current-buffer (generate-new-buffer " *org-semantic-fontify*" t)
              ;; `delay-mode-hooks', so a user's `org-mode-hook' does not
              ;; run in a buffer that holds six lines of text.
              (delay-mode-hooks (org-mode))
              (current-buffer)))))

(defun org-semantic-results--faces-only (s)
  "S with every text property but `face' removed.

The others are what would break this buffer: org's `keymap' would
answer keys meant for the list, `invisible' would hide text the line
numbers still count, and `display' would put something else where
the note's own characters are."
  (let ((out (copy-sequence s)) (i 0) (n (length s)))
    (set-text-properties 0 n nil out)
    (while (< i n)
      (let ((next (or (next-single-property-change i 'face s) n))
            (face (get-text-property i 'face s)))
        (when face (put-text-property i next 'face face out))
        (setq i next)))
    out))

(defun org-semantic-results--hide-link-syntax (s)
  "Hide the bracket parts of each link in S, leaving its description.

This is what `org-link-descriptive' asks for and what
`org-toggle-link-display' toggles.  It is done here and not
inherited, because org 9.8 hides links through `org-fold-core',
which must be initialised in the buffer that hides them.  A list of
passages is not an org buffer, so it uses its own invisibility spec.

Org makes `org-link-descriptive' buffer-local in every org buffer,
so toggling it in a note changes that note alone.  What this reads
is the global value.

A link that spans two lines is left alone.  Each line drawn is a
line of the note, and hiding part of a link across a newline would
leave a line whose text nobody can point at."
  (when (and (boundp 'org-link-bracket-re) org-link-bracket-re)
    (let ((from 0))
      (while (string-match org-link-bracket-re s from)
        (let ((mb (match-beginning 0))
              (me (match-end 0))
              (desc-b (match-beginning 2))
              (desc-e (match-end 2)))
          (unless (string-search "\n" (substring s mb me))
            (if desc-b
                (progn
                  ;; `[[target][' before the description, `]]' after it.
                  (put-text-property mb desc-b 'invisible 'org-semantic-results-link s)
                  (put-text-property desc-e me 'invisible 'org-semantic-results-link s))
              ;; No description: the target is what is shown, so only the
              ;; brackets go.
              (put-text-property mb (+ mb 2) 'invisible 'org-semantic-results-link s)
              (put-text-property (- me 2) me 'invisible 'org-semantic-results-link s)))
          (setq from me)))))
  s)

(defun org-semantic-results--fontified (text)
  "TEXT with org's faces on it, or TEXT itself if that cannot be done."
  (if (or (not org-semantic-results-fontify)
          (string-empty-p text)
          (not (require 'org nil t)))
      text
    ;; Read here, and not inside the fontifier.  `org-mode' makes
    ;; `org-link-descriptive' buffer-local in every buffer it starts
    ;; (org.el 5181), so asking there gets that buffer's own answer, which
    ;; is always t.
    (let ((descriptive (bound-and-true-p org-link-descriptive)))
      (condition-case nil
          (with-current-buffer (org-semantic-results--fontifier)
            (let ((inhibit-read-only t))
              (erase-buffer)
              (insert text)
              (font-lock-ensure)
              (let ((out (org-semantic-results--faces-only (buffer-string))))
                (if descriptive
                    (org-semantic-results--hide-link-syntax out)
                  out))))
        ;; A fontifier that fails must not cost the search its results.
        (error text)))))

(defun org-semantic-results--block (hit first claimed)
  "Draw HIT as a string, or nil if every line of it was already shown.

FIRST says this is the leading passage of its section, which
carries the outline path.  The passages after it name their lines
instead.  CLAIMED is the claim map, and is added to."
  (let* ((file (org-semantic-hit-file hit))
         (start (org-semantic-hit-start-line hit))
         (end (org-semantic-hit-end-line hit))
         (text (org-semantic-results--fontified (or (org-semantic-hit-text hit) "")))
         (lines (and (not (string-empty-p text)) (split-string text "\n")))
         (stale (or (null lines)
                    (null start) (null end)
                    (/= (length lines) (1+ (- end start)))))
         (owned (unless stale
                  (org-semantic-results--claim file start end claimed)))
         (item (org-semantic-results--item-create
                :hit hit :file file
                :line (if stale (org-semantic-hit-line hit) start))))
    ;; Nothing left to show: every line of this passage was drawn in full
    ;; further up, so drawing it again would only repeat it.
    (unless (and owned (not (cl-some #'identity owned)))
      (with-temp-buffer
        ;; The text carries `read-only' properties, which would otherwise
        ;; refuse the next insertion beside them.
        (let ((inhibit-read-only t))
          (org-semantic-results--insert-block-head hit item first)
          (if stale
              (org-semantic-results--insert-stale item)
            (org-semantic-results--insert-lines item lines start owned))
          (insert "\n"))
        (buffer-string)))))

(defun org-semantic-results--sections (hit)
  "The outline path of HIT below its note, or nil if it is the note itself.

The stored heading begins with the note's `#+title:', which the
address already names by its file, so it is dropped.  On one vault
it repeated the filename on 85 hits in 88."
  (let ((parts (split-string (or (plist-get hit :heading) "") " > " t)))
    (when (cdr parts)
      (string-join (cdr parts) " > "))))

(defun org-semantic-results--link (text target props &optional line help)
  "Propertize TEXT as a link to TARGET, over PROPS.

LINE is the line it goes to, where that means anything.  HELP is
the `help-echo'.  TARGET is a symbol and not a function, so a test
can read back off the text what a piece of this buffer points at."
  (apply #'propertize text
         'face 'org-semantic-results-link
         'org-semantic-target target
         'help-echo (or help "mouse-2: go here")
         ;; The mouse properties belong here and nowhere else.  On every
         ;; line of a hit they make the whole result one large button: the
         ;; passage lights up under the pointer, a click jumps instead of
         ;; placing point, and the text cannot be selected.
         'mouse-face 'highlight
         'keymap org-semantic-results-passage-map
         'follow-link t
         (append (and line (list 'org-semantic-line line)) props)))

(defun org-semantic-results--plain (text props line)
  "Propertize TEXT as part of a head that is not a link, over PROPS.

LINE is the passage's own, so that point between the links, on the
score or on a separator, still goes to the passage.

Each piece of the head is propertized on its own and the results
are concatenated.  `propertize' overrides what a string already
carries, so one pass at the end would give every link the passage's
line."
  (apply #'propertize text 'org-semantic-line line props))

(defun org-semantic-results--insert-block-head (hit item first)
  "Insert the line above HIT's passage, for ITEM.
FIRST is as in `org-semantic-results--block'.

The address is four links, not one.  It names a directory, a note,
a section and a line, and each goes to the thing it names: the
directory through `org-semantic-results-reveal-function', the note
at its top, the section at its heading, and the line at the
passage.

Only the leading passage of a section carries the address.  The
passages after it name their line alone, because the path is
already above them."
  (let* ((score (org-semantic-ui-score hit))
         (props (list 'org-semantic-item item
                      'org-semantic-hit hit
                      'org-semantic-file (org-semantic-results--item-file item)
                      ;; Decoration, exactly as on a passage line, and for the
                      ;; same reason.  An address too long for the window
                      ;; continues under the path rather than at column 0 --
                      ;; which is further left than anything else in the
                      ;; buffer, so it reads as a broken line rather than as a
                      ;; wrapped one, and does it right beside passage lines
                      ;; that wrap correctly.  Deep breadcrumbs and a long tag
                      ;; string are what reach it, which is why no short
                      ;; fixture ever showed it.
                      'wrap-prefix (make-string (+ 4 (length score)) ?\s)
                      'read-only t))
         (start (org-semantic-results--item-line item))
         (from (org-semantic-hit-start-line hit))
         (to (org-semantic-hit-end-line hit))
         (path (or (org-semantic-hit-path hit) ""))
         (directory (directory-file-name (or (file-name-directory path) "")))
         (sections (org-semantic-results--sections hit))
         (annotation (and first (org-semantic-ui-annotate hit)))
         ;; No `help-echo' here: nothing outside a link is clickable now, so
         ;; promising a mouse-2 would be promising something that does not
         ;; happen.
         (plain (lambda (text face)
                  (org-semantic-results--plain
                   (propertize text 'face face) props start)))
         (sep (lambda (s) (funcall plain s 'org-semantic-results-location))))
    (insert
     (concat
      (funcall plain "  " 'default)
      (funcall plain score 'org-semantic-results-score)
      (funcall plain "  " 'default)
      (when first
        (concat
         ;; A note at the vault root has no directory to show, and would
         ;; otherwise be introduced by a bare separator.
         (unless (string-empty-p directory)
           (concat (org-semantic-results--link
                    directory 'directory props nil
                    "mouse-2: show this directory")
                   (funcall sep " / ")))
         (org-semantic-results--link
          (file-name-nondirectory path) 'file props 1
          "mouse-2: open this note")
         (when sections
           (concat (funcall sep " > ")
                   (org-semantic-results--link
                    sections 'heading props (org-semantic-hit-line hit)
                    "mouse-2: go to this section")))
         (funcall sep " > ")))
      ;; A range, with both ends reachable.  A passage is lines FROM to TO,
      ;; and either end is a place to go: the top to read it, the bottom to
      ;; continue past it.
      ;;
      ;; Only when the span is real.  `item-line' is the heading's line for
      ;; a passage the note has outgrown, so comparing the two keeps a
      ;; stale block from offering a range it cannot honour.  A one-line
      ;; passage says "line".
      (if (and from to (eql start from) (> to from))
          (concat (funcall plain "lines " 'default)
                  (org-semantic-results--link
                   (number-to-string from) 'line props from
                   "mouse-2: go to where this passage starts")
                  (funcall sep "–")
                  (org-semantic-results--link
                   (number-to-string to) 'line props to
                   "mouse-2: go to where this passage ends"))
        (concat (funcall plain "line " 'default)
                (org-semantic-results--link
                 (number-to-string start) 'line props start
                 "mouse-2: go to this passage")))
      ;; One space, as org separates a headline from its tags.  The middot
      ;; in the header lines holds apart unrelated facts; tags follow what
      ;; they belong to.
      (if annotation
          (concat (funcall plain " " 'default)
                  (funcall plain annotation 'org-semantic-results-annotation))
        "")
      (funcall plain "\n" 'default)))))

(defun org-semantic-results--insert-lines (item lines start owned)
  "Insert LINES for ITEM as the note's own lines, the first being START.

OWNED says, per line, whether this drawing is the first to show it.
Everything drawn goes in the gutter.  The passage is inserted
exactly as the note has it, which is what makes each line
addressable."
  (let* ((width (length (number-to-string (+ start (length lines)))))
         (limit org-semantic-results-passage-lines)
         (shown 0)
         (number start))
    (dolist (line lines)
      (let* ((mine (pop owned))
             (gutter (propertize
                      (if org-semantic-results-line-numbers
                          (format (format "  %%%ds  " width) number)
                        "    ")
                      'face 'org-semantic-results-gutter
                      ;; The shape both ways of writing back to a note
                      ;; want: a protected run at the start of the line
                      ;; whose end is where the note's own text begins.
                      'occur-prefix t
                      'front-sticky t 'rear-nonsticky t
                      'read-only t))
             (props (list 'org-semantic-item item
                          'org-semantic-hit (org-semantic-results--item-hit item)
                          'org-semantic-file (org-semantic-results--item-file item)
                          'org-semantic-line number
                          'org-semantic-primary (and mine t)
                          ;; Decoration, so it goes here and not into the
                          ;; text: a wrapped line continues under the
                          ;; gutter rather than under the note's own
                          ;; first column.
                          'wrap-prefix (make-string (length gutter) ?\s))))
        (when (and limit (>= shown limit))
          (setq props (append (list 'invisible 'org-semantic-results
                                    'org-semantic-elided t)
                              props)))
        ;; The dimming goes on the note's own text, and not over the
        ;; gutter, which keeps its own face.  Over the whole line it
        ;; overrides the gutter's face, and the left edge then comes out
        ;; ragged.
        (insert (apply #'propertize
                       (concat gutter
                               (if mine
                                   line
                                 (propertize
                                  line 'face 'org-semantic-results-duplicate))
                               "\n")
                       props)))
      (setq number (1+ number)
            shown (1+ shown)))
    (when (and limit (> (length lines) limit))
      (setf (org-semantic-results--item-elided item) (- (length lines) limit))
      ;; Indented to the gutter, not to a constant.  With line numbers on,
      ;; the gutter is wider than four columns.
      (org-semantic-results--insert-elision
       item (make-string (+ 4 (if org-semantic-results-line-numbers width 0)) ?\s)))))

(defun org-semantic-results--insert-elision (item indent)
  "Insert the marker standing in for the lines ITEM folded away.
INDENT is the gutter it lines up with."
  (insert (propertize
           (format "%s⋯ %d line%s\n"
                   indent
                   (org-semantic-results--item-elided item)
                   (if (= (org-semantic-results--item-elided item) 1) "" "s"))
           'face 'org-semantic-results-elision
           'org-semantic-item item
           'org-semantic-elision t
           'help-echo "TAB: show the rest of this passage"
           'read-only t)))

(defun org-semantic-results--insert-stale (item)
  "Insert, for ITEM, what stands in for a passage the note outgrew.

The server sends an empty passage when the note is shorter than
the span the index recorded.  An empty string against a span of
several lines is therefore this, and not a blank line."
  (insert (propertize
           (concat "    (this passage could not be read: the note has changed"
                   " since it was indexed)\n")
           'face 'org-semantic-results-stale
           'org-semantic-item item
           ;; No `org-semantic-line': nothing may offer to go where it has
           ;; just said it cannot read.
           'read-only t)))


;;;; Errors

(defconst org-semantic-results--latching '("config-drift" "model-missing")
  "The failures asked about once per search rather than once per reply.

Said in full the first time, and kept to a line after that.  Both
describe a state of the vault and not of the request: a policy that
has drifted stays drifted, and a model that is not downloaded stays
so, so a second reply to the same search cannot answer differently.
A mistyped model, or a vault that has gone, is about the request
and is not latched.

Per search.  `org-semantic-results--search' clears the latch,
because a new search is a new question.")

(cl-defun org-semantic-results--render-error (error-object)
  "Draw what ERROR-OBJECT says, and what can be done about it."
  (let* ((inhibit-read-only t)
         (data (plist-get error-object :data))
         (remedy (org-semantic-ui-remedy error-object org-semantic-results--mode))
         (kind (org-semantic-ui-remedy-kind remedy))
         (latching (member kind org-semantic-results--latching)))
    ;; A refusal for the model this buffer is fetching is the wait, and
    ;; arrives as an error because a search cannot be answered during a
    ;; download.  `org-semantic-results--fetching' is tested first: without
    ;; it, a `model-missing' carrying no model compares nil against nil,
    ;; and every such refusal is drawn as a wait for a download nobody
    ;; started.
    (when (and org-semantic-results--fetching
               (equal kind "model-missing")
               (equal (plist-get data :model) (car org-semantic-results--fetching)))
      (org-semantic-results--waiting (car org-semantic-results--fetching)
                                     (cdr org-semantic-results--fetching))
      (setq mode-line-process " [downloading]")
      (force-mode-line-update)
      (cl-return-from org-semantic-results--render-error))
    (setq mode-line-process nil)
    (force-mode-line-update)
    (if (and latching (member kind org-semantic-results--latched))
        (save-excursion
          (goto-char (point-max))
          (insert (propertize (format "\n%s\n"
                                      (org-semantic-ui-remedy-message remedy))
                              'face 'org-semantic-results-stale 'read-only t)))
      (when latching (push kind org-semantic-results--latched))
      (erase-buffer)
      (org-semantic-results--insert-header nil nil)
      (insert (propertize (format "  %s\n"
                                  (org-semantic-ui-remedy-message remedy))
                          'face 'org-semantic-results-stale 'read-only t))
      (goto-char (point-min))
      (org-semantic-results--ask remedy error-object))))

(defun org-semantic-results--ask (remedy error-object)
  "Ask in the minibuffer what to do about REMEDY, which ERROR-OBJECT carries.

Nothing is drawn in the buffer for this.  The buffer states the
problem, and the question is asked once.

An active minibuffer is left alone.  The reply arrives at any time,
and a prompt raised over another prompt eats the keystrokes meant
for it.  The sentence in the buffer is then the whole answer, and
`org-semantic-results-reindex' makes the same call the offer would
have made.

`quit' is caught because this runs from a timer.  `jsonrpc.el'
dispatches each message from a timer of its own, so an escaping
\\`C-g' would show \"Error running timer\"."
  (when-let* ((offers (cl-remove-if-not #'org-semantic-ui-offer-key
                                        (org-semantic-ui-remedy-offers remedy))))
    (unless (active-minibuffer-window)
      (let* ((keys (append (mapcar #'org-semantic-ui-offer-key offers) (list ?q)))
             ;; Nil, so six lines of menu do not land in `*Messages*'.  On
             ;; Emacs 29 and later, `read-char-choice' reads through a real
             ;; minibuffer and every prompt it draws is logged.
             (message-log-max nil)
             (choice (condition-case nil
                         (read-char-choice
                          (org-semantic-results--ask-prompt remedy offers) keys)
                       (quit ?q))))
        ;; The minibuffer exits on the answer, but its last line stays in
        ;; the echo area, where "Choice: l" reads as a prompt still
        ;; waiting.
        (message nil)
        (unless (eq choice ?q)
          (funcall (org-semantic-results--offer-action
                    (cdr (cl-find-if (lambda (o)
                                       (eq (org-semantic-ui-offer-key o) choice))
                                     offers))
                    error-object)
                   nil))))))

(defun org-semantic-results--ask-prompt (remedy offers)
  "The question to ask about REMEDY, listing OFFERS and what each does.

Laid out over several lines, which the echo area grows to fit, so
that each offer can say what it costs.  Indexing takes minutes and
a word search takes seconds."
  (concat
   (format "%s\n\n" (org-semantic-ui-remedy-message remedy))
   (mapconcat
    (lambda (o)
      (format "  [%c] %s — %s\n"
              (org-semantic-ui-offer-key o)
              (car o)
              (org-semantic-results--offer-help (cdr o))))
    offers
    "")
   "  [q] leave it\n\nChoice: "))

(defun org-semantic-results--offer-help (action)
  "What ACTION would do, in a few words.

Each says what it costs, because one of these takes minutes and
the rest do not."
  (pcase action
    ('download "fetches the weights and nothing else; minutes")
    ('index "builds the index this search needs, which takes minutes")
    ('index-full "rebuilds from scratch, re-embedding everything")
    ('lexical "needs no embedding model")
    ('choose-model "search one of the models that is built")
    ('waive "search the index as it stands, under the policy it was built with")
    ('show-changed "list the settings that moved")
    (_ "")))

(defun org-semantic-results--offer-action (action error-object)
  "A function doing ACTION, which ERROR-OBJECT asked for.

Takes one ignored argument, so that it can be a button's `action'
as well as something `org-semantic-results--ask' calls."
  (let ((os-buffer (current-buffer))
        (os-data (plist-get error-object :data)))
    (lambda (_button)
      (with-current-buffer os-buffer
        (pcase action
          ;; Searches by word once, and does not change what this buffer is
          ;; set to want.  It answers one refusal, and is not a statement
          ;; about how the user prefers to search, which is
          ;; `org-semantic-results-ranking'.
          ('lexical (org-semantic-results--search "lexical"))
          ('waive
           (setq org-semantic-results--policy nil)
           (org-semantic-results--search))
          ('show-changed
           (message "org-semantic: %s"
                    (mapconcat #'identity
                               (append (plist-get os-data :changed) nil)
                               ", ")))
          ('choose-model
           (let ((known (append (or (plist-get os-data :known)
                                    (plist-get os-data :built))
                                nil)))
             (setq org-semantic-results--model
                   (completing-read "Model: " known nil t))
             (org-semantic-results--search)))
          ('download (org-semantic-results--download (plist-get os-data :model)))
          ((or 'index 'index-full)
           (org-semantic-results--reindex (eq action 'index-full))))))))

(defun org-semantic-results--download (model)
  "Fetch MODEL, then search again.  Nothing is indexed.

One thing at a time.  If the search then finds no index, it says so
itself and asks about building one.

The buffer says it is waiting, and is replaced when the results
arrive.  Keeping the refusal on screen would report that nothing
had been fetched for the length of the fetch.  There is no progress
to show inside it, because fastembed exposes no increments, so the
line gives the size and says that the results will appear."
  (let ((os-buffer (current-buffer)))
    (setq mode-line-process " [downloading]")
    (force-mode-line-update)
    (setq org-semantic-results--fetching (cons model nil))
    (org-semantic-results--waiting model nil)
    (org-semantic-download
     :model model
     :progress (lambda (report)
                 (org-semantic-report-message report)
                 ;; The size arrives in the one report the fetch makes, a
                 ;; moment after the request.  Until then the line stands
                 ;; without it.
                 (when-let* ((bytes (plist-get report :bytes)))
                   (when (buffer-live-p os-buffer)
                     (with-current-buffer os-buffer
                       (setq org-semantic-results--fetching (cons model bytes))
                       (org-semantic-results--waiting model bytes)))))
     :success (lambda (_result)
                (when (buffer-live-p os-buffer)
                  (with-current-buffer os-buffer
                    ;; Cleared first: the search this fires can refuse for
                    ;; another reason, which is a question again.
                    (setq org-semantic-results--fetching nil)
                    (org-semantic-results--search))))
     :failure (lambda (error-object)
                (when (buffer-live-p os-buffer)
                  (with-current-buffer os-buffer
                    (setq org-semantic-results--fetching nil)
                    (org-semantic-results--render-error error-object)))))
    (message "org-semantic: downloading %s..." model)))

(defun org-semantic-results--waiting (model bytes)
  "Say that MODEL is being fetched, and how large it is if BYTES is known."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (org-semantic-results--insert-header nil nil)
    (insert (propertize
             (format "  please wait: the %s model%s is downloading.\n  \
The search results will appear here by themselves when downloading has finished.\n"
                     model
                     (if bytes
                         (format " (%s)" (file-size-human-readable bytes 'si " " "B"))
                       ""))
             'face 'org-semantic-results-location
             'read-only t))
    (goto-char (point-min))))

(defun org-semantic-results--render-waiting ()
  "Say that the search is held until the index finishes.

Under `org-semantic-wait-for-index' the old hits must not be shown,
so they are replaced rather than marked.  The search is sent from
the run's reply, and the results appear here."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (org-semantic-results--insert-header nil nil)
    (insert (propertize
             "  please wait: this vault is being indexed.\n  \
The search results will appear here by themselves when indexing has finished.\n"
             'face 'org-semantic-results-location
             'read-only t))
    (goto-char (point-min)))
  (setq mode-line-process " [waiting for the index]")
  (force-mode-line-update))

(defun org-semantic-results--reindex (full)
  "Index this buffer's vault, then search again.  FULL rebuilds from scratch."
  (let ((os-buffer (current-buffer))
        (os-vault org-semantic-results--vault))
    (setq mode-line-process " [indexing]")
    (force-mode-line-update)
    (org-semantic-index
     :vault os-vault
     :full full
     :progress #'org-semantic-report-message
     :success (lambda (_result)
                (when (buffer-live-p os-buffer)
                  (with-current-buffer os-buffer
                    (org-semantic-results--search))))
     :failure (lambda (error-object)
                (when (buffer-live-p os-buffer)
                  (with-current-buffer os-buffer
                    (org-semantic-results--render-error error-object)))))
    (message "org-semantic: indexing %s..." (abbreviate-file-name os-vault))))


;;;; Getting about

(defun org-semantic-results--item-at-point ()
  "The block point is in, or nil."
  (or (get-text-property (point) 'org-semantic-item)
      (and (> (point) (point-min))
           (get-text-property (1- (point)) 'org-semantic-item))))

(defun org-semantic-results--items ()
  "Where each block begins, in the order they are drawn."
  (let ((pos (point-min))
        (out nil))
    (while (< pos (point-max))
      (when (get-text-property pos 'org-semantic-item)
        (push pos out))
      (setq pos (next-single-property-change
                 pos 'org-semantic-item nil (point-max))))
    (nreverse out)))

(defun org-semantic-results--first-item ()
  "Put point on the first block, if there is one."
  (let ((items (org-semantic-results--items)))
    (when items (goto-char (car items)))))

(defun org-semantic-results--move (n)
  "Go N blocks from the one point is in, and return where.
N may be negative.  Returns nil, moving nothing, if there is no
such block."
  (let* ((items (org-semantic-results--items))
         (here (org-semantic-results--item-at-point))
         (index (and here
                     (cl-position-if
                      (lambda (pos)
                        (eq here (get-text-property pos 'org-semantic-item)))
                      items)))
         (target (cond ((null items) nil)
                       ((null index) (if (< n 0) (1- (length items)) 0))
                       (t (+ index n)))))
    (when (and target (>= target 0) (< target (length items)))
      (goto-char (nth target items))
      (point))))

(defun org-semantic-results--file-at-point ()
  "The note point is in, or nil."
  (or (get-text-property (point) 'org-semantic-file)
      (let ((item (org-semantic-results--item-at-point)))
        (and item (org-semantic-results--item-file item)))))

(defun org-semantic-results--line-at-point ()
  "The line in the note point is on, or the block's own if it is not on one."
  (or (get-text-property (point) 'org-semantic-line)
      (let ((item (org-semantic-results--item-at-point)))
        (and item (org-semantic-results--item-line item)))))

(defun org-semantic-results-reveal-in-dired (directory file)
  "Show DIRECTORY in Dired, with point on FILE.

The default `org-semantic-results-reveal-function'.  `dired-jump'
is preferred because it puts point on the note and not only in the
directory.  It has been in `dired' since Emacs 28, and the fallback
covers a Dired that has been replaced."
  (if (fboundp 'dired-jump)
      (dired-jump nil file)
    (dired directory)))

(defun org-semantic-results--visit (&rest keys)
  "Go where point says, passing KEYS on to `org-semantic-ui-visit'.

A directory is the one target that is not a place in a note, so it
is handled apart.  Every other target differs only in the line it
carries."
  (if (eq (get-text-property (point) 'org-semantic-target) 'directory)
      (let ((file (org-semantic-results--file-at-point)))
        (unless file (user-error "Nothing to show here"))
        (funcall org-semantic-results-reveal-function
                 (file-name-directory file) file)
        nil)
    (org-semantic-results--visit-note keys)))

(defun org-semantic-results--visit-note (keys)
  "Go to the note point names, passing KEYS on to `org-semantic-ui-visit'."
  (let ((file (org-semantic-results--file-at-point))
        (line (org-semantic-results--line-at-point))
        (buffer (current-buffer)))
    (unless file (user-error "Nothing to go to here"))
    (unless line (user-error "That passage could not be read; nowhere to go"))
    (let ((window (apply #'org-semantic-ui-visit file line keys)))
      ;; This is what lets `M-g M-n' continue from this buffer, and what
      ;; `M-0 RET' needs to quit the window.  The note is named by its
      ;; window and not by `current-buffer', which is still this buffer
      ;; when the note was only displayed.
      (next-error-found buffer (and window (window-buffer window)))
      window)))

(defun org-semantic-results-goto ()
  "Go to the line point is on, in its note."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--visit :select t))

(defun org-semantic-results-goto-other-window ()
  "Go to the line point is on, in another window."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--visit :select t :other-window t))

(defun org-semantic-results-display ()
  "Show the line point is on without leaving this buffer."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--visit))

(defun org-semantic-results-mouse-goto (event)
  "Go to what EVENT was clicked on."
  (interactive "e" org-semantic-results-mode)
  (let ((posn (event-end event)))
    (with-current-buffer (window-buffer (posn-window posn))
      (goto-char (posn-point posn))
      (org-semantic-results--visit :select t))))

(defun org-semantic-results-next (&optional n)
  "Go to the Nth next passage, and show it.

At either end it says so in the echo area and does not signal.  The
end of a list is not a mistake, and `user-error' rings the bell for
it."
  (interactive "p" org-semantic-results-mode)
  (let ((n (or n 1)))
    (if (org-semantic-results--move n)
        (org-semantic-results--visit)
      (message (if (< n 0) "No previous passage" "No next passage")))))

(defun org-semantic-results-previous (&optional n)
  "Go to the Nth previous passage, and show it."
  (interactive "p" org-semantic-results-mode)
  (org-semantic-results-next (- (or n 1))))

(defun org-semantic-results-next-note (&optional n)
  "Go to the first passage of the Nth next note, and show it.

Skips the rest of this note, its sections and their passages, where
\\[org-semantic-results-next] would step through them one at a time."
  (interactive "p" org-semantic-results-mode)
  (dotimes (_ (abs (or n 1)))
    (let ((file (org-semantic-results--file-at-point))
          (step (if (< (or n 1) 0) -1 1)))
      (while (and (org-semantic-results--move step)
                  (equal file (org-semantic-results--file-at-point))))))
  (org-semantic-results--visit))

(defun org-semantic-results-previous-note (&optional n)
  "Go to the first passage of the Nth previous note, and show it.

Skips the rest of this note, its sections and their passages, where
\\[org-semantic-results-previous] would step through them one at a time."
  (interactive "p" org-semantic-results-mode)
  (org-semantic-results-next-note (- (or n 1))))

(defun org-semantic-results--next-error (n reset)
  "Go N passages and show the one arrived at.  RESET starts from the first.

The buffer-local `next-error-function', and its contract is that
variable's.  N is how many to move and may be negative.  N of zero
means the passage point is already on, which is what
`next-error-follow-minor-mode' calls, so it must move nothing.  The
target window is selected: `next-error-no-select' wraps the call in
`save-selected-window' and restores the selection itself."
  (when reset
    (goto-char (point-min))
    (org-semantic-results--first-item))
  (unless (zerop n)
    (unless (org-semantic-results--move n)
      (user-error "No %s passage" (if (< n 0) "previous" "further"))))
  ;; The buffer may be showing in a window whose point would otherwise be
  ;; restored from under us.  Occur and xref both do this.
  (let ((window (get-buffer-window (current-buffer))))
    (when window (set-window-point window (point))))
  (org-semantic-results--visit :select t))


;;;; Folding a long passage

(defun org-semantic-results-toggle-passage ()
  "Show or fold away the rest of the passage point is in.

Only a long passage has a rest: one running to more than
`org-semantic-results-passage-lines' lines is cut there, and what is
left over becomes a marker saying how many lines it stands for."
  (interactive nil org-semantic-results-mode)
  (let ((item (org-semantic-results--item-at-point)))
    (unless (and item (org-semantic-results--item-elided item))
      (user-error "Nothing folded away here"))
    (let ((inhibit-read-only t)
          (bounds (org-semantic-results--elided-bounds item))
          (hidden nil))
      (unless bounds (user-error "Nothing folded away here"))
      (setq hidden (get-text-property (car bounds) 'invisible))
      (if hidden
          (remove-text-properties (car bounds) (cdr bounds) '(invisible nil))
        (put-text-property (car bounds) (cdr bounds)
                           'invisible 'org-semantic-results))
      (org-semantic-results--redraw-elision item (not hidden)))))

(defun org-semantic-results--property-run (item property)
  "Where ITEM's run of PROPERTY starts and ends, or nil.
Walked by property change and not by character, because the runs
are whole lines."
  (let ((pos (point-min))
        (start nil)
        (end nil))
    (while (and (< pos (point-max)) (not end))
      (let ((mine (and (get-text-property pos property)
                       (eq item (get-text-property pos 'org-semantic-item)))))
        (cond ((and mine (not start)) (setq start pos))
              ((and start (not mine)) (setq end pos))))
      (setq pos (next-single-property-change pos property nil (point-max))))
    (and start (cons start (or end (point-max))))))

(defun org-semantic-results--elided-bounds (item)
  "Where the folded tail of ITEM starts and ends, or nil."
  (org-semantic-results--property-run item 'org-semantic-elided))

(defun org-semantic-results--redraw-elision (item folded)
  "Rewrite ITEM's fold marker to say whether it is FOLDED."
  (let ((start (car (org-semantic-results--property-run
                     item 'org-semantic-elision))))
    (when start
      (save-excursion
        (goto-char start)
        (let ((line-end (min (point-max) (1+ (line-end-position))))
              (count (org-semantic-results--item-elided item)))
          (delete-region start line-end)
          (insert (propertize
                   (if folded
                       (format "    ⋯ %d line%s\n" count (if (= count 1) "" "s"))
                     "    ⋯ TAB to fold again\n")
                   'face 'org-semantic-results-elision
                   'org-semantic-item item
                   'org-semantic-elision t
                   'help-echo "TAB: fold or show the rest of this passage"
                   'read-only t)))))))


;;;; Asking again, differently

(defun org-semantic-results-revert ()
  "Run the same search again and redraw the org-semantic results buffer.

No note is opened, read or written: this buffer visits no file, so
there is nothing on disk to revert."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--search))

(defun org-semantic-results--revert (&optional _ignore-auto _noconfirm)
  "Run the same search again, for `revert-buffer-function'.
`org-semantic-results-revert' is the command; this is the hook, so
that \\[revert-buffer] and anything calling it programmatically
agree with `g'."
  (org-semantic-results--search))

(defun org-semantic-results-set-query (query &optional mode)
  "Search this vault for QUERY instead, ranked by MODE if it is given.

The one prompt here that keeps INITIAL-INPUT, against
`read-string''s advice, because this command exists to edit the
query.  As a default it would need \\`M-n' before every
refinement."
  (interactive
   (let ((asked (org-semantic-results--read-query
                 org-semantic-results--mode org-semantic-results--query)))
     (list (car asked) (cdr asked)))
   org-semantic-results-mode)
  (setq org-semantic-results--query query)
  (when mode (setq org-semantic-results--mode mode))
  (org-semantic-results--search))

(defun org-semantic-results--rank (mode)
  "Rank by MODE from now on in this buffer, and search again."
  (setq org-semantic-results--mode mode)
  (message "org-semantic: ranking by %s"
           (if (equal mode "lexical") "word (lexical)" "meaning (semantic)"))
  (org-semantic-results--search))

(defun org-semantic-results-rank-by-meaning ()
  "Rank by meaning, over the semantic index, and search again."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--rank "semantic"))

(defun org-semantic-results-rank-by-word ()
  "Rank by word, over the lexical index, and search again.

Two keys, and not one key that toggles.  A toggle cannot be pressed
without first knowing which ranking is in force, where `M-s' and
`M-l' each mean one thing.

This is not the offer a failure makes, which searches by word once
and leaves the buffer's ranking alone."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--rank "lexical"))

(defun org-semantic-results-toggle-connector ()
  "Toggle the logical connector between AND and OR.

Refuses on a semantic search.  An embedding has no terms to join,
and the server ignores the parameter there instead of failing, so a
key that changed it would appear to have done something."
  (interactive nil org-semantic-results-mode)
  (unless (equal org-semantic-results--mode "lexical")
    (user-error "Only a word search has terms to join"))
  (setq org-semantic-results--connector
        (if (eq (org-semantic-results--joined) 'or) 'and 'or))
  (message "org-semantic: joining the terms with %s"
           (upcase (symbol-name org-semantic-results--connector)))
  (org-semantic-results--search))

(defun org-semantic-results--set-k (k)
  "Let K notes appear, and say what that means."
  (setq org-semantic-results--k (max 1 k))
  ;; Both numbers, because k counts notes: a vault kept in three large
  ;; files answers a k of fifty with nine hits.
  (message "org-semantic: k = %d notes (at most %d passages each)"
           org-semantic-results--k (or org-semantic-results--per-file 3))
  (org-semantic-results--search))

(defun org-semantic-results-more-notes ()
  "Let more notes appear."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--set-k (* 2 (or org-semantic-results--k 8))))

(defun org-semantic-results-fewer-notes ()
  "Let fewer notes appear."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--set-k (/ (or org-semantic-results--k 8) 2)))

(defun org-semantic-results-set-notes (n)
  "Let at most N notes appear.

The exact value, where `k' and `K' double and halve it.  Raising it
widens the list.  It is the number the header calls `k'."
  (interactive
   (list (read-number "Notes at most: " (or org-semantic-results--k 8)))
   org-semantic-results-mode)
  (org-semantic-results--set-k n))

(defun org-semantic-results-set-passages (n)
  "Let each note contribute at most N passages.

The exact value, where `+' and `-' double and halve it.  Raise it
for a vault that keeps a year of meetings in one file, where every
hit comes from that file."
  (interactive
   (list (read-number "Passages per note at most: "
                      (or org-semantic-results--per-file 3)))
   org-semantic-results-mode)
  (org-semantic-results--set-per-file n))

(defun org-semantic-results--set-per-file (n)
  "Let each note contribute N passages, and say what that means."
  (setq org-semantic-results--per-file (max 1 n))
  (message "org-semantic: at most %d passages per note (k = %d notes)"
           org-semantic-results--per-file (or org-semantic-results--k 8))
  (org-semantic-results--search))

(defun org-semantic-results-more-passages ()
  "Let each note contribute more passages.

Doubles, as the note cap does.  A step of one is too small: the
advice for a year of meetings in one file is 25 passages.  Use
\\[org-semantic-results-set-passages] for an exact value."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--set-per-file (* 2 (or org-semantic-results--per-file 3))))

(defun org-semantic-results-fewer-passages ()
  "Let each note contribute fewer passages."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--set-per-file (/ (or org-semantic-results--per-file 3) 2)))

(defun org-semantic-results-reindex (&optional arg)
  "Index this buffer's vault and search again.
ARG is as in `org-semantic-reindex': two prefixes rebuild from scratch."
  (interactive "P" org-semantic-results-mode)
  (org-semantic-results--reindex (cdr (org-semantic--reindex-flags arg))))

(provide 'org-semantic-results)
;;; org-semantic-results.el ends here
