;;; goblins-workspace.el --- Sidebar workspace for Goblins -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; M-x goblins-workspace shows a sidebar listing every goblin, grouped by
;; raid and then by project directory, beside a main window holding the last
;; used goblin terminal (or the Goblins status buffer).
;;
;; Window management is deliberately pluggable: set
;; `goblins-workspace-switch-or-create-function' to e.g.
;; `tabspaces-switch-or-create-workspace' to use your own workspace package.

;;; Code:

(require 'cl-lib)
(require 'subr-x)
(require 'goblins)
(require 'goblins-views)
(require 'goblins-raids)
(require 'project)
(require 'hl-line)

(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))
(declare-function evil-make-intercept-map "evil-core" (keymap &optional state aux))
(declare-function goblins-shell "goblins-tramp" (&optional session display source))
(declare-function goblins-eshell "goblins-tramp" (&optional session display source))
(declare-function goblins-dired "goblins-tramp" (&optional session display source))
(defvar ghostel-identity)
(defvar ghostel--process)

(defcustom goblins-workspace-name "goblins"
  "Name passed to `goblins-workspace-switch-or-create-function'."
  :type 'string :group 'goblins)

(defcustom goblins-sidebar-width 34
  "Width of the Goblins sidebar in columns."
  :type 'integer :group 'goblins)

(defvar goblins-workspace-switch-or-create-function #'goblins-workspace-default-switch
  "Function called with the workspace name to switch to or create a workspace.
`goblins-workspace' then builds the sidebar layout if it is not yet there.
The default saves the window configuration and clears the frame.  Point it at
your workspace package instead, for example
`tabspaces-switch-or-create-workspace'.")

(defvar goblins-workspace-quit-function #'goblins-workspace-default-quit
  "Function called with no arguments to leave the workspace.
The default closes the sidebar and restores the configuration saved by
`goblins-workspace-default-switch'.  Set this to close your workspace instead.")

(defface goblins-sidebar-header '((t :inherit goblins-heading))
  "Raid and directory headings in the sidebar." :group 'goblins)
(defface goblins-sidebar-selected '((t :inherit highlight :extend t))
  "The sidebar line at point, which the sidebar's keys act on." :group 'goblins)
(defface goblins-sidebar-active '((t :weight bold))
  "The goblin or shell whose terminal is shown in the main window." :group 'goblins)
(defface goblins-sidebar-active-marker '((t :inherit font-lock-keyword-face :weight bold))
  "The bar before the goblin or shell shown in the main window." :group 'goblins)

(defvar-local goblins--sidebar-source nil
  "The status buffer whose snapshot this sidebar shows.")
(defcustom goblins-sidebar-show-finished nil
  "Whether the sidebar lists stopped and failed goblins.
The status buffer always lists them."
  :type 'boolean :group 'goblins)

(defvar goblins--sidebar-folds (make-hash-table :test #'equal)
  "Folded group keys per server state directory, for this Emacs session only.")

(defun goblins--sidebar-collapsed ()
  "Group keys whose members are hidden in this sidebar's server."
  (gethash goblins--directory goblins--sidebar-folds))

(defun goblins--sidebar-set-collapsed (keys)
  (puthash goblins--directory keys goblins--sidebar-folds))
(defvar-local goblins--sidebar-active-overlays nil)

;;;; Data

(defun goblins--sidebar-groups (sessions)
  "Return SESSIONS as ((RAID-KEY RAID-NAME (DIR . SESSIONS)...)...).
Raids come first by name, then goblins outside any raid (RAID-KEY nil).
Directories and sessions keep snapshot order inside each group."
  (let (raids)
    (dolist (session sessions)
      (let* ((raid (plist-get session :raid))
             (rkey (plist-get raid :id))
             (dir (let ((cwd (plist-get session :cwd)))
                    (if (stringp cwd) (directory-file-name cwd) "")))
             (entry (or (assoc rkey raids)
                        (car (push (list rkey (plist-get raid :name)) raids))))
             (dirs (cddr entry))
             (group (or (assoc dir dirs)
                        (let ((new (list dir)))
                          (setcdr (cdr entry) (append dirs (list new)))
                          new))))
        (setcdr group (append (cdr group) (list session)))))
    (sort raids
          (lambda (a b)
            (cond ((and (car a) (null (car b))) t)
                  ((and (null (car a)) (car b)) nil)
                  (t (string< (or (cadr a) "") (or (cadr b) ""))))))))

(defun goblins--sidebar-pending (session-id permissions)
  "Count pending PERMISSIONS requests of SESSION-ID."
  (cl-count-if (lambda (r) (and (equal (plist-get r :session) session-id)
                                (equal (plist-get r :state) "pending")))
               permissions))

;;;; Rendering

(defun goblins--sidebar-shells (source)
  "Hash of session ID to the open shells of SOURCE's daemon, by number."
  (let ((shells (make-hash-table :test #'equal))
        (directory (buffer-local-value 'goblins--directory source)))
    (dolist (buffer (buffer-list))
      (with-current-buffer buffer
        (when (and goblins--shell-number (equal goblins--directory directory))
          (push buffer (gethash (alist-get 'session (bound-and-true-p ghostel-identity)) shells)))))
    (maphash (lambda (id buffers)
               (puthash id (sort buffers (lambda (a b)
                                           (< (buffer-local-value 'goblins--shell-number a)
                                              (buffer-local-value 'goblins--shell-number b))))
                        shells))
             shells)
    shells))

(defun goblins--sidebar-key-at (pos)
  "Return the identity of the line at POS, for restoring point."
  (save-excursion
    (goto-char pos)
    (let ((p (line-beginning-position)))
      (or (and (get-text-property p 'goblins-terminal)
               (cons 'terminal (get-text-property p 'goblins-terminal)))
          (and (get-text-property p 'goblins-id)
               (cons 'id (get-text-property p 'goblins-id)))
          (and (get-text-property p 'goblins-group)
               (cons 'group (get-text-property p 'goblins-group)))))))

(defun goblins--sidebar-find (key)
  "Return the start of the line identified by KEY, or nil."
  (when key
    (let ((prop (pcase (car key)
                  ('id 'goblins-id) ('terminal 'goblins-terminal) (_ 'goblins-group)))
          (pos (point-min)) found)
      (while (and (not found) (setq pos (next-single-property-change pos prop)))
        (when (equal (get-text-property pos prop) (cdr key))
          (setq found pos)))
      (or found
          (and (equal (get-text-property (point-min) prop) (cdr key))
               (point-min))))))

(defun goblins--sidebar-badge (pending)
  (if (> pending 0) (propertize (format " !%d" pending) 'face 'goblins-attention) ""))

(defun goblins--sidebar-insert-group (key indent label collapsed &optional properties pending)
  "Insert the heading of group KEY, with text PROPERTIES and a PENDING badge.
Return non-nil unless KEY is in COLLAPSED."
  (let* ((folded (member key collapsed))
         (text (concat
                (propertize
                 (concat (make-string indent ?\s) (if folded "▸ " "▾ ")
                         (goblins--safe label))
                 'face 'goblins-sidebar-header)
                (goblins--sidebar-badge (or pending 0))
                "\n")))
    (add-text-properties 0 (length text) (list 'goblins-group key) text)
    (add-text-properties 0 (length text) properties text)
    (insert text)
    (not folded)))

(defun goblins--sidebar-insert-shell (shell depth id group-key)
  "Insert the line of SHELL, a terminal inside goblin ID, at DEPTH."
  (let ((start (point))
        (number (buffer-local-value 'goblins--shell-number shell)))
    (insert (make-string (* 2 depth) ?\s) "› "
            (if (process-live-p (buffer-local-value 'ghostel--process shell))
                (format "shell%s" (if (> number 1) (format " %d" number) ""))
              (propertize (format "shell%s (exited)" (if (> number 1) (format " %d" number) ""))
                          'face 'goblins-dim))
            "\n")
    (add-text-properties start (point) (list 'goblins-terminal shell 'goblins-parent id
                                             'goblins-in-group group-key))))

(defun goblins--sidebar-insert-tree (tree depth group-key permissions shells)
  "Insert the line of TREE's goblin, its SHELLS' lines, then its children."
  (let* ((session (car tree))
         (id (plist-get session :id))
         (pending (goblins--sidebar-pending id permissions))
         (name (goblins--safe (plist-get session :agent_name)))
         (finished (member (plist-get session :state) '("stopped" "failed")))
         (start (point)))
    (insert (make-string (* 2 depth) ?\s)
            (goblins--state-glyph (plist-get session :state)) " "
            (if finished (propertize name 'face 'goblins-dim) name)
            (propertize (format " (%s)" (goblins--safe (plist-get session :name)))
                        'face 'goblins-dim)
            (goblins--exec-summary session)
            (goblins--sidebar-badge pending)
            "\n")
    (add-text-properties start (point) (list 'goblins-id id 'goblins-in-group group-key))
    (when (> pending 0)
      (put-text-property start (point) 'goblins-pending t))
    (dolist (shell (gethash id shells))
      (goblins--sidebar-insert-shell shell (1+ depth) id group-key))
    (dolist (child (cdr tree))
      (goblins--sidebar-insert-tree child (1+ depth) group-key permissions shells))))

(defun goblins--sidebar-finished-p (session)
  (member (plist-get session :state) '("stopped" "failed")))

(defun goblins--sidebar-sessions (snapshot)
  "The sessions of SNAPSHOT the sidebar lists."
  (let ((sessions (append (plist-get snapshot :sessions) nil)))
    (if goblins-sidebar-show-finished
        sessions
      (cl-remove-if #'goblins--sidebar-finished-p sessions))))

(defun goblins--sidebar-group-keys (session)
  "The keys of the groups that contain SESSION."
  (let* ((rkey (goblins--session-raid-id session))
         (cwd (plist-get session :cwd))
         (dir (if (stringp cwd) (directory-file-name cwd) "")))
    (delq nil (list (and rkey (list 'raid rkey)) (list 'dir rkey dir)))))

(defun goblins--sidebar-text (source collapsed)
  "Return the sidebar text for SOURCE's snapshot, folding the groups in COLLAPSED.
The text is built apart from the sidebar so a failure cannot blank it."
  (with-temp-buffer
    (let ((snapshot (and (buffer-live-p source)
                         (buffer-local-value 'goblins--snapshot source))))
      (if (not snapshot)
          (insert (propertize
                   (format "%s\n" (goblins--safe
                                   (if (buffer-live-p source)
                                       (replace-regexp-in-string
                                        "\\<r\\( to reconnect\\| now\\)" "g\\1"
                                        (buffer-local-value 'goblins--notice source))
                                     "No server; g to reconnect")))
                   'face 'goblins-dim))
        (let* ((sessions (goblins--sidebar-sessions snapshot))
               (shells (goblins--sidebar-shells source))
               (permissions (append (plist-get snapshot :permissions) nil))
               (count (lambda (members)
                        (apply #'+ (mapcar (lambda (s) (goblins--sidebar-pending
                                                        (plist-get s :id) permissions))
                                           members)))))
          (if (null sessions)
              (insert (propertize "No running goblins\n" 'face 'goblins-dim))
            (dolist (raid (goblins--sidebar-groups sessions))
              (let* ((rkey (car raid))
                     (indent (if rkey 2 0)))
                (when (or (null rkey)
                          (goblins--sidebar-insert-group
                           (list 'raid rkey) 0
                           (format "raid %s" (or (cadr raid) rkey)) collapsed
                           (list 'goblins-raid (or (cadr raid) rkey))
                           (funcall count (apply #'append (mapcar #'cdr (cddr raid))))))
                  (dolist (dir (cddr raid))
                    (let ((gkey (list 'dir rkey (car dir))))
                      (when (goblins--sidebar-insert-group
                             gkey indent
                             (if (string-empty-p (car dir)) "unknown"
                               (abbreviate-file-name (file-name-as-directory (car dir))))
                             collapsed nil (funcall count (cdr dir)))
                        (dolist (tree (goblins--agent-forest (cdr dir)))
                          (goblins--sidebar-insert-tree
                           tree (1+ (/ indent 2)) gkey permissions shells)))))))))))
      (buffer-string))))

(defun goblins--sidebar-render ()
  "Redraw the sidebar from its source buffer's snapshot, keeping point."
  (let* ((text (goblins--sidebar-text goblins--sidebar-source (goblins--sidebar-collapsed)))
         (inhibit-read-only t)
         (windows (get-buffer-window-list (current-buffer) nil t))
         (keys (mapcar (lambda (w) (cons w (goblins--sidebar-key-at (window-point w))))
                       windows))
         (point-key (goblins--sidebar-key-at (point)))
         (line (line-number-at-pos)))
    (erase-buffer)
    (insert text)
    (dolist (entry keys)
      (when (window-live-p (car entry))
        (set-window-point (car entry)
                          (or (goblins--sidebar-find (cdr entry))
                              (save-excursion (goto-char (point-min))
                                              (forward-line (1- line))
                                              (point))))))
    (goto-char (or (goblins--sidebar-find point-key)
                   (progn (goto-char (point-min)) (forward-line (1- line)) (point))))
    (goblins--sidebar-update-active)))

(defun goblins--sidebar-render-for (source)
  "Redraw every sidebar showing SOURCE."
  (dolist (buffer (buffer-list))
    (when (eq (buffer-local-value 'goblins--sidebar-source buffer) source)
      (with-current-buffer buffer
        (condition-case err (goblins--sidebar-render)
          (error (message "goblins sidebar: %s" (error-message-string err))))))))

(defun goblins--sidebar-render-all ()
  "Redraw every sidebar, after goblin shells opened, exited or closed."
  (dolist (buffer (buffer-list))
    (when (buffer-local-value 'goblins--sidebar-source buffer)
      (with-current-buffer buffer
        (condition-case err (goblins--sidebar-render)
          (error (message "goblins sidebar: %s" (error-message-string err))))))))

(defun goblins--sidebar-after-snapshot ()
  (goblins--sidebar-render-for (current-buffer)))
(add-hook 'goblins-snapshot-hook #'goblins--sidebar-after-snapshot)

;;;; Windows

(defun goblins--sidebar-window ()
  (get-buffer-window goblins--sidebar-buffer-name))

(defun goblins--sidebar-update-active ()
  "Mark the goblins and shells whose terminals are shown beside the sidebar."
  (when (buffer-live-p (get-buffer goblins--sidebar-buffer-name))
    (with-current-buffer goblins--sidebar-buffer-name
      (mapc #'delete-overlay goblins--sidebar-active-overlays)
      (setq goblins--sidebar-active-overlays nil)
      (dolist (shown (mapcar #'window-buffer (goblins--main-windows)))
        (let* ((key (buffer-local-value 'goblins--session-key shown))
               (id (if key (nth 2 key) (buffer-local-value 'goblins--view-session shown)))
               (pos (if (buffer-local-value 'goblins--shell-number shown)
                        (goblins--sidebar-find (cons 'terminal shown))
                      (and id (goblins--sidebar-find (cons 'id id))))))
          (when pos
            ;; Bold, with a bar over the line's first (indentation) column;
            ;; the line at point keeps its own highlight.
            (let ((line (make-overlay pos (save-excursion (goto-char pos)
                                                          (1+ (line-end-position)))))
                  (bar (make-overlay pos (1+ pos))))
              (overlay-put line 'face 'goblins-sidebar-active)
              (overlay-put bar 'display (propertize "▌" 'face 'goblins-sidebar-active-marker))
              (push line goblins--sidebar-active-overlays)
              (push bar goblins--sidebar-active-overlays))))))))

(defun goblins--sidebar-window-changed (_frame-or-window)
  (goblins--sidebar-update-active))
(add-hook 'window-buffer-change-functions #'goblins--sidebar-window-changed)
(add-hook 'window-selection-change-functions #'goblins--sidebar-window-changed)

;;;; Commands

(defun goblins--sidebar-lines (direction)
  "Move to the next line in DIRECTION (1 or -1): a group, a goblin or a shell."
  (let ((start (point)) found)
    (while (and (not found) (zerop (forward-line direction)))
      (when (or (get-text-property (point) 'goblins-id)
                (get-text-property (point) 'goblins-terminal)
                (get-text-property (point) 'goblins-group))
        (setq found t)))
    (if found
        (beginning-of-line)
      (goto-char start))))

(defun goblins-sidebar-next (&optional n)
  "Move to the next goblin or group, N times."
  (interactive "p")
  (dotimes (_ (abs (or n 1)))
    (goblins--sidebar-lines (if (< (or n 1) 0) -1 1))))

(defun goblins-sidebar-previous (&optional n)
  "Move to the previous goblin or group, N times."
  (interactive "p")
  (goblins-sidebar-next (- (or n 1))))

(defun goblins--sidebar-ensure-source ()
  "Return this sidebar's status buffer, recreating and reconnecting it if killed."
  (unless (buffer-live-p goblins--sidebar-source)
    (setq goblins--sidebar-source (goblins--status-buffer goblins--directory))
    (with-current-buffer goblins--sidebar-source (goblins-refresh)))
  goblins--sidebar-source)

(defun goblins--sidebar-session (id)
  (cl-find id (append (plist-get (buffer-local-value 'goblins--snapshot goblins--sidebar-source)
                                 :sessions)
                      nil)
           :key (lambda (s) (plist-get s :id)) :test #'equal))

(defun goblins-sidebar-toggle ()
  "Fold or unfold the group at point, or the group of the goblin at point."
  (interactive)
  (let ((key (or (get-text-property (line-beginning-position) 'goblins-group)
                 (get-text-property (line-beginning-position) 'goblins-in-group))))
    (unless key (user-error "Nothing to fold here"))
    (let ((folds (goblins--sidebar-collapsed)))
      (goblins--sidebar-set-collapsed
       (if (member key folds) (remove key folds) (cons key folds))))
    (let ((anchor (cons 'group key)))
      (goblins--sidebar-render)
      (when-let* ((pos (goblins--sidebar-find anchor))) (goto-char pos)))))

(defun goblins-sidebar-open ()
  "Show the goblin at point in the main window, attaching if necessary.
On a shell show it; on a raid heading open its page; on a directory heading
fold or unfold it."
  (interactive)
  (cond
   ((get-text-property (line-beginning-position) 'goblins-terminal)
    (goblins-select-goblin (goblins--sidebar-ensure-source)
                           (get-text-property (line-beginning-position) 'goblins-parent)
                           (get-text-property (line-beginning-position) 'goblins-terminal)))
   ((get-text-property (line-beginning-position) 'goblins-id)
    (goblins-open-session (goblins--sidebar-ensure-source)
                          (get-text-property (line-beginning-position) 'goblins-id)))
   ((get-text-property (line-beginning-position) 'goblins-raid)
    (goblins--display-main
     (goblins-raid-buffer (goblins--sidebar-ensure-source)
                          (get-text-property (line-beginning-position) 'goblins-raid))))
   (t (goblins-sidebar-toggle))))

(defun goblins--sidebar-goblin-at-point ()
  "Return (SOURCE . SESSION-ID) for the goblin on this line, or of its shell."
  (let ((id (or (get-text-property (line-beginning-position) 'goblins-id)
                (get-text-property (line-beginning-position) 'goblins-parent))))
    (unless id (user-error "Select a goblin"))
    (cons (goblins--sidebar-ensure-source) id)))

(defun goblins-sidebar-permissions ()
  "Show the permission requests of the goblin at point in the main window.
The current request is focused; earlier ones follow as history."
  (interactive)
  (let ((goblin (goblins--sidebar-goblin-at-point)))
    (goblins--show-in-main (goblins-permissions-buffer (car goblin) (cdr goblin)))))

(defun goblins-sidebar-invite ()
  "Invite the goblin at point into a raid you choose.
On a raid heading, invite a goblin you choose into that raid."
  (interactive)
  (let ((id (get-text-property (line-beginning-position) 'goblins-id))
        (raid (get-text-property (line-beginning-position) 'goblins-raid))
        (source (goblins--sidebar-ensure-source)))
    (cond
     (id
      (let* ((snapshot (buffer-local-value 'goblins--snapshot source))
             (names (mapcar (lambda (r) (plist-get r :name)) (goblins--snapshot-raids snapshot))))
        (unless names (user-error "No raid yet; create one with r"))
        (when (goblins--session-raid-id (goblins--snapshot-session snapshot id))
          (user-error "%s is already in a raid"
                      (goblins--session-path (goblins--snapshot-session snapshot id))))
        (goblins-invite-to-raid
         source id (completing-read
                    (format "Invite %s to raid: "
                            (goblins--session-path (goblins--snapshot-session snapshot id)))
                    names nil t))))
     (raid
      (let ((snapshot (buffer-local-value 'goblins--snapshot source)))
        (goblins--raid-invite
         source raid (plist-get (goblins--read-session (goblins--raid-candidates snapshot)
                                                       (format "Invite to %s: " raid))
                                :id))))
     (t (user-error "Select a goblin or a raid")))))

(defun goblins-sidebar-info ()
  "Show what the daemon knows about the goblin at point in the main window."
  (interactive)
  (let ((goblin (goblins--sidebar-goblin-at-point)))
    (goblins--show-in-main (goblins-info-buffer (car goblin) (cdr goblin)))))

(defun goblins--sidebar-directory-at-point ()
  "Return the project directory of the goblin or directory group on this line."
  (let* ((props (list (or (get-text-property (line-beginning-position) 'goblins-id)
                          (get-text-property (line-beginning-position) 'goblins-parent))
                      (get-text-property (line-beginning-position) 'goblins-group)))
         (id (car props))
         (group (or (cadr props)
                    (get-text-property (line-beginning-position) 'goblins-in-group)))
         (cwd (if id
                  (plist-get (goblins--sidebar-session id) :cwd)
                (and (eq (car-safe group) 'dir) (nth 2 group)))))
    (if (and (stringp cwd) (not (string-empty-p cwd)))
        cwd
      (user-error "Select a goblin or a directory with a known project"))))

(defun goblins--sidebar-run-in (directory)
  (with-current-buffer (goblins--sidebar-ensure-source)
    (goblins-run directory)))

(defun goblins-sidebar-run ()
  "Run a new goblin in the project of the goblin or directory at point."
  (interactive)
  (goblins--sidebar-run-in (goblins--sidebar-directory-at-point)))

(defun goblins-sidebar-run-in-project ()
  "Choose a project, then run a new goblin there."
  (interactive)
  (goblins--sidebar-run-in (project-prompt-project-dir)))

(defun goblins--sidebar-next-waiting (source key)
  "Return the ID of the next goblin of SOURCE with a pending request after the
line identified by KEY, in sidebar order with every group unfolded.  Wrap
around, but never return the goblin on that line itself."
  (with-temp-buffer
    (insert (goblins--sidebar-text source nil))
    (let* ((here (and key (goblins--sidebar-find key)))
           (from (if here (save-excursion (goto-char here) (forward-line 1) (point))
                   (point-min)))
           (found (or (text-property-any from (point-max) 'goblins-pending t)
                      (and here (text-property-any (point-min) here 'goblins-pending t)))))
      (and found (get-text-property found 'goblins-id)))))

(defun goblins-sidebar-next-pending ()
  "Move to the next goblin with a pending request, unfolding its groups.
Goblins in folded groups count too, in the order of the unfolded sidebar."
  (interactive)
  (let* ((source (goblins--sidebar-ensure-source))
         (target (goblins--sidebar-next-waiting source (goblins--sidebar-key-at (point)))))
    (if (not target)
        (message "No other goblin is waiting for a decision")
      (let* ((session (goblins--snapshot-session
                       (buffer-local-value 'goblins--snapshot source) target))
             (keys (goblins--sidebar-group-keys session))
             (folds (goblins--sidebar-collapsed)))
        (when (cl-intersection folds keys :test #'equal)
          (goblins--sidebar-set-collapsed (cl-set-difference folds keys :test #'equal))
          (goblins--sidebar-render))
        (goto-char (goblins--sidebar-find (cons 'id target)))))))

(defun goblins-sidebar-stop ()
  "Stop the goblin at point, and its children, after confirming."
  (interactive)
  (let* ((goblin (goblins--sidebar-goblin-at-point))
         (source (car goblin))
         (snapshot (buffer-local-value 'goblins--snapshot source))
         (session (goblins--snapshot-session snapshot (cdr goblin)))
         (children (goblins--live-descendants snapshot (cdr goblin))))
    (when (y-or-n-p (format "Stop %s%s? " (goblins--session-path session)
                            (if children
                                (format " and its %d running descendant%s"
                                        (length children) (if (cdr children) "s" ""))
                              "")))
      (goblins--host-call source 'sessions.stop
                          (append (list :session (cdr goblin))
                                  (and children (list :kill_children t))))
      (message "Stopping %s" (goblins--session-path session)))))

(defun goblins--sidebar-open-in (open)
  "Call OPEN with the session record and status buffer of the goblin at
point (or of its shell); it shows the buffer right of the goblin's agent."
  (let* ((goblin (goblins--sidebar-goblin-at-point))
         (session (goblins--snapshot-session
                   (buffer-local-value 'goblins--snapshot (car goblin)) (cdr goblin))))
    (unless session (user-error "The daemon no longer retains this goblin"))
    (funcall open session nil (car goblin))))

(defun goblins-sidebar-terminal ()
  "Open a new shell terminal inside the goblin at point; it is listed below it."
  (interactive)
  (goblins--sidebar-open-in #'goblins-shell))

(defun goblins-sidebar-eshell ()
  "Open Eshell inside the goblin at point."
  (interactive)
  (goblins--sidebar-open-in #'goblins-eshell))

(defun goblins-sidebar-dired ()
  "Open Dired inside the goblin at point, in its working directory."
  (interactive)
  (goblins--sidebar-open-in #'goblins-dired))

(defun goblins-sidebar-kill-execs ()
  "Kill every shell and command running inside the goblin at point."
  (interactive)
  (let* ((goblin (goblins--sidebar-goblin-at-point))
         (session (goblins--snapshot-session
                   (buffer-local-value 'goblins--snapshot (car goblin)) (cdr goblin))))
    (when (y-or-n-p (format "Kill all shells and commands in %s? "
                            (goblins--session-path session)))
      (goblins--host-call (car goblin) 'sessions.kill_execs (list :session (cdr goblin))))))

(defun goblins-sidebar-destroy-raid ()
  "Dissolve the raid on this heading after confirming.  Its goblins keep running."
  (interactive)
  (let ((raid (get-text-property (line-beginning-position) 'goblins-raid)))
    (unless raid (user-error "Select a raid heading"))
    (goblins-destroy-raid (goblins--sidebar-ensure-source) raid)))

(defun goblins-sidebar-help ()
  "Describe the sidebar keys."
  (interactive)
  (goblins--describe-keys
   "Goblins sidebar"
   '(("Move" ("j / k" . "next / previous line")
             ("n" . "next goblin waiting for a decision")
             ("TAB" . "fold the group"))
     ("Goblin" ("RET" . "show or attach its terminal; on a shell, show it")
               ("p" . "permission requests")
               ("I" . "info")
               ("K" . "stop it (and its children)")
               ("t" . "a shell terminal inside it, listed below it")
               ("e" . "Eshell inside it")
               ("d" . "Dired inside it")
               ("X" . "kill its shells and commands"))
     ("Raids" ("r" . "create a raid around the goblin, or join one")
              ("i" . "invite the goblin to a raid; on a raid, invite into it")
              ("RET" . "on a raid: open its page")
              ("D" . "on a raid: dissolve it"))
     ("Run" ("c" . "run a goblin in the project at point")
            ("C" . "run a goblin in a project you choose"))
     ("Workspace" ("g" . "reconnect") ("q" . "leave") ("?" . "this help")))))

(defun goblins-sidebar-refresh ()
  "Reconnect the server buffer for a fresh snapshot."
  (interactive)
  (if (buffer-live-p goblins--sidebar-source)
      (with-current-buffer goblins--sidebar-source (goblins-refresh))
    (goblins--sidebar-ensure-source)))

(defun goblins-workspace-default-quit ()
  "Close the sidebar and restore the saved window configuration."
  (when-let* ((window (goblins--sidebar-window))) (delete-window window))
  (when (get-buffer goblins--sidebar-buffer-name)
    (kill-buffer goblins--sidebar-buffer-name))
  (when-let* ((config (frame-parameter nil 'goblins-workspace-saved-config)))
    (set-frame-parameter nil 'goblins-workspace-saved-config nil)
    (set-window-configuration config)))

(defun goblins-workspace-quit ()
  "Leave the Goblins workspace, saving the selected goblin's windows."
  (interactive)
  (goblins--save-layout)
  (funcall goblins-workspace-quit-function))

;;;; Mode

(defvar goblins-sidebar-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (dolist (binding '(("j" . goblins-sidebar-next) ("k" . goblins-sidebar-previous)
                       ("<down>" . goblins-sidebar-next) ("<up>" . goblins-sidebar-previous)
                       ("RET" . goblins-sidebar-open) ("p" . goblins-sidebar-permissions)
                       ("i" . goblins-sidebar-invite) ("I" . goblins-sidebar-info)
                       ("r" . goblins-raid) ("?" . goblins-sidebar-help)
                       ("n" . goblins-sidebar-next-pending) ("K" . goblins-sidebar-stop)
                       ("D" . goblins-sidebar-destroy-raid)
                       ("t" . goblins-sidebar-terminal) ("e" . goblins-sidebar-eshell)
                       ("d" . goblins-sidebar-dired) ("X" . goblins-sidebar-kill-execs)
                       ("TAB" . goblins-sidebar-toggle) ("<tab>" . goblins-sidebar-toggle)
                       ("g" . goblins-sidebar-refresh)
                       ("c" . goblins-sidebar-run) ("C" . goblins-sidebar-run-in-project)
                       ("q" . goblins-workspace-quit)))
      (define-key map (kbd (car binding)) (cdr binding)))
    map))

;; Keep Evil optional; its motion state would otherwise shadow j/k/p/RET.
;; The sidebar's own motion-state keys intercept: no minor mode, such as
;; evil-snipe with its t, can shadow them.  Other keys (a leader on SPC,
;; C-w, gg, /) still go through Evil.
(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-sidebar-mode 'motion)
  (evil-define-key* 'motion goblins-sidebar-mode-map
    (kbd "j") #'goblins-sidebar-next
    (kbd "k") #'goblins-sidebar-previous
    (kbd "RET") #'goblins-sidebar-open
    (kbd "p") #'goblins-sidebar-permissions
    (kbd "i") #'goblins-sidebar-invite
    (kbd "I") #'goblins-sidebar-info
    (kbd "r") #'goblins-raid
    (kbd "?") #'goblins-sidebar-help
    (kbd "n") #'goblins-sidebar-next-pending
    (kbd "K") #'goblins-sidebar-stop
    (kbd "D") #'goblins-sidebar-destroy-raid
    (kbd "t") #'goblins-sidebar-terminal
    (kbd "e") #'goblins-sidebar-eshell
    (kbd "d") #'goblins-sidebar-dired
    (kbd "X") #'goblins-sidebar-kill-execs
    (kbd "TAB") #'goblins-sidebar-toggle
    (kbd "<tab>") #'goblins-sidebar-toggle
    (kbd "gr") #'goblins-sidebar-refresh
    (kbd "c") #'goblins-sidebar-run
    (kbd "C") #'goblins-sidebar-run-in-project
    (kbd "q") #'goblins-workspace-quit)
  (evil-make-intercept-map goblins-sidebar-mode-map 'motion t))

(define-derived-mode goblins-sidebar-mode special-mode "Goblins-Sidebar"
  "Sidebar listing goblins grouped by raid and directory.
\\<goblins-sidebar-mode-map>
\\[goblins-sidebar-next] and \\[goblins-sidebar-previous] move, \\[goblins-sidebar-open] opens or attaches,
\\[goblins-sidebar-permissions] shows permission requests, \\[goblins-sidebar-toggle] folds, \\[goblins-workspace-quit] leaves."
  (setq-local truncate-lines t
              cursor-type nil
              window-size-fixed 'width
              mode-line-format nil
              header-line-format " Goblins   ? keys"
              revert-buffer-function (lambda (&rest _) (goblins-sidebar-refresh))
              hl-line-face 'goblins-sidebar-selected)
  ;; Point shows as the highlighted line, never as a cursor, Evil's included.
  (dolist (cursor '(evil-default-cursor evil-normal-state-cursor evil-motion-state-cursor
                    evil-emacs-state-cursor evil-visual-state-cursor
                    evil-operator-state-cursor))
    (set (make-local-variable cursor) (list nil)))
  (hl-line-mode 1))

;;;; Entry points

(defun goblins-workspace-default-switch (_name)
  "Save the window configuration and use the whole frame for the workspace."
  (unless (frame-parameter nil 'goblins-workspace-saved-config)
    (set-frame-parameter nil 'goblins-workspace-saved-config
                         (current-window-configuration)))
  (delete-other-windows))

(defun goblins--sidebar-buffer (source)
  (let ((buffer (get-buffer-create goblins--sidebar-buffer-name)))
    (with-current-buffer buffer
      (unless (derived-mode-p 'goblins-sidebar-mode) (goblins-sidebar-mode))
      (setq goblins--sidebar-source source)
      (setq goblins--directory (buffer-local-value 'goblins--directory source))
      (goblins--sidebar-render))
    buffer))

(defun goblins--workspace-restore (source)
  "Show the goblin of SOURCE's server selected last, with its saved windows.
Else the most recently used goblin terminal, else the status buffer alone."
  (let* ((directory (buffer-local-value 'goblins--directory source))
         (last (and (equal (car goblins--last-goblin) directory)
                    (with-current-buffer source
                      (goblins--terminal-buffer (cdr goblins--last-goblin)))
                    (cdr goblins--last-goblin)))
         (recent (cl-find-if (lambda (buffer)
                               (let ((key (buffer-local-value 'goblins--session-key buffer)))
                                 (and key (equal (car key) directory) (buffer-live-p buffer))))
                             (buffer-list)))
         (id (or last (and recent (nth 2 (buffer-local-value 'goblins--session-key recent))))))
    (if id
        (progn (goblins--enter-goblin source id)
               (goblins--popup-follow source id))
      (setq goblins--last-goblin nil)
      (let ((window (goblins--only-agent-window)))
        (goblins--set-agent-buffer window source)
        (select-window window)))))

;;;###autoload
(defun goblins-workspace (&optional directory)
  "Open the Goblins workspace: a sidebar of goblins beside the main view.
Beside it the goblin selected last: its agent terminal on the left and its
saved windows on the right.  Without any goblin terminal the status buffer
shows until the first agent opens.
With a prefix argument, prompt for the daemon state directory.
Switching or creating the workspace is delegated to
`goblins-workspace-switch-or-create-function'."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "
                                            (or goblins--directory
                                                goblins-state-directory
                                                (goblins--default-directory))))))
  (let ((source (goblins--status-buffer directory)))
    (if (goblins--sidebar-window)
        ;; Already shown: these windows are the selected goblin's.
        (goblins--save-layout)
      (funcall goblins-workspace-switch-or-create-function goblins-workspace-name))
    (with-current-buffer source
      (unless (or goblins--connection (process-live-p goblins--server-process))
        (goblins-refresh)))
    (let ((sidebar (goblins--sidebar-buffer source)))
      (unless (goblins--sidebar-window)
        (let ((window (display-buffer-in-side-window
                       sidebar `((side . left) (slot . 0)
                                 (window-width . ,goblins-sidebar-width)
                                 (window-parameters (no-delete-other-windows . t))))))
          (set-window-dedicated-p window t))))
    (goblins--workspace-restore source)
    (goblins--sidebar-update-active)))

(provide 'goblins-workspace)
;;; goblins-workspace.el ends here
