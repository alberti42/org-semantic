;;; org-semantic-doctor-tests.el --- Tests for the doctor buffer -*- lexical-binding: t; -*-

;; SPDX-License-Identifier: MIT

;;; Commentary:

;; What the doctor buffer draws, and what RET does about a finding.
;;
;; The report is a plist off the wire, so a fabricated one drives the
;; whole of the drawing with no server and no vault.  The fixture builds
;; one with every key present, the optional ones as nil, because that is
;; what the server sends and keeping null apart from absent is exactly
;; this side's problem.

;;; Code:

(require 'ert)
(require 'cl-lib)
(require 'org-semantic-doctor)

(defun org-semantic-doctor-tests--report (&rest overrides)
  "A doctor report as the server sends one, with OVERRIDES applied."
  (let ((report (list :vault "/vault"
                      :notes "/vault"
                      :state "/vault/.org-semantic"
                      :notesFound 3
                      :semantic []
                      :lexical nil
                      :files (vector
                              '(:name ".org-semantic-config.json" :state "absent")
                              '(:name ".org-semantic-ignore" :state "absent")
                              '(:name ".org-semantic-vault.json" :state "absent"))
                      :downloads '(:models "/cache/fastembed"
                                   :modelBytes 133000000
                                   :classifier "/cache/org-semantic/lid.176.ftz"
                                   :classifierPresent t)
                      :findings [])))
    (while overrides
      (setq report (plist-put report (pop overrides) (pop overrides))))
    report))

(defmacro org-semantic-doctor-tests--in (report &rest body)
  "Draw REPORT into a live buffer, run BODY in it, and kill it after.

A live buffer, and not the one `with-temp-buffer' makes: half of
these tests read the text properties the drawing leaves behind and
one presses a key, so the buffer has to outlive the drawing."
  (declare (indent 1))
  `(let ((os-buffer (generate-new-buffer " *doctor test*")))
     (unwind-protect
         (with-current-buffer os-buffer
           (org-semantic-doctor-mode)
           (org-semantic-doctor--draw ,report "/vault")
           ,@body)
       (kill-buffer os-buffer))))

(defun org-semantic-doctor-tests--text (report)
  "REPORT drawn, as plain text."
  (org-semantic-doctor-tests--in report
    (buffer-substring-no-properties (point-min) (point-max))))


;;;; What it draws

(ert-deftest the-doctor-names-the-notes-only-when-they-are-elsewhere ()
  "The one place a user sees where a vault's notes are.

Both halves matter because it is a condition, and a condition
inverts without failing: silent when they differ leaves the split
invisible from inside Emacs -- the reply carries `notes' and
nothing shows it -- and spoken when they do not adds a line to
every ordinary vault saying the notes are where you asked."
  (let ((ordinary (org-semantic-doctor-tests--text
                   (org-semantic-doctor-tests--report)))
        (split (org-semantic-doctor-tests--text
                (org-semantic-doctor-tests--report
                 :vault "/state/notes" :notes "/elsewhere/org"))))
    (should-not (string-match-p "^Notes" ordinary))
    (should (string-match-p "Notes  /elsewhere/org" split))
    (should (string-match-p "Vault  /state/notes" split))))

(ert-deftest the-doctor-says-a-healthy-vault-is-healthy ()
  "A vault with no problem says so, rather than leaving a blank space.

A report that simply stops reads as a report that failed."
  (let ((text (org-semantic-doctor-tests--text
               (org-semantic-doctor-tests--report))))
    (should (string-match-p "Nothing to report" text))))

(ert-deftest the-doctor-states-a-configuration-file-that-is-absent ()
  "Absent is drawn, never left out.

Two of the three files are optional, so a reader asking why a
setting has no effect has to be able to see that the file is not
there.  Leaving the row out looks the same as the file being fine."
  (let ((text (org-semantic-doctor-tests--text
               (org-semantic-doctor-tests--report
                :files (vector '(:name ".org-semantic-config.json" :state "absent")
                               '(:name ".org-semantic-ignore" :state "read" :holds "2 rules")
                               '(:name ".org-semantic-vault.json" :state "unreadable"))))))
    (should (string-match-p "org-semantic-config.json *absent" text))
    (should (string-match-p "org-semantic-ignore *2 rules" text))
    (should (string-match-p "org-semantic-vault.json *will not read" text))))

(ert-deftest the-doctor-keeps-problems-and-notices-apart ()
  "Two headings and never one list.

A reader acts on the first group and only reads the second, so
mixing them makes every line ask to be judged."
  (let ((text (org-semantic-doctor-tests--text
               (org-semantic-doctor-tests--report
                :findings (vector '(:kind "exclude-unreadable" :severity "problem"
                                    :message "the list will not read" :remedy "edit"
                                    :file "/vault/.org-semantic-ignore")
                                  '(:kind "indexing" :severity "notice"
                                    :message "a run is going now" :remedy "wait"))))))
    (should (string-match-p "Problems" text))
    (should (string-match-p "Worth knowing" text))
    (should (< (string-match "Problems" text) (string-match "Worth knowing" text)))
    ;; A problem is present, so the healthy sentence is not.
    (should-not (string-match-p "Nothing to report" text))
    ;; `wait' offers nothing: a run finishes on its own.
    (should-not (string-match-p "RET.*going now" text))))

(ert-deftest the-doctor-says-what-ret-would-do ()
  "An offer is drawn beside the finding it belongs to.

A rebuild is minutes, so the line says so: a single word offering
one does not let a reader decide."
  (let ((text (org-semantic-doctor-tests--text
               (org-semantic-doctor-tests--report
                :findings (vector '(:kind "config-drift" :severity "problem"
                                    :message "built under another policy"
                                    :remedy "reindex-full"))))))
    (should (string-match-p "RET  rebuild" text))
    (should (string-match-p "re-embeds every note" text))))


;;;; What RET does about one

(ert-deftest the-doctor-acts-on-the-remedy-and-not-on-the-sentence ()
  "RET reads `remedy', so rewording a message cannot change what it does.

The message here says one thing and the remedy another.  Keyed on
the prose, this test opens nothing."
  (let ((opened nil))
    (cl-letf (((symbol-function 'org-semantic-ui-visit)
               (lambda (file _line &rest _) (setq opened file))))
      (org-semantic-doctor-tests--in
          (org-semantic-doctor-tests--report
           :findings (vector '(:kind "exclude-unreadable"
                               :severity "problem"
                               :message "rebuild everything now"
                               :remedy "edit"
                               :file "/vault/.org-semantic-ignore")))
        (goto-char (point-min))
        (should (search-forward "rebuild everything now" nil t))
        (beginning-of-line)
        (org-semantic-doctor-act)))
    (should (equal opened "/vault/.org-semantic-ignore"))))

(ert-deftest the-doctor-refuses-to-act-where-there-is-no-finding ()
  "RET on a fact is an error and not a guess.

Acting on the nearest finding instead would rebuild a vault from a
keystroke pressed on the line naming a download directory."
  (org-semantic-doctor-tests--in (org-semantic-doctor-tests--report)
    (goto-char (point-min))
    (should-error (org-semantic-doctor-act) :type 'user-error)))

(ert-deftest the-doctor-asks-again-rather-than-redrawing ()
  "`g' re-sends the request.

Redrawing what is already in the buffer would say the vault is
still broken after the reader has just fixed it."
  (let ((asked 0))
    (cl-letf (((symbol-function 'org-semantic-doctor-report)
               (lambda (&rest _)
                 (setq asked (1+ asked))
                 (org-semantic-doctor-tests--report))))
      (org-semantic-doctor-tests--in (org-semantic-doctor-tests--report)
        (revert-buffer)
        (should (= asked 1))))))

(provide 'org-semantic-doctor-tests)
;;; org-semantic-doctor-tests.el ends here
