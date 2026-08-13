;;; org-semantic-results-tests.el --- tests for the results buffer -*- lexical-binding: t; -*-

;; SPDX-License-Identifier: MIT

;;; Commentary:

;; The same division as `org-semantic-tests': most of these need no
;; server, because what the buffer has to get right is what it does with
;; a reply rather than how it got one.  A hit is a plist, so a fabricated
;; one drives the whole renderer -- which is where the value is, since
;; the things that go wrong here are silent.  A passage drawn against the
;; wrong line numbers still looks like a passage.
;;
;; The rest drive the real binary over the *lexical* index, which needs
;; no embedding model, so the file runs offline in about a second.
;;
;; Run them with:
;;
;;   make test-elisp

;;; Code:

(require 'ert)
(require 'cl-lib)
(require 'org-semantic)
(require 'org-semantic-ui)
(require 'org-semantic-results)
(require 'org-semantic-tests)

(defun org-semantic-results-tests--hit (&rest overrides)
  "A hit as the server sends one, with OVERRIDES applied.

Every key is present, since the server always sends all of them
and the optional ones as null -- which is a distinction the client
has to keep, so a fixture that omitted them would be testing an
easier problem."
  (let ((hit (list :score 0.75 :z 2.1
                   :path "notes/a.org" :file "/vault/notes/a.org"
                   :headingLine 3 :startLine 4 :endLine 6
                   :id nil :title "A" :section nil :heading "A > Section"
                   :tags [] :todo nil :priority nil :lang nil
                   :text "one\ntwo\nthree")))
    (while overrides
      (setq hit (plist-put hit (pop overrides) (pop overrides))))
    hit))

(defun org-semantic-results-tests--targets ()
  "Each address segment on the line at point, as (TARGET LINE TEXT).

Read off the text properties rather than off the rendering,
because the two can disagree in exactly the way that matters: the
first version of this drew a perfectly correct-looking line whose
four links all carried the passage's line, so every one of them
went to the same place."
  (save-excursion
    (goto-char (line-beginning-position))
    (let ((end (line-end-position))
          (out nil))
      (while (< (point) end)
        (let ((target (get-text-property (point) 'org-semantic-target))
              (next (or (next-single-property-change (point) 'org-semantic-target nil end)
                        end)))
          (when target
            (push (list target
                        (get-text-property (point) 'org-semantic-line)
                        (buffer-substring-no-properties (point) next))
                  out))
          (goto-char next)))
      (nreverse out))))

(defmacro org-semantic-results-tests--drawn (hits &rest body)
  "Draw HITS in a results buffer and run BODY there."
  (declare (indent 1))
  `(with-temp-buffer
     (org-semantic-results-mode)
     (setq org-semantic-results--vault "/vault"
           org-semantic-results--query "q"
           org-semantic-results--mode "lexical")
     (org-semantic-results--render (list :hits ,hits))
     ,@body))

(defun org-semantic-results-tests--passage-lines ()
  "The note line each drawn passage line claims, in the order drawn."
  (let ((out nil))
    (save-excursion
      (goto-char (point-min))
      (while (not (eobp))
        (when (get-text-property (line-beginning-position) 'occur-prefix)
          (push (get-text-property (line-beginning-position) 'org-semantic-line)
                out))
        (forward-line 1)))
    (nreverse out)))

(defun org-semantic-results-tests--occurrences (string)
  "How many times STRING appears in the buffer."
  (save-excursion
    (goto-char (point-min))
    (let ((n 0))
      (while (search-forward string nil t) (setq n (1+ n)))
      n)))

(defvar org-semantic-results-tests--asked nil
  "Every prompt `read-char-choice' was handed, most recent first.")

(defmacro org-semantic-results-tests--answering (key &rest body)
  "Run BODY with the offer prompt answered by KEY, recording what was asked.

`read-char-choice' is the question, so standing in for it is how a
keystroke gets made without a terminal.

KEY may be nil, which stands for `C-g': the stub signals `quit'
instead of returning, since that is what a real dismissal does and
the code has to survive it.

What was asked is *reset* rather than rebound, so it outlives BODY
and a test may assert on it afterwards.  A `let' would unwind on the
way out and read as \"nothing was asked\", which is a passing test
for the wrong reason."
  (declare (indent 1))
  `(progn
     (setq org-semantic-results-tests--asked nil)
     (cl-letf (((symbol-function 'read-char-choice)
                (lambda (prompt _chars &optional _inhibit)
                  (push prompt org-semantic-results-tests--asked)
                  (if ,key ,key (signal 'quit nil)))))
       ,@body)))

(defun org-semantic-results-tests--repeated ()
  "The note lines drawn a second time, in the order drawn."
  (let ((out nil))
    (save-excursion
      (goto-char (point-min))
      (while (not (eobp))
        (let ((start (line-beginning-position)))
          (when (and (get-text-property start 'occur-prefix)
                     (not (get-text-property start 'org-semantic-primary)))
            (push (get-text-property start 'org-semantic-line) out)))
        (forward-line 1)))
    (nreverse out)))


;;;; What a score may be written as

(ert-deftest each-part-of-an-address-goes-where-it-says ()
  "Four links, four destinations -- and the line is what tells them apart.

The directory has no line and is revealed instead; the note opens
at its top; the section goes to its heading; the passage goes to
where it starts.  This is asserted on the properties because the
rendering cannot show it: the first version drew exactly this line
with all four links carrying the passage's line, since a final
`propertize' over the whole string overrode what each piece had
already been given."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :path "lit/2024/note.org" :file "/vault/lit/2024/note.org"
             :heading "Note title > Observations"
             :headingLine 12 :startLine 25 :endLine 27))
    (goto-char (point-min))
    (should (re-search-forward "lines 25" nil t))
    (let ((parts (org-semantic-results-tests--targets)))
      (should (equal (mapcar #'car parts) '(directory file heading line line)))
      (should (equal (nth 0 parts) '(directory nil "lit/2024")))
      (should (equal (nth 1 parts) '(file 1 "note.org")))
      (should (equal (nth 2 parts) '(heading 12 "Observations")))
      ;; Both ends of the passage, each reachable on its own.
      (should (equal (nth 3 parts) '(line 25 "25")))
      (should (equal (nth 4 parts) '(line 27 "27"))))))

(ert-deftest a-passage-of-one-line-is-not-a-range ()
  "And a stale one names no range it could not honour.

The span of a passage the note has outgrown cannot be trusted --
that is what makes it stale -- so the block falls back to the one
line it can still reach, which is the heading's."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit :startLine 8 :endLine 8 :text "one"))
    (goto-char (point-min))
    (should (re-search-forward "line 8" nil t))
    (should (equal (org-semantic-results-tests--targets)
                   '((directory nil "notes") (file 1 "a.org")
                     (heading 3 "Section") (line 8 "8")))))
  ;; Stale: three lines of span, no text to fill them.
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit :startLine 8 :endLine 10 :text ""))
    (goto-char (point-min))
    (should (re-search-forward "line 3" nil t))
    (let ((parts (org-semantic-results-tests--targets)))
      (should (equal (mapcar #'car parts) '(directory file heading line)))
      (should (equal (nth 3 parts) '(line 3 "3"))))))

(ert-deftest only-a-link-answers-the-mouse ()
  "A passage is text: selectable, unlit, and click-to-place-point.

Every line a hit was drawn on used to carry `mouse-face',
`follow-link' and a keymap, which made the whole result one large
button -- the passage lit up under the pointer and a click jumped
instead of placing point, so the text could not be selected.  The
head's own separators and score had them too, promising a middle
click that went nowhere in particular.

Keyboard navigation is unaffected and deliberately so: RET comes
from the major mode's map, not from the text, so going to the line
under point survives having no mouse affordance on it."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit))
    (goto-char (point-min))
    (let ((lit 0) (inert 0))
      (while (not (eobp))
        (let ((target (get-text-property (point) 'org-semantic-target))
              (mouse (get-text-property (point) 'mouse-face)))
          (if target
              (progn (setq lit (1+ lit))
                     (should (eq mouse 'highlight))
                     (should (get-text-property (point) 'follow-link))
                     (should (get-text-property (point) 'keymap)))
            (setq inert (1+ inert))
            ;; Everything that is not a link: the score, the separators,
            ;; the tags, and every line of the passage itself.
            (should-not mouse)
            (should-not (get-text-property (point) 'follow-link))
            (should-not (get-text-property (point) 'keymap))))
        (forward-char 1))
      (should (> lit 0))
      (should (> inert 0)))
    ;; And the passage lines still say which note line they are, which is
    ;; what RET, `n' and `next-error' all read.
    (should (equal (org-semantic-results-tests--passage-lines) '(4 5 6)))))

(ert-deftest the-gutter-is-the-same-width-on-every-passage-line ()
  "Including the lines a repeat dims, which used to lose it.

The dimming was applied over the whole line, so it overrode the
gutter's own face and a repeated line came out with four columns of
margin where its neighbours had none, or the other way about.  The
left edge went ragged for a reason no reader could see.

Also: the gutter must not be the face `display-line-numbers' uses.
Many themes give `line-number' a *background*, which painted the
blank gutter as a grey block marking a margin that carries nothing."
  (org-semantic-results-tests--drawn
      ;; The second hit repeats two of the first's lines, so the claim map
      ;; dims them.
      (list (org-semantic-results-tests--hit :startLine 4 :endLine 6)
            (org-semantic-results-tests--hit :startLine 6 :endLine 8
                                             :text "three\nfour\nfive"))
    (let ((widths nil))
      (goto-char (point-min))
      (while (not (eobp))
        (when (get-text-property (line-beginning-position) 'occur-prefix)
          (let ((p (line-beginning-position)) (n 0))
            (while (eq (get-text-property p 'face) 'org-semantic-results-gutter)
              (setq n (1+ n) p (1+ p)))
            (push n widths)))
        (forward-line 1))
      (should (> (length widths) 3))
      (should (equal (delete-dups (copy-sequence widths)) '(4))))
    ;; And a dimmed line is still dimmed -- the fix moved the face, not
    ;; removed it.
    (goto-char (point-min))
    (should (re-search-forward "^    three$" nil t))
    (should (re-search-forward "^    three$" nil t))
    (goto-char (+ 4 (line-beginning-position)))
    (should (eq (get-text-property (point) 'face) 'org-semantic-results-duplicate)))
  (should-not (eq (face-attribute 'org-semantic-results-gutter :inherit) 'line-number)))

(ert-deftest hidden-lines-are-counted-and-not-also-elided-by-emacs ()
  "One statement of what was folded away, in the right place.

`(SYMBOL . t)' in `buffer-invisibility-spec' asks Emacs to draw its
own `...' where the hidden text was -- at the end of the last
visible line, saying less precisely what `⋯ 2 lines' already says
one line below.  The symbol alone hides without commentary."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :startLine 1 :endLine 5 :text "one\ntwo\nthree\nfour\nfive"))
    (let ((org-semantic-results-passage-lines 3))
      (org-semantic-results--render
       (list :hits (list (org-semantic-results-tests--hit
                          :startLine 1 :endLine 5
                          :text "one\ntwo\nthree\nfour\nfive")))))
    (should (memq 'org-semantic-results buffer-invisibility-spec))
    (should-not (assq 'org-semantic-results buffer-invisibility-spec))
    ;; The label says how many, and lines up with the text it stands for.
    (goto-char (point-min))
    (should (re-search-forward "^    ⋯ 2 lines$" nil t))))

(ert-deftest the-note-title-is-not-repeated-beside-its-filename ()
  "The heading begins with the title, which the file already names.

Dropping it is a trade, not a free win: it costs the notes whose
title says what the filename does not.  It is taken because those
are rare -- 3 of 88 hits on the vault this was measured against."
  (should (equal (org-semantic-results--sections
                  '(:heading "Note title > Observations > Deeper"))
                 "Observations > Deeper"))
  ;; A hit on the note itself has no section below it, and so no segment.
  (should-not (org-semantic-results--sections '(:heading "Note title")))
  (should-not (org-semantic-results--sections '(:heading nil))))

(ert-deftest a-note-at-the-vault-root-has-no-directory-part ()
  "And no separator introducing one that is not there."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :path "README.org" :file "/vault/README.org"
             :heading "Imported notes" :startLine 8 :endLine 8 :text "one"))
    (goto-char (point-min))
    (should (re-search-forward "line 8" nil t))
    (let ((parts (org-semantic-results-tests--targets)))
      (should (equal (mapcar #'car parts) '(file line)))
      (should-not (string-match-p " / " (buffer-substring-no-properties
                                         (line-beginning-position)
                                         (line-end-position)))))))

(ert-deftest only-the-leading-passage-of-a-section-carries-the-address ()
  "The passages after it name their line alone.

The path is already above them, and only the line has changed."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit :startLine 4 :endLine 6)
            (org-semantic-results-tests--hit :startLine 40 :endLine 42
                                             :text "ten\neleven\ntwelve"))
    (goto-char (point-min))
    (should (re-search-forward "lines 4–6$" nil t))
    (should (equal (mapcar #'car (org-semantic-results-tests--targets))
                   '(directory file heading line line)))
    (should (re-search-forward "lines 40–42$" nil t))
    (should (equal (mapcar #'car (org-semantic-results-tests--targets))
                   '(line line)))))

(ert-deftest revealing-a-directory-is-the-user-s-to-replace ()
  "A directory is the one target that is not a place in a note.

So it is the one reached through a function rather than through a
line, and the function is a defcustom because Dired is only what
Emacs happens to have."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit))
    ;; Found by its property, not by its text: the vault path appears in
    ;; the header too, and matching that would test the header.
    (goto-char (point-min))
    (while (and (not (eobp))
                (not (eq (get-text-property (point) 'org-semantic-target) 'directory)))
      (forward-char 1))
    (should (eq (get-text-property (point) 'org-semantic-target) 'directory))
    (let (asked)
      (let ((org-semantic-results-reveal-function
             (lambda (directory file) (setq asked (list directory file)))))
        (org-semantic-results-goto))
      (should (equal asked '("/vault/notes/" "/vault/notes/a.org"))))))

(ert-deftest the-prefixes-ask-the-common-question-first ()
  "One `C-u' asks the ranking; two ask about the length of the list too.

Choosing between meaning and word is the common reason to reach for
a prefix, and it used to drag two questions about list length
behind it -- three answers to change one thing.  Swapping the two
levels fails nothing and looks like nothing, which is why the
mapping is a function and this test holds it."
  (should (equal (org-semantic--find-prompts nil) '(nil . nil)))
  (should (equal (org-semantic--find-prompts '(4)) '(t . nil)))
  (should (equal (org-semantic--find-prompts '(16)) '(t . t)))
  ;; Deeper, and a plain number, still resolve to one of the two.
  (should (equal (org-semantic--find-prompts '(64)) '(t . t)))
  (should (equal (org-semantic--find-prompts 3) '(t . nil))))

(ert-deftest the-ranking-prompt-offers-both-rankings ()
  "The setting is the default, not text typed into the minibuffer.

Passed as INITIAL-INPUT -- which `completing-read' calls deprecated
-- the current ranking is what a completion UI filters its
candidates by, so a prompt for choosing between two offered one:
whichever you already had, never the one you were reaching for."
  (let (seen)
    (cl-letf (((symbol-function 'completing-read)
               (lambda (_prompt collection &optional _pred _match initial _hist def &rest _)
                 (setq seen (list :collection collection :initial initial :default def))
                 "lexical")))
      (let ((org-semantic-results-ranking "semantic"))
        (should (equal (org-semantic--read-ranking) "lexical"))))
    ;; Both, in a stable order, whatever shape the table takes.
    (should (equal (all-completions "" (plist-get seen :collection))
                   '("semantic" "lexical")))
    (should-not (plist-get seen :initial))
    (should (equal (plist-get seen :default) "semantic"))))

(ert-deftest each-ranking-says-which-index-it-reads ()
  "Two words alone invite the wrong guess: one search, ordered twice.

They are separate indexes, built by separate commands and never
merged, so the prompt names the index each one reads.  The
annotation rides the table's own metadata rather than
`completion-extra-properties', which is global state a front-end
may rebind."
  (let (table)
    (cl-letf (((symbol-function 'completing-read)
               (lambda (_prompt collection &rest _) (setq table collection) "semantic")))
      (org-semantic--read-ranking))
    ;; The table carries the annotation itself.
    (should (equal (completion-metadata "" table nil)
                   '(metadata (annotation-function . org-semantic--ranking-annotation)))))
  ;; And the annotations distinguish the two by *index*, not by adjective.
  (let ((semantic (org-semantic--ranking-annotation "semantic"))
        (lexical (org-semantic--ranking-annotation "lexical")))
    (should (string-match-p "meaning" semantic))
    (should (string-match-p "embedding index" semantic))
    (should (string-match-p "word" lexical))
    (should (string-match-p "BM25 index" lexical))
    (should-not (equal semantic lexical)))
  ;; Nothing else is offered, so nothing else can go unexplained.
  (should-not (org-semantic--ranking-annotation "fused")))

(ert-deftest a-search-asks-for-one-ranking-and-never-both ()
  "The two are ranked separately, so a search names exactly one.

A BM25 score has no common scale with a cosine -- not even with
another model's cosine -- so a merged list would order results by a
number that means different things in different rows."
  (let (params)
    (cl-letf (((symbol-function 'org-semantic-search-async)
               (lambda (&rest args) (setq params args) nil))
              ((symbol-function 'org-semantic-connection) #'ignore))
      (org-semantic-results-tests--drawn nil
        (setq org-semantic-results--mode "lexical")
        (org-semantic-results--search)))
    (should (equal (plist-get (cdr params) :mode) "lexical"))
    ;; One value, not a pair and not a list: there is no shape here that
    ;; could carry two rankings.
    (should (stringp (plist-get (cdr params) :mode)))))

(ert-deftest every-prompt-remembers-what-was-searched-for ()
  "One history, shared by all three prompts, and an interned symbol.

Interned matters and is not decoration: `savehist-minibuffer-hook'
records whichever variable a minibuffer used, but skips one whose
symbol it cannot `intern-soft' -- so an uninterned history is the
one thing that would silently fail to survive a session.

The three prompts share it because they are the same question
asked from different places; separate histories would mean `M-p'
in the results buffer could not reach what was typed to get there."
  (let ((prompts nil))
    (cl-letf (((symbol-function 'read-string)
               (lambda (_prompt &optional initial history default &rest _)
                 (push (list history initial default) prompts)
                 "typed"))
              ((symbol-function 'org-semantic-find) #'ignore)
              ((symbol-function 'org-semantic-results--search) #'ignore)
              ((symbol-function 'use-region-p) #'ignore)
              ((symbol-function 'thing-at-point) (lambda (&rest _) "atpoint")))
      (call-interactively #'org-semantic-find-at-point)
      (let ((org-semantic-results--query "old"))
        (call-interactively #'org-semantic-results-set-query)))
    (should (= 2 (length prompts)))
    (should (cl-every (lambda (p) (eq (nth 0 p) 'org-semantic-search-history)) prompts))
    (should (intern-soft "org-semantic-search-history"))
    ;; The thing at point is a suggestion, so it arrives as the default;
    ;; the query being refined is text to edit, so it arrives as initial.
    (let ((at-point (car (last prompts)))
          (refine (car prompts)))
      (should (equal (nth 2 at-point) "atpoint"))
      (should-not (nth 1 at-point))
      (should (equal (nth 1 refine) "old")))))

(ert-deftest the-buffer-asks-to-be-shown-and-does-not-insist ()
  "The action is passed to `display-buffer', not written into the user's alist.

`display-buffer-alist' is consulted first, so a preference passed
this way is overridden by anything the user set without either
side having to know about the other.  Writing to that option
instead would put this package in front of what the user asked
for, which is why the test is that the action *travels* rather
than that a window appears somewhere."
  (let (action)
    (cl-letf (((symbol-function 'pop-to-buffer)
               (lambda (_buffer &optional a &rest _) (setq action a)))
              ((symbol-function 'org-semantic-results--search) #'ignore)
              ((symbol-function 'org-semantic-vault-or-error) (lambda (&rest _) "/vault"))
              ((symbol-function 'read-string) (lambda (&rest _) "q")))
      (let ((org-semantic-results-display-action '(a-deliberate-marker)))
        (org-semantic-find "q")
        (should (equal action '(a-deliberate-marker)))))))

(ert-deftest a-word-score-never-grows-a-sigma ()
  "BM25 is unbounded and comparable with nothing, so it gets no scale.

The server sends no `z' for a word hit precisely because there is
no floor to stand one against.  Inventing a sigma, a percentage or
a bar for it would be measuring a scale that does not exist."
  (should (equal (org-semantic-ui-score
                  (org-semantic-results-tests--hit :score 11.9 :z nil))
                 "11.900"))
  ;; And a meaning score is never shown without one, since the raw
  ;; cosine sits on a large per-model offset and says nothing alone.
  (should (equal (org-semantic-ui-score
                  (org-semantic-results-tests--hit :score 0.883 :z 2.14))
                 "0.883 (+2.1σ)")))

(ert-deftest a-candidate-carries-its-hit ()
  "The string and the hit travel together, so nothing keeps a list in step."
  (let* ((hit (org-semantic-results-tests--hit))
         (candidate (org-semantic-ui-candidate hit)))
    (should (string-match-p "A > Section" candidate))
    (should (eq (org-semantic-ui-candidate-hit candidate) hit))
    (should-not (org-semantic-ui-candidate-hit "not a candidate"))))


;;;; Grouping

(ert-deftest a-flat-list-of-hits-regroups-into-notes-and-sections ()
  "The wire is flat and a note's hits do not arrive together.

The server ranks every section against every other one, so two
sections of one note are separated in the list by whatever scored
between them.  Gathering the note back up is the client's job."
  (let* ((a1 (org-semantic-results-tests--hit :file "/v/a.org" :headingLine 3))
         (b (org-semantic-results-tests--hit :file "/v/b.org" :headingLine 3))
         (a2 (org-semantic-results-tests--hit :file "/v/a.org" :headingLine 40))
         (groups (org-semantic-results--group (list a1 b a2))))
    (should (equal (mapcar #'car groups) '("/v/a.org" "/v/b.org")))
    ;; Both of a.org's sections, under the one note, in first-seen order.
    (should (equal (mapcar #'car (cdr (assoc "/v/a.org" groups))) '(3 40)))
    (should (equal (mapcar #'car (cdr (assoc "/v/b.org" groups))) '(3)))))

(ert-deftest two-sections-that-spell-their-heading-the-same-are-two-sections ()
  "Grouped on the heading's line, never on its text.

The server groups on the text, so a note carrying two sections
whose outline path spells the same -- a year of meetings, each
with its own Decisions -- hands them over as one group with two
different heading lines.  Keyed on the text they would collapse
into one, and half the passages would be filed under the other
one's line."
  (let* ((one (org-semantic-results-tests--hit
               :file "/v/m.org" :headingLine 10 :heading "M > Decisions"))
         (two (org-semantic-results-tests--hit
               :file "/v/m.org" :headingLine 90 :heading "M > Decisions"))
         (groups (org-semantic-results--group (list one two))))
    (should (equal (mapcar #'car (cdr (assoc "/v/m.org" groups))) '(10 90)))))

(ert-deftest a-section-is-read-in-the-order-the-note-has-it ()
  "Ranking chooses which sections to show; it does not order one.

The passages of a section are pieces of one continuous text, and
they arrive ranked -- with BM25 they arrive tied, so the order was
whatever the sort happened to leave.  Read in line order they also
make the overlap sensible: the earlier passage owns the paragraph
they share."
  (let* ((late (org-semantic-results-tests--hit
                :startLine 20 :endLine 22 :score 0.9))
         (early (org-semantic-results-tests--hit
                 :startLine 4 :endLine 6 :score 0.9))
         (groups (org-semantic-results--group (list late early)))
         (section (cdr (car (cdr (car groups))))))
    (should (equal (mapcar (lambda (h) (plist-get h :startLine)) section)
                   '(4 20)))))


;;;; Drawing a passage

(ert-deftest a-passage-is-shown-as-the-lines-it-came-from ()
  "The nth line of the text is line START-LINE + n of the note.

The whole of the buffer's addressing rests on this: the server
sends the note's own lines joined, so each drawn line can carry
its own number and be gone to on its own.  Nothing else here would
fail if it stopped being true."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :startLine 10 :endLine 12 :text "alpha\nbeta\ngamma"))
    (should (equal (org-semantic-results-tests--passage-lines) '(10 11 12)))
    ;; And the note's text is drawn exactly as the note has it: the
    ;; gutter carries everything drawn, so that the correspondence above
    ;; stays checkable against the file.
    (goto-char (point-min))
    (should (re-search-forward "^ +alpha$" nil t))))

(ert-deftest a-passage-the-note-outgrew-claims-no-line ()
  "An empty passage is not a blank line, and offers no jump.

The server sends an empty string when the note has been cut
shorter than the span the index recorded.  Drawn as one blank
line it would claim a line number it cannot honour, and RET would
go to the wrong place with nothing looking wrong."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :startLine 10 :endLine 12 :text ""))
    (should (equal (org-semantic-results-tests--passage-lines) nil))
    (goto-char (point-min))
    (should (re-search-forward "could not be read" nil t))
    ;; Nothing on that line offers to go anywhere.
    (should-not (get-text-property (point) 'org-semantic-line))))

(ert-deftest a-line-shown-twice-is-owned-once ()
  "Passages overlap on purpose, so one drawing of a line owns it.

Consecutive passages of a section begin with the last paragraph of
the one before, so an idea cut in half is whole in both.  Whoever
draws a line first owns it; the repeat is dimmed and marked, which
is what an editable version would have to know before writing
anything back."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :startLine 4 :endLine 6 :text "one\ntwo\nthree")
            (org-semantic-results-tests--hit
             :startLine 6 :endLine 8 :text "three\nfour\nfive"))
    (should (equal (org-semantic-results-tests--passage-lines) '(4 5 6 6 7 8)))
    ;; Line 6 is drawn twice and owned by the first drawing of it.
    (should (equal (org-semantic-results-tests--repeated) '(6)))))

(ert-deftest a-passage-with-nothing-left-to-show-is-not-repeated ()
  "A paragraph too long to split yields passages naming all of it.

Every remnant of such a paragraph carries the whole paragraph's
span, so the server can send two hits whose text is identical.
Drawing the second says nothing the first did not."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit
             :startLine 4 :endLine 5 :text "one\ntwo")
            (org-semantic-results-tests--hit
             :startLine 4 :endLine 5 :text "one\ntwo"))
    (should (equal (org-semantic-results-tests--passage-lines) '(4 5)))
    (goto-char (point-min))
    (should (re-search-forward "1 passage left out" nil t))))


;;;; Getting about

(ert-deftest next-error-visits-the-passages-in-the-order-they-are-shown ()
  "And a move of nothing moves nothing, which is what follow mode needs.

`next-error-follow-minor-mode' calls the function with an ARG of
zero after every command, meaning \"the one point is on\".  Made to
step by one instead, it would walk away down the buffer on its
own."
  (let ((went nil))
    (cl-letf (((symbol-function 'org-semantic-ui-visit)
               (lambda (file line &rest _) (push (cons file line) went) nil))
              ((symbol-function 'next-error-found) #'ignore))
      (org-semantic-results-tests--drawn
          (list (org-semantic-results-tests--hit
                 :file "/v/a.org" :startLine 4 :endLine 5 :text "one\ntwo")
                (org-semantic-results-tests--hit
                 :file "/v/b.org" :headingLine 9 :startLine 9 :endLine 9
                 :text "only"))
        (org-semantic-results--next-error 0 t)
        (org-semantic-results--next-error 1 nil)
        ;; Each block goes to where its passage starts, not to the
        ;; heading that owns it -- a.org's section begins at line 3 and
        ;; the passage that matched at line 4.
        (should (equal (nreverse went)
                       '(("/v/a.org" . 4) ("/v/b.org" . 9))))
        ;; A move of zero shows the same one again, from the same place.
        (setq went nil)
        (let ((before (point)))
          (org-semantic-results--next-error 0 nil)
          (should (= before (point))))
        (should (equal went '(("/v/b.org" . 9))))))))

(ert-deftest a-passage-is-gone-to-by-the-line-under-point ()
  "Not by the top of its section: the buffer shows every line separately.

A section runs to hundreds of lines, so going to its heading
whichever line was clicked would land a long way from the words
that matched."
  (let ((went nil))
    (cl-letf (((symbol-function 'org-semantic-ui-visit)
               (lambda (file line &rest _) (push (cons file line) went) nil))
              ((symbol-function 'next-error-found) #'ignore))
      (org-semantic-results-tests--drawn
          (list (org-semantic-results-tests--hit
                 :file "/v/a.org" :headingLine 3
                 :startLine 10 :endLine 12 :text "alpha\nbeta\ngamma"))
        (goto-char (point-min))
        (should (re-search-forward "beta" nil t))
        (org-semantic-results-goto)
        (should (equal went '(("/v/a.org" . 11))))))))


;;;; Reaching a note

(ert-deftest a-narrowed-note-is-widened-before-a-line-is-counted ()
  "Line numbers are counted over the whole note, so a restriction must go.

A buffer narrowed to some other subtree counts from its own
beginning, so the jump lands somewhere else entirely and nothing
looks wrong -- there is a passage on the screen either way."
  (let ((file (make-temp-file "org-semantic-visit" nil ".org")))
    (unwind-protect
        (progn
          (with-temp-file file
            (insert "one\ntwo\nthree\nfour\nfive\nsix\n"))
          (let ((buffer (find-file-noselect file)))
            (unwind-protect
                (progn
                  (with-current-buffer buffer
                    (narrow-to-region (point-min) (point-min))
                    (should (buffer-narrowed-p)))
                  (org-semantic-ui-visit file 4)
                  (with-current-buffer buffer
                    (should-not (buffer-narrowed-p))
                    (should (equal (buffer-substring (line-beginning-position)
                                                     (line-end-position))
                                   "four"))))
              (kill-buffer buffer))))
      (delete-file file))))

(ert-deftest showing-a-note-is-not-going-to-it ()
  "Without SELECT the note is displayed and this buffer keeps point.

Which is what a preview needs, and what `next-error-no-select'
assumes -- it puts the selected window back itself, so the
non-selecting case has to be a real one and not a jump undone
afterwards."
  (let ((file (make-temp-file "org-semantic-visit" nil ".org")))
    (unwind-protect
        (progn
          (with-temp-file file (insert "one\ntwo\nthree\n"))
          (let ((buffer (find-file-noselect file)))
            (unwind-protect
                (let ((window (org-semantic-ui-visit file 2)))
                  (should (eq (window-buffer window) buffer))
                  ;; Point is placed in that window, not merely shown.
                  (should (= (with-current-buffer buffer
                               (line-number-at-pos (window-point window)))
                             2)))
              (kill-buffer buffer))))
      (delete-file file))))


;;;; Folding a long passage

(ert-deftest a-folded-passage-comes-back-and-goes-away-again ()
  "TAB is the only thing here that rewrites the buffer after it is drawn.

Both halves matter: the hidden lines are still in the buffer, so
they carry their numbers and would still be written back, and the
marker has to stop claiming lines are folded once they are not."
  (let ((org-semantic-results-passage-lines 3))
    (org-semantic-results-tests--drawn
        (list (org-semantic-results-tests--hit
               :startLine 4 :endLine 9
               :text "one\ntwo\nthree\nfour\nfive\nsix"))
      ;; Hidden, but drawn: every line still claims its own number.
      (should (equal (org-semantic-results-tests--passage-lines)
                     '(4 5 6 7 8 9)))
      (goto-char (point-min))
      (org-semantic-results--first-item)
      (let* ((item (org-semantic-results--item-at-point))
             (bounds (org-semantic-results--elided-bounds item)))
        (should (= (org-semantic-results--item-elided item) 3))
        (should (eq (get-text-property (car bounds) 'invisible)
                    'org-semantic-results))
        (should (string-match-p "⋯ 3 lines" (buffer-string)))
        (org-semantic-results-toggle-passage)
        (should-not (get-text-property
                     (car (org-semantic-results--elided-bounds item))
                     'invisible))
        (should-not (string-match-p "⋯ 3 lines" (buffer-string)))
        (org-semantic-results-toggle-passage)
        (should (eq (get-text-property
                     (car (org-semantic-results--elided-bounds item))
                     'invisible)
                    'org-semantic-results))
        (should (string-match-p "⋯ 3 lines" (buffer-string)))))))

(ert-deftest a-passage-that-fits-has-nothing-to-unfold ()
  "And says so rather than doing nothing, which reads as a broken key."
  (org-semantic-results-tests--drawn
      (list (org-semantic-results-tests--hit))
    (goto-char (point-min))
    (org-semantic-results--first-item)
    (should-error (org-semantic-results-toggle-passage) :type 'user-error)))


;;;; Asking again, differently

(ert-deftest g-asks-again-rather-than-redrawing ()
  "The notes may have moved on, so what is here is a picture and not a cache."
  (let ((asked 0))
    (cl-letf (((symbol-function 'org-semantic-ui-ask)
               (lambda (_driver _params) (cl-incf asked))))
      (org-semantic-results-tests--drawn
          (list (org-semantic-results-tests--hit))
        (revert-buffer)
        (should (= asked 1))))))

(ert-deftest the-two-limits-say-which-of-them-moved ()
  "`+' counts notes, not hits, which is the surprising half of the interface.

A vault kept in a few large files answers a large k with very few
hits, and no amount of raising it helps until the per-note limit
moves too -- so the message names both."
  (let ((asked 0) (said nil))
    (cl-letf (((symbol-function 'org-semantic-ui-ask)
               (lambda (_driver _params) (cl-incf asked)))
              ((symbol-function 'message)
               (lambda (fmt &rest args) (setq said (apply #'format fmt args)))))
      (org-semantic-results-tests--drawn
          (list (org-semantic-results-tests--hit))
        (org-semantic-results-more-notes)
        (should (= org-semantic-results--k 16))
        (should (string-match-p "16 notes" said))
        (should (string-match-p "passages each" said))
        (org-semantic-results-fewer-notes)
        (should (= org-semantic-results--k 8))
        ;; Never to nothing: a k of zero answers with an empty list and
        ;; looks exactly like a query that matched nothing.
        (dotimes (_ 6) (org-semantic-results-fewer-notes))
        (should (= org-semantic-results--k 1))
        (should (= asked 8))))))

(ert-deftest matching-any-of-the-words-is-a-question-only-about-words ()
  "An embedding has no terms to match any of, so the key refuses.

The server ignores `any' on a semantic search rather than
failing, which would make the key look as though it had worked."
  (cl-letf (((symbol-function 'org-semantic-ui-ask) #'ignore))
    (org-semantic-results-tests--drawn
        (list (org-semantic-results-tests--hit))
      (setq org-semantic-results--mode "semantic")
      (should-error (org-semantic-results-toggle-any) :type 'user-error)
      (should-not org-semantic-results--any)
      (setq org-semantic-results--mode "lexical")
      (org-semantic-results-toggle-any)
      (should org-semantic-results--any))))


;;;; One search in flight

(ert-deftest at-most-one-search-is-ever-in-flight ()
  "The queue is one deep, and what is in it is the most recent ask.

Nothing on the server supersedes a search, so a client that fires
per keystroke gets a reply per keystroke.  Holding the latest and
firing it from the previous reply bounds the queue with no
protocol at all."
  (let ((sent nil) (settle nil) (id 0))
    (cl-letf (((symbol-function 'org-semantic-search-async)
               (lambda (query &rest keys)
                 (push query sent)
                 (setq settle (plist-get keys :success))
                 (setq id (1+ id)))))
      (let ((driver (org-semantic-ui-driver-create)))
        (org-semantic-ui-ask driver '(:query "a" :vault "/v"))
        (org-semantic-ui-ask driver '(:query "b" :vault "/v"))
        (org-semantic-ui-ask driver '(:query "c" :vault "/v"))
        ;; Only the first went out; b was replaced by c while it waited.
        (should (equal sent '("a")))
        (funcall settle '(:hits []))
        (should (equal (nreverse sent) '("a" "c")))
        ;; And nothing is left waiting behind it.
        (should-not (org-semantic-ui-driver-pending driver))))))

(ert-deftest a-failure-does-not-wedge-the-driver ()
  "The request is cleared before anything else, on both paths.

Left set by a failure, the driver would be permanently in flight
and would never ask for anything again -- and nothing would say
so, since the buffer would simply stop changing."
  (let ((sent nil) (fail nil))
    (cl-letf (((symbol-function 'org-semantic-search-async)
               (lambda (query &rest keys)
                 (push query sent)
                 (setq fail (plist-get keys :failure))
                 1)))
      (let ((driver (org-semantic-ui-driver-create)))
        (org-semantic-ui-ask driver '(:query "a" :vault "/v"))
        (funcall fail '(:message "no"))
        (should-not (org-semantic-ui-driver-request driver))
        (org-semantic-ui-ask driver '(:query "b" :vault "/v"))
        (should (equal (nreverse sent) '("a" "b")))))))

(ert-deftest an-abandoned-driver-answers-nobody ()
  "A killed buffer's reply has nowhere to go, so it is dropped.

Only abandoning does this.  A reply overtaken by a newer query is
*not* stale -- it is the best anyone has until the next one lands,
and dropping it would blank the buffer for a round trip."
  (let ((sent nil) (settle nil) (answered 0))
    (cl-letf (((symbol-function 'org-semantic-search-async)
               (lambda (query &rest keys)
                 (push query sent)
                 (setq settle (plist-get keys :success))
                 1)))
      (let ((driver (org-semantic-ui-driver-create
                     :on-reply (lambda (_reply) (cl-incf answered)))))
        (org-semantic-ui-ask driver '(:query "a" :vault "/v"))
        (org-semantic-ui-ask driver '(:query "b" :vault "/v"))
        (org-semantic-ui-driver-abandon driver)
        (funcall settle '(:hits []))
        (should (= answered 0))
        ;; And what was waiting is gone rather than fired.
        (should (equal sent '("a")))))))


;;;; What an error offers

(ert-deftest an-error-with-nothing-to-decide-offers-nothing ()
  "Absence of a label is the signal, not an omission.

The server labels what a client must act on, so an error with no
`kind' is one to show and nothing else."
  (let ((remedy (org-semantic-ui-remedy '(:message "something went wrong"))))
    (should-not (org-semantic-ui-remedy-kind remedy))
    (should (equal (org-semantic-ui-remedy-message remedy)
                   "something went wrong"))
    (should-not (org-semantic-ui-remedy-offers remedy))))

(ert-deftest an-offer-comes-from-the-remedy-and-not-from-the-prose ()
  "`remedy' is the machine form, sent so that nobody parses a sentence."
  (let ((remedy (org-semantic-ui-remedy
                 '(:message "no semantic index"
                   :data (:kind "no-index" :remedy "index"))
                 "semantic")))
    (should (equal (org-semantic-ui-remedy-kind remedy) "no-index"))
    (should (equal (cdr (assoc "Build it" (org-semantic-ui-remedy-offers remedy)))
                   'index))
    ;; The word index costs seconds and is often already there.
    (should (rassq 'lexical (org-semantic-ui-remedy-offers remedy))))
  ;; Which is not worth offering to a search that is already by word.
  (let ((remedy (org-semantic-ui-remedy
                 '(:message "no lexical index"
                   :data (:kind "no-index" :remedy "index"))
                 "lexical")))
    (should-not (rassq 'lexical (org-semantic-ui-remedy-offers remedy))))
  ;; And a layout too old to read asks for the expensive one.
  (let ((remedy (org-semantic-ui-remedy
                 '(:message "written under an older layout"
                   :data (:kind "index-layout" :remedy "reindex-full")))))
    (should (rassq 'index-full (org-semantic-ui-remedy-offers remedy)))))

(ert-deftest a-missing-model-is-offered-as-a-download ()
  "The index is here and the weights are not, so the offer fetches weights.

`download' and emphatically not `index'.  It was `index' once, and
that was a bug rather than a shorthand: an incremental run on a
vault whose notes have not changed embeds nothing, so it loads no
model, fetches nothing, and reports success -- after which the
search refuses in the very same words.  Reproduced end to end
before this changed."
  (let* ((remedy (org-semantic-ui-remedy
                  '(:message "the bge-small-en model is not downloaded yet"
                    :data (:kind "model-missing" :model "bge-small-en"
                           :remedy "download"))
                  "semantic"))
         (offers (org-semantic-ui-remedy-offers remedy)))
    (should (equal (org-semantic-ui-remedy-kind remedy) "model-missing"))
    (should (equal (cdr (assoc "Download it" offers)) 'download))
    (should-not (rassq 'index offers))
    (should-not (rassq 'index-full offers))
    (should-not (assoc "Build it" offers))
    ;; The word index needs no model at all, so it is worth offering.
    (should (rassq 'lexical offers))))

(ert-deftest a-fetch-already-running-is-not-offered-again ()
  "Typing keeps asking, and every answer must not invite a second start.

A search-as-you-type client repeats the query per keystroke, so it
meets this refusal over and over.  Offering to download each time
would send the user into an error of a different kind, since one
index per vault is all there is -- so the error carries whether a
run is already fetching, which is the one error that has to."
  (let ((offers (org-semantic-ui-remedy-offers
                 (org-semantic-ui-remedy
                  '(:message "the e5-small model is being downloaded now"
                    :data (:kind "model-missing" :model "e5-small"
                           :remedy "download" :indexing t))
                  "semantic"))))
    (should-not (rassq 'download offers))
    (should (rassq 'retry offers)))
  ;; And `:json-false' is not nil, which is the trap that would make every
  ;; refusal look like a fetch in progress.
  (let ((offers (org-semantic-ui-remedy-offers
                 (org-semantic-ui-remedy
                  '(:message "the e5-small model is not downloaded yet"
                    :data (:kind "model-missing" :model "e5-small"
                           :remedy "download" :indexing :json-false))
                  "semantic"))))
    (should (rassq 'download offers))
    (should-not (rassq 'retry offers))))

(ert-deftest a-condition-that-holds-is-said-once-however-often-it-is-met ()
  "The point of the latch, and the reason live search needs one.

Both of these describe the vault rather than the request, so
asking again cannot answer differently.  Ten keystrokes would
otherwise redraw the same prompt ten times."
  (dolist (kind '("config-drift" "model-missing"))
    (let ((error-object (list :message (format "%s happened" kind)
                              :data (list :kind kind :remedy "index"))))
      (with-temp-buffer
        (org-semantic-results-mode)
        (setq org-semantic-results--vault "/vault"
              org-semantic-results--query "q")
        (org-semantic-results-tests--answering ?q
          (dotimes (_ 5) (org-semantic-results--render-error error-object)))
        (should (member kind org-semantic-results--latched))
        ;; Asked once, however many times the condition was met.  This is
        ;; what the latch is for now that the offer is a question: five
        ;; prompts would be five stolen keystrokes.
        (should (= 1 (length org-semantic-results-tests--asked)))
        ;; And still said each time, as a line -- silence would read as
        ;; the search having quietly worked.
        (should (= 5 (org-semantic-results-tests--occurrences
                      (format "%s happened" kind)))))))
  ;; A kind that is *not* latched is drawn in full every time, because the
  ;; next request really may answer differently.
  (with-temp-buffer
    (org-semantic-results-mode)
    (setq org-semantic-results--vault "/vault" org-semantic-results--query "q")
    (org-semantic-results-tests--answering ?q
      (dotimes (_ 3)
        (org-semantic-results--render-error
         '(:message "no such model" :data (:kind "unknown-model" :known ["a"]))))
      (should-not org-semantic-results--latched)
      ;; Asked all three times, since the next request really may differ.
      (should (= 3 (length org-semantic-results-tests--asked))))))

(ert-deftest the-drift-prompt-is-raised-once ()
  "A drifted policy holds until the user acts, so it is said once.

Said in full on every reply, a search-as-you-type buffer would ask
the same question on every keystroke."
  (let ((error-object '(:message "the policy has changed"
                        :data (:kind "config-drift" :remedy "reindex-full"
                               :changed ["languages"]))))
    (with-temp-buffer
      (org-semantic-results-mode)
      (setq org-semantic-results--vault "/vault"
            org-semantic-results--query "q")
      (org-semantic-results-tests--answering ?q
        (org-semantic-results--render-error error-object)
        (should (member "config-drift" org-semantic-results--latched))
        (should (= 1 (length org-semantic-results-tests--asked)))
        (org-semantic-results--render-error error-object)
        ;; Said again, but as a line rather than asked again.
        (should (string-match-p "the policy has changed" (buffer-string)))
        (should (= 1 (length org-semantic-results-tests--asked)))))))

(ert-deftest an-offer-is-asked-in-the-minibuffer-and-not-drawn ()
  "The offers are a question now, not a row of buttons in the buffer.

What the buffer keeps is the sentence: a question is asked once and
then gone, so a buffer that said nothing would leave someone
looking at an empty list with no account of why.  It must not keep
the buttons as well -- two ways to answer one question, one of them
already dismissed."
  (with-temp-buffer
    (org-semantic-results-mode)
    (setq org-semantic-results--vault "/vault"
          org-semantic-results--query "q"
          org-semantic-results--mode "semantic")
    (org-semantic-results-tests--answering ?q
      (org-semantic-results--render-error
       '(:message "the e5-small model is not downloaded yet"
         :data (:kind "model-missing" :model "e5-small" :remedy "download"))))
    (should (string-match-p "not downloaded yet" (buffer-string)))
    ;; Nothing to press: no buttons, and no bracketed labels drawn either.
    (should-not (next-button (point-min)))
    (should-not (string-match-p "\\[Download it\\]" (buffer-string)))
    ;; And the question named every offer with the key that answers it.
    (let ((prompt (car org-semantic-results-tests--asked)))
      (should (string-match-p "\\[d\\] Download it" prompt))
      (should (string-match-p "\\[l\\] Search by word (lexical)" prompt))
      (should (string-match-p "\\[q\\] leave it" prompt))
      ;; Each says what it costs.  A single-letter menu that does not is
      ;; asking the reader to remember which of these takes minutes.
      (should (string-match-p "needs no embedding model" prompt)))))

(ert-deftest answering-by-word-does-not-make-it-the-buffer-s-ranking ()
  "`l\=' searches by word once; it does not redefine what the buffer wants.

The bug it fixes was reported as \"sticky configuration\": pressing
it out of a refusal set the buffer to lexical for good, so every
later query in that buffer was answered by word with nothing saying
why.  It is an escape from one refusal, not a preference --
`org-semantic-results-ranking\=' is where a preference goes.

And the header must not lie about which of the two answered, which
is why the mode asked is recorded separately.  Binding the buffer\='s
own mode around the request would pass this test and still print
\"semantic\" over results found by word, since the reply is rendered
long after the binding is gone."
  (let ((asked nil))
    (cl-letf (((symbol-function 'org-semantic-ui-ask)
               (lambda (_driver params) (push (plist-get params :mode) asked))))
      (with-temp-buffer
        (org-semantic-results-mode)
        (setq org-semantic-results--vault "/vault"
              org-semantic-results--query "q"
              org-semantic-results--mode "semantic")
        (org-semantic-results-tests--answering ?l
          (org-semantic-results--render-error
           '(:message "no semantic index" :data (:kind "no-index" :remedy "index"))))
        ;; Asked by word...
        (should (equal asked '("lexical")))
        (should (equal org-semantic-results--asked-mode "lexical"))
        ;; ...and the buffer still wants what it wanted.
        (should (equal org-semantic-results--mode "semantic"))
        ;; So the next search goes back to it.
        (org-semantic-results--search)
        (should (equal (car asked) "semantic"))))))

(ert-deftest answering-with-a-download-fetches-and-indexes-nothing ()
  "`d\=' fetches the weights the error named, and builds nothing.

The whole point of the method it calls.  Sending an index instead
was the bug: on a vault whose notes have not changed there is
nothing to embed, so no model is loaded and nothing is fetched,
while the run reports success."
  (let ((fetched nil) (indexed 0))
    (cl-letf (((symbol-function 'org-semantic-download)
               (lambda (&rest args) (setq fetched (plist-get args :model))))
              ((symbol-function 'org-semantic-index)
               (lambda (&rest _) (setq indexed (1+ indexed)))))
      (with-temp-buffer
        (org-semantic-results-mode)
        (setq org-semantic-results--vault "/vault"
              org-semantic-results--query "q"
              org-semantic-results--mode "semantic")
        (org-semantic-results-tests--answering ?d
          (org-semantic-results--render-error
           '(:message "the e5-small model is not downloaded yet"
             :data (:kind "model-missing" :model "e5-small" :remedy "download"))))
        (should (equal fetched "e5-small"))
        (should (= 0 indexed))))))

(ert-deftest dismissing-the-question-does-nothing-at-all ()
  "Quitting is an answer, and it must not act and must not signal.

The question runs inside a timer, where an unhandled `quit' becomes
\"Error running timer\" in the echo area for having declined an
offer.

**The escaping quit is caught here rather than left to ert**, which
records it as a QUIT result, prints it, and exits 0 -- so the
neutered version of this test passed the gate.  A gate that cannot
fail is not a gate."
  (let ((searched 0))
    (cl-letf (((symbol-function 'org-semantic-results--search)
               (lambda (&rest _) (setq searched (1+ searched)))))
      (with-temp-buffer
        (org-semantic-results-mode)
        (setq org-semantic-results--vault "/vault"
              org-semantic-results--query "q"
              org-semantic-results--mode "semantic")
        ;; nil stands for C-g: the stub signals `quit' rather than returning.
        (should
         (eq 'returned
             (org-semantic-results-tests--answering nil
               (condition-case nil
                   (progn
                     (org-semantic-results--render-error
                      '(:message "no index" :data (:kind "no-index" :remedy "index")))
                     'returned)
                 (quit 'quit)))))
        (should (= 0 searched))
        (should (equal org-semantic-results--mode "semantic"))
        (should (string-match-p "no index" (buffer-string)))))))

(ert-deftest an-error-with-nothing-to-decide-asks-nothing ()
  "No label means no offers, so there is no question to ask.

The server labels what a client must act on; an unlabelled error is
to be shown and nothing else.  Asking \"[q] leave it\" about it
would be a question with one answer."
  (with-temp-buffer
    (org-semantic-results-mode)
    (setq org-semantic-results--vault "/vault" org-semantic-results--query "q")
    (org-semantic-results-tests--answering ?q
      (org-semantic-results--render-error '(:message "the vault has vanished")))
    (should (string-match-p "vanished" (buffer-string)))
    (should-not org-semantic-results-tests--asked)))

(ert-deftest org-semantic-ui-offer-keys-are-unambiguous ()
  "A key is a label's own initial, so the collisions are what to check.

`[d] Download it' can be read without being learned, which is why
the key follows the label -- but `config-drift' offers \"Search
anyway\" beside \"Show what changed\" and both begin with an S, so
one of them has to be overridden.  What has to hold, for every
failure a client can meet and under either ranking: the keys on
offer are distinct, and none of them is `q', which always means
leave it."
  (let ((kinds '("no-index" "model-missing" "config-drift" "index-layout"
                 "unknown-model" "ambiguous-model" "index-corrupt" "indexing"))
        (remedies '("index" "reindex-full" "wait")))
    (dolist (kind kinds)
      (dolist (mode '("semantic" "lexical"))
        (dolist (indexing '(t :json-false))
          (dolist (remedy remedies)
            (let* ((offers (org-semantic-ui-remedy-offers
                            (org-semantic-ui-remedy
                             (list :message "something"
                                   :data (list :kind kind :remedy remedy
                                               :indexing indexing))
                             mode)))
                   (keys (mapcar #'org-semantic-ui-offer-key offers)))
              (dolist (key keys)
                (should key)
                (should-not (eq key ?q)))
              (should (equal keys (delete-dups (copy-sequence keys)))))))))
    ;; And the key really is the label's initial where nothing overrode it,
    ;; which is the property that makes the prompt readable rather than learnt.
    (should (eq ?d (org-semantic-ui-offer-key '("Download it" . index))))
    (should (eq ?b (org-semantic-ui-offer-key '("Build it" . index))))
    (should (eq ?c (org-semantic-ui-offer-key '("Show what changed" . show-changed))))))


;;;; Settings, and the manual that lists them

(defun org-semantic-results-tests--settings (group)
  "Every user option under GROUP, its subgroups included."
  (let (out)
    (dolist (member (get group 'custom-group))
      (pcase (cadr member)
        ('custom-variable (push (car member) out))
        ('custom-group
         (setq out (append (org-semantic-results-tests--settings (car member))
                           out)))))
    out))

(ert-deftest every-setting-is-written-down ()
  "A setting the manual does not mention is one nobody finds.

Checked rather than remembered, because the failure is quiet in
the direction that matters: the package goes on working perfectly
with a new option that no user has ever heard of.  Four of these
had drifted out of the manual before this test existed."
  (let ((manual (expand-file-name "docs/manual.org" org-semantic-tests--root)))
    (unless (file-readable-p manual) (ert-skip "no manual to check against"))
    (let ((text (with-temp-buffer
                  (insert-file-contents manual)
                  (buffer-string)))
          (settings (org-semantic-results-tests--settings 'org-semantic))
          (missing nil))
      ;; The group has to have been found at all, or this passes by
      ;; checking nothing.
      (should (> (length settings) 5))
      (dolist (setting settings)
        (unless (string-match-p (regexp-quote (symbol-name setting)) text)
          (push setting missing)))
      (should-not missing))))


;;;; Against the binary, over the word index

(ert-deftest a-word-search-fills-a-results-buffer ()
  "End to end: index a vault, search it, and land on the note.

Over the word index, which needs no embedding model."
  (org-semantic-tests--with-server
    (org-semantic-tests--with-vault dir
      (let ((done nil))
        (org-semantic-index :vault dir :mode "lexical"
                            :success (lambda (_r) (setq done t))
                            :failure (lambda (e) (setq done (list 'failed e))))
        (should (eq (org-semantic-tests--wait 120 (lambda () done)) t)))
      (let ((buffer (org-semantic-results--buffer dir)))
        (unwind-protect
            (with-current-buffer buffer
              (setq org-semantic-results--vault dir
                    org-semantic-results--query "turbo"
                    org-semantic-results--mode "lexical")
              (org-semantic-results--search)
              (should (org-semantic-tests--wait
                       30 (lambda () org-semantic-results--hits)))
              (should (= (length org-semantic-results--hits) 1))
              (should next-error-function)
              (should (string-match-p "pumps.org" (buffer-string)))
              ;; The fixture's notes put their heading on line 3 and its
              ;; one sentence on line 4.
              (goto-char (point-min))
              (org-semantic-results--first-item)
              (should (equal (org-semantic-results--file-at-point)
                             (expand-file-name "pumps.org" dir)))
              (should (= (org-semantic-results--line-at-point) 4)))
          (kill-buffer buffer))))))

(ert-deftest a-vault-with-no-index-is-asked-about-for-real ()
  "The one that drives a real refusal all the way to the question.

Every other test here fabricates the error plist.  This one has the
server produce it, so it covers the whole path: an unindexed vault,
a `no-index' reply, the sentence in the buffer and the question in
the minibuffer.

It has to answer, and that is worth knowing before writing another
like it: unstubbed, `read-char-choice' reads stdin, and in batch
that is an immediate `end-of-file' *inside the timer* -- which ert
reports as a passing test with a stray error printed beside it.  Any
test that lets an error render must answer the question."
  (org-semantic-tests--with-server
    (org-semantic-tests--with-vault dir
      (let ((buffer (org-semantic-results--buffer dir)))
        (unwind-protect
            (with-current-buffer buffer
              (setq org-semantic-results--vault dir
                    org-semantic-results--query "turbo"
                    org-semantic-results--mode "lexical")
              (org-semantic-results-tests--answering ?q
                (org-semantic-results--search)
                (should (org-semantic-tests--wait
                         30 (lambda () org-semantic-results-tests--asked))))
              ;; The question the server's own refusal produced.
              (should (string-match-p "\\[b\\] Build it"
                                      (car org-semantic-results-tests--asked)))
              ;; And the buffer holds the account of it, with nothing to press.
              (should (string-match-p "index" (buffer-string)))
              (should-not (next-button (point-min))))
          (kill-buffer buffer))))))

(provide 'org-semantic-results-tests)
;;; org-semantic-results-tests.el ends here
