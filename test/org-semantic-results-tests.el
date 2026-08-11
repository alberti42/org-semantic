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
      (org-semantic-results--render-error error-object)
      (should org-semantic-results--drift)
      (should (= 1 (org-semantic-results-tests--occurrences "Rebuild fully")))
      (org-semantic-results--render-error error-object)
      ;; Said again, but as a line rather than as the whole prompt.
      (should (string-match-p "the policy has changed" (buffer-string)))
      (should (= 1 (org-semantic-results-tests--occurrences "Rebuild fully"))))))


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

(ert-deftest a-vault-with-no-index-offers-to-build-one ()
  "The failure is drawn as something to press, not asked as a question.

A reply arrives whenever it arrives, so a prompt raised from its
callback interrupts whatever the user was typing elsewhere."
  (org-semantic-tests--with-server
    (org-semantic-tests--with-vault dir
      (let ((buffer (org-semantic-results--buffer dir)))
        (unwind-protect
            (with-current-buffer buffer
              (setq org-semantic-results--vault dir
                    org-semantic-results--query "turbo"
                    org-semantic-results--mode "lexical")
              (org-semantic-results--search)
              (should (org-semantic-tests--wait
                       30 (lambda () (string-match-p "Build it"
                                                     (buffer-string)))))
              ;; A button, and nothing was pressed to find that out.
              (goto-char (point-min))
              (should (re-search-forward "Build it" nil t))
              (should (button-at (match-beginning 0))))
          (kill-buffer buffer))))))

(provide 'org-semantic-results-tests)
;;; org-semantic-results-tests.el ends here
