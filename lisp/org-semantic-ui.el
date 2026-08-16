;;; org-semantic-ui.el --- Shared pieces of the org-semantic interfaces -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.4.1
;; Package-Requires: ((emacs "29.1"))
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; What both ways of searching need, and neither owns: the results
;; buffer in `org-semantic-results', and a minibuffer interface later.
;; They differ only in how they draw a hit, so everything up to the
;; drawing is here.  How to reach a hit, how to write its score, what an
;; error offers, and how to keep one search in flight.
;;
;; Two rules for a caller.  Write a score with `org-semantic-ui-score'
;; and do not format one yourself: the two rankings are on different
;; scales, and only that function knows both.  And read a failure with
;; `org-semantic-ui-remedy', which returns symbols and decides nothing --
;; each interface asks in its own way.

;;; Code:

(require 'cl-lib)
(require 'org-semantic)


;;;; Reaching a hit

(cl-defun org-semantic-ui-visit (file line &key select other-window)
  "Show FILE at LINE, and return the window it is shown in.

With SELECT, the window is selected too, and OTHER-WINDOW chooses
between this window and another.  Without SELECT the file is only
displayed: point moves to LINE in that window and the selected
window does not change, which is what a preview and
`next-error-no-select' need.

The buffer is widened if it was narrowed.  Line numbers are counted
over the whole note, so a narrowed buffer would count from its own
beginning and land somewhere else."
  (let ((buffer (find-file-noselect file))
        (window nil))
    (if select
        (progn
          (if other-window
              (pop-to-buffer buffer)
            (pop-to-buffer-same-window buffer))
          (setq window (selected-window)))
      ;; `inhibit-same-window', or `display-buffer' can choose the window
      ;; that holds the list, and the preview then replaces the list.
      (setq window (display-buffer buffer '(nil (inhibit-same-window . t)))))
    (when window
      (with-selected-window window
        (when (buffer-narrowed-p) (widen))
        (goto-char (point-min))
        (forward-line (1- (max 1 (or line 1))))
        ;; Through `fboundp': this package does not require org.
        (when (and (derived-mode-p 'org-mode)
                   (fboundp 'org-fold-show-set-visibility))
          (org-fold-show-set-visibility 'canonical))
        (recenter 0)))
    window))


;;;; Writing a hit down

(defun org-semantic-ui-score (hit)
  "How well HIT matched, written the way its ranking allows.

A semantic score is a cosine, and is shown with the `z' the server
sends beside it: the standard deviations above that corpus's own
floor.  The cosine cannot be read alone, because two unrelated
passages already score 0.56 or 0.80, depending on the model.

A word score is BM25.  It has no fixed scale and no floor, so the
server sends no `z' and this prints it raw.  Do not give one a
sigma, a percentage, a bar or a threshold."
  (let ((score (plist-get hit :score))
        (z (plist-get hit :z)))
    (cond ((null score) "")
          (z (format "%.3f (%+.1fσ)" score z))
          (t (format "%.3f" score)))))

(defun org-semantic-ui-candidate (hit)
  "HIT written as one line, carrying the hit itself as a property.

The score and the outline path.  `org-semantic-ui-candidate-hit'
reads the hit back off the string, so a caller keeps no parallel
list.

The string is not unique: two passages of one section differ only
in their lines.  A completion table must key on the property."
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

Its TODO keyword, its priority and its tags.  Shown in the
right-hand column of a completion, and to the right of a heading
line in a results buffer."
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
`show-changed'.  Symbols, not functions: the caller arranges what
each action costs.

An error with no KIND gets no offers.  The server labels what a
client must act on, so no label means there is nothing to decide
and the message is shown as it stands.

An action comes from `data.remedy', the machine form, and never
from the prose."
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
            ;; The word index costs seconds, and is often already built.
            ("no-index"
             (append build
                     (unless (equal mode "lexical")
                       '(("Lexical search (by word)" . lexical)))))
            ;; The index is here and the model is not: a vault copied to
            ;; another machine, or a cleared cache.  A search never
            ;; downloads, so the offer is the download itself.
            ("model-missing"
             (append
              ;; Not offered while a fetch is running: a second one is
              ;; refused.  Nothing takes its place, because the message
              ;; already says the download is in progress.
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

Two cases.  A collision: `config-drift' offers \"Search anyway\"
beside \"Show what changed\".  And two labels for one action: a full
rebuild is \"Rebuild fully\" or \"Rebuild from scratch\", and answers
to `b' in both.

Every other offer takes its own initial, so rewording a label moves
its key.  Keep the two in step.
`org-semantic-ui-offer-keys-are-unambiguous' fails on a collision.")

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

The server answers every search and supersedes none, so ten
keystrokes get ten replies.  This keeps one request in flight,
holds the most recent parameters wanted, and sends them from the
previous reply.  A slow search therefore delays the next request
instead of causing a timeout."
  (request nil :documentation "The id of the request in flight, or nil.")
  (pending nil :documentation "The parameters wanted next, or nil.")
  (epoch 0 :documentation "\
Bumped by `org-semantic-ui-driver-abandon'.  A reply from an
earlier epoch is dropped, which is how a killed buffer stops
hearing about the search it asked for.")
  (on-reply #'ignore :documentation "Called with each reply plist.")
  (on-error #'ignore :documentation "\
Called with the raw JSON-RPC error plist, which is what a failure
callback is handed -- not a signalled `org-semantic-error'.")
  (on-waiting #'ignore :documentation "\
Called with the vault when a search is held for an index, and
called again from the reply that releases it.  Only under
`org-semantic-wait-for-index'.")
  (held nil :documentation "\
The function waiting on `org-semantic-index-finished-functions',
or nil.  Removed by a newer query and by
`org-semantic-ui-driver-abandon'."))

(defcustom org-semantic-wait-for-index nil
  "Whether a search waits for an index this Emacs is building.

Off, the default, answers from the version committed before the
run started.  The reply says the index is a version behind, and
the results buffer marks the list.

On, the search is held and sent when the run replies.  Use it when
a stale answer is worse than a slow one.  A rebuild takes minutes,
so the buffer says what it is waiting for.

It covers the runs this Emacs started, which is what
`org-semantic-reindex' and `org-semantic-auto-reindex-mode' make.
A run in another process -- a shell, a cron job, another Emacs --
is invisible to this server and cannot be waited for."
  :type 'boolean
  :group 'org-semantic)

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

For a results buffer that is killed, or pointed at another vault.

Do not call it when a newer search is sent.  A reply overtaken by a
later query is still the best result available, and dropping it
would leave the buffer empty for one round trip."
  (cl-incf (org-semantic-ui-driver-epoch driver))
  (org-semantic-ui--release driver)
  (setf (org-semantic-ui-driver-request driver) nil
        (org-semantic-ui-driver-pending driver) nil))

(defun org-semantic-ui--release (driver)
  "Stop DRIVER waiting for an index, if it is."
  (when-let* ((fn (org-semantic-ui-driver-held driver)))
    (remove-hook 'org-semantic-index-finished-functions fn)
    (setf (org-semantic-ui-driver-held driver) nil)))

(defun org-semantic-ui--hold (driver params vault)
  "Hold PARAMS on DRIVER until the index of VAULT ends, then send them.

The run answers the request that started it, so there is nothing to
poll and no notification to invent: this waits on
`org-semantic-index-finished-functions', which that reply runs.

Both outcomes release it.  A run that fails must not leave a search
waiting for ever."
  ;; `os-' prefixes: the closure outlives this call, and a name that
  ;; anything has `defvar'-ed would bind dynamically and be unwound.
  (let* ((os-driver driver)
         (os-params params)
         (os-vault vault)
         (os-epoch (org-semantic-ui-driver-epoch driver))
         (os-done nil))
    (setq os-done
          (lambda (finished &rest _)
            (when (equal finished os-vault)
              (org-semantic-ui--release os-driver)
              (when (= os-epoch (org-semantic-ui-driver-epoch os-driver))
                (org-semantic-ui--fire os-driver os-params)))))
    (setf (org-semantic-ui-driver-held driver) os-done)
    (add-hook 'org-semantic-index-finished-functions os-done)
    (funcall (org-semantic-ui-driver-on-waiting driver) vault)
    ;; The run can end between the check that sent us here and the line
    ;; above.  Its hook then fired before we were on it, and nothing would
    ;; ever release this.  Asking again after registering closes that, and
    ;; costs a hash lookup.
    (unless (org-semantic-indexing-p vault)
      (funcall os-done vault))))

(defun org-semantic-ui--fire (driver params)
  "Send PARAMS on DRIVER now, and send whatever is pending from the reply.

Under `org-semantic-wait-for-index', a search for a vault this
Emacs is indexing is held instead of sent, and goes out when the
run replies.  Only that vault: the server reports its own runs, so
an index in another process is not visible and cannot be waited
for."
  ;; Whatever this driver was waiting for, it is not waiting for it now:
  ;; either we are about to send, or `--hold' is about to wait afresh.
  ;; Left on the hook, the old closure would fire a superseded query.
  (org-semantic-ui--release driver)
  (let ((vault (plist-get params :vault)))
    ;; The vault must come from PARAMS.  `org-semantic-indexing-p' falls back
    ;; to the current buffer's, which in a results buffer is not the vault
    ;; being searched, and which raises when there is none.
    (if (and org-semantic-wait-for-index vault (org-semantic-indexing-p vault))
        (org-semantic-ui--hold driver params vault)
      (org-semantic-ui--send driver params))))

(defun org-semantic-ui--send (driver params)
  "Send PARAMS on DRIVER, and send whatever is pending from the reply."
  ;; `os-' prefixes, as everywhere a callback outlives its call.  A name
  ;; that anything has `defvar'-ed binds dynamically, and is unwound
  ;; before the reply arrives.
  (let* ((os-driver driver)
         (os-epoch (org-semantic-ui-driver-epoch driver))
         (os-settle
          (lambda (reply error)
            ;; Cleared first, and on both paths.  A failure that left it
            ;; set would hold the driver in flight for ever.
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
    ;; the setting, and a waived `config-drift' could not be waived: the
    ;; caller drops `:config', and the setting puts it back.
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
