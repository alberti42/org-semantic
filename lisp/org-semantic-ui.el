;;; org-semantic-ui.el --- Shared pieces of the org-semantic interfaces -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.2.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; What both ways of searching need, and neither owns.  There are two:
;; a results buffer, in `org-semantic-results', and -- later -- narrowing
;; in the minibuffer.  They differ in how they draw a hit and in nothing
;; else, so what is here is everything up to the drawing: how to reach a
;; hit, how to write its score down, how to phrase what an error asks
;; for, and how to keep at most one search in flight.
;;
;; It is a separate file rather than the bottom of the buffer mode
;; because the second interface is meant to arrive without moving any of
;; this: a file is the only structure that makes "shared" checkable.
;;
;; Two things here are not obvious and are the reason the file exists.
;;
;; A SCORE IS NOT A NUMBER YOU MAY DECORATE.  A semantic score is a
;; cosine sitting on a large constant offset -- unrelated passages score
;; 0.56 under one model and 0.80 under another -- so it means nothing
;; without the `z' beside it, which says how far above that model's own
;; floor it is.  A word score is BM25, unbounded, and comparable with
;; nothing: not with another query's scores, not with a cosine.  So it
;; gets no sigma, no percentage, no bar and no threshold, ever.
;; `org-semantic-ui-score' is the one place that knows this, and both
;; interfaces call it rather than formatting a number themselves.
;;
;; A FAILURE IS ANSWERED BY AN OFFER, AND WHO ASKS IS NOT DECIDED HERE.
;; `org-semantic-ui-remedy' says what could be offered and chooses
;; nothing: it hands back symbols, so a caller arranges what "index this
;; vault" costs, and a test can assert on the answer where it could not
;; assert on a closure.  The results buffer asks in the minibuffer with a
;; key per offer; a narrowing interface will have its own moment to ask.
;; What both need is that replies arrive asynchronously, so whoever does
;; ask must not raise a prompt from inside the callback -- see
;; `org-semantic-results--ask', which is where that care lives.

;;; Code:

(require 'cl-lib)
(require 'org-semantic)


;;;; Reaching a hit

(cl-defun org-semantic-ui-visit (file line &key select other-window)
  "Show FILE at LINE, and return the window it is shown in.

With SELECT, the window is selected as well: OTHER-WINDOW then
chooses between this window and another.  Without it the file is
merely displayed -- point lands on LINE in that window and the
selected window does not change, which is what a preview and
`next-error-no-select' both need and what
`org-semantic-visit-hit' cannot do.

The buffer is widened if it was narrowed.  Line numbers are
counted over the whole note, so a buffer narrowed to some other
subtree would otherwise count them from its own beginning and
land, silently, somewhere else."
  (let ((buffer (find-file-noselect file))
        (window nil))
    (if select
        (progn
          (if other-window
              (pop-to-buffer buffer)
            (pop-to-buffer-same-window buffer))
          (setq window (selected-window)))
      ;; **Never in the window we were called from.**  Without
      ;; `inhibit-same-window', `display-buffer' is free to choose the very
      ;; window holding the list, and previewing then replaces the list with the
      ;; note -- so walking down with `n' reached the last hit and the buffer
      ;; you were walking was gone, which reads as the command having jumped
      ;; into the note.  A preview must leave its own list on screen.
      (setq window (display-buffer buffer '(nil (inhibit-same-window . t)))))
    (when window
      (with-selected-window window
        (when (buffer-narrowed-p) (widen))
        (goto-char (point-min))
        (forward-line (1- (max 1 (or line 1))))
        ;; Reached through `fboundp' rather than by requiring org: a hit
        ;; is a line in a file, and nothing here needs org to find it.
        (when (and (derived-mode-p 'org-mode)
                   (fboundp 'org-fold-show-set-visibility))
          (org-fold-show-set-visibility 'canonical))
        (recenter 0)))
    window))


;;;; Writing a hit down

(defun org-semantic-ui-score (hit)
  "How well HIT matched, written the way its ranking allows.

Two rankings with nothing in common, so two spellings.  A semantic
score is a cosine, and the cosine of two unrelated passages is not
zero but 0.56 under one model and 0.80 under another -- the
informative part of the number sits on a large constant.  So it is
shown with the `z' the server sends beside it, which is how far
above that corpus's own floor the hit is, in its standard
deviations, and is the only part comparable between models.

A word score is BM25.  It is unbounded, it rises with how rare the
terms are and how many of them hit, and so it is comparable with
nothing at all -- not with another query's scores on the same
vault, and not with a cosine.  There is no floor to stand it
against, which is why the server sends no `z' for one.  It is
therefore printed raw: no sigma, no percentage, no bar, no
threshold.  Any of those would be measuring a scale that does not
exist."
  (let ((score (plist-get hit :score))
        (z (plist-get hit :z)))
    (cond ((null score) "")
          (z (format "%.3f (%+.1fσ)" score z))
          (t (format "%.3f" score)))))

(defun org-semantic-ui-candidate (hit)
  "HIT written as one line, carrying the hit itself as a property.

The score and the outline path, which is what the buffer puts at
the head of a passage and what the minibuffer will offer as a
completion.  `org-semantic-ui-candidate-hit' reads the hit back
off it, so a caller never has to keep a parallel list in step with
the strings.

Not guaranteed unique: two passages of one section differ only in
their lines, and both are honestly described by the same line.  A
completion table therefore keys on the property and not on the
string."
  (let ((line (format "%s  %s"
                      (org-semantic-ui-score hit)
                      (or (plist-get hit :heading) ""))))
    (propertize line 'org-semantic-hit hit)))

(defun org-semantic-ui-candidate-hit (candidate)
  "The hit CANDIDATE was made from, or nil."
  (and (stringp candidate)
       (> (length candidate) 0)
       (get-text-property 0 'org-semantic-hit candidate)))

(defun org-semantic-ui-annotate (hit)
  "What is worth saying about HIT besides its heading, or nil.

Its TODO keyword, its priority and its tags -- the facts org
carries that a reader recognises at a glance.  The right-hand
column of a completion, and the right of a heading line in a
results buffer."
  (let* ((todo (plist-get hit :todo))
         (priority (plist-get hit :priority))
         (tags (append (plist-get hit :tags) nil))
         (parts (delq nil
                      (list todo
                            (and priority (format "[#%s]" priority))
                            (and tags (concat ":" (mapconcat #'identity tags ":")
                                              ":"))))))
    (and parts (mapconcat #'identity parts " "))))

(defun org-semantic-ui-group (hit)
  "The note HIT is in, as the vault spells it.

Vault-relative, so it is what to show; `org-semantic-hit-file' is
what to open.  The group of a completion, and the file line of a
results buffer."
  (org-semantic-hit-path hit))


;;;; What an error asks for

(defun org-semantic-ui-remedy (error-object &optional mode)
  "What ERROR-OBJECT means, and what could be offered about it.

ERROR-OBJECT is the raw JSON-RPC error a failure callback is
handed -- a plist of `:code', `:message' and `:data' -- and not a
signalled `org-semantic-error'.  MODE is the search mode that
failed, \"semantic\" or \"lexical\", which decides one offer.

Returns (KIND MESSAGE . OFFERS).  KIND is the server's label or
nil, MESSAGE is the sentence to show, and OFFERS is a list of
\(LABEL . ACTION) where ACTION is one of the symbols `index',
`index-full', `lexical', `choose-model', `waive' and
`show-changed'.  Symbols rather than functions: what \"index this
vault\" costs is the caller's to arrange, and a symbol can be
asserted in a test where a closure cannot.

An error with no KIND gets no offers, and that is the contract
rather than an omission: the server labels what a client must act
on, so the absence of a label says there is nothing to decide and
the message is to be shown as it stands.

The action for the labels that have one comes from `data.remedy',
which is the machine form the server sends precisely so that
nobody parses the prose to find out which call to make."
  (let* ((data (plist-get error-object :data))
         (kind (plist-get data :kind))
         (message (or (plist-get error-object :message) "the search failed"))
         (remedy (plist-get data :remedy))
         (build (pcase remedy
                  ("index" '(("Build it" . index)))
                  ("reindex-full" '(("Rebuild from scratch" . index-full)))
                  ("download" '(("Download it" . download)))
                  (_ nil)))
         (offers
          (pcase kind
            ('nil nil)
            ;; The word index costs seconds where the semantic one costs
            ;; minutes, and is very often already built -- so a vault
            ;; that cannot answer by meaning can usually answer now.
            ("no-index"
             (append build
                     (unless (equal mode "lexical")
                       '(("Lexical search (by word)" . lexical)))))
            ;; The index is here and the model that built it is not -- a vault
            ;; copied to another machine, or a cleared cache.  Named as a
            ;; download rather than as a build, because that is the part that
            ;; takes the minutes: the search refuses instantly rather than
            ;; fetching hundreds of megabytes inside a query, and `index' is the
            ;; call that fetches, reports its size, and has the hours to do it.
            ("model-missing"
             (append
              ;; Not offered while a run is already fetching it.  Pressing it
              ;; then would be refused in its turn -- one index per vault -- so
              ;; the offer would be inviting the user into a second error.  This
              ;; is the one error that carries `indexing', because it is the one
              ;; a client repeats: every keystroke of a search-as-you-type asks
              ;; again, and without it the hundredth refusal reads as the first.
              ;; And nothing is offered in its place.  "Try again" was, and it
              ;; only re-ran the search -- which `g' does, and which any new
              ;; search does, so it was a manual poll wearing the clothes of a
              ;; decision.  For a fetch this client started there is a reply to
              ;; wait for; for anyone else's there is nothing we could offer that
              ;; the user cannot already do.  The message says a download is
              ;; running, which is the whole of what is known.
              (unless (org-semantic-true-p (plist-get data :indexing))
                '(("Download it" . download)))
              (unless (equal mode "lexical")
                '(("Lexical search (by word)" . lexical)))))
            ("config-drift"
             (append '(("Rebuild fully" . index-full)
                       ("Search anyway" . waive))
                     '(("Show what changed" . show-changed))))
            ((or "unknown-model" "ambiguous-model")
             '(("Choose a model" . choose-model)))
            (_ build))))
    (cons kind (cons message offers))))

(defun org-semantic-ui-remedy-kind (remedy)
  "The server's label in REMEDY, or nil."
  (car remedy))

(defun org-semantic-ui-remedy-message (remedy)
  "The sentence to show for REMEDY."
  (cadr remedy))

(defun org-semantic-ui-remedy-offers (remedy)
  "What REMEDY could offer to do, as a list of (LABEL . ACTION)."
  (cddr remedy))

(defconst org-semantic-ui--offer-keys
  '(("Show what changed" . ?c)
    ("Rebuild fully" . ?b)
    ("Rebuild from scratch" . ?b))
  "Offers whose key is not the first letter of their label.

Two reasons to be here.  A collision: `config-drift' offers \"Search
anyway\" beside \"Show what changed\", and both begin with an S.

And: **two labels for one action answer to one key.**  A full
rebuild is offered as \"Rebuild fully\" when a policy has drifted and
as \"Rebuild from scratch\" when a layout is too old to read, and it
is `b' -- building -- in both, the same letter as \"Build it\".  `r'
would suggest the letter told a *rebuild* from a *build*, which it
cannot, because no failure ever offers both: one says there is no
index, the other that the one there is cannot be used.  An imagined
distinction is worse than none.

Everything else takes its initial, which is the whole point:
`[d] Download it' can be read without being learned, where a key
chosen for the *call* rather than for the label gives
`[i] Download it' and asks the reader to hold the mapping in their
head.  The cost is that rewording a label moves its key, so keep
the two in step -- and note that the failure is caught rather than
silent: `org-semantic-ui-offer-keys-are-unambiguous' walks every
failure a client can meet and asserts that no two offers in any of
them answer to the same key.")

(defun org-semantic-ui-offer-key (offer)
  "The key that answers OFFER, which is one (LABEL . ACTION) pair.

The label's own initial, downcased, unless
`org-semantic-ui--offer-keys' names another one to break a tie."
  (or (cdr (assoc (car offer) org-semantic-ui--offer-keys))
      (downcase (aref (car offer) 0))))


;;;; One search in flight

(cl-defstruct (org-semantic-ui-driver
               (:constructor org-semantic-ui-driver-create)
               (:copier nil))
  "One search in flight, the next one wanted, and nothing queued between.

Nothing on the server supersedes a search: ten keystrokes are ten
replies, every one of them answered, in arrival order.  Managing
that on the server was considered and refused -- it would have to
read ahead over a channel that also carries cancellations to know
which search replaced which, and two searches differing in vault
or mode are no such thing.

So the client bounds the queue at one instead, with no protocol at
all: keep a single request in flight, hold the latest parameters
wanted, and fire them from the previous reply.  It needs nothing
from the far side, it self-adapts when searches slow to seconds
during a rebuild, and it removes the timeouts, which are the thing
that actually goes wrong."
  (request nil :documentation "The id of the request in flight, or nil.")
  (pending nil :documentation "The parameters wanted next, or nil.")
  (epoch 0 :documentation "\
Bumped by `org-semantic-ui-driver-abandon'.  A reply from an
earlier epoch is dropped, which is how a killed buffer stops
hearing about the search it asked for.")
  (on-reply #'ignore :documentation "Called with each reply plist.")
  (on-error #'ignore :documentation "\
Called with the raw JSON-RPC error plist, which is what a failure
callback is handed -- not a signalled `org-semantic-error'."))

(defconst org-semantic-ui--search-keys
  '(:vault :k :per-file :merge-by-section :mode :model :any :config)
  "The keyword arguments of `org-semantic-search-async', bar the query.
A driver's parameters are filtered to these before being passed
on, so that a caller may carry its own keys in the same plist
without `cl-defun' refusing them.")

(defun org-semantic-ui-ask (driver params)
  "Ask for PARAMS through DRIVER, coalescing on the one search in flight.

PARAMS is a plist of `:query' and the keyword arguments of
`org-semantic-search-async'.  If a search is already out, PARAMS
merely replaces whatever was waiting -- the queue is one deep, and
what is waiting is always the most recent thing asked for."
  (if (org-semantic-ui-driver-request driver)
      (setf (org-semantic-ui-driver-pending driver) params)
    (org-semantic-ui--fire driver params)))

(defun org-semantic-ui-driver-abandon (driver)
  "Make DRIVER forget what it wanted and ignore what is still coming.

For a results buffer being killed, or pointed at another vault:
the reply for a search nobody is waiting for now has nowhere to
go.  Deliberately not called when firing a newer search -- a reply
overtaken by a later query is still the best thing anyone has, and
dropping it would leave the screen blank for a round trip that
buys nothing."
  (cl-incf (org-semantic-ui-driver-epoch driver))
  (setf (org-semantic-ui-driver-request driver) nil
        (org-semantic-ui-driver-pending driver) nil))

(defun org-semantic-ui--fire (driver params)
  "Send PARAMS on DRIVER now, and send whatever is pending from the reply."
  ;; `os-' throughout, as everywhere a callback outlives the call that
  ;; made it: a name someone has `defvar'-ed anywhere in their
  ;; configuration binds dynamically instead of lexically, and is
  ;; unwound again long before the reply arrives.  See
  ;; `org-semantic--call-async', where this cost a silent bug once.
  (let* ((os-driver driver)
         (os-epoch (org-semantic-ui-driver-epoch driver))
         (os-settle
          (lambda (reply error)
            ;; Cleared first, and on both paths: a failure that left this
            ;; set would wedge the driver as permanently in flight, and
            ;; nothing would ever be asked again.
            (setf (org-semantic-ui-driver-request os-driver) nil)
            (when (= os-epoch (org-semantic-ui-driver-epoch os-driver))
              (if error
                  (funcall (org-semantic-ui-driver-on-error os-driver) error)
                (funcall (org-semantic-ui-driver-on-reply os-driver) reply))
              (let ((next (org-semantic-ui-driver-pending os-driver)))
                (when next
                  (setf (org-semantic-ui-driver-pending os-driver) nil)
                  (org-semantic-ui--fire os-driver next)))))))
    ;; PARAMS is the whole truth about this search, including the absence
    ;; of a policy.  `org-semantic-search-async' otherwise falls back to
    ;; the setting, which would make a waived `config-drift' unwaivable:
    ;; the caller drops `:config' to say "search the index as it stands"
    ;; and the global would put it straight back.
    (let ((org-semantic-config (plist-get params :config)))
      (setf (org-semantic-ui-driver-request driver)
            (apply #'org-semantic-search-async
                   (plist-get params :query)
                   (append
                    (org-semantic-ui--keys params)
                    (list :success (lambda (reply) (funcall os-settle reply nil))
                          :failure (lambda (err) (funcall os-settle nil err)))))))))

(defun org-semantic-ui--keys (params)
  "The part of PARAMS `org-semantic-search-async' will accept."
  (let (out)
    (dolist (key org-semantic-ui--search-keys)
      (let ((value (plist-get params key)))
        (when value (setq out (cons value (cons key out))))))
    (nreverse out)))

(provide 'org-semantic-ui)
;;; org-semantic-ui.el ends here
