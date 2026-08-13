;;; org-semantic-results.el --- A buffer of org-semantic hits -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.2.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; `org-semantic-find' searches the current buffer's vault and shows what
;; came back in a buffer you can walk with `n' and `p' and open with
;; RET -- the shape `grep' and `occur' established, and wired into
;; `next-error' so `M-g M-n' works from anywhere.
;;
;; What makes this not a grep list is that a hit is a *passage*: several
;; lines of prose, shown as the note's own lines rather than shredded one
;; match to a line.  Four things follow from that, and each is easy to
;; undo by accident.
;;
;; THE PASSAGE IS THE NOTE'S LINES, IN ORDER, UNALTERED.  The server
;; sends the note's lines `startLine' to `endLine' joined with newlines,
;; read back when the search was answered.  So the nth line of the text
;; *is* line startLine + n of the note.  That one equality is what lets
;; each line carry its own number, be jumped to on its own, and -- later
;; -- be written back.  It is checked at render time rather than assumed,
;; because the server sends an empty string when the note has been cut
;; shorter than the span, and an empty passage is not a blank line.
;;
;; SO THE CONTENT IS INSERTED VERBATIM.  No filling, no truncation, no
;; indentation inside it, no `display' property over it.  Everything
;; drawn goes in the gutter -- the few columns at the start of each
;; passage line, which is also where the provenance properties live.
;; Break this and the correspondence above becomes unverifiable, which is
;; the thing a writable version would have to stand on.
;;
;; A HIT IS ADDRESSED BY FILE AND LINE, AND NOTHING ELSE.  Not by its
;; `:ID:', which in a note of three hundred meetings is the same for
;; every hit in it, and not by matching its heading text, which was
;; recorded before the user's last edit while the line was not.
;;
;; AND THE SAME LINE CAN BE SHOWN TWICE.  Consecutive passages of one
;; section deliberately overlap by a paragraph, and a paragraph too long
;; to be split anywhere sensible yields several passages naming the whole
;; of it.  So a claim map decides which drawing of a line owns it; the
;; others are dimmed, and a passage with nothing left to claim is dropped
;; rather than repeated.  Harmless while this is read-only, and exactly
;; what an editable version would need to know.

;;; Code:

(require 'cl-lib)
(require 'button)
;; Preloaded, so this costs nothing at runtime -- but without it the
;; compiler does not know the `occur-' names the gutter is shaped for.
(require 'replace)
(require 'org-semantic)
(require 'org-semantic-ui)


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

Off, the gutter is a plain indent.  On, it carries each line's
number in its note, which is what the passage's lines really are.
Either way the gutter is where a line's file and number are
recorded, so this changes what is drawn and nothing else."
  :type 'boolean)

(defcustom org-semantic-results-ranking "semantic"
  "Which ranking `org-semantic-find' asks for unless told otherwise.

\"semantic\" finds notes by meaning and needs the embedding index;
\"lexical\" finds them by word, from an index that builds in
seconds.  \"ask\" settles it per search, which is what a single
prefix argument does anyway -- so set it if you have no usual
answer, rather than pressing \\[universal-argument] every time.

It is the `mode' the server is asked for, spelled `ranking' here so
as not to read as a setting for `org-semantic-results-mode'.

Three values rather than two exclusive minor modes, which was
considered: a minor mode toggles a behaviour, where this is one
choice out of several, and a pair of them would need hand-written
exclusivity and would have a fourth state -- both on -- meaning
nothing."
  :type '(choice (const :tag "By meaning" "semantic")
                 (const :tag "By word" "lexical")
                 (const :tag "Ask each time" "ask")))

(defcustom org-semantic-results-connector 'and
  "How the terms of a word search are joined: `and' or `or'.

`and' answers with the notes carrying every term, `or' with those
carrying any of them.  It is the *default* for a search; `l' in the
results buffer -- `l' for logic -- swaps it for the one in front of
you, as `r' swaps the ranking.

A word search only.  An embedding has no terms to join, so the
semantic ranking ignores this and the key refuses rather than
pretending to have changed something.

Named for the logic and not for the wire, which spells the same
thing as a boolean called `any' -- that is the server's spelling of
a detail, and there is no reason to make a reader of Emacs learn it.
`AND' and `OR' are also writable in the query itself, with
parentheses and `NOT', so this is the default rather than the only
way to say it."
  :type '(choice (const :tag "All terms (AND)" and)
                 (const :tag "Any term (OR)" or)))

(defcustom org-semantic-results-fontify t
  "Whether to show a passage with org's own faces on it.

A passage is org text, and reading it without emphasis, verbatim,
headings and block markers is reading it worse than the note does.
It is fontified by inserting it into a hidden buffer in `org-mode'
and copying the faces back out --- the trick `magit' uses for diffs.

**Only `face' is copied, and the characters are never touched.** The
nth line of a passage is line `startLine' + n of the note, which is
what makes each line addressable and one day writable; a rendering
that replaced or moved text would end that.  So org's `keymap',
`invisible' and `display' properties are left behind, which is also
why a link still shows its brackets.

Costs about 0.8 ms a passage, against 0.1 ms unfontified, and needs
org loaded --- which it will be, since you are searching org notes.
Where it is not, this silently does nothing rather than failing.

One case is wrong and worth knowing, because it is narrower than it
sounds.  A block's opening line is not part of a chunk's text while
its body is, so a passage that begins inside a long block starts at
the first body line and runs to the `#+end_' -- org then sees an end
marker with no beginning, and fontifies the body as prose.  Measured
on a 90-line `#+begin_src': the span came back 6..96 where the marker
is line 5.

Two things that are *not* problems, though they sound like they should
be.  Folding away the tail of a passage hides nothing from org: the
whole passage is fontified first, and `invisible' is added after.  And
emphasis is never cut, because a paragraph too long for one passage
gives every piece of itself the whole paragraph's span -- so what is
shown is always a complete paragraph."
  :type 'boolean)

(defcustom org-semantic-results-display-action
  '(display-buffer-reuse-mode-window)
  "How the results buffer asks to be shown, as a `display-buffer' ACTION.

**A default, never a decision.**  `display-buffer-alist' is a user
option and is consulted *before* the ACTION a caller passes, so
anything set there wins over this without having to know it
exists.  That is why this package does not add an entry to
`display-buffer-alist' itself: a package writing to a user's own
option would sit in front of what the user asked for.

The default expresses a *behaviour* and not a layout: reuse a
window already showing results, so searching again does not open
another one.  Where that window goes, and how large it is, is
taste, and taste is the user's -- with nothing reusable it simply
falls through to how Emacs shows any other buffer.

For a results panel down the right-hand side, put this in your
configuration rather than here:

  (add-to-list \\='display-buffer-alist
               \\='((derived-mode . org-semantic-results-mode)
                 (display-buffer-reuse-mode-window
                  display-buffer-in-direction
                  display-buffer-use-some-window)
                 (direction . right)
                 (window-width . 0.5)))

Order matters there, and not obviously: `display-buffer-use-some-window'
falls back to `get-largest-window' and so all but always succeeds,
which leaves anything after it unreachable.  Put it last."
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

**Not `shadow'.**  It was, which made the head of a block the
dimmest thing in it and its body the brightest -- so a screen of
hits read as one wall of prose with no visible seam between
entries.

A weight and not a colour, because the address beside it is already
a link and a second colour would compete with it.  `bold' rather
than `semi-bold': a font without a semi-bold face falls back to
normal, which would leave the head as flat as the body again on
someone else's machine and say nothing about it.")

(defface org-semantic-results-location '((t :inherit shadow))
  "Face for the separators between the parts of a hit's address.")

(defface org-semantic-results-link '((t :inherit link))
  "Face for the parts of a hit's address that go somewhere.

Inherits `link', so they look like every other link in Emacs --
which is the point: each part of the address goes somewhere
different, and nothing else says so.")

(defface org-semantic-results-annotation '((t :inherit shadow))
  "Face for a hit's TODO keyword, priority and tags.")

(defface org-semantic-results-gutter '((t :inherit shadow))
  "Face for the few columns at the start of a passage line.

`shadow' and **not `line-number', which many themes give a
background**: the gutter is blank unless
`org-semantic-results-line-numbers' is on, so a background painted
it as a grey block four columns wide against nothing else in the
buffer -- decoration marking a margin that carries no information.
`shadow' colours the digits when there are digits and is invisible
when there are not.")

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

What the buffer *wants*, which `r' changes and a one-off search
does not.  See `org-semantic-results--asked-mode' for what is on
screen.")

(defvar-local org-semantic-results--asked-mode nil
  "The ranking that produced what is drawn, or nil before any reply.

Usually the same as `org-semantic-results--mode', and deliberately
not always: the offer that answers a refusal by word searches once
without redefining what the buffer wants, and the header has to say
which ranking the results in front of you came from rather than
which one the next search will use.")

(defvar-local org-semantic-results--k nil
  "How many notes may appear, or nil for the server's default.")

(defvar-local org-semantic-results--per-file nil
  "How many passages one note may contribute, or nil for the default.")

(defvar-local org-semantic-results--merge nil
  "Whether a section divided into several passages answers as one hit.")

(defvar-local org-semantic-results--fetching nil
  "(MODEL . BYTES) while a download this buffer started is running.

What it buys is not asking a question that has already been
answered: a search sent while the fetch is in flight is refused with
`model-missing' all over again, and offering \"try again\" to
someone who is already waiting for the thing is a poll loop by hand.
The buffer says it is waiting instead, which is true and which ends
by itself, because the download's own reply re-runs the search.

Only *ours*.  A fetch started by another Emacs or a shell sends us
nothing when it lands, so there the offer to search again is the
honest one -- we cannot know when to stop waiting.")

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
  "m"         #'org-semantic-results-rank-by-meaning
  "w"         #'org-semantic-results-rank-by-word
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
  ;; Shadows `special-mode-map''s `revert-buffer' for one reason: `C-h m' shows
  ;; the *command's* docstring, so an inherited binding described this as
  ;; replacing the buffer's text with a file's -- which is what it does in a
  ;; buffer visiting a file, and nothing like what it does here.
  "g"         #'org-semantic-results-revert
  "f"         #'next-error-follow-minor-mode
  ;; And under the name `occur' and `grep' give it, which is muscle memory
  ;; worth not breaking for anyone arriving from one of those.
  "C-c C-f"   #'next-error-follow-minor-mode)

(defvar org-semantic-results-passage-map
  (let ((map (make-sparse-keymap)))
    (define-key map [mouse-2] #'org-semantic-results-mouse-goto)
    map)
  "Keymap put on every line a hit was drawn on.")

;; Keys are named by command in the docstring below, so they render as the key
;; itself and cannot go stale when one is rebound -- but **only inside the
;; lists**.  A form 40 characters wide that renders as one character wraps the
;; source at a width the reader never sees, and a paragraph written that way
;; comes out ragged in a way that looks like a mistake, because it is one.  The
;; flowing paragraphs therefore name no keys at all.
;;
;; `\<...>' sits on the line between the summary and the body: it renders as
;; nothing, so that line becomes the blank one that belongs there anyway.
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
  \\[org-semantic-results-rank-by-meaning]  rank by meaning (semantic)
  \\[org-semantic-results-rank-by-word]  rank by word (lexical)
  \\[org-semantic-results-toggle-connector]  join the terms with AND or OR

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
  ;; Wrapped rather than truncated, with `wrap-prefix' carrying the
  ;; continuation under the gutter: a note's paragraph may be one very
  ;; long line, and truncating it would hide the words that matched.  A
  ;; file line is still one *logical* line, which is what the numbers and
  ;; the properties are attached to -- the same arrangement
  ;; `display-line-numbers' makes in an ordinary buffer.
  (setq-local truncate-lines nil)
  (setq-local word-wrap t)
  (setq next-error-function #'org-semantic-results--next-error)
  (setq next-error-last-buffer (current-buffer))
  ;; The symbol alone, **not `(symbol . t)`**: the cons is what asks Emacs to
  ;; draw its own `...' where the hidden text was, which lands at the end of
  ;; the last visible line and says a second time what `⋯ 3 lines' already
  ;; says -- less precisely, and in the wrong place.
  (add-to-invisibility-spec 'org-semantic-results)
  (add-hook 'kill-buffer-hook #'org-semantic-results--abandon nil t))

(defun org-semantic-results--abandon ()
  "Stop caring about the search this buffer asked for."
  (when org-semantic-results--driver
    (org-semantic-ui-driver-abandon org-semantic-results--driver)))


;;;; Asking

(defun org-semantic--find-prompts (arg)
  "Return (RANKING . LIMITS): what a raw prefix ARG asks to be asked.

Ordered by how often it is wanted, which is what a second `C-u'
should mean -- the same rule `org-semantic--reindex-flags' follows:

  plain      neither; the settings decide.
  \\[universal-argument]        the ranking, and only that.  Choosing between
             meaning and word is the common reason to reach for a
             prefix at all, and it used to drag two questions
             about list length along behind it.
  \\[universal-argument] \\[universal-argument]    the ranking and the limits.

A function of its own so a test can hold the mapping: an
interactive spec is not otherwise checkable, and swapping these
two fails nothing and looks like nothing."
  (let ((level (prefix-numeric-value arg)))
    (cond ((null arg) (cons nil nil))
          ((>= level 16) (cons t t))
          (t (cons t nil)))))

(defconst org-semantic--rankings
  '(("semantic" . "by meaning, over the embedding index")
    ("lexical"  . "by word, over the BM25 index"))
  "The two rankings, and what each one is.

**Each names its own index, and that is the point of saying it.**
`semantic' and `lexical' read as two orderings of one result set,
which is the opposite of the truth: they are separate indexes,
built by separate commands, searched by separate code, and never
merged -- a BM25 score has no common scale with a cosine, so there
is no list they could both belong to.  A prompt offering two words
and no explanation invites exactly the wrong guess.")

(defun org-semantic--ranking-annotation (candidate)
  "What CANDIDATE means, for the ranking prompt's right-hand column."
  (let ((what (cdr (assoc candidate org-semantic--rankings))))
    (and what (concat "  " what))))

(defun org-semantic--read-ranking ()
  "Ask which ranking to use, offering both and saying what each is.

The setting is the *default*, not text put into the minibuffer:
`completing-read' calls INITIAL-INPUT deprecated and says to use
DEF instead, and here the reason is plain to see.  Inserted as
input, \"semantic\" is what a completion UI filters the candidates
by -- so the prompt for choosing between two rankings offered
exactly one of them, and it was never the one you were reaching
for.

The annotation rides the *table* rather than
`completion-extra-properties': a table carries its own metadata
wherever it is passed, where the variable is global state a
front-end is free to rebind."
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
(defun org-semantic-find (query &optional arg)
  "Search the current buffer's vault for QUERY and show what comes back.

One ranking is used, never both: `semantic' finds notes by meaning
and `lexical' by word, and the two are ranked separately because a
score from one has no meaning beside a score from the other.  `r'
in the results buffer switches.

With one prefix ARG, ask which ranking; with two, ask about the
length of the list as well.  See `org-semantic--find-prompts'.

A query may carry predicates the server reads out of it --
`tag:x', `dir:x', `todo:x', `lang:x' for a word search, and any of
them negated with a leading `-' -- with the rest as free text."
  (interactive
   (list (read-string "Search notes for: " nil 'org-semantic-search-history)
         current-prefix-arg))
  (let* ((asks (org-semantic--find-prompts arg))
         (vault (org-semantic-vault-or-error))
         (mode (if (or (car asks) (equal org-semantic-results-ranking "ask"))
                   (org-semantic--read-ranking)
                 org-semantic-results-ranking))
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
         ;; A *suggestion*, so it goes in as the default and not as text
         ;; already typed: RET takes it, `M-n' fetches it for editing, and
         ;; anything else replaces it without having to be deleted first.
         ;; `read-string' says outright that INITIAL-INPUT "has been
         ;; superseded by DEFAULT-VALUE and should normally be nil in new
         ;; code".
         (query (read-string (if thing
                                 (format "Search notes for (default %s): " thing)
                               "Search notes for: ")
                             nil 'org-semantic-search-history thing)))
    (org-semantic-find query arg)))

(defvar org-semantic-search-history nil
  "Queries searched for, most recent first.

The ordinary Emacs arrangement, and it needs nothing else to be
useful: `M-p' and `M-n' walk it in the minibuffer, and
`savehist-mode' carries it between sessions **by itself** --
`savehist-minibuffer-hook' records whichever history variable each
minibuffer used, so an interned symbol passed to `read-string' is
picked up without anyone adding it to
`savehist-additional-variables'.

A `defvar' rather than a `defcustom': a history is data the
package accumulates, not a setting anyone chooses.")

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

MODE asks in that ranking *this once*, without making it what the
buffer wants -- for the offer that gets an answer out of a refusal,
where a vault missing its semantic index or its model can usually
still answer by word.  Pressing that must not silently redefine
every later query in the buffer, which it did, with nothing saying
why.

What the header shows is `org-semantic-results--asked-mode', the
ranking that produced what is on screen, and not what the buffer
will ask next.  Binding the buffer's own mode around the request
instead looked equivalent and was worse: the reply is rendered long
after the binding is gone, so the header said \"semantic\" over
results found by word."
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
                   (org-semantic-results--render-error error-object))))))))
  (setq org-semantic-results--asked-mode (or mode org-semantic-results--mode))
  ;; **A new search is a new question.**  The latch stops one reply being asked
  ;; about twice; it is not a decision to stop asking, and left uncleared it was
  ;; exactly that -- once you had answered a missing model, no later search in
  ;; this buffer offered anything again, and killing the buffer was the only way
  ;; back.  Reported as a sticky setting, which is what it looks like from the
  ;; outside.
  (setq org-semantic-results--latched nil)
  (setq org-semantic-results--started (float-time))
  (setq mode-line-process " [searching]")
  (force-mode-line-update)
  ;; Only when there is nothing to look at yet.  A buffer already showing
  ;; hits keeps them until the new ones arrive: they are a moment out of
  ;; date, which is better to read than an empty buffer is.
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
        ;; Absent when waived, and the driver takes that literally --
        ;; which is the whole of how a drifted policy is searched anyway.
        :config (and org-semantic-results--policy org-semantic-config)))


;;;; Grouping, and the lines a drawing owns

(defun org-semantic-results--group (hits)
  "Arrange HITS as ((FILE . ((LINE . HITS) ...)) ...), in the order drawn.

Grouped on the heading's *line* and never on its text.  The server
groups on the text, so two sections of one note whose outline
paths spell the same are handed over as one group carrying two
different heading lines -- and a note kept as a year of meetings
is exactly where that happens.  Its groups do not arrive together
either, since they are ranked against every other note's, so a
note is gathered here rather than assumed contiguous.

Order is first appearance throughout, which is the server's
ranking: the best note first, and within it the best section."
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
    ;; ranked, which is right for choosing *which* sections to show and
    ;; wrong for reading one: the passages of a section are pieces of one
    ;; continuous text, and a tie in the scores would otherwise decide
    ;; which piece came first.  It also settles the overlap: consecutive
    ;; passages share a paragraph, and in document order the earlier one
    ;; owns it, so what is dimmed is the repeat rather than whichever
    ;; copy happened to score higher.
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

There is something to decide because the same line really can be
drawn twice.  Consecutive passages of one section begin with the
last paragraph of the one before, on purpose, so an idea cut in
half is still whole in both; and a paragraph too long to split
anywhere sensible is cut into pieces that all name the whole
paragraph, so several passages can carry identical text.  Whoever
draws it first owns it, the rest are dimmed, and nothing has to be
reconciled later."
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
        ;; Drawn into a string first, because how many passages a note
        ;; really contributes is not known until the claim map has been
        ;; asked -- and the note's own line says that number.
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

COUNTS asks for the third line, the one saying how much came back.
It is left off when nothing did and the reason is about to be
given instead: \"0 notes, 0 passages\" above an explanation of why
there is no index reads as an answer, and it is not one."
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

Its `#+title:', or its filename without the extension when it has
none -- the server has already made that substitution, so this is
the title it sends, and the fallback here is for a reply that
somehow carries none.

**The path is not repeated here.**  It used to be, and the address
line under it names the directory and the file as separate links, so
the same string was drawn twice in three lines.  A title is not
unique where a path is -- two notes in different folders can share
one -- and that is what the address line one line down settles."
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
              ;; `delay-mode-hooks', so a user's `org-mode-hook' -- which may
              ;; start a modeline, a folding scheme, or anything else -- does not
              ;; run in a buffer that exists to hold six lines of text.
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

(defun org-semantic-results--fontified (text)
  "TEXT with org's faces on it, or TEXT itself if that cannot be done."
  (if (or (not org-semantic-results-fontify)
          (string-empty-p text)
          (not (require 'org nil t)))
      text
    (condition-case nil
        (with-current-buffer (org-semantic-results--fontifier)
          (let ((inhibit-read-only t))
            (erase-buffer)
            (insert text)
            (font-lock-ensure)
            (org-semantic-results--faces-only (buffer-string))))
      ;; A fontifier that fails must not cost the search its results.
      (error text))))

(defun org-semantic-results--block (hit first claimed)
  "Draw HIT as a string, or nil if every line of it was already shown.

FIRST says this is the leading passage of its section, which is
what carries the outline path -- the ones after it name their
lines instead, since repeating the heading under itself says
nothing.  CLAIMED is the claim map, and is added to."
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
address already names by its file, so it is dropped: on this
author's vault it repeated the filename on 85 hits in 88.  What it
costs is the three where they differ -- a `README.org' titled for
what it contains -- and that is the trade taken."
  (let ((parts (split-string (or (plist-get hit :heading) "") " > " t)))
    (when (cdr parts)
      (string-join (cdr parts) " > "))))

(defun org-semantic-results--link (text target props &optional line help)
  "Propertize TEXT as a link to TARGET, over PROPS.

LINE is the line it goes to, where that means anything.  HELP is
the `help-echo'.  TARGET is a **symbol** and not a function: what
a piece of this buffer points at is then something a test can read
back off the text, which a closure would not be."
  (apply #'propertize text
         'face 'org-semantic-results-link
         'org-semantic-target target
         'help-echo (or help "mouse-2: go here")
         ;; **The mouse affordances live here and nowhere else.**  They were
         ;; on every line a hit was drawn on, which made the whole result one
         ;; large button: the passage lit up under the pointer, a click
         ;; jumped instead of placing point, and the text could not be
         ;; selected.  Now what looks like a link is what behaves like one,
         ;; and a passage is text.
         'mouse-face 'highlight
         'keymap org-semantic-results-passage-map
         'follow-link t
         (append (and line (list 'org-semantic-line line)) props)))

(defun org-semantic-results--plain (text props line)
  "Propertize TEXT as part of a head that is not a link, over PROPS.

LINE is the passage's own, so that point anywhere between the
links -- on the score, on a separator -- still goes to the
passage.

Every piece of the head is propertized on its own and the results
concatenated, rather than the line being propertized once at the
end: `propertize' overrides what a string already carries, so a
final pass would give every link the passage's line and quietly
undo the whole point of having four of them."
  (apply #'propertize text 'org-semantic-line line props))

(defun org-semantic-results--insert-block-head (hit item first)
  "Insert the line above HIT's passage, for ITEM.
FIRST is as in `org-semantic-results--block'.

**The address is four links, not one.**  It names a directory, a
note, a section and a line, and each goes to the thing it names --
the directory in `org-semantic-results-reveal-function', the note
at its top, the section at its heading, the line at the passage.
A single target for a line that says four things leaves most of
what it displays inert, and leaves the reader guessing which of
the four it will pick.

Only the leading passage of a section carries the address; the
ones after it name their line alone, since the path is already
above them and only the line has changed."
  (let* ((props (list 'org-semantic-item item
                      'org-semantic-hit hit
                      'org-semantic-file (org-semantic-results--item-file item)
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
      (funcall plain (org-semantic-ui-score hit) 'org-semantic-results-score)
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
      ;; **A range, with both ends reachable.**  A passage is lines FROM to
      ;; TO, and either end is somewhere you might want to be -- the top to
      ;; read it, the bottom to carry on past it.  One link to the middle of
      ;; that is a range described and not offered.
      ;;
      ;; Only when the span is real: `item-line' is the heading's line for a
      ;; passage the note has outgrown, so comparing the two is what keeps a
      ;; stale block from advertising a range it cannot honour.  A one-line
      ;; passage says "line", since a range of one is not one.
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
      ;; One space, as org itself separates a headline from its tags.  The
      ;; middot the header lines use is for holding apart facts that have
      ;; nothing to do with each other; tags follow what they belong to.
      (if annotation
          (concat (funcall plain " " 'default)
                  (funcall plain annotation 'org-semantic-results-annotation))
        "")
      (funcall plain "\n" 'default)))))

(defun org-semantic-results--insert-lines (item lines start owned)
  "Insert LINES for ITEM as the note's own lines, the first being START.

OWNED says, per line, whether this drawing is the first to show
it.  Everything drawn goes in the gutter: the passage itself is
inserted exactly as the note has it, because what makes a line
addressable -- and one day writable -- is that it is the note's
line and not a rendering of one."
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
        ;; The dimming goes on the note's own text and **not over the gutter**,
        ;; which keeps its own face.  Applied to the whole line it overrode the
        ;; gutter's, so a repeated line lost the margin its neighbours had and
        ;; the left edge came out ragged — the same four columns present on some
        ;; lines and absent on others, for no reason a reader could see.
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
      ;; Indented to the gutter, not to a constant: with line numbers on the
      ;; gutter is wider than four columns, and a label that assumed four sat
      ;; left of the text it stands in for.
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

The server sends an empty passage when the note has been cut
shorter than the span the index recorded, so an empty string
against a span of several lines is this and not a blank line."
  (insert (propertize
           (concat "    (this passage could not be read: the note has changed"
                   " since it was indexed)\n")
           'face 'org-semantic-results-stale
           'org-semantic-item item
           ;; Deliberately no `org-semantic-line': nothing here may offer
           ;; to go somewhere it has just said it cannot find.
           'read-only t)))


;;;; Errors

(defconst org-semantic-results--latching '("config-drift" "model-missing")
  "The failures asked about once per search rather than once per reply.

Said in full the first time and kept to a line after that.  Both of
these describe a state of the vault rather than of the request: a
policy that has drifted stays drifted, and a model that is not
downloaded stays undownloaded, so a *second reply* to the same
search cannot answer differently.  Anything else -- a mistyped
model, a vault that vanished -- is about the request.

**Per search, and that is the whole of it.**
`org-semantic-results--search' clears the latch, because a search
the user asked for is a question they are entitled to be asked
again.  Without that it read as a setting: answer the missing-model
question once and no later search in the buffer offered anything,
with killing the buffer the only way back.  Reported as sticky, and
from the outside that is exactly what it is.")

(cl-defun org-semantic-results--render-error (error-object)
  "Draw what ERROR-OBJECT says, and what can be done about it."
  (let* ((inhibit-read-only t)
         (data (plist-get error-object :data))
         (remedy (org-semantic-ui-remedy error-object org-semantic-results--mode))
         (kind (org-semantic-ui-remedy-kind remedy))
         (latching (member kind org-semantic-results--latching)))
    ;; A refusal for the very model this buffer is fetching is not news, and
    ;; not a question: it is the wait, arriving as an error because a search
    ;; cannot be answered mid-download.
    ;; `org-semantic-results--fetching' first, and not as a formality: without it
    ;; a `model-missing' carrying no model at all compares nil against nil and
    ;; every such refusal is drawn as a wait for a download nobody started.
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

Nothing is drawn in the buffer for this: the buffer states the
problem and the question is asked once, where a question belongs.

An active minibuffer is left alone.  The reply arrives whenever it
arrives, and the user may be typing something else entirely by
then; a prompt cannot be raised over another prompt without eating
the keystrokes meant for it.  The sentence in the buffer is then the
whole answer, and nothing is lost -- `org-semantic-results-reindex'
makes the same call the offer would have made.

`quit' is caught because this runs from a timer: `jsonrpc.el'
dispatches each message from one of its own rather than from the
process filter, so an escaping \\`C-g' would be \"Error running
timer\" for having declined an offer."
  (when-let* ((offers (cl-remove-if-not #'org-semantic-ui-offer-key
                                        (org-semantic-ui-remedy-offers remedy))))
    (unless (active-minibuffer-window)
      (let* ((keys (append (mapcar #'org-semantic-ui-offer-key offers) (list ?q)))
             ;; Nil, so six lines of menu do not land in `*Messages*'.  On Emacs
             ;; 29 and later `read-char-choice' reads through a real minibuffer,
             ;; and every prompt it draws is logged like any other message -- so
             ;; the buffer fills with a transcript of questions already answered,
             ;; which is of no use to anybody and is what a click on the echo area
             ;; opens.
             (message-log-max nil)
             (choice (condition-case nil
                         (read-char-choice
                          (org-semantic-results--ask-prompt remedy offers) keys)
                       (quit ?q))))
        ;; And clear what is left of it.  The minibuffer exits on the answer but
        ;; its last line stays in the echo area -- "Choice: l" sitting there after
        ;; the question is over reads as a prompt still waiting.
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
that each offer can say what it costs -- indexing is minutes and
searching by word is seconds, and a single-letter menu that does
not say so is asking the user to remember which."
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

Each says what it costs, because one of these is minutes and the
rest are not, and a menu of single letters that does not say so is
asking the reader to remember which."
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
as well as something `org-semantic-results--ask' calls: the offers
were a row of buttons before they were a question, and the shape
costs nothing to keep."
  (let ((os-buffer (current-buffer))
        (os-data (plist-get error-object :data)))
    (lambda (_button)
      (with-current-buffer os-buffer
        (pcase action
          ;; Searches by word *once*, and does not change what this buffer is
          ;; set to want.  It is an escape from one refusal, not a statement
          ;; about how the user prefers to search -- and it read as the latter,
          ;; because the setting stuck and every later query in the buffer was
          ;; answered by word with nothing saying why.  Someone who does prefer
          ;; it says so in `org-semantic-results-ranking'.
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

One thing at a time: if the search then finds no index, it says so
itself and asks about building one -- which is a question the user
gets to answer rather than minutes of embedding nobody asked for.

**The buffer says it is waiting, and is replaced when it is not.**
It kept the refusal up instead -- \"the e5-small model is not
downloaded yet\" -- for the length of the fetch, so a page that had
been told to fetch went on reporting that nothing had been fetched,
with only the mode line and one echo-area line saying otherwise.  A
model is minutes and there is no progress to show inside it:
fastembed exposes no increments, so the announcement carries a size
and nothing that counts up to it.  Saying which size, and that the
results will arrive by themselves, is the whole of what can honestly
be offered."
  (let ((os-buffer (current-buffer)))
    (setq mode-line-process " [downloading]")
    (force-mode-line-update)
    (setq org-semantic-results--fetching (cons model nil))
    (org-semantic-results--waiting model nil)
    (org-semantic-download
     :model model
     :progress (lambda (report)
                 (org-semantic-report-message report)
                 ;; The size arrives in the one report the fetch makes, a moment
                 ;; after the request.  Until then the line stands without it,
                 ;; rather than there being no line at all.
                 (when-let* ((bytes (plist-get report :bytes)))
                   (when (buffer-live-p os-buffer)
                     (with-current-buffer os-buffer
                       (setq org-semantic-results--fetching (cons model bytes))
                       (org-semantic-results--waiting model bytes)))))
     :success (lambda (_result)
                (when (buffer-live-p os-buffer)
                  (with-current-buffer os-buffer
                    ;; Cleared first: the search this fires may refuse for some
                    ;; other reason, and that is a question again.
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
is preferred because it lands on the note rather than merely in
the directory holding it; it has been in `dired' itself since
Emacs 28, and the fallback covers a Dired that has been replaced."
  (if (fboundp 'dired-jump)
      (dired-jump nil file)
    (dired directory)))

(defun org-semantic-results--visit (&rest keys)
  "Go where point says, passing KEYS on to `org-semantic-ui-visit'.

A directory is the one thing here that is not a place in a note,
so it is the one target handled apart -- everything else differs
only in which line it carries."
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
      ;; What makes `M-g M-n' carry on from this buffer afterwards, and
      ;; what `M-0 RET' needs to be able to quit the window.  The note is
      ;; named by its window rather than by `current-buffer', which is
      ;; still this buffer whenever the note was only displayed.
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
  "Go to the Nth next passage, and show it."
  (interactive "p" org-semantic-results-mode)
  (unless (org-semantic-results--move (or n 1))
    (user-error "No next passage"))
  (org-semantic-results--visit))

(defun org-semantic-results-previous (&optional n)
  "Go to the Nth previous passage, and show it."
  (interactive "p" org-semantic-results-mode)
  (org-semantic-results-next (- (or n 1))))

(defun org-semantic-results-next-note (&optional n)
  "Go to the first passage of the Nth next note, and show it.

Skips whatever is left of this note -- its remaining sections and
their passages -- where \\[org-semantic-results-next] would step
through them one passage at a time."
  (interactive "p" org-semantic-results-mode)
  (dotimes (_ (abs (or n 1)))
    (let ((file (org-semantic-results--file-at-point))
          (step (if (< (or n 1) 0) -1 1)))
      (while (and (org-semantic-results--move step)
                  (equal file (org-semantic-results--file-at-point))))))
  (org-semantic-results--visit))

(defun org-semantic-results-previous-note (&optional n)
  "Go to the first passage of the Nth previous note, and show it.

Skips whatever is left of this note -- its remaining sections and
their passages -- where \\[org-semantic-results-previous] would step
through them one passage at a time."
  (interactive "p" org-semantic-results-mode)
  (org-semantic-results-next-note (- (or n 1))))

(defun org-semantic-results--next-error (n reset)
  "Go N passages and show the one arrived at.  RESET starts from the first.

The buffer-local `next-error-function'.  Its contract is the
variable's own: N is how many to move and may be negative, and N
of zero means the passage point is already on -- which is the case
`next-error-follow-minor-mode' uses, so it must move nothing at
all.  The target window is selected on purpose:
`next-error-no-select' wraps the call in `save-selected-window'
and puts things back itself."
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
Walked by property change rather than by character: the runs are
whole lines, and stepping through every one of them to find a
boundary that is already recorded is work for nothing."
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

(defun org-semantic-results-set-query (query)
  "Search this vault for QUERY instead.

The one prompt here that keeps INITIAL-INPUT, against
`read-string''s advice, because this command exists to *edit* the
query rather than to suggest one: offering it as a default would
mean pressing \\`M-n' before every refinement."
  (interactive
   (list (read-string "Search notes for: " org-semantic-results--query
                      'org-semantic-search-history))
   org-semantic-results-mode)
  (setq org-semantic-results--query query)
  (org-semantic-results--search))

(defun org-semantic-results--rank (mode)
  "Rank by MODE from now on in this buffer, and search again."
  (setq org-semantic-results--mode mode)
  (message "org-semantic: ranking by %s"
           (if (equal mode "lexical") "word (lexical)" "meaning (semantic)"))
  (org-semantic-results--search))

(defun org-semantic-results-rank-by-meaning ()
  "Rank by meaning -- the semantic index -- and search again."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--rank "semantic"))

(defun org-semantic-results-rank-by-word ()
  "Rank by word -- the lexical index -- and search again.

Two keys rather than one that toggles, which is what this was.  A
toggle cannot be pressed without first knowing which ranking is in
force, so the same key means two things depending on state the user
has to go and read; `m' and `w' each mean one thing and can be
pressed blind.  `w' is also the key the failure prompt uses for the
same choice.

This is not the same as the offer in that prompt, which searches by
word *once* and leaves the buffer's ranking alone: pressing a key
here is a statement about what this buffer is for."
  (interactive nil org-semantic-results-mode)
  (org-semantic-results--rank "lexical"))

(defun org-semantic-results-toggle-connector ()
  "Swap how the terms of this word search are joined: AND or OR.

Refuses on a semantic search rather than appearing to work: an
embedding has no terms to join, and the server ignores the parameter
there instead of failing, so a key that quietly flipped it would
look as though it had done something."
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
  ;; Spelled out because this is the surprising part of the interface:
  ;; the number counts notes, so a vault kept in three large files
  ;; answers a k of fifty with nine hits and no argument raises it.
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

The exact value, where `k' and `K' double and halve it.  A vault
kept in a few large files is the case that wants one: raising this
is what widens the list, and it is the number the header calls `k'."
  (interactive
   (list (read-number "Notes at most: " (or org-semantic-results--k 8)))
   org-semantic-results-mode)
  (org-semantic-results--set-k n))

(defun org-semantic-results-set-passages (n)
  "Let each note contribute at most N passages.

The exact value, where `+' and `-' double and halve it.  A year of
meetings in one file is the case that wants one: every hit comes from
that file, so this is the only number that deepens the list."
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

Doubles, as the note cap does: one story for both keys rather than
two conventions to remember.  It is also the size of step this
number wants -- the manual's advice for a year of meetings in one
file is 25, which stepping by one does not reach.  An exact value
comes from the two prefix arguments on a search."
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
