;;; org-semantic-doctor.el --- What a vault has, and what is wrong -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Andrea Alberti

;; Author: Andrea Alberti <a.alberti82@gmail.com>
;; Version: 0.6.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: outlines, matching, convenience
;; URL: https://github.com/alberti42/org-semantic
;; SPDX-License-Identifier: MIT

;;; Commentary:

;; `org-semantic-doctor' asks the server what a vault has and what is
;; wrong with it, and draws the answer in a buffer.
;;
;; It is a buffer and not an echo-area line because the answer does not
;; fit on one.  The command it replaces printed a machine reply at a
;; person: seven facts on one line, in the reply's own vocabulary, and
;; nothing about what to do.
;;
;; The report is facts first and findings after.  A reader comes here
;; because something looks wrong, and the facts are what make a finding
;; make sense: which indexes exist, where the notes are, what each
;; configuration file holds.
;;
;; Problems and notices are drawn under separate headings and never in
;; one list.  A reader acts on the first group and only reads the second.
;;
;; A finding is acted on with RET on its own line.  There is no key per
;; offer: two findings can want different things, and point already says
;; which one is meant.  What RET does comes from the finding's `remedy',
;; never from its sentence.

;;; Code:

(require 'org-semantic)
(require 'org-semantic-ui)
(require 'seq)

(defface org-semantic-doctor-heading '((t :inherit bold))
  "Face for a section heading in the doctor's report."
  :group 'org-semantic)

(defface org-semantic-doctor-problem '((t :inherit error))
  "Face for a finding that makes a search answer wrongly or not at all."
  :group 'org-semantic)

(defface org-semantic-doctor-notice '((t :inherit warning))
  "Face for a finding that is worth knowing and breaks nothing."
  :group 'org-semantic)

(defface org-semantic-doctor-offer '((t :inherit shadow))
  "Face for the line saying what RET does about a finding."
  :group 'org-semantic)

(defvar-local org-semantic-doctor--vault nil
  "The vault this buffer reports on, so `g' can ask again.")

(defconst org-semantic-doctor--offers
  '(("edit" . "open this file")
    ("index" . "index this vault")
    ("reindex-full" . "rebuild this vault from scratch, which re-embeds every note")
    ("download" . "download the model")
    ("wait" . nil))
  "What each `remedy' does, said for a reader of this buffer.

The sentence says what it costs where that is minutes, because a
single word offering a rebuild does not.  `wait' offers nothing: a
run finishes on its own.")


;;;; Asking

(defun org-semantic-doctor-report (&optional vault)
  "Return what the server says about VAULT: its facts and its findings.

Nothing is cached.  A reader asks because something looks wrong,
and an answer from before they edited a file helps nobody."
  (org-semantic--call
   "doctor"
   (org-semantic--params :vault (or vault (org-semantic-vault-or-error)))))


;;;; Drawing

(defun org-semantic-doctor--heading (text)
  "Insert TEXT as a section heading."
  (insert "\n" (propertize text 'face 'org-semantic-doctor-heading) "\n"))

(defun org-semantic-doctor--fact (label value)
  "Insert one LABEL and its VALUE, aligned."
  (insert (format "%-7s%s\n" label value)))

(defun org-semantic-doctor--built (report)
  "Insert what REPORT says is built."
  (let ((semantic (append (plist-get report :semantic) nil))
        (lexical (plist-get report :lexical)))
    (org-semantic-doctor--heading "Built")
    (unless (or semantic lexical)
      (insert "  nothing\n"))
    (dolist (s semantic)
      (insert (format "  semantic  %-14s  %s note%s%s%s\n"
                      (plist-get s :model)
                      (plist-get s :notes)
                      (if (= 1 (plist-get s :notes)) "" "s")
                      (if (org-semantic-true-p (plist-get s :readable))
                          "" "  unreadable")
                      (if (org-semantic-true-p (plist-get s :cached))
                          "" "  model not downloaded"))))
    (when lexical
      (let ((langs (append (plist-get lexical :languages) nil)))
        (insert (format "  lexical   %-14s  %s note%s%s\n"
                        (if langs (mapconcat #'identity langs ", ") "unreadable")
                        (plist-get lexical :notes)
                        (if (= 1 (plist-get lexical :notes)) "" "s")
                        (if (org-semantic-true-p (plist-get lexical :folding))
                            "  accents folded" "")))))))

(defun org-semantic-doctor--configuration (report)
  "Insert what REPORT says about the vault's configuration files."
  (org-semantic-doctor--heading "Configuration")
  (dolist (c (append (plist-get report :files) nil))
    ;; Absent is stated rather than left out.  Two of the three files are
    ;; optional, and a reader asking why a setting has no effect needs to
    ;; see that the file is not there.
    (insert (format "  %-26s  %s\n"
                    (plist-get c :name)
                    (or (plist-get c :holds)
                        (if (equal (plist-get c :state) "absent")
                            "absent"
                          "will not read"))))))

(defun org-semantic-doctor--downloads (report)
  "Insert where REPORT says the downloads are."
  (let ((d (plist-get report :downloads)))
    (org-semantic-doctor--heading "Downloads")
    (insert (format "  models      %s  (%s)\n"
                    (abbreviate-file-name (plist-get d :models))
                    (let ((bytes (plist-get d :modelBytes)))
                      (if (and bytes (> bytes 0))
                          (format "%.0f MB" (/ bytes 1e6))
                        "nothing yet"))))
    (insert (format "  classifier  %s  (%s)\n"
                    (abbreviate-file-name (plist-get d :classifier))
                    (if (org-semantic-true-p (plist-get d :classifierPresent))
                        "downloaded" "nothing yet")))))

(defun org-semantic-doctor--findings (report)
  "Insert what REPORT found, problems first and under their own heading."
  (let ((all (append (plist-get report :findings) nil)))
    (dolist (group '(("problem" "Problems" org-semantic-doctor-problem)
                     ("notice" "Worth knowing" org-semantic-doctor-notice)))
      (when-let* ((these (seq-filter
                          (lambda (f) (equal (plist-get f :severity) (nth 0 group)))
                          all)))
        (org-semantic-doctor--heading (nth 1 group))
        (dolist (f these)
          (org-semantic-doctor--finding f (nth 2 group)))))
    (unless (seq-some (lambda (f) (equal (plist-get f :severity) "problem")) all)
      (insert "\nNothing to report.\n"))))

(defun org-semantic-doctor--finding (finding face)
  "Insert one FINDING, drawn in FACE, and what RET would do about it.

The whole of a finding carries it as a text property, so RET acts
on the line under point without the buffer being parsed back."
  (let ((from (point))
        (offer (cdr (assoc (plist-get finding :remedy) org-semantic-doctor--offers))))
    (insert "  " (propertize (plist-get finding :message) 'face face) "\n")
    (when offer
      (insert "    " (propertize (concat "RET  " offer)
                                 'face 'org-semantic-doctor-offer)
              "\n"))
    (put-text-property from (point) 'org-semantic-doctor-finding finding)))

(defun org-semantic-doctor--draw (report vault)
  "Draw REPORT about VAULT into the current buffer."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (setq org-semantic-doctor--vault vault)
    (org-semantic-doctor--fact "Vault" (abbreviate-file-name (plist-get report :vault)))
    (let ((notes (plist-get report :notes)))
      (cond ((null notes)
             (org-semantic-doctor--fact "Notes" "not known — see below"))
            ((not (equal notes (plist-get report :vault)))
             (org-semantic-doctor--fact "Notes" (abbreviate-file-name notes)))))
    (org-semantic-doctor--fact "Index" (abbreviate-file-name (plist-get report :state)))
    (when-let* ((found (plist-get report :notesFound)))
      (org-semantic-doctor--fact
       "Found" (format "%s .org file%s" found (if (= found 1) "" "s"))))
    (org-semantic-doctor--built report)
    (org-semantic-doctor--configuration report)
    (org-semantic-doctor--downloads report)
    (org-semantic-doctor--findings report)
    (goto-char (point-min))))


;;;; Acting

(defun org-semantic-doctor--run (vault full)
  "Index VAULT, and draw the report again when the run lands.

FULL rebuilds from scratch.  The report is what the run was asked
for, so it is asked again rather than left saying what was wrong
before the remedy was applied."
  (let ((os-buffer (current-buffer)))
    (org-semantic-index
     :vault vault
     :full full
     :progress #'org-semantic-report-message
     :success (lambda (_)
                (when (buffer-live-p os-buffer)
                  (with-current-buffer os-buffer (org-semantic-doctor-revert))))
     :failure (lambda (error-object)
                (message "org-semantic: %s"
                         (or (plist-get error-object :message) "the index failed"))))))

(defun org-semantic-doctor-act ()
  "Do what the finding under point needs.

What is done comes from the finding's `remedy', never from its
sentence, so a reworded message cannot change what RET does."
  (interactive)
  (let ((finding (get-text-property (point) 'org-semantic-doctor-finding))
        (vault org-semantic-doctor--vault))
    (unless finding
      (user-error "No finding on this line"))
    (pcase (plist-get finding :remedy)
      ;; Line 1, and not the line the message names.  Parsing a number out
      ;; of a sentence is what the rest of this file refuses to do, and a
      ;; configuration file is short enough that its top is in view.
      ("edit" (let ((file (plist-get finding :file)))
                (unless file
                  (user-error "This finding names no file"))
                (org-semantic-ui-visit file 1 :select t)))
      ("index" (org-semantic-doctor--run vault nil))
      ("reindex-full" (org-semantic-doctor--run vault t))
      ("download"
       (let ((model (plist-get finding :model)))
         (unless model
           (user-error "This finding names no model"))
         (org-semantic-download
          :model model
          :progress #'org-semantic-report-message
          :success (lambda (_) (message "org-semantic: %s is downloaded" model))
          :failure (lambda (e) (message "org-semantic: %s"
                                        (or (plist-get e :message) "the download failed"))))))
      (_ (user-error "Nothing to do about this one")))))

(defun org-semantic-doctor-revert (&rest _)
  "Ask the server again and draw the answer."
  (interactive)
  (let ((vault org-semantic-doctor--vault))
    (org-semantic-doctor--draw (org-semantic-doctor-report vault) vault)))

(defvar-keymap org-semantic-doctor-mode-map
  :doc "Keymap for `org-semantic-doctor-mode'."
  "RET" #'org-semantic-doctor-act)

(define-derived-mode org-semantic-doctor-mode special-mode "org-semantic doctor"
  "Major mode for what a vault has and what is wrong with it.
\\<org-semantic-doctor-mode-map>
The report is facts first and findings after: a finding only makes
sense beside what the vault holds.

  \\[org-semantic-doctor-act]  do what the finding under point needs
  \\[revert-buffer]  ask again
  \\[quit-window]  leave

Problems make a search answer wrongly or not at all.  Notices break
nothing and explain a report that looks wrong for a minute."
  (setq-local revert-buffer-function #'org-semantic-doctor-revert)
  (setq-local truncate-lines nil))

;;;###autoload
(defun org-semantic-doctor (&optional vault)
  "Say in a buffer what VAULT has, and what is wrong with it."
  (interactive)
  (let* ((vault (or vault (org-semantic-vault-or-error)))
         (report (org-semantic-doctor-report vault))
         (buffer (get-buffer-create "*org-semantic doctor*")))
    (with-current-buffer buffer
      (unless (derived-mode-p 'org-semantic-doctor-mode)
        (org-semantic-doctor-mode))
      (org-semantic-doctor--draw report vault))
    (pop-to-buffer buffer)))

(provide 'org-semantic-doctor)
;;; org-semantic-doctor.el ends here
