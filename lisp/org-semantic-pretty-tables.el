;;; org-semantic-pretty-tables.el --- Aligned tables in org-semantic results -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Assisted-by: Claude:claude-opus-4-8
;; Version: 0.8.1
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; An adaptor of `pretty-tables' for the results buffer of
;; `org-semantic-results'.  `org-semantic-pretty-tables-mode' draws the
;; Org tables of the passages with aligned, wrapped columns, as
;; `pretty-tables-for-org-mode' draws them in a note.  The buffer text
;; is not changed: each row is covered by an overlay whose `display' is
;; the drawn row.  The results buffer is read-only, so the row point is
;; on stays drawn, except during an Isearch, where it is shown as its
;; text.  Setting `pretty-tables-reveal' to `always' shows it as its
;; text whenever point is on it.
;;
;; The results buffer is not in `org-mode', so the tables are found by
;; the text and faces of each passage line after its gutter:
;;
;; - A row is a line whose text starts, after its indentation, with `|'
;;   in the face `org-table', which the passage's fontification puts on
;;   an Org table and not on a `|' line in a block.  A line drawn
;;   before in another passage has the face
;;   `org-semantic-results-duplicate' in place of org's faces, and is
;;   taken as a row when it starts with `|'.
;; - A run of rows next to a line starting with `+' is a table.el table,
;;   and is not drawn.
;; - A row folded away by `org-semantic-results-passage-lines' is not
;;   part of the table.  Unfolding the passage draws the table again
;;   with it.
;;
;; The screen lines of a drawn row after the first start with the
;; `wrap-prefix' of the line, which is as wide as the gutter, followed
;; by the indentation of the table.
;;
;; This file needs Emacs 31.1 and `pretty-tables' 0.4.0, which the
;; rest of org-semantic does not.  It is part of the org-semantic
;; package and compiles without `pretty-tables': the mode requires
;; `pretty-tables-for-org' when it is turned on, and not before.
;; `org-semantic-results-mode' loads this file and turns the mode on
;; when `pretty-tables' is installed and
;; `org-semantic-results-pretty-tables' is non-nil.

;;; Code:

(declare-function pretty-tables-enable "ext:pretty-tables")
(declare-function pretty-tables-disable "ext:pretty-tables")
(declare-function pretty-tables-for-org-table "ext:pretty-tables-for-org")
(declare-function pretty-tables-for-org-draw-separator "ext:pretty-tables-for-org")

(defun org-semantic-pretty-tables--text-start (bol)
  "Return where the text of the note starts on the line at BOL.
That is after the gutter, which has the property `occur-prefix'.  On a
line that is not a passage line, return nil."
  (when (get-text-property bol 'occur-prefix)
    (next-single-property-change
     bol 'occur-prefix nil (save-excursion (goto-char bol) (pos-eol)))))

(defun org-semantic-pretty-tables--row-start (bol)
  "Return the `|' that starts a table row on the line at BOL, or nil."
  (let ((text (org-semantic-pretty-tables--text-start bol)))
    (when text
      (save-excursion
        (goto-char text)
        (skip-chars-forward " \t")
        (and (eq (char-after) ?|)
             (seq-some (lambda (face)
                         (memq face '(org-table org-semantic-results-duplicate)))
                       (ensure-list (get-text-property (point) 'face)))
             (point))))))

(defun org-semantic-pretty-tables--border-p (bol)
  "Return non-nil when the line at BOL is a border of a table.el table."
  (let ((text (org-semantic-pretty-tables--text-start bol)))
    (when text
      (save-excursion
        (goto-char text)
        (skip-chars-forward " \t")
        (eq (char-after) ?+)))))

(defun org-semantic-pretty-tables--tables (beg end)
  "Return the Org tables of the passages that overlap BEG to END.
Each is a table as `pretty-tables-enable' describes it."
  (save-excursion
    (goto-char beg)
    (forward-line 0)
    ;; Back to the first row of the table BEG is in, folded rows
    ;; included: folding a passage refontifies only the folded lines.
    (while (and (not (bobp))
                (save-excursion
                  (forward-line -1)
                  (org-semantic-pretty-tables--row-start (point))))
      (forward-line -1))
    (let (tables)
      (while (and (< (point) end) (not (eobp)))
        (if (not (org-semantic-pretty-tables--row-start (point)))
            (forward-line 1)
          (let ((table-el (and (not (bobp))
                               (save-excursion
                                 (forward-line -1)
                                 (org-semantic-pretty-tables--border-p (point)))))
                starts)
            (while (let ((start (org-semantic-pretty-tables--row-start (point))))
                     (when start
                       (unless (invisible-p (point))
                         (push start starts))
                       (forward-line 1)
                       t)))
            (when (org-semantic-pretty-tables--border-p (point))
              (setq table-el t))
            (when (and starts (not table-el))
              (push (pretty-tables-for-org-table (nreverse starts)) tables)))))
      (nreverse tables))))

(defun org-semantic-pretty-tables--prefix (pos)
  "Return the prefix of the screen lines of a row drawn from POS.
It is the `wrap-prefix' of the line, as wide as the gutter, followed
by the text from the end of the gutter to POS."
  (let ((text (org-semantic-pretty-tables--text-start
               (save-excursion (goto-char pos) (pos-bol)))))
    (concat (or (get-text-property pos 'wrap-prefix) "")
            (buffer-substring (or text pos) pos))))

;;;###autoload
(define-minor-mode org-semantic-pretty-tables-mode
  "Display the Org tables of org-semantic results with aligned columns.
The buffer text is not changed.  The row point is on stays drawn,
except during an Isearch, where it is shown as its text.  Setting
`pretty-tables-reveal' to `always' shows it as its text whenever point
is on it, except after a scroll command moved point onto it."
  :lighter nil
  (if org-semantic-pretty-tables-mode
      (progn
        (require 'pretty-tables-for-org)
        (pretty-tables-enable
         :tables #'org-semantic-pretty-tables--tables
         :separator #'pretty-tables-for-org-draw-separator
         :face 'org-table
         :prefix #'org-semantic-pretty-tables--prefix))
    (pretty-tables-disable)))

(provide 'org-semantic-pretty-tables)
;;; org-semantic-pretty-tables.el ends here
