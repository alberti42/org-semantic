;;; org-semantic-pretty-tables-tests.el --- tests for the tables adaptor -*- lexical-binding: t; -*-

;; SPDX-License-Identifier: MIT

;;; Commentary:

;; A results buffer is drawn from fabricated hits, as in
;; `org-semantic-results-tests', and the tables in it are drawn by
;; `org-semantic-pretty-tables-mode'.  The tests need Emacs 31.1 and
;; `pretty-tables' on the `load-path', and are skipped without them.
;; The file loads without them, because the adaptor does.

;;; Code:

(require 'ert)
(require 'org-semantic-results-tests)
(require 'org-semantic-pretty-tables)

(defvar pretty-tables-width)

(defun org-semantic-pretty-tables-tests--available-p ()
  "Return non-nil when the adaptor can draw tables here."
  (and (version<= "31.1" emacs-version)
       (require 'pretty-tables-for-org nil t)))

(defmacro org-semantic-pretty-tables-tests--drawn (hits &rest body)
  "Draw HITS in a results buffer with the tables drawn, and run BODY there.
The buffer has a name that does not start with a space and
`noninteractive' is nil while font-lock is turned on, or font-lock
does not turn on in batch mode."
  (declare (indent 1) (debug t))
  `(let ((buf (generate-new-buffer "org-semantic-pretty-tables-test")))
     (unwind-protect
         (with-current-buffer buf
           (org-semantic-results-mode)
           (setq org-semantic-results--vault "/vault"
                 org-semantic-results--query "q"
                 org-semantic-results--mode "lexical")
           (org-semantic-results--render (list :hits ,hits))
           (let ((noninteractive nil))
             (font-lock-mode 1))
           (setq-local pretty-tables-width 80)
           (org-semantic-pretty-tables-mode 1)
           (jit-lock-fontify-now)
           ,@body)
       (kill-buffer buf))))

(defun org-semantic-pretty-tables-tests--overlays ()
  "Return the row overlays in the buffer, in buffer order."
  (sort (seq-filter (lambda (ov) (overlay-get ov 'pretty-tables))
                    (overlays-in (point-min) (point-max)))
        (lambda (a b) (< (overlay-start a) (overlay-start b)))))

(defun org-semantic-pretty-tables-tests--rows ()
  "Return the string drawing each row, without text properties."
  (mapcar (lambda (ov)
            (substring-no-properties (overlay-get ov 'pretty-tables-string)))
          (org-semantic-pretty-tables-tests--overlays)))

(ert-deftest org-semantic-pretty-tables-draws-a-table ()
  "The rows of a table in a passage are drawn aligned."
  (skip-unless (org-semantic-pretty-tables-tests--available-p))
  (org-semantic-pretty-tables-tests--drawn
      (list (org-semantic-results-tests--hit
             :text "| a | b |\n|---+---|\n| one | two |"))
    (should (equal (org-semantic-pretty-tables-tests--rows)
                   '("| a   | b   |" "|-----+-----|" "| one | two |")))))

(ert-deftest org-semantic-pretty-tables-not-in-a-block ()
  "A line starting with `|' in a source block is not a row."
  (skip-unless (org-semantic-pretty-tables-tests--available-p))
  (org-semantic-pretty-tables-tests--drawn
      (list (org-semantic-results-tests--hit
             :text "#+begin_src sh\n| x | y |\n#+end_src"))
    (should-not (org-semantic-pretty-tables-tests--overlays))))

(ert-deftest org-semantic-pretty-tables-not-table-el ()
  "A table.el table is not drawn."
  (skip-unless (org-semantic-pretty-tables-tests--available-p))
  (org-semantic-pretty-tables-tests--drawn
      (list (org-semantic-results-tests--hit
             :text "+---+---+\n| x | y |\n+---+---+"))
    (should-not (org-semantic-pretty-tables-tests--overlays))))

(ert-deftest org-semantic-pretty-tables-prefix ()
  "The screen lines of a row after the first start under the table.
The prefix is as wide as the gutter, and holds no line number."
  (skip-unless (org-semantic-pretty-tables-tests--available-p))
  (let ((org-semantic-results-line-numbers t))
    (org-semantic-pretty-tables-tests--drawn
        (list (org-semantic-results-tests--hit
               :text "  | a | b |\n  |---+---|\n  | one | two |"))
      (let* ((ov (car (org-semantic-pretty-tables-tests--overlays)))
             (start (overlay-start ov))
             (bol (save-excursion (goto-char start) (pos-bol)))
             (gutter (- (next-single-property-change bol 'occur-prefix) bol))
             (prefix (get-text-property
                      0 'line-prefix (overlay-get ov 'pretty-tables-string))))
        (should (string-match-p "[0-9]" (buffer-substring bol start)))
        (should (equal (substring-no-properties prefix)
                       (make-string (+ gutter 2) ?\s)))))))

(ert-deftest org-semantic-pretty-tables-folded-rows ()
  "A folded row is not drawn, and unfolding draws the table with it."
  (skip-unless (org-semantic-pretty-tables-tests--available-p))
  (let ((org-semantic-results-passage-lines 2))
    (org-semantic-pretty-tables-tests--drawn
        (list (org-semantic-results-tests--hit
               :text "| a | b |\n|---+---|\n| one | two |"))
      (should (equal (org-semantic-pretty-tables-tests--rows)
                     '("| a | b |" "|---+---|")))
      (goto-char (overlay-start (car (org-semantic-pretty-tables-tests--overlays))))
      (org-semantic-results-toggle-passage)
      (jit-lock-fontify-now)
      (should (equal (org-semantic-pretty-tables-tests--rows)
                     '("| a   | b   |" "|-----+-----|" "| one | two |"))))))

(ert-deftest org-semantic-pretty-tables-mode-off ()
  "Turning the mode off shows the tables as text."
  (skip-unless (org-semantic-pretty-tables-tests--available-p))
  (org-semantic-pretty-tables-tests--drawn
      (list (org-semantic-results-tests--hit
             :text "| a | b |\n|---+---|\n| one | two |"))
    (should (org-semantic-pretty-tables-tests--overlays))
    (org-semantic-pretty-tables-mode -1)
    (should-not (org-semantic-pretty-tables-tests--overlays))))

(provide 'org-semantic-pretty-tables-tests)
;;; org-semantic-pretty-tables-tests.el ends here
