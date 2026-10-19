;;; goblins.el --- Goblins agent status and approvals -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; Run M-x goblins-status; use M-x goblins-start-server if needed.
;; Connects directly to the daemon's host socket using JSON-RPC API 1.

;;; Code:

(require 'cl-lib)
(require 'jsonrpc)
(require 'magit-section)
(require 'subr-x)

(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))
(declare-function project-root "project" (project))
(declare-function ghostel-exec "ghostel" (buffer program &optional args identity))
(declare-function goblins--display-new-agent "goblins-views" (buffer))
(defvar goblins--last-goblin)
(declare-function goblins--note-selected "goblins-views" (key))
(declare-function goblins--session-label "goblins-views" (snapshot id))
(defvar ghostel-kill-buffer-on-exit)
(defvar ghostel-identity)
(declare-function goblin-mode "goblins-terminal" (&optional arg))

(defgroup goblins nil "Goblins agent status and approvals." :group 'tools)

(defcustom goblins-executable "goblins"
  "Packaged Goblins executable used to start and stop the server.
Use an absolute path if Goblins is not on Emacs' `exec-path'."
  :type 'string)

(defcustom goblins-state-directory nil
  "Daemon state directory, or nil to use the CLI's default.
Set this to the directory passed to `goblins --state-dir'."
  :type '(choice (const :tag "Automatic" nil) directory))

(defvar-local goblins--directory nil)
(defvar-local goblins--connection nil)
(defvar-local goblins--server-process nil)
(defvar-local goblins--instance nil)
(defvar-local goblins--subscription nil)
(defvar-local goblins--sequence nil)
(defvar-local goblins--snapshot nil)
(defvar-local goblins--notice "Disconnected; r to reconnect")
(defvar-local goblins--decisions nil)
(defvar-local goblins--details nil
  "Details identity as (KIND ID INSTANCE), or nil for the overview.")
(defvar-local goblins--overview-position nil)
(defvar-local goblins--overview-folds nil)
(defvar goblins--terminal-buffers (make-hash-table :test #'equal)
  "Emacs-only mapping from (directory instance session) to Ghostel buffers.")
(defvar-local goblins--session-key nil)
(defvar-local goblins--shell-number nil
  "This goblin shell's number among its goblin's open shells, from 1.")
(defvar goblins-run-history nil)

(defcustom goblins-reconnect-delays '(1 2 5 10 30)
  "Seconds to wait before each attempt to reconnect a lost server connection.
The last delay repeats until a connection succeeds.  nil disables reconnecting."
  :type '(repeat number))

(defvar goblins-request-hook nil
  "Abnormal hook run with each permission request that became pending.
Functions run in the server's status buffer and receive the request record.
Requests already pending when Emacs first connects are not announced.")

(defvar goblins-approval-hook nil
  "Abnormal hook run with each goblin whose agent began waiting at its own
permission prompt (one without `bypassPermissions').  Functions run in the
server's status buffer and receive the session record.  Goblins already
waiting when Emacs first connects are not announced.")

(defvar-local goblins--reconnect-timer nil)
(defvar-local goblins--reconnect-attempt 0)
(defvar-local goblins--seen-requests 'unset
  "IDs of the pending requests already announced, or `unset' before the first
snapshot.")
(defvar-local goblins--seen-approvals 'unset
  "IDs of the goblins waiting at their agent's own permission prompt at the
last snapshot, or `unset' before the first.")

(defclass goblins-section (magit-section) ())
(defclass goblins-agent-section (goblins-section) ())
(defclass goblins-request-section (goblins-section)
  ((record :initarg :record :initform nil)
   ;; Rendered with its fields and preview, not as a heading only.
   (details :initarg :details :initform nil)))
(defclass goblins-fold-section (goblins-section) ()
  "Sections TAB folds, also in the details view.")
(defclass goblins-summary-section (goblins-fold-section) ())
(defclass goblins-diff-section (goblins-fold-section) ())
(defclass goblins-diff-file-section (goblins-fold-section) ())

(defface goblins-key '((t :inherit font-lock-keyword-face :weight bold))
  "Keys in a dev shell refresh summary.")
(defface goblins-added '((t :inherit success))
  "Added packages, inputs and new values.")
(defface goblins-removed '((t :inherit error))
  "Removed packages, inputs and old values.")
(defface goblins-changed '((t :inherit warning))
  "Changed entries.")
(defface goblins-attention '((t :inherit warning :weight bold))
  "Refresh details that need the approver's attention.")
(defface goblins-dim '((t :inherit shadow))
  "Unchanged or secondary summary text.")

(defun goblins--diff-face (magit fallback)
  "Use MAGIT's diff face when Magit is installed, else FALLBACK from diff-mode."
  (if (facep magit) magit fallback))

(defun goblins--patch-files (patch)
  "Split a git PATCH into (PATH STATUS LINES) per file, without Git headers."
  (let (files header)
    (dolist (line (split-string patch "\n"))
      (cond
       ((string-prefix-p "diff --git " line)
        (push (list (if (string-match " b/\\(.*\\)\\'" line)
                        (match-string 1 line)
                      (substring line 11))
                    "modified" nil)
              files)
        (setq header t))
       ((null files))
       ((and header (string-prefix-p "new file" line)) (setf (nth 1 (car files)) "new file"))
       ((and header (string-prefix-p "deleted file" line)) (setf (nth 1 (car files)) "deleted"))
       ((string-prefix-p "@@" line)
        (setq header nil)
        (push line (nth 2 (car files))))
       ((not header) (push line (nth 2 (car files))))))
    (mapcar (lambda (file) (list (nth 0 file) (nth 1 file) (nreverse (nth 2 file))))
            (nreverse files))))

;; magit-section buffers render `font-lock-face', not `face'.
(defun goblins--face (text face)
  (propertize (goblins--safe text) 'font-lock-face face))

(defun goblins--nix-file-p (path)
  (string-suffix-p ".nix" path))

(defun goblins--old-new (old new)
  "OLD in the removed face and NEW in the added face; either may be nil."
  (cond ((and old new) (concat (goblins--face old 'goblins-removed) " → "
                               (goblins--face new 'goblins-added)))
        (new (goblins--face new 'goblins-added))
        (old (goblins--face old 'goblins-removed))
        (t "")))

(defun goblins--tag (change)
  (pcase change
    ("added" (propertize "[A] " 'font-lock-face 'goblins-added))
    ("removed" (propertize "[R] " 'font-lock-face 'goblins-removed))
    (_ (propertize "[C] " 'font-lock-face 'goblins-changed))))

(defun goblins--summary-row (key value first)
  (insert "  " (propertize (format "%-10s" (if first key "")) 'font-lock-face 'goblins-key)
          value "\n"))

(defun goblins--summary-rows (key values)
  "Insert VALUES under KEY, or \"unchanged\"."
  (if (null values)
      (goblins--summary-row key (propertize "unchanged" 'font-lock-face 'goblins-dim) t)
    (let ((first t))
      (dolist (value values)
        (goblins--summary-row key value first)
        (setq first nil)))))

(defun goblins--locked (locked)
  (when locked
    (concat (plist-get locked :id)
            (if-let* ((date (plist-get locked :date))) (format " (%s)" date) ""))))

(defun goblins--versions (versions)
  (when versions
    (mapconcat (lambda (v) (if (string-empty-p v) "?" v)) (append versions nil) ", ")))

(defun goblins--attention-text (note)
  (pcase (plist-get note :kind)
    ("input-source" (format "input '%s' now comes from %s (was %s)"
                            (plist-get note :input) (plist-get note :new) (plist-get note :old)))
    ("new-input" (format "new input '%s' from %s" (plist-get note :input) (plist-get note :source)))
    ("nix-config" "the flake declares nixConfig (ignored, never applied)")
    ("local-builds" (format "%s derivation(s) will be built on this machine" (plist-get note :count)))
    ("shell-hook" "shellHook changed; it runs inside the sandbox after approval")
    (kind (format "%s" kind))))

(defun goblins--insert-devshell-summary (request preview)
  "Insert the refresh summary of REQUEST from its structured PREVIEW."
  (let ((files (append (plist-get preview :files) nil)))
    (magit-insert-section (goblins-summary-section (list 'summary request))
      (magit-insert-heading
        (propertize "Dev shell refresh" 'font-lock-face 'magit-section-heading)
        (format " %s · generation %s → %s"
                (goblins--safe (plist-get preview :reference))
                (plist-get preview :from) (plist-get preview :to)))
      (goblins--summary-rows
       "Attention"
       (or (mapcar (lambda (note) (goblins--face (concat "! " (goblins--attention-text note))
                                                 'goblins-attention))
                   (plist-get preview :attention))
           (list (propertize "nothing unusual · inputs locked to allowed sources"
                             'font-lock-face 'success))))
      (let* ((nix (cl-remove-if-not (lambda (f) (goblins--nix-file-p (plist-get f :path))) files))
             (other (- (length files) (length nix))))
        (goblins--summary-rows
         "Source"
         (append
          (mapcar (lambda (f)
                    (concat (goblins--safe (plist-get f :path)) " "
                            (goblins--face (format "+%s" (or (plist-get f :added) "?")) 'goblins-added) " "
                            (goblins--face (format "-%s" (or (plist-get f :removed) "?")) 'goblins-removed)))
                  nix)
          (when (> other 0)
            (list (propertize (format "%d other file%s changed (see Other changes)"
                                      other (if (= other 1) "" "s"))
                              'font-lock-face 'goblins-dim))))))
      (goblins--summary-rows
       "Inputs"
       (mapcar (lambda (c)
                 (let* ((old (plist-get c :old)) (new (plist-get c :new))
                        (old-source (plist-get old :source)) (new-source (plist-get new :source)))
                   (concat (goblins--tag (plist-get c :change))
                           (goblins--safe (plist-get c :name)) "  "
                           (if (and old-source new-source (not (equal old-source new-source)))
                               (concat (goblins--old-new old-source new-source) "  ")
                             (concat (goblins--safe (or new-source old-source)) "  "))
                           (goblins--old-new (goblins--locked old) (goblins--locked new)))))
               (plist-get preview :inputs)))
      (goblins--summary-rows
       "Packages"
       (mapcar (lambda (c)
                 (concat (goblins--tag (plist-get c :change))
                         (goblins--safe (plist-get c :name)) " "
                         (goblins--old-new (goblins--versions (plist-get c :old))
                                           (goblins--versions (plist-get c :new)))))
               (plist-get preview :packages)))
      (goblins--summary-rows
       "Env"
       (mapcar (lambda (c)
                 (concat (goblins--tag (plist-get c :change))
                         (goblins--safe (plist-get c :name)) " "
                         (if (eq (plist-get c :long) t)
                             (propertize "(long value; see the diff)" 'font-lock-face 'goblins-dim)
                           (goblins--old-new (plist-get c :old) (plist-get c :new)))))
               (plist-get preview :env)))
      (let ((build (or (plist-get preview :build) 0)))
        (goblins--summary-row
         "Cost"
         (concat (format "%s to fetch%s · " (plist-get preview :fetch)
                         (if-let* ((d (plist-get preview :download))) (format " (%s)" (goblins--safe d)) ""))
                 (propertize (format "%s to build" build)
                             'font-lock-face (if (> build 0) 'goblins-attention 'default)))
         t))
      (when-let* ((incoming (append (plist-get preview :incoming) nil)))
        ;; Everything the dry run fetches or builds; long lists start folded.
        (magit-insert-section (goblins-fold-section (list 'incoming request) (> (length incoming) 10))
          (magit-insert-heading
            (concat "  " (propertize (format "%-10s" "Incoming") 'font-lock-face 'goblins-key)
                    (format "%d package%s" (length incoming) (if (= (length incoming) 1) "" "s"))))
          (dolist (item incoming)
            (insert "  " (make-string 10 ?\s)
                    (if (equal (plist-get item :action) "build")
                        (propertize "[B] " 'font-lock-face 'goblins-attention)
                      (propertize "[F] " 'font-lock-face 'goblins-added))
                    (goblins--safe (plist-get item :name)) " "
                    (goblins--safe (or (goblins--versions (plist-get item :versions)) "")) "\n")))))))

(defun goblins--insert-diff-files (request files)
  (dolist (file files)
    (let ((nix (goblins--nix-file-p (car file))))
      (magit-insert-section (goblins-diff-file-section (list request (car file)) (not nix))
        (magit-insert-heading
          (propertize (format "%-11s%s" (nth 1 file) (goblins--safe (car file)))
                      'font-lock-face
                      (goblins--diff-face 'magit-diff-file-heading 'diff-file-header)))
        (dolist (line (nth 2 file))
          ;; The newline carries the face too, so backgrounds span the window.
          (insert (propertize
                   (concat (goblins--safe (string-replace "\t" "    " line)) "\n")
                   'font-lock-face
                   (pcase (and (> (length line) 0) (aref line 0))
                     (?@ (goblins--diff-face 'magit-diff-hunk-heading 'diff-hunk-header))
                     (?+ (goblins--diff-face 'magit-diff-added 'diff-added))
                     (?- (goblins--diff-face 'magit-diff-removed 'diff-removed))
                     (_ (goblins--diff-face 'magit-diff-context 'diff-context))))))))))

(defun goblins--insert-diff (request patch)
  "Insert PATCH of REQUEST like a Magit diff: Nix files expanded first, and
all other files in a collapsed section of their own."
  (require 'diff-mode)
  (let* ((files (goblins--patch-files patch))
         (nix (cl-remove-if-not (lambda (f) (goblins--nix-file-p (car f))) files))
         (other (cl-remove-if (lambda (f) (goblins--nix-file-p (car f))) files))
         (count (lambda (n) (format " (%d file%s)" n (if (= n 1) "" "s")))))
    (insert "\n")
    (magit-insert-section (goblins-diff-section (list 'nix-diff request))
      (magit-insert-heading
        (propertize "Nix changes" 'font-lock-face 'magit-section-heading)
        (funcall count (length nix)))
      (unless nix (insert "No Nix changes\n"))
      (goblins--insert-diff-files request nix))
    (when other
      (magit-insert-section (goblins-diff-section (list 'other-diff request) t)
        (magit-insert-heading
          (propertize "Other changes" 'font-lock-face 'magit-section-heading)
          (funcall count (length other)))
        (goblins--insert-diff-files request other)))))

(defun goblins--default-directory ()
  (let ((root (or (getenv "XDG_RUNTIME_DIR")
                  (format "/run/user/%s" (user-uid)))))
    (if (file-directory-p root)
        (expand-file-name "goblins" root)
      (format "/tmp/goblins-control-%s" (user-uid)))))

(defun goblins--safe (value)
  "Render VALUE on one line, escaping control and bidi formatting characters."
  (replace-regexp-in-string
   "[[:cntrl:]\u2028\u2029\u202a-\u202e\u2066-\u2069]"
   (lambda (s) (format "\\u%04x" (string-to-char s)))
   (format "%s" (or value "—")) t t))

(defun goblins--field (label value)
  (insert (format "    %-12s%s\n" label (goblins--safe value))))

(defun goblins--wrapped-field (label value)
  "Insert a field like `goblins--field', VALUE wrapped to the window.
Long values such as host commands break at spaces where they can, and
continue under the value; no character of VALUE is changed or dropped."
  (let* ((indent (make-string 16 ?\s))
         (window (get-buffer-window (current-buffer) t))
         (width (max 20 (- (if window (window-body-width window) 100) 17)))
         (rest (goblins--safe value))
         (first t))
    (while (or first (not (string-empty-p rest)))
      (let ((cut (if (<= (length rest) width)
                     (length rest)
                   (let ((space (cl-position ?\s rest :end width :from-end t)))
                     (if (and space (> space 0)) (1+ space) width)))))
        (insert (if first (format "    %-12s" label) indent)
                (substring rest 0 cut) "\n")
        (setq rest (substring rest cut) first nil)))))

(defun goblins--insert-host-command (preview)
  "Insert a host command's PREVIEW as fields: its warning, command,
directory, each variable and time limit.  These are the daemon's escaped
values, so no request text can add a line or a field."
  (insert "    " (propertize (goblins--safe (car (split-string
                                                   (or (plist-get preview :description) "")
                                                   "\n")))
                               'face 'goblins-dim)
          "\n")
  (goblins--wrapped-field "Command:" (plist-get preview :command))
  (goblins--wrapped-field "Directory:" (plist-get preview :directory))
  (seq-doseq (variable (plist-get preview :environment))
    (goblins--wrapped-field "Variable:" variable))
  (goblins--wrapped-field "Time limit:" (plist-get preview :time_limit)))

(defun goblins--request-subject (record)
  "What RECORD asks for, on one line.  A host command is cut to 60
characters: its approval shows it whole.  A paused raid's prompt, whose
package is the raid's name, says what it waits for."
  (let ((subject (goblins--safe (plist-get record :package))))
    (pcase (plist-get record :kind)
      ("host-command" (truncate-string-to-width subject 60 nil nil "…"))
      ("raid-owner" (format "raid %s paused%s" subject
                            (if (equal (plist-get record :state) "pending")
                                ": pick a new owner" "")))
      (_ subject))))

(defun goblins--created-by (session)
  "The name of the raid owner that ran SESSION, or nil if the host started it."
  (let ((creator (plist-get session :created_by)))
    (and (listp creator) (plist-get creator :agent_name))))

(defun goblins--raid-owner-member-line (member)
  "MEMBER of a paused raid's prompt on one line: name, configuration, role,
state, and whether its owner created it."
  (concat (goblins--safe (plist-get member :agent_name))
          (format " (%s)" (goblins--safe (plist-get member :name)))
          (if-let* ((role (plist-get member :role)) ((stringp role)))
              (format "  %s" (goblins--safe role)) "")
          (format "  %s" (goblins--safe (plist-get member :state)))
          (if (eq (plist-get member :created) t) "  created" "")))

(defun goblins--insert-raid-owner-fields (preview)
  "Insert a paused raid's PREVIEW as fields: the raid, its former owner and
each member."
  (goblins--field "Raid:" (plist-get preview :name))
  (goblins--field "Former owner:"
                  (let ((former (plist-get preview :former_owner)))
                    (and (listp former) (plist-get former :agent_name))))
  (let ((members (append (plist-get preview :members) nil)))
    (if (null members)
        (goblins--field "Members:" "none")
      (dolist (member members)
        ;; Already escaped: `goblins--field' would escape it again.
        (insert (format "    %-12s%s\n" (if (eq member (car members)) "Members:" "")
                        (goblins--raid-owner-member-line member)))))))

(defun goblins--awaiting-preview-p (record)
  "Whether RECORD is a dev shell refresh whose preview is not ready yet."
  (and (equal (plist-get record :kind) "devshell")
       (not (plist-get record :preview))))

(defun goblins--insert-request (record &optional details hide heading)
  "Insert RECORD as a section; DETAILS adds its fields and preview.
HIDE starts it folded and HEADING replaces the default heading text."
  (magit-insert-section section
    (goblins-request-section (plist-get record :id) hide)
    (oset section record record)
    (oset section details details)
    (magit-insert-heading
      (or heading
          (format "  %s  %s  [%s]"
                  (goblins--request-agent-label record)
                  (goblins--request-subject record)
                  (goblins--safe (plist-get record :state)))))
    (when details
      (goblins--field "Reason:" (plist-get record :reason))
      (goblins--field "Session:" (plist-get record :session))
      (goblins--field "Request:" (plist-get record :id))
      (when-let* ((preview (plist-get record :preview))
                  ((plist-get preview :command)))
        (goblins--insert-host-command preview))
      (when-let* ((preview (plist-get record :preview))
                  ((equal (plist-get record :kind) "raid-owner")))
        (goblins--insert-raid-owner-fields preview))
      (when-let* ((preview (plist-get record :preview))
                  ((not (equal (plist-get preview :kind) "devshell")))
                  ((not (equal (plist-get record :kind) "raid-owner")))
                  ((not (plist-get preview :command))))
        ;; A multi-line description is escaped line by line.
        (let ((lines (split-string
                      (or (plist-get preview :description)
                          (if-let* ((error (plist-get preview :error)))
                              error
                            (format "In store: %s; download: %s; build required: %s"
                                    (if (eq (plist-get preview :in_store) t) "yes" "no")
                                    (or (plist-get preview :download) "unknown")
                                    (if (eq (plist-get preview :build_required) t) "yes" "no"))))
                      "\n")))
          (goblins--wrapped-field "Preview:" (car lines))
          (dolist (line (cdr lines))
            (goblins--wrapped-field "" line))))
      (when (goblins--awaiting-preview-p record)
        (goblins--field "Preview:" "evaluating the refresh; its preview follows"))
      (when-let* ((message (plist-get record :message)))
        (goblins--field "Message:" message))
      (when-let* ((preview (plist-get record :preview)))
        (when (equal (plist-get preview :kind) "devshell")
          (insert "\n")
          (goblins--insert-devshell-summary (plist-get record :id) preview))
        ;; Older daemons send a text description and a diff without a kind.
        (let ((patch (plist-get preview :diff)))
          (unless (string-empty-p (or patch ""))
            (goblins--insert-diff (plist-get record :id) patch))))
      (insert "\n"))))

(defun goblins--request-agent-label (request)
  "Name REQUEST's sandbox, from the snapshot while its session is retained."
  (let ((agent (cl-find (plist-get request :session)
                        (append (plist-get goblins--snapshot :sessions) nil)
                        :key (lambda (s) (plist-get s :id)) :test #'equal)))
    (goblins--safe (or (plist-get agent :agent_name) (plist-get request :agent_name)))))

(defun goblins--finished-p (session)
  "Whether SESSION has stopped or failed."
  (member (plist-get session :state) '("stopped" "failed")))

(defun goblins--insert-agent-fields (agent)
  (goblins--field "Session:" (plist-get agent :id))
  (goblins--field "State:" (plist-get agent :state))
  (goblins--field "Activity:" (or (plist-get agent :agent_state) "unknown"))
  (goblins--field "Config:" (plist-get agent :name))
  (when-let* ((creator (goblins--created-by agent)))
    (goblins--field "Created by:" creator))
  (goblins--field "Startup:" (string-join (append (plist-get agent :initial_packages) nil) ", "))
  (goblins--field "Granted:" (string-join (append (plist-get agent :packages) nil) ", "))
  (when-let* ((auto (append (plist-get agent :auto_approve) nil)))
    (goblins--field "Auto:" (string-join auto ", ")))
  (when-let* ((detail (plist-get agent :detail)))
    (goblins--field "Detail:" detail)))

(defun goblins--insert-agent (agent)
  "Insert AGENT's line as a Magit section."
  (magit-insert-section (goblins-agent-section (plist-get agent :id))
    (magit-insert-heading
      (format "  %-20s %-12s %-16s %s%s"
              (goblins--safe (plist-get agent :agent_name))
              (goblins--safe (plist-get agent :state))
              (goblins--safe (or (plist-get agent :agent_state) "unknown"))
              (goblins--safe (plist-get agent :name))
              (if (goblins--terminal-buffer (plist-get agent :id))
                  "  [t: terminal]" "")))))

(defun goblins-toggle-section ()
  "Expand or collapse the section at point: a group of the overview, or a
summary or diff section, which also fold in the details view."
  (interactive)
  (when-let* ((section (magit-current-section)))
    (when (or (object-of-class-p section 'goblins-fold-section)
              (and (not goblins--details) (oref section children)))
      (magit-section-toggle section))))

(defun goblins-details ()
  "Show the selected sandbox or request in the details view."
  (interactive)
  (unless goblins--details
    (let* ((section (magit-current-section))
           (kind (cond ((object-of-class-p section 'goblins-agent-section) 'agent)
                       ((object-of-class-p section 'goblins-request-section) 'request))))
      (when kind
        (setq goblins--overview-position
              (cons (magit-section-ident section) (- (point) (oref section start)))
              goblins--overview-folds nil)
        (cl-labels ((remember (item)
                      (when (oref item children)
                        (push (cons (magit-section-ident item) (oref item hidden))
                              goblins--overview-folds))
                      (mapc #'remember (oref item children))))
          (remember magit-root-section))
        (setq goblins--details (list kind (oref section value) goblins--instance))
        (goto-char (point-min))
        (goblins--render)))))

(defun goblins-back ()
  "Return from details to the overview, preserving selection and folds."
  (interactive)
  (when goblins--details
    (setq goblins--details nil)
    (goblins--render)
    (dolist (fold goblins--overview-folds)
      (when-let* ((section (magit-get-section (car fold))))
        (oset section hidden (cdr fold))))
    (magit-section-show magit-root-section)
    (goto-char (point-min))
    (when-let* ((section (magit-get-section (car goblins--overview-position))))
      (goto-char (min (+ (oref section start) (cdr goblins--overview-position))
                      (1- (oref section end)))))
    (setq goblins--overview-position nil goblins--overview-folds nil)))

(defun goblins--render-details ()
  "Render live details for the retained immutable identity."
  (let* ((kind (nth 0 goblins--details))
         (id (nth 1 goblins--details))
         (record (and (equal (nth 2 goblins--details) goblins--instance)
                      (cl-find id (append (plist-get goblins--snapshot
                                                     (if (eq kind 'agent) :sessions :permissions)) nil)
                               :key (lambda (r) (plist-get r :id)) :test #'equal)))
         (offset (point))
         (inhibit-read-only t))
    (erase-buffer)
    (magit-insert-section (goblins-section 'details-root)
      (insert (propertize "Goblins details\n" 'font-lock-face 'magit-section-heading)
              (goblins--safe goblins--notice) "\n")
      (goblins--insert-messaging-warning goblins--snapshot)
      (insert "b / q: back to overview    t: terminal    a / d: decide request\n\n")
      (cond
       ((null record) (insert "This entry is no longer retained. Return to the overview with b.\n"))
       ((eq kind 'agent)
        (magit-insert-section (goblins-agent-section id)
          (magit-insert-heading (goblins--safe (plist-get record :agent_name)))
          (goblins--insert-agent-fields record)))
       (t (goblins--insert-request record t))))
    (magit-section-show magit-root-section)
    (goto-char (min offset (1- (point-max))))
    ;; Entering details starts on the identity-bearing section, so actions
    ;; use the displayed request even before the user navigates its fields.
    (unless (or (object-of-class-p (magit-current-section) 'goblins-agent-section)
                (object-of-class-p (magit-current-section) 'goblins-request-section))
      (when-let* ((section (car (oref magit-root-section children))))
        (goto-char (oref section start))))
    (set-buffer-modified-p nil)))

(defun goblins--messaging-warning (snapshot)
  "What the host must know when SNAPSHOT's communications log fails, or nil.
Messages stop then, while goblins keep running and raids keep their owners."
  (let ((health (plist-get snapshot :messaging)))
    (cond ((eq (plist-get health :audit_failed) t)
           "Messaging stopped: the communications log cannot be written")
          ((eq (plist-get health :raid_audit_failed) t)
           "Raid messages stopped: raid events could not be logged (log full)"))))

(defun goblins--insert-messaging-warning (snapshot)
  "Insert SNAPSHOT's messaging warning on a line of its own, if any."
  (when-let* ((warning (goblins--messaging-warning snapshot)))
    (insert (propertize warning 'font-lock-face 'goblins-attention 'face 'goblins-attention)
            "\n")))

(defun goblins--update-header ()
  (setq-local header-line-format
              (if goblins--details
                  " b/q back to overview   t terminal   a/d decide   r refresh"
                (if (bound-and-true-p evil-local-mode)
                    " R run   RET details   t terminal   a/d decide   TAB fold   j/k navigate   gr refresh   q quit"
                  " R run   RET details   t terminal   a/d decide   TAB fold   n/p navigate   g refresh   q quit"))))

(defun goblins--render ()
  (goblins--update-header)
  (if goblins--details (goblins--render-details) (goblins--render-overview))
  (run-hooks 'goblins-snapshot-hook))

(defun goblins--render-overview ()
  "Render state, preserving section identity and folding across updates."
  (let* ((section (magit-current-section))
         (ident (and section (magit-section-ident section)))
         (offset (and section (- (point) (oref section start))))
         (inhibit-read-only t)
         (sessions (append (plist-get goblins--snapshot :sessions) nil))
         (active (cl-remove-if #'goblins--finished-p sessions))
         (stopped (cl-remove-if-not #'goblins--finished-p sessions))
         (requests (append (plist-get goblins--snapshot :permissions) nil))
         (pending (cl-remove-if-not
                   (lambda (r) (equal (plist-get r :state) "pending")) requests)))
    (erase-buffer)
    (magit-insert-section (goblins-section 'root)
      (insert (propertize "Goblins\n" 'font-lock-face 'magit-section-heading)
              (goblins--safe goblins--directory) "\n"
              (goblins--safe goblins--notice) "\n")
      (goblins--insert-messaging-warning goblins--snapshot)
      (insert "\n")
      (magit-insert-section (goblins-section 'agents)
        (magit-insert-heading (format "Agents (%d)" (length active)))
        (unless active (insert "  No active agents\n"))
        (mapc #'goblins--insert-agent active)
        (insert "\n"))
      (magit-insert-section (goblins-section 'pending)
        (magit-insert-heading (format "Pending requests (%d)" (length pending)))
        (unless pending (insert "  No pending requests\n\n"))
        (mapc #'goblins--insert-request pending))
      (magit-insert-section (goblins-section 'recent t)
        (magit-insert-heading "Recent requests")
        (mapc #'goblins--insert-request (cl-set-difference requests pending)))
      (magit-insert-section (goblins-section 'stopped t)
        (magit-insert-heading (format "Stopped agents (%d)" (length stopped)))
        (mapc #'goblins--insert-agent stopped)))
    ;; Apply the saved hidden flags to display overlays after insertion.
    (magit-section-show magit-root-section)
    ;; Never fall back to the row now occupying a vanished request's position.
    (goto-char (point-min))
    (when-let* ((successor (and ident (magit-get-section ident))))
      (goto-char (min (+ (oref successor start) offset)
                      (1- (oref successor end))))))
  (set-buffer-modified-p nil))

(defun goblins--disconnect ()
  (let ((connection goblins--connection))
    (setq goblins--connection nil
          goblins--subscription nil)
    (when connection
      ;; `jsonrpc-shutdown' only waits for the process to end: close it first.
      (delete-process (jsonrpc--process connection))
      (jsonrpc-shutdown connection t))))

(defun goblins--quiet-sentinel (sentinel process change)
  "Run jsonrpc's SENTINEL for PROCESS and CHANGE without its exit message.
jsonrpc announces every closed connection, twice for each deliberate
disconnect; the status buffer's notice already reports it."
  (let ((inhibit-message t) (message-log-max nil))
    (funcall sentinel process change)))

(defun goblins--cancel-reconnect ()
  (when goblins--reconnect-timer
    (cancel-timer goblins--reconnect-timer)
    (setq goblins--reconnect-timer nil)))

(defun goblins--schedule-reconnect ()
  "Arrange the next reconnection attempt; return its delay, or nil if none."
  (goblins--cancel-reconnect)
  (when-let* ((delays goblins-reconnect-delays)
              (delay (nth (min goblins--reconnect-attempt (1- (length delays))) delays)))
    (let ((buffer (current-buffer)))
      (setq goblins--reconnect-timer
            (run-at-time
             delay nil
             (lambda ()
               (when (buffer-live-p buffer)
                 (with-current-buffer buffer
                   (setq goblins--reconnect-timer nil)
                   (unless (or goblins--connection (process-live-p goblins--server-process))
                     (setq goblins--reconnect-attempt (1+ goblins--reconnect-attempt))
                     (goblins--connect-now))))))))
    delay))

(defun goblins--fail (message)
  (goblins--disconnect)
  (let ((delay (goblins--schedule-reconnect)))
    (setq goblins--notice
          (concat message
                  (when (and goblins--decisions
                             (let (pending)
                               (maphash (lambda (_id state)
                                          (when (eq state t) (setq pending t)))
                                        goblins--decisions)
                               pending))
                    "; in-flight decision outcome unknown")
                  (if delay
                      (format "; reconnecting in %ss (r now)" delay)
                    "; r to reconnect"))))
  (goblins--render))

(defun goblins--announce-new (items seen hook)
  "Run HOOK with each of ITEMS whose id the buffer-local SEEN did not hold,
unless it is `unset', and leave SEEN holding ITEMS' ids.  SEEN is updated
before HOOK runs: a hook that waits, as a desktop notification does, can
process the next snapshot meanwhile, which must not announce them again."
  (let ((before (symbol-value seen)))
    (set seen (mapcar (lambda (item) (plist-get item :id)) items))
    (unless (eq before 'unset)
      (dolist (item items)
        (unless (member (plist-get item :id) before)
          (condition-case-unless-debug err
              (run-hook-with-args hook item)
            (error (message "%s: %S" hook err))))))))

(defun goblins--note-requests ()
  "Run `goblins-request-hook' for requests that became pending, and
`goblins-approval-hook' for goblins that began waiting for approval."
  ;; Settled requests never become pending again, so only pending IDs matter.
  (goblins--announce-new (cl-remove-if-not
                          (lambda (r) (equal (plist-get r :state) "pending"))
                          (append (plist-get goblins--snapshot :permissions) nil))
                         'goblins--seen-requests 'goblins-request-hook)
  (goblins--note-approvals))

(defun goblins--waiting-approval (snapshot)
  "The running goblins of SNAPSHOT waiting at their agent's own permission
prompt."
  (cl-remove-if-not (lambda (s) (and (equal (plist-get s :state) "running")
                                     (equal (plist-get s :agent_state) "waiting-approval")))
                    (append (plist-get snapshot :sessions) nil)))

(defun goblins--note-approvals ()
  "Run `goblins-approval-hook' for goblins that began waiting for approval.
A goblin that stops waiting and waits again is announced again: it is a new
prompt."
  (goblins--announce-new (goblins--waiting-approval goblins--snapshot)
                         'goblins--seen-approvals 'goblins-approval-hook))

(defun goblins--echo-unless-shown (buffer text)
  "Show TEXT in the echo area when no window shows status BUFFER."
  (unless (get-buffer-window buffer t)
    (message "Goblins: %s" text)))

(defun goblins--server-command (action)
  "Run server ACTION asynchronously for this buffer's state directory."
  (goblins--ensure-status-buffer)
  (when (process-live-p goblins--server-process)
    (user-error "A server command is already in progress"))
  (let ((buffer (current-buffer))
        (output (generate-new-buffer " *goblins server command*")))
    (when (equal action "stop") (goblins--disconnect))
    (setq goblins--notice (if (equal action "start")
                              "Starting server…"
                            "Stopping server and all agents…"))
    (goblins--render)
    (condition-case err
        (setq goblins--server-process
              (make-process
               :name "goblins-server-command" :buffer output
               :command (list goblins-executable "--state-dir" goblins--directory
                              "server" action)
               :connection-type 'pipe :noquery t
               :sentinel
               (lambda (process _event)
                 (when (memq (process-status process) '(exit signal))
                   (unwind-protect
                       (when (buffer-live-p buffer)
                         (with-current-buffer buffer
                           (when (eq process goblins--server-process)
                             (setq goblins--server-process nil)
                             (let ((result (with-current-buffer output
                                             (string-trim (buffer-string)))))
                               (if (= (process-exit-status process) 0)
                                   (progn
                                     (goblins--echo-unless-shown buffer result)
                                     (if (equal action "start")
                                         (goblins-refresh)
                                       (setq goblins--snapshot nil goblins--instance nil
                                             goblins--sequence nil goblins--decisions nil
                                             goblins--notice "Server stopped")
                                       (goblins--render)))
                                 (goblins--fail
                                  (format "Server %s failed: %s" action result))
                                 (goblins--echo-unless-shown buffer goblins--notice))))))
                     (when (buffer-live-p output) (kill-buffer output)))))))
      (error
       (kill-buffer output)
       (setq goblins--server-process nil)
       (goblins--fail (format "Cannot %s server: %s" action
                              (error-message-string err)))
       (goblins--echo-unless-shown buffer goblins--notice)))))

(defun goblins-start-server ()
  "Start the server and connect, opening its status buffer if needed."
  (interactive)
  (goblins--server-command "start"))

;;;###autoload
(defun goblins-ensure-server (&optional directory)
  "Start the server for DIRECTORY unless it is already running, then connect.
Unlike `goblins-start-server' this never displays the status buffer: it
reports in the echo area, so it suits init files and hooks."
  (interactive)
  (with-current-buffer (goblins--status-buffer directory)
    (cond (goblins--subscription (message "Goblins: Server already running"))
          ((process-live-p goblins--server-process)
           (message "Goblins: A server command is already in progress"))
          ;; `server start' leaves a running server alone, so this also
          ;; covers one that is up but not yet connected.
          (t (goblins--server-command "start")))))

(defun goblins-stop-server ()
  "Stop the server and all its agents, opening its status buffer if needed."
  (interactive)
  (goblins--server-command "stop"))

(defun goblins--changed (_connection method params)
  (if (and (eq method 'state.changed)
           (equal (plist-get params :subscription) goblins--subscription)
           (integerp (plist-get params :sequence))
           goblins--sequence
           (= (plist-get params :sequence) (1+ goblins--sequence))
           (equal (plist-get (plist-get params :snapshot) :instance)
                  goblins--instance))
      (progn
        (setq goblins--sequence (plist-get params :sequence)
              goblins--snapshot (plist-get params :snapshot))
        (goblins--note-requests)
        (goblins--render))
    (goblins--fail "State stream changed or lost events")))

(defun goblins--request (method params success)
  "Send METHOD with PARAMS; call SUCCESS in the status buffer."
  (let ((buffer (current-buffer))
        (connection goblins--connection))
    (jsonrpc-async-request
     connection method params :timeout 3
     :success-fn (lambda (result)
                   (when (buffer-live-p buffer)
                     (with-current-buffer buffer
                       (when (eq connection goblins--connection)
                         (funcall success result)))))
     :error-fn (lambda (error)
                 (when (buffer-live-p buffer)
                   (with-current-buffer buffer
                     (when (eq connection goblins--connection)
                       (if (not (jsonrpc-running-p connection))
                           (goblins--fail "Disconnected")
                         (when (eq method 'permissions.decide)
                           (remhash (plist-get params :request) goblins--decisions))
                         (goblins--fail
                          (format "RPC error: %s" (plist-get error :message))))))))
     :timeout-fn (lambda ()
                   (when (buffer-live-p buffer)
                     (with-current-buffer buffer
                       (when (eq connection goblins--connection)
                         (goblins--fail "RPC timed out"))))))))

;;;###autoload
(defun goblins-refresh ()
  "Reconnect and obtain a fresh authoritative snapshot."
  (interactive)
  (goblins--ensure-status-buffer)
  (when (process-live-p goblins--server-process)
    (user-error "A server command is in progress; status will update automatically"))
  (setq goblins--reconnect-attempt 0)
  (goblins--connect-now))

(defun goblins--connect-now ()
  "Connect this status buffer and subscribe to the daemon's state."
  (goblins--cancel-reconnect)
  (goblins--disconnect)
  ;; Clear stale display before changing daemon identities.
  (setq goblins--snapshot nil goblins--instance nil goblins--sequence nil
        goblins--decisions (make-hash-table :test #'equal)
        goblins--notice "Connecting…")
  (goblins--render)
  (let ((buffer (current-buffer)))
    (condition-case err
        (progn
          (setq goblins--connection
                (make-instance
                 'jsonrpc-process-connection
                 :name (format "goblins %s" goblins--directory)
                 :process (make-network-process
                           :name "goblins" :family 'local
                           :service (expand-file-name "host.sock" goblins--directory)
                           :coding 'binary :noquery t)
                 :notification-dispatcher
                 (lambda (connection method params)
                   (when (buffer-live-p buffer)
                     (with-current-buffer buffer
                       (when (eq connection goblins--connection)
                         (goblins--changed connection method params)))))
                 :on-shutdown
                 (lambda (connection)
                   (when (buffer-live-p buffer)
                     (with-current-buffer buffer
                       (when (eq connection goblins--connection)
                         (setq goblins--connection nil)
                         (goblins--fail "Disconnected")))))))
          (add-function :around (process-sentinel (jsonrpc--process goblins--connection))
                        #'goblins--quiet-sentinel)
          (goblins--request
           'initialize '(:api 1)
           (lambda (result)
             (if (not (and (equal (plist-get result :api) 1)
                           (equal (plist-get result :role) "host")
                           (stringp (plist-get result :instance))))
                 (goblins--fail "Incompatible Goblins daemon")
               (setq goblins--instance (plist-get result :instance))
               (goblins--request
                'state.subscribe (make-hash-table)
                (lambda (state)
                  (if (not (and (stringp (plist-get state :subscription))
                                (integerp (plist-get state :sequence))
                                (equal (plist-get (plist-get state :snapshot) :instance)
                                       goblins--instance)))
                      (goblins--fail "Invalid initial snapshot")
                    (setq goblins--subscription (plist-get state :subscription)
                          goblins--sequence (plist-get state :sequence)
                          goblins--snapshot (plist-get state :snapshot)
                          goblins--notice "Connected"
                          goblins--reconnect-attempt 0)
                    (goblins--note-requests)
                    (goblins--render))))))))
      (error (goblins--fail (format "Cannot connect: %s"
                                   (error-message-string err)))))))

(defun goblins--host-call (source method params)
  "Call the host METHOD with PARAMS on SOURCE's connection and return the result.
Daemon refusals become `user-error's and leave the connection alone."
  (with-current-buffer source
    (unless (and goblins--connection goblins--subscription)
      (user-error "Disconnected from the Goblins server"))
    (condition-case err
        (jsonrpc-request goblins--connection method params :timeout 5)
      (jsonrpc-error
       (user-error "%s" (or (alist-get 'jsonrpc-error-message (cdr err)) "Request failed"))))))

(defun goblins--section-shows-p (section)
  "Whether SECTION, a request section, shows its whole request: rendered
with its details, and neither it nor a parent folded."
  (and (object-of-class-p section 'goblins-request-section)
       (oref section details)
       (cl-loop for s = section then (oref s parent)
                while s never (oref s hidden))))

(defun goblins--decide (approved)
  (let* ((section (and (derived-mode-p 'goblins-status-mode) (magit-current-section)))
         (record
          (if (derived-mode-p 'goblins-status-mode)
              (and section (object-of-class-p section 'goblins-request-section)
                   (oref section record))
            (goblins--ensure-connected)
            (goblins--read-request (if approved "Accept request: " "Deny request: ")
                                   (current-buffer)))))
    (goblins--decide-record record approved (and section (goblins--section-shows-p section)))))

(defun goblins--request-candidates (records)
  "RECORDS, pending requests, as completion candidates like \"gribble: jq\",
numbered \"<2>\" and on when they would read alike.  Each carries its
request ID as the `goblins-request' property."
  (let ((seen (make-hash-table :test #'equal)))
    (mapcar (lambda (record)
              (let* ((label (format "%s: %s"
                                    (goblins--session-label goblins--snapshot
                                                            (plist-get record :session))
                                    (goblins--request-subject record)))
                     (count (puthash label (1+ (gethash label seen 0)) seen)))
                (propertize (if (> count 1) (format "%s <%d>" label count) label)
                            'goblins-request (plist-get record :id))))
            records)))

(defun goblins--request-annotation (record)
  "Describe the pending request RECORD for completion: its kind and reason."
  (concat (propertize " " 'display '(space :align-to 40))
          (propertize (format "%-9s" (goblins--safe (or (plist-get record :kind) "package")))
                      'face 'goblins-dim)
          " " (goblins--safe (or (plist-get record :reason) ""))))

(defun goblins--read-request (prompt source)
  "Choose a pending request of SOURCE's server with PROMPT; return its record.
Candidates are in the `goblins-request' completion category."
  (let* ((records (cl-remove-if-not
                   (lambda (request) (equal (plist-get request :state) "pending"))
                   (append (plist-get (buffer-local-value 'goblins--snapshot source)
                                      :permissions)
                           nil)))
         (candidates (with-current-buffer source (goblins--request-candidates records)))
         (annotate (lambda (candidate)
                     (when-let* ((record (cl-find (get-text-property 0 'goblins-request candidate)
                                                  records :key (lambda (r) (plist-get r :id))
                                                  :test #'equal)))
                       (goblins--request-annotation record)))))
    (unless records (user-error "No pending requests"))
    (let* ((choice (completing-read
                    prompt
                    (lambda (string predicate action)
                      (if (eq action 'metadata)
                          `(metadata (category . goblins-request)
                                     (annotation-function . ,annotate))
                        (complete-with-action action candidates string predicate)))
                    nil t))
           (found (car (member choice candidates))))
      (unless found (user-error "Select a request"))
      (nth (cl-position found candidates :test #'equal) records))))

(declare-function goblins--review-request "goblins-views" (source record))
(declare-function goblins--popup-show "goblins-views" (source id))

(defun goblins--decide-record (record approved &optional shown)
  "Send the decision APPROVED for RECORD, a pending request, from a status buffer.
SHOWN means the caller displays the whole request.  A host command is
accepted only then: its command, directory, variables and time limit are
what the approval grants, and a list or completion shows only a label.
Otherwise its details open instead."
  (let ((id (plist-get record :id)))
    (unless (and goblins--connection goblins--subscription)
      (user-error "Disconnected; use M-x goblins-refresh to reconnect"))
    (unless (equal (plist-get record :state) "pending")
      (user-error "Place point on a pending request"))
    (when (equal (plist-get record :kind) "raid-owner")
      (when (fboundp 'goblins--popup-show)
        (goblins--popup-show (current-buffer) (plist-get record :session)))
      (user-error "A paused raid takes a new owner or dissolves: o, x or k in its request popup"))
    (when (and approved (equal (plist-get record :kind) "host-command") (not shown))
      (when (fboundp 'goblins--review-request)
        (goblins--review-request (current-buffer) record))
      (user-error "Review the whole host command, then accept it there"))
    ;; The preview is the approver's whole basis for a refresh.
    (when (and approved (goblins--awaiting-preview-p record))
      (user-error "The refresh is still being evaluated; decide once its preview is shown"))
    (when (gethash id goblins--decisions)
      (user-error "Decision already sent for this request"))
    ;; Capture the displayed immutable identity before yielding to process I/O.
    (puthash id t goblins--decisions)
    (goblins--request
     'permissions.decide
     (list :session (plist-get record :session) :request id
           :approval (plist-get record :approval)
           :approved (if approved t :json-false))
     (lambda (_result)
       (puthash id 'acknowledged goblins--decisions)
       (setq goblins--notice
             (format "%s %s / %s%s"
                     (if approved "Accepted" "Denied")
                     (plist-get record :agent_name) (plist-get record :package)
                     (if approved "; provisioning continues" "")))
       (goblins--render)))))

(defun goblins-accept ()
  "Accept the request at point in status, or choose one from another buffer."
  (interactive)
  (goblins--decide t))

(defun goblins-deny ()
  "Deny the request at point in status, or choose one from another buffer."
  (interactive)
  (goblins--decide nil))

(defun goblins-quit ()
  "Return from details, or close status without stopping agents."
  (interactive)
  (if (and (derived-mode-p 'goblins-status-mode) goblins--details)
      (goblins-back)
    (let ((buffer (if (derived-mode-p 'goblins-status-mode)
                      (current-buffer)
                    (get-buffer (format "*Goblins: %s*" (goblins--resolve-directory))))))
      (when buffer
        (if (eq (window-buffer (selected-window)) buffer)
            (quit-window t)
          (kill-buffer buffer))))))

(defun goblins--choose (prompt choices empty-message)
  "Select a value from CHOICES, an alist, using PROMPT."
  (unless choices (user-error "%s" empty-message))
  (let ((choice (assoc (completing-read prompt choices nil t) choices)))
    (unless choice (user-error "Select an entry"))
    (cdr choice)))

(defun goblins--terminal-buffer (session)
  (let ((buffer (gethash (list goblins--directory goblins--instance session)
                         goblins--terminal-buffers)))
    (and (buffer-live-p buffer) buffer)))

(defun goblins--render-status-buffers ()
  (dolist (buffer (buffer-list))
    (with-current-buffer buffer
      (when (derived-mode-p 'goblins-status-mode) (goblins--render)))))

(defun goblins--forget-terminal ()
  (remhash goblins--session-key goblins--terminal-buffers)
  (goblins--render-status-buffers))

(defvar goblins--pins (make-hash-table :test #'equal)
  "/goblins: host to its pin (DIRECTORY INSTANCE SESSION).
A host is resolved to one goblin session once; every later use passes that
identity to `goblins exec', which fails rather than reach another goblin.
Only releasing the stopped goblin's host removes its pin.")

(defun goblins-pin (host)
  "HOST's pin (DIRECTORY INSTANCE SESSION), or nil."
  (gethash host goblins--pins))

(defun goblins-pin-host (host directory instance session)
  "Pin /goblins:HOST: to SESSION of the daemon at DIRECTORY, INSTANCE.
Return HOST, or signal an error when HOST is pinned to another goblin.  The
daemon calls this through emacsclient before opening a goblin's files."
  (unless (and (stringp host) (stringp directory) (stringp instance) (stringp session))
    (error "Goblins: invalid pin"))
  (let ((pin (list directory instance session))
        (old (gethash host goblins--pins)))
    (cond ((null old) (puthash host pin goblins--pins) host)
          ((equal old pin) host)
          (t (error "Goblins: /goblins:%s: is pinned to another goblin" host)))))

(defun goblins--pin-or-id (host directory instance session)
  "HOST pinned to SESSION, or SESSION itself when HOST is pinned elsewhere."
  (condition-case nil
      (goblins-pin-host host directory instance session)
    (error (goblins-pin-host session directory instance session))))

(defun goblins--release-stopped-pins ()
  "Release the pins of this status buffer's goblins that stopped.
Also those of another instance of this daemon, which restarted."
  (when (and goblins--snapshot goblins--instance)
    (let ((sessions (append (plist-get goblins--snapshot :sessions) nil)))
      (maphash
       (lambda (host pin)
         (when (and (equal (car pin) goblins--directory)
                    (or (not (equal (nth 1 pin) goblins--instance))
                        (not (equal (plist-get (cl-find (nth 2 pin) sessions
                                                        :key (lambda (s) (plist-get s :id))
                                                        :test #'equal)
                                               :state)
                                    "running"))))
           (if (fboundp 'goblins-tramp-release)
               (goblins-tramp-release host (nth 2 pin))
             (remhash host goblins--pins))))
       (copy-hash-table goblins--pins)))))

(defun goblins--reset-stopped-terminals ()
  "Move goblin terminals whose goblin stopped out of its sandbox.
Uses each terminal's immutable session identity, pinned or not."
  (when (and goblins--snapshot goblins--instance)
    (let ((directory goblins--directory)
          (instance goblins--instance)
          (sessions (append (plist-get goblins--snapshot :sessions) nil)))
      (dolist (buffer (buffer-list))
        (with-current-buffer buffer
          (let ((identity (bound-and-true-p ghostel-identity)))
            (when (and (memq (alist-get 'kind identity) '(goblins goblins-shell))
                       (stringp default-directory)
                       (string-prefix-p "/goblins:" default-directory)
                       (equal goblins--directory directory)
                       (or (not (equal (alist-get 'instance identity) instance))
                           (not (equal (plist-get (cl-find (alist-get 'session identity) sessions
                                                           :key (lambda (s) (plist-get s :id))
                                                           :test #'equal)
                                                  :state)
                                       "running"))))
              (setq default-directory (expand-file-name "~/")))))))))

(defun goblins--after-snapshot ()
  (goblins--reset-stopped-terminals)
  (goblins--release-stopped-pins))
(add-hook 'goblins-snapshot-hook #'goblins--after-snapshot)

(defun goblins--osc7-directory (report host)
  "The /goblins: directory for an OSC 7 REPORT from inside goblin HOST."
  (when (string-match "\\`\\(?:file\\|kitty-shell-cwd\\)://[^/]*\\(/.*\\)\\'" report)
    (let* ((raw (match-string 1 report))
           (path (if (string-prefix-p "file:" report)
                     (progn
                       (require 'url-util)
                       (decode-coding-string
                        (url-unhex-string (encode-coding-string raw 'utf-8) t) 'utf-8-unix))
                   raw)))
      (unless (string-match-p "[\0\n]" path)
        (file-name-as-directory (format "/goblins:%s:%s" host path))))))

(defun goblins--terminal-host ()
  "The /goblins: host of this goblin terminal, from its identity or snapshot."
  (or (alist-get 'host ghostel-identity)
      (when-let* ((key goblins--session-key)
                  (source (get-buffer (format "*Goblins: %s*" (car key))))
                  (session (cl-find (nth 2 key)
                                    (append (plist-get (buffer-local-value 'goblins--snapshot source)
                                                       :sessions)
                                            nil)
                                    :key (lambda (s) (plist-get s :id)) :test #'equal)))
        (plist-get session :agent_name))))

(defun goblins--terminal-session ()
  "The session record of this goblin terminal, from its daemon's snapshot."
  (when-let* ((id (alist-get 'session ghostel-identity))
              (source (cl-find-if (lambda (buffer)
                                    (and (eq (buffer-local-value 'major-mode buffer)
                                             'goblins-status-mode)
                                         (equal (buffer-local-value 'goblins--directory buffer)
                                                goblins--directory)))
                                  (buffer-list))))
    (cl-find id (append (plist-get (buffer-local-value 'goblins--snapshot source) :sessions) nil)
             :key (lambda (s) (plist-get s :id)) :test #'equal)))

(defun goblins--pinned-terminal-host ()
  "This goblin terminal's /goblins: host, pinned to its goblin.
Pinned before any TRAMP use, from the terminal's identity."
  (when-let* ((host (goblins--terminal-host)))
    (goblins--pin-or-id host goblins--directory
                        (alist-get 'instance ghostel-identity)
                        (alist-get 'session ghostel-identity))))

(defun goblins--agent-directory ()
  "The directory of this agent terminal: its goblin's working directory.
That is the project, bound at the same path inside the sandbox, so
`project-find-file' and friends work on the host.  A goblin without one
works in its own /workspace, which only exists inside it."
  (let ((cwd (plist-get (goblins--terminal-session) :cwd)))
    (if (and (stringp cwd) (not (string-empty-p cwd)))
        (file-name-as-directory cwd)
      (when-let* ((host (goblins--pinned-terminal-host)))
        (format "/goblins:%s:/workspace/" host)))))

(defun goblins--ghostel-directory (function directory)
  "Track DIRECTORY reports of goblin terminals.
An agent terminal stays in its goblin's working directory, see
`goblins--agent-directory'.  A goblin shell follows the report as a
/goblins: path: the sandbox's hostname is not an SSH destination, and its
directories need not exist on the host.  Ordinary Ghostel buffers are left
alone."
  (pcase (alist-get 'kind ghostel-identity)
    ('goblins
     (when-let* ((path (goblins--agent-directory)))
       (setq default-directory path)))
    ('goblins-shell
     (when-let* ((host (goblins--pinned-terminal-host))
                 (path (goblins--osc7-directory directory host)))
       (setq default-directory path)))
    (_ (funcall function directory))))

(with-eval-after-load 'ghostel
  (advice-add 'ghostel--update-directory :around #'goblins--ghostel-directory))

(defun goblins-visit ()
  "Open the agent at point in status, or choose a terminal from elsewhere.
On other status sections, toggle their visibility."
  (interactive)
  (if (derived-mode-p 'goblins-status-mode)
      (let ((section (magit-current-section)))
        (if (and section (object-of-class-p section 'goblins-agent-section))
            (if-let* ((buffer (goblins--terminal-buffer (oref section value))))
                (pop-to-buffer buffer)
              (user-error "No Emacs terminal for this agent; launch with M-x goblins-run"))
          (magit-section-toggle section)))
    (let ((directory (goblins--resolve-directory)) choices)
      (maphash (lambda (key buffer)
                 (when (and (equal (car key) directory) (buffer-live-p buffer))
                   (push (cons (format "%s [%s]" (buffer-name buffer) (nth 2 key)) buffer)
                         choices)))
               goblins--terminal-buffers)
      (pop-to-buffer (goblins--choose "Goblin terminal: " choices
                                     "No Emacs-launched terminal buffers for this server")))))

(defun goblins--configurations ()
  "Read current configuration names and manifest from the packaged CLI."
  (with-temp-buffer
    (let ((code (process-file goblins-executable nil t nil "configurations")))
      (unless (eq code 0)
        (user-error "Cannot list goblins: %s" (string-trim (buffer-string))))
      (goto-char (point-min))
      (let ((value (json-parse-buffer :object-type 'plist :array-type 'list)))
        (unless (and (stringp (plist-get value :configuration))
                     (file-name-absolute-p (plist-get value :configuration))
                     (consp (plist-get value :names))
                     (cl-every #'stringp (plist-get value :names)))
          (user-error "Invalid Goblins configuration list"))
        value))))

(defun goblins--read-configuration (names)
  "Choose from NAMES, using the displayed default for empty input."
  (let* ((default (car names))
         (selected (completing-read (format "Run goblin (default %s): " default)
                                    names nil t nil 'goblins-run-history default))
         (name (if (string-empty-p selected) default selected)))
    (unless (member name names)
      (user-error "Select a configured goblin"))
    name))

;;;###autoload
(defun goblins--launch-location (&optional nix)
  "Return (CWD . DEV-SHELL) for a launch from the current buffer.
Inside a project.el project, launch from its root and, when the root has a
flake.nix, ask whether to use it as the dev shell.  Otherwise use
`default-directory' and no dev shell.  NIX non-nil means the configuration
has allowNix: its agent runs nix develop itself, so there is no question."
  (let* ((project (and (require 'project nil t) (project-current)))
         (cwd (expand-file-name (if project (project-root project) default-directory))))
    (cons cwd
          (and project
               (not nix)
               (file-exists-p (expand-file-name "flake.nix" cwd))
               (y-or-n-p (format "Use %s as the dev shell? "
                                 (abbreviate-file-name (expand-file-name "flake.nix" cwd))))
               (directory-file-name (file-truename cwd))))))

(defcustom goblins-remove-nobreak-underline t
  "Whether agent terminals draw no-break spaces without an underline.
Claude's TUI uses U+00A0 in glyphs such as its thinking indicator, and Emacs
paints those with the underlined `nobreak-space' face."
  :type 'boolean)

(defvar goblins-snapshot-hook nil
  "Normal hook run in a status buffer after it renders a new snapshot.")

(defun goblins--require-ghostel ()
  ;; Require the installed module directly, bypassing Ghostel's auto-download.
  (unless (and (require 'ghostel nil t) (require 'ghostel-module nil t)
               (fboundp 'ghostel-exec))
    (user-error "Goblins terminals require installed Ghostel with its native module")))

(defun goblins--open-terminal (session agent-name cwd &optional display)
  "Attach a new Ghostel buffer to SESSION and return it.
Call in a connected status buffer.  AGENT-NAME names the buffer and CWD is its
`default-directory'.  DISPLAY, default `goblins--display-new-agent', shows
the new buffer.
On failure the buffer is killed and the error re-signalled."
  (goblins--require-ghostel)
  (let* ((instance goblins--instance)
         (key (list goblins--directory instance session))
         (executable goblins-executable)
         (buffer (generate-new-buffer (format "*Goblin: %s*" agent-name))))
    (condition-case err
        (progn
          (with-current-buffer buffer (setq default-directory cwd))
          (funcall (or display #'goblins--display-new-agent) buffer)
          (let ((ghostel-kill-buffer-on-exit nil))
            (ghostel-exec buffer executable
                          (list "--state-dir" (car key) "attach" session
                                "--instance" instance)
                          `((kind . goblins) (session . ,session) (instance . ,instance))))
          (with-current-buffer buffer
            (setq-local goblins--session-key key)
            (setq-local goblins--directory (car key))
            (setq-local ghostel-kill-buffer-on-exit nil)
            (when goblins-remove-nobreak-underline
              (face-remap-add-relative 'nobreak-space :underline nil))
            (goblin-mode 1)
            (add-hook 'kill-buffer-hook #'goblins--forget-terminal nil t))
          (puthash key buffer goblins--terminal-buffers)
          (goblins--note-selected (cons (car key) session))
          (goblins--render-status-buffers)
          buffer)
      (error
       (when (buffer-live-p buffer) (kill-buffer buffer))
       (signal (car err) (cdr err))))))

(defun goblins--launch-key ()
  "A fresh idempotency key for `sessions.start'."
  (concat "emacs-" (md5 (format "%s%s%s" (float-time) (random) (emacs-pid)))))

(defun goblins-run (&optional directory display)
  "Choose a configured goblin and launch it in a new Ghostel buffer.
Launch from the current project's root, or the current directory outside a
project; see `goblins--launch-location' for the dev shell choice.  Use this
status buffer's server, or the default server outside a status buffer.
DIRECTORY, if given, replaces the current directory as the launch location,
and DISPLAY, default `goblins--display-new-agent', shows the new terminal.
The status buffer is never displayed.
Only launches made here are linked by the terminal command."
  (interactive)
  (let ((default-directory (file-name-as-directory
                            (expand-file-name (or directory default-directory)))))
    (when (file-remote-p default-directory)
      (user-error (if (string-prefix-p "/goblins:" default-directory)
                      "This buffer is inside a goblin; run goblins from a local directory"
                    "Goblins requires a local working directory")))
    (goblins--require-ghostel)
    (let* ((state-directory (or goblins--directory goblins-state-directory
                                (goblins--default-directory)))
           (configs (goblins--configurations))
           (name (goblins--read-configuration (plist-get configs :names)))
           (location (goblins--launch-location
                      (member name (plist-get configs :nix))))
           (cwd (car location))
           (dev-shell (cdr location))
           (status (goblins--connect state-directory)))
      (with-current-buffer status
        (let* ((connection goblins--connection)
               (launch (jsonrpc-request
                        connection 'sessions.start
                        (append
                         (list :key (goblins--launch-key)
                               :name name :configuration (plist-get configs :configuration)
                               :cwd cwd :rows 24 :cols 80)
                         (and dev-shell (list :dev_shell dev-shell)))
                        :timeout 3))
               (session (plist-get launch :session)))
          (condition-case err
              (goblins--open-terminal session (plist-get launch :agent_name) cwd display)
            (error
             (ignore-errors
               (jsonrpc-request connection 'sessions.stop (list :session session) :timeout 3))
             (signal (car err) (cdr err)))))))))

(defvar goblins-status-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map magit-section-mode-map)
    (define-key map (kbd "g") #'goblins-refresh)
    (define-key map (kbd "r") #'goblins-refresh)
    (define-key map (kbd "R") #'goblins-run)
    (define-key map (kbd "a") #'goblins-accept)
    (define-key map (kbd "d") #'goblins-deny)
    (define-key map (kbd "q") #'goblins-quit)
    map))

;; Update existing keymaps too when this file is reloaded.
(dolist (binding '(("RET" . goblins-details) ("TAB" . goblins-toggle-section)
                   ("<tab>" . goblins-toggle-section) ("b" . goblins-back)
                   ("t" . goblins-visit)))
  (define-key goblins-status-mode-map (kbd (car binding)) (cdr binding)))

;; Also remove the previous bindings when reloading into an existing Emacs.
(define-key goblins-status-mode-map (kbd "s") nil)
(define-key goblins-status-mode-map (kbd "S") nil)

;; Keep Evil optional and support either package load order.  These bindings
;; belong only to this mode; in particular, preserve Evil's gg/g prefixes.
(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-status-mode 'normal)
  (evil-define-key* '(normal motion) goblins-status-mode-map
    (kbd "j") #'magit-section-forward
    (kbd "k") #'magit-section-backward
    (kbd "h") #'magit-section-hide
    (kbd "l") #'magit-section-show
    (kbd "TAB") #'goblins-toggle-section
    (kbd "<tab>") #'goblins-toggle-section
    (kbd "RET") #'goblins-details
    (kbd "b") #'goblins-back
    (kbd "t") #'goblins-visit
    (kbd "za") #'goblins-toggle-section
    (kbd "a") #'goblins-accept
    (kbd "d") #'goblins-deny
    (kbd "r") #'goblins-refresh
    (kbd "gr") #'goblins-refresh
    (kbd "s") nil
    (kbd "S") nil
    (kbd "R") #'goblins-run
    (kbd "q") #'goblins-quit))

(define-derived-mode goblins-status-mode magit-section-mode "Goblins"
  "Status of Goblins agents and permission requests.
\<goblins-status-mode-map>
Use \[goblins-accept] to accept and \[goblins-deny] to deny.
Use \[goblins-refresh] to reconnect."
  (goblins--update-header)
  (setq-local revert-buffer-function (lambda (&rest _) (goblins-refresh)))
  (add-hook 'kill-buffer-hook #'goblins--disconnect nil t)
  (add-hook 'kill-buffer-hook #'goblins--cancel-reconnect nil t))

(defun goblins--resolve-directory (&optional directory)
  "Resolve DIRECTORY, the current Goblins context, or the configured default."
  (directory-file-name
   (expand-file-name (or directory goblins--directory goblins-state-directory
                         (goblins--default-directory)))))

(defun goblins--status-buffer (&optional directory)
  "Return an initialized status buffer for DIRECTORY without connecting."
  (let* ((directory (goblins--resolve-directory directory))
         (buffer (get-buffer-create (format "*Goblins: %s*" directory))))
    (with-current-buffer buffer
      (unless (derived-mode-p 'goblins-status-mode)
        (goblins-status-mode))
      (setq goblins--directory directory))
    buffer))

(defun goblins--ensure-status-buffer ()
  "Switch to initialized status before changing any frontend state."
  (unless (and (derived-mode-p 'goblins-status-mode) goblins--directory)
    (pop-to-buffer (goblins--status-buffer))))

(defun goblins--connect (&optional directory)
  "Return DIRECTORY's status buffer, connected and with a snapshot.
Unlike `goblins-status' this never displays the buffer."
  (let ((buffer (goblins--status-buffer directory)))
    (with-current-buffer buffer
      (unless (or goblins--connection (process-live-p goblins--server-process))
        (goblins-refresh))
      (let ((deadline (+ (float-time) 3)))
        (while (and goblins--connection (not goblins--subscription)
                    (< (float-time) deadline))
          (accept-process-output nil 0.01)))
      (unless goblins--subscription
        (user-error "Server disconnected; use M-x goblins-start-server")))
    buffer))

(defun goblins--ensure-connected (&optional directory)
  "Open status for DIRECTORY and await its initial snapshot."
  (goblins-status directory)
  (let ((deadline (+ (float-time) 3)))
    (while (and goblins--connection (not goblins--subscription)
                (< (float-time) deadline))
      (accept-process-output nil 0.01)))
  (unless goblins--subscription
    (user-error "Server disconnected; use M-x goblins-start-server")))

;; The workspace lives in its own file, which loads only when first used.  A
;; plain autoload form, not a cookie, so it works without generated autoloads.
(autoload 'goblins-workspace "goblins-workspace" nil t)
(autoload 'goblins-sidebar-dwim "goblins-workspace" nil t)
(autoload 'goblins-sidebar-toggle "goblins-workspace" nil t)
(autoload 'goblins-show-in-workspace "goblins-workspace" nil t)
(autoload 'goblins-messages "goblins-messages" nil t)
(autoload 'goblins-shell "goblins-tramp" nil t)
(autoload 'goblins-eshell "goblins-tramp" nil t)
(autoload 'goblins-dired "goblins-tramp" nil t)
(autoload 'goblins-find-file "goblins-tramp" nil t)

;;;###autoload
(defun goblins-status (&optional directory)
  "Show Goblins status for DIRECTORY.
With a prefix argument, prompt for the daemon state directory."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "
                                            (or goblins--directory
                                                goblins-state-directory
                                                (goblins--default-directory))))))
  (pop-to-buffer (goblins--status-buffer directory))
  (unless (or goblins--connection (process-live-p goblins--server-process))
    (goblins-refresh)))

(provide 'goblins)

;; The /goblins: TRAMP method, registered whenever TRAMP loads.
(with-eval-after-load 'tramp
  (require 'goblins-tramp))

;; Files a goblin's editor sends wait for approval: the server must check
;; them before visiting, which is what would load TRAMP and goblins-tramp.
(declare-function goblins-tramp--server-execute "goblins-tramp" (function proc files &rest args))
(defun goblins--server-execute (function proc files &rest args)
  "Around `server-execute': hand /goblins: files to `goblins-editor-approval'."
  (if (cl-some (lambda (file) (string-prefix-p "/goblins:" (car file))) files)
      (progn (require 'goblins-tramp)
             (apply #'goblins-tramp--server-execute function proc files args))
    (apply function proc files args)))
(with-eval-after-load 'server
  (advice-add 'server-execute :around #'goblins--server-execute))

;; Terminals turn on `goblin-mode'; loaded last since it builds on this file.
(require 'goblins-terminal)
;; Commands on a chosen goblin, and what Consult, Embark and Marginalia make
;; of goblins once they load.
(require 'goblins-completion)
;;; goblins.el ends here
