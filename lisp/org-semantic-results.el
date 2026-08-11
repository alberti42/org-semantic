;;; org-semantic-results.el --- A buffer of org-semantic hits -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.1.0
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
seconds.  It is the `mode' the server is asked for, spelled
`ranking' here so as not to read as a setting for
`org-semantic-results-mode'."
  :type '(choice (const :tag "By meaning" "semantic")
                 (const :tag "By word" "lexical")))


;;;; Faces

(defface org-semantic-results-header '((t :inherit bold))
  "Face for the lines at the top of a results buffer.")

(defface org-semantic-results-file
  '((t :inherit font-lock-function-name-face :weight bold))
  "Face for the line naming a note.")

(defface org-semantic-results-heading '((t :inherit default))
  "Face for a hit's outline path.")

(defface org-semantic-results-score '((t :inherit shadow))
  "Face for how well a hit matched.")

(defface org-semantic-results-location '((t :inherit shadow))
  "Face for the file and line a hit is at.")

(defface org-semantic-results-annotation '((t :inherit shadow))
  "Face for a hit's TODO keyword, priority and tags.")

(defface org-semantic-results-gutter '((t :inherit line-number))
  "Face for the few columns at the start of a passage line.")

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
  "Which ranking is being asked for, \"semantic\" or \"lexical\".")

(defvar-local org-semantic-results--k nil
  "How many notes may appear, or nil for the server's default.")

(defvar-local org-semantic-results--per-file nil
  "How many passages one note may contribute, or nil for the default.")

(defvar-local org-semantic-results--merge nil
  "Whether a section divided into several passages answers as one hit.")

(defvar-local org-semantic-results--any nil
  "Whether a word query matches notes carrying any of its terms.")

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

(defvar-local org-semantic-results--drift nil
  "Whether a drifted policy has already been raised in this buffer.
The condition holds until the user acts on it, so it is said once
and then kept to a line -- otherwise every later search says it
again.")


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
  "m"         #'org-semantic-results-toggle-ranking
  "M"         #'org-semantic-results-toggle-merge
  "a"         #'org-semantic-results-toggle-any
  "+"         #'org-semantic-results-more-notes
  "-"         #'org-semantic-results-fewer-notes
  "P"         #'org-semantic-results-set-per-file
  "="         #'org-semantic-results-describe-hit
  "R"         #'org-semantic-results-reindex
  "C-c C-f"   #'next-error-follow-minor-mode)

(defvar org-semantic-results-passage-map
  (let ((map (make-sparse-keymap)))
    (define-key map [mouse-2] #'org-semantic-results-mouse-goto)
    map)
  "Keymap put on every line a hit was drawn on.")

(define-derived-mode org-semantic-results-mode special-mode "org-semantic"
  "Major mode for a list of org-semantic hits.

\\<org-semantic-results-mode-map>A passage is shown as the note's
own lines, so \\[org-semantic-results-goto] goes to the line under
point rather than to the top of the section it belongs to, and
\\[org-semantic-results-display] shows it without leaving this
buffer.  \\[revert-buffer] asks again rather than redrawing what
is here, since the notes may have moved on.

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
  (add-to-invisibility-spec '(org-semantic-results . t))
  (add-hook 'kill-buffer-hook #'org-semantic-results--abandon nil t))

(defun org-semantic-results--abandon ()
  "Stop caring about the search this buffer asked for."
  (when org-semantic-results--driver
    (org-semantic-ui-driver-abandon org-semantic-results--driver)))


;;;; Asking

;;;###autoload
(defun org-semantic-find (query &optional arg)
  "Search the current buffer's vault for QUERY and show what comes back.

With a prefix ARG, ask for the ranking, how many notes may appear
and how many passages any one of them may contribute, instead of
taking them from the settings.

A query may carry predicates the server reads out of it --
`tag:x', `-tag:x', `dir:x', `todo:x', and `lang:x' for a word
search -- with the rest of it as free text."
  (interactive
   (list (read-string "Search notes for: ")
         current-prefix-arg))
  (let* ((vault (org-semantic-vault-or-error))
         (mode (if arg
                   (completing-read "Rank by: " '("semantic" "lexical") nil t
                                    org-semantic-results-ranking)
                 org-semantic-results-ranking))
         (k (and arg (read-number "Notes at most: " 8)))
         (per-file (and arg (read-number "Passages per note at most: " 3)))
         (buffer (org-semantic-results--buffer vault)))
    (with-current-buffer buffer
      (setq org-semantic-results--vault vault
            org-semantic-results--query query
            org-semantic-results--mode mode
            org-semantic-results--k k
            org-semantic-results--per-file per-file)
      (org-semantic-results--search))
    (pop-to-buffer buffer)))

;;;###autoload
(defun org-semantic-find-at-point (&optional arg)
  "Search for the region, or the thing at point.  ARG is as in `org-semantic-find'."
  (interactive "P")
  (let ((query (if (use-region-p)
                   (buffer-substring-no-properties (region-beginning) (region-end))
                 (or (thing-at-point 'symbol t) ""))))
    (org-semantic-find (read-string "Search notes for: " query) arg)))

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

(defun org-semantic-results--search ()
  "Ask again for what this buffer is set to want."
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

(defun org-semantic-results--params ()
  "What this buffer wants, as parameters for a search."
  (list :query (or org-semantic-results--query "")
        :vault org-semantic-results--vault
        :k org-semantic-results--k
        :per-file org-semantic-results--per-file
        :merge-by-section org-semantic-results--merge
        :mode org-semantic-results--mode
        :model (or org-semantic-results--model org-semantic-model)
        :any org-semantic-results--any
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
          (org-semantic-results--insert-file (car file) (length blocks))
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
                            (format "per-file=%s passages"
                                    (or org-semantic-results--per-file 3))
                            (and org-semantic-results--merge "merged by section")
                            (and (equal org-semantic-results--mode "lexical")
                                 org-semantic-results--any
                                 "any term")))))
    (insert (propertize
             (format "org-semantic  %s  %s\n"
                     org-semantic-results--mode
                     (if (and org-semantic-results--query
                              (not (string-empty-p org-semantic-results--query)))
                         (format "%S" org-semantic-results--query)
                       "(no query)"))
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

(defun org-semantic-results--insert-file (file passages)
  "Insert the line naming FILE, which contributed PASSAGES of them."
  (insert (propertize
           (format "%s  ·  %d passage%s\n"
                   (file-relative-name file org-semantic-results--vault)
                   passages (if (= passages 1) "" "s"))
           'face 'org-semantic-results-file
           'org-semantic-file file
           'org-semantic-group 'file
           'read-only t)))

(defun org-semantic-results--block (hit first claimed)
  "Draw HIT as a string, or nil if every line of it was already shown.

FIRST says this is the leading passage of its section, which is
what carries the outline path -- the ones after it name their
lines instead, since repeating the heading under itself says
nothing.  CLAIMED is the claim map, and is added to."
  (let* ((file (org-semantic-hit-file hit))
         (start (org-semantic-hit-start-line hit))
         (end (org-semantic-hit-end-line hit))
         (text (or (org-semantic-hit-text hit) ""))
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

(defun org-semantic-results--insert-block-head (hit item first)
  "Insert the line or two above HIT's passage, for ITEM.
FIRST is as in `org-semantic-results--block'.

These lines go to where the passage starts, not to the heading
that owns it.  Every line in this buffer goes where it says, and
what these say is which passage they head: a section can run to
hundreds of lines, so arriving at its heading is arriving with the
words that matched somewhere off the bottom of the window.  The
heading is drawn for orientation -- it says which section this is
-- and `org-semantic-results-describe-hit' gives its line for
anyone who wants it."
  (let ((props (list 'org-semantic-item item
                     'org-semantic-hit hit
                     'org-semantic-file (org-semantic-results--item-file item)
                     'mouse-face 'highlight
                     'keymap org-semantic-results-passage-map
                     'follow-link t
                     'help-echo "mouse-2: go to this passage"
                     'read-only t))
        (annotation (org-semantic-ui-annotate hit)))
    (insert (apply #'propertize
                   (format "  %s  %s\n"
                           (propertize (org-semantic-ui-score hit)
                                       'face 'org-semantic-results-score)
                           (propertize
                            (if first
                                (or (plist-get hit :heading) "")
                              (format "lines %s–%s"
                                      (org-semantic-hit-start-line hit)
                                      (org-semantic-hit-end-line hit)))
                            'face 'org-semantic-results-heading))
                   'org-semantic-line (org-semantic-results--item-line item)
                   props))
    (when first
      (insert (apply #'propertize
                     (format "  %s%s\n"
                             (propertize
                              (format "%s:%s"
                                      (org-semantic-hit-path hit)
                                      (org-semantic-results--item-line item))
                              'face 'org-semantic-results-location)
                             (if annotation
                                 (propertize (concat "  ·  " annotation)
                                             'face 'org-semantic-results-annotation)
                               ""))
                     'org-semantic-line (org-semantic-hit-line hit)
                     props)))))

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
                          'wrap-prefix (make-string (length gutter) ?\s)
                          'mouse-face 'highlight
                          'keymap org-semantic-results-passage-map
                          'follow-link t
                          'help-echo "mouse-2: go to this line")))
        (unless mine
          (setq props (append (list 'face 'org-semantic-results-duplicate) props)))
        (when (and limit (>= shown limit))
          (setq props (append (list 'invisible 'org-semantic-results
                                    'org-semantic-elided t)
                              props)))
        (insert (apply #'propertize (concat gutter line "\n") props)))
      (setq number (1+ number)
            shown (1+ shown)))
    (when (and limit (> (length lines) limit))
      (setf (org-semantic-results--item-elided item) (- (length lines) limit))
      (org-semantic-results--insert-elision item))))

(defun org-semantic-results--insert-elision (item)
  "Insert the marker standing in for the lines ITEM folded away."
  (insert (propertize
           (format "    ⋯ %d line%s\n"
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

(defun org-semantic-results--render-error (error-object)
  "Draw what ERROR-OBJECT says, and what can be done about it."
  (let* ((inhibit-read-only t)
         (remedy (org-semantic-ui-remedy error-object org-semantic-results--mode))
         (kind (org-semantic-ui-remedy-kind remedy))
         (drift (equal kind "config-drift")))
    (setq mode-line-process nil)
    (force-mode-line-update)
    ;; A drifted policy holds until the user acts on it, so it is said in
    ;; full once and kept to a line after that.  Unlatched, a search on
    ;; every keystroke would raise it on every keystroke.
    (if (and drift org-semantic-results--drift)
        (save-excursion
          (goto-char (point-max))
          (insert (propertize (format "\n%s\n"
                                      (org-semantic-ui-remedy-message remedy))
                              'face 'org-semantic-results-stale 'read-only t)))
      (when drift (setq org-semantic-results--drift t))
      (erase-buffer)
      (org-semantic-results--insert-header nil nil)
      (insert (propertize (format "  %s\n\n"
                                  (org-semantic-ui-remedy-message remedy))
                          'face 'org-semantic-results-stale 'read-only t))
      (dolist (offer (org-semantic-ui-remedy-offers remedy))
        (insert "  ")
        (insert-text-button
         (format "[%s]" (car offer))
         'action (org-semantic-results--offer-action (cdr offer) error-object)
         'follow-link t
         'help-echo (org-semantic-results--offer-help (cdr offer)))
        (insert "\n"))
      (goto-char (point-min)))))

(defun org-semantic-results--offer-help (action)
  "What ACTION would do, in a few words."
  (pcase action
    ('index "build the index this search needs")
    ('index-full "rebuild from scratch, which re-embeds everything")
    ('lexical "search by word instead, from an index that builds in seconds")
    ('retry "ask again")
    ('choose-model "search one of the models that is built")
    ('waive "search the index as it stands, under the policy it was built with")
    ('show-changed "list the settings that moved")
    (_ "")))

(defun org-semantic-results--offer-action (action error-object)
  "A button function doing ACTION, which ERROR-OBJECT asked for.

Offered as a button rather than asked as a question: a reply
arrives whenever it arrives, and a prompt raised from a callback
interrupts whatever the user was doing somewhere else."
  (let ((os-buffer (current-buffer))
        (os-data (plist-get error-object :data)))
    (lambda (_button)
      (with-current-buffer os-buffer
        (pcase action
          ('lexical
           (setq org-semantic-results--mode "lexical")
           (org-semantic-results--search))
          ('retry (org-semantic-results--search))
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
          ((or 'index 'index-full)
           (org-semantic-results--reindex (eq action 'index-full))))))))

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

(defun org-semantic-results--visit (&rest keys)
  "Go where point says, passing KEYS on to `org-semantic-ui-visit'."
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
  "Go to the first passage of the Nth next note."
  (interactive "p" org-semantic-results-mode)
  (dotimes (_ (abs (or n 1)))
    (let ((file (org-semantic-results--file-at-point))
          (step (if (< (or n 1) 0) -1 1)))
      (while (and (org-semantic-results--move step)
                  (equal file (org-semantic-results--file-at-point))))))
  (org-semantic-results--visit))

(defun org-semantic-results-previous-note (&optional n)
  "Go to the first passage of the Nth previous note."
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
  "Show or fold away the rest of the passage point is in."
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

(defun org-semantic-results--revert (&optional _ignore-auto _noconfirm)
  "Ask again rather than redraw.
The notes may have moved on since, and what is here is a picture
of what they said then."
  (org-semantic-results--search))

(defun org-semantic-results-set-query (query)
  "Search this vault for QUERY instead."
  (interactive
   (list (read-string "Search notes for: " org-semantic-results--query))
   org-semantic-results-mode)
  (setq org-semantic-results--query query)
  (org-semantic-results--search))

(defun org-semantic-results-toggle-ranking ()
  "Swap between ranking by meaning and ranking by word."
  (interactive nil org-semantic-results-mode)
  (setq org-semantic-results--mode
        (if (equal org-semantic-results--mode "lexical") "semantic" "lexical"))
  (message "org-semantic: ranking by %s"
           (if (equal org-semantic-results--mode "lexical") "word" "meaning"))
  (org-semantic-results--search))

(defun org-semantic-results-toggle-merge ()
  "Fold a section answering as several passages into one hit, or stop."
  (interactive nil org-semantic-results-mode)
  (setq org-semantic-results--merge (not org-semantic-results--merge))
  (message "org-semantic: %s"
           (if org-semantic-results--merge
               "one hit per section, spanning all of its passages"
             "each passage on its own"))
  (org-semantic-results--search))

(defun org-semantic-results-toggle-any ()
  "Match notes carrying any of the query's terms, or all of them."
  (interactive nil org-semantic-results-mode)
  (unless (equal org-semantic-results--mode "lexical")
    (user-error "Only a word search has terms to match any of"))
  (setq org-semantic-results--any (not org-semantic-results--any))
  (message "org-semantic: matching %s of the terms"
           (if org-semantic-results--any "any" "all"))
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

(defun org-semantic-results-set-per-file (n)
  "Let each note contribute N passages."
  (interactive
   (list (read-number "Passages per note at most: "
                      (or org-semantic-results--per-file 3)))
   org-semantic-results-mode)
  (setq org-semantic-results--per-file (max 1 n))
  (org-semantic-results--search))

(defun org-semantic-results-reindex (&optional arg)
  "Index this buffer's vault and search again.
ARG is as in `org-semantic-reindex': two prefixes rebuild from scratch."
  (interactive "P" org-semantic-results-mode)
  (org-semantic-results--reindex (cdr (org-semantic--reindex-flags arg))))

(defun org-semantic-results-describe-hit ()
  "Say everything known about the passage point is in."
  (interactive nil org-semantic-results-mode)
  (let ((hit (or (get-text-property (point) 'org-semantic-hit)
                 (let ((item (org-semantic-results--item-at-point)))
                   (and item (org-semantic-results--item-hit item))))))
    (unless hit (user-error "No passage here"))
    (message "%s  %s:%s  lines %s-%s%s%s"
             (org-semantic-ui-score hit)
             (org-semantic-hit-path hit)
             (org-semantic-hit-line hit)
             (org-semantic-hit-start-line hit)
             (org-semantic-hit-end-line hit)
             (let ((annotation (org-semantic-ui-annotate hit)))
               (if annotation (concat "  " annotation) ""))
             (let ((id (plist-get hit :id)))
               (if id (format "  id:%s" id) "")))))

(provide 'org-semantic-results)
;;; org-semantic-results.el ends here
