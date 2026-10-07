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

(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))

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
(defface goblins-sidebar-active '((t :inherit highlight :extend t))
  "The goblin whose terminal is shown in the main window." :group 'goblins)

(defvar-local goblins--sidebar-source nil
  "The status buffer whose snapshot this sidebar shows.")
(defvar-local goblins--sidebar-collapsed nil
  "Group keys whose members are hidden.")
(defvar-local goblins--sidebar-active-overlay nil)

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

(defun goblins--sidebar-key-at (pos)
  "Return the identity of the line at POS, for restoring point."
  (save-excursion
    (goto-char pos)
    (let ((p (line-beginning-position)))
      (or (and (get-text-property p 'goblins-id)
               (cons 'id (get-text-property p 'goblins-id)))
          (and (get-text-property p 'goblins-group)
               (cons 'group (get-text-property p 'goblins-group)))))))

(defun goblins--sidebar-find (key)
  "Return the start of the line identified by KEY, or nil."
  (when key
    (let ((prop (if (eq (car key) 'id) 'goblins-id 'goblins-group))
          (pos (point-min)) found)
      (while (and (not found) (setq pos (next-single-property-change pos prop)))
        (when (equal (get-text-property pos prop) (cdr key))
          (setq found pos)))
      (or found
          (and (equal (get-text-property (point-min) prop) (cdr key))
               (point-min))))))

(defun goblins--sidebar-insert-group (key indent label collapsed &optional properties)
  "Insert the heading of group KEY, with text PROPERTIES.
Return non-nil unless KEY is in COLLAPSED."
  (let* ((folded (member key collapsed))
         (text (propertize
               (concat (make-string indent ?\s) (if folded "▸ " "▾ ")
                       (goblins--safe label) "\n")
               'face 'goblins-sidebar-header
               'goblins-group key)))
    (add-text-properties 0 (length text) properties text)
    (insert text)
    (not folded)))

(defun goblins--sidebar-insert-tree (tree depth group-key permissions)
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
            (if (> pending 0)
                (propertize (format " !%d" pending) 'face 'goblins-attention)
              "")
            "\n")
    (add-text-properties start (point) (list 'goblins-id id 'goblins-in-group group-key))
    (dolist (child (cdr tree))
      (goblins--sidebar-insert-tree child (1+ depth) group-key permissions))))

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
                                        "\\<r to reconnect" "g to reconnect"
                                        (buffer-local-value 'goblins--notice source))
                                     "No server; g to reconnect")))
                   'face 'goblins-dim))
        (let ((sessions (append (plist-get snapshot :sessions) nil))
              (permissions (append (plist-get snapshot :permissions) nil)))
          (if (null sessions)
              (insert (propertize "No goblins\n" 'face 'goblins-dim))
            (dolist (raid (goblins--sidebar-groups sessions))
              (let* ((rkey (car raid))
                     (indent (if rkey 2 0)))
                (when (or (null rkey)
                          (goblins--sidebar-insert-group
                           (list 'raid rkey) 0
                           (format "raid %s" (or (cadr raid) rkey)) collapsed
                           (list 'goblins-raid (or (cadr raid) rkey))))
                  (dolist (dir (cddr raid))
                    (let ((gkey (list 'dir rkey (car dir))))
                      (when (goblins--sidebar-insert-group
                             gkey indent
                             (if (string-empty-p (car dir)) "unknown"
                               (abbreviate-file-name (file-name-as-directory (car dir))))
                             collapsed)
                        (dolist (tree (goblins--agent-forest (cdr dir)))
                          (goblins--sidebar-insert-tree
                           tree (1+ (/ indent 2)) gkey permissions)))))))))))
      (buffer-string))))

(defun goblins--sidebar-render ()
  "Redraw the sidebar from its source buffer's snapshot, keeping point."
  (let* ((text (goblins--sidebar-text goblins--sidebar-source goblins--sidebar-collapsed))
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

(defun goblins--sidebar-after-snapshot ()
  (goblins--sidebar-render-for (current-buffer)))
(add-hook 'goblins-snapshot-hook #'goblins--sidebar-after-snapshot)

;;;; Windows

(defun goblins--sidebar-window ()
  (get-buffer-window goblins--sidebar-buffer-name))

(defun goblins--sidebar-update-active ()
  "Mark the goblin whose terminal is shown in the main window."
  (when (buffer-live-p (get-buffer goblins--sidebar-buffer-name))
    (with-current-buffer goblins--sidebar-buffer-name
      (when goblins--sidebar-active-overlay
        (delete-overlay goblins--sidebar-active-overlay))
      (let* ((window (goblins--main-window))
             (shown (and window (window-buffer window)))
             (key (and shown (buffer-local-value 'goblins--session-key shown)))
             (id (if key
                     (nth 2 key)
                   (and shown (buffer-local-value 'goblins--view-session shown))))
             (pos (and id (goblins--sidebar-find (cons 'id id)))))
        (when pos
          (setq goblins--sidebar-active-overlay
                (make-overlay pos (save-excursion (goto-char pos)
                                                  (1+ (line-end-position)))))
          (overlay-put goblins--sidebar-active-overlay 'face 'goblins-sidebar-active))))))

(defun goblins--sidebar-window-changed (_frame-or-window)
  (goblins--sidebar-update-active))
(add-hook 'window-buffer-change-functions #'goblins--sidebar-window-changed)
(add-hook 'window-selection-change-functions #'goblins--sidebar-window-changed)

;;;; Commands

(defun goblins--sidebar-lines (direction)
  "Move to the next line in DIRECTION (1 or -1) that is a group or a goblin."
  (let ((start (point)) found)
    (while (and (not found) (zerop (forward-line direction)))
      (when (or (get-text-property (point) 'goblins-id)
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
    (setq goblins--sidebar-collapsed
          (if (member key goblins--sidebar-collapsed)
              (delete key goblins--sidebar-collapsed)
            (cons key goblins--sidebar-collapsed)))
    (let ((anchor (cons 'group key)))
      (goblins--sidebar-render)
      (when-let* ((pos (goblins--sidebar-find anchor))) (goto-char pos)))))

(defun goblins-sidebar-open ()
  "Show the goblin at point in the main window, attaching if necessary.
On a raid heading open its page; on a directory heading fold or unfold it."
  (interactive)
  (cond
   ((get-text-property (line-beginning-position) 'goblins-id)
    (goblins-open-session (goblins--sidebar-ensure-source)
                          (get-text-property (line-beginning-position) 'goblins-id)))
   ((get-text-property (line-beginning-position) 'goblins-raid)
    (goblins--display-main
     (goblins-raid-buffer (goblins--sidebar-ensure-source)
                          (get-text-property (line-beginning-position) 'goblins-raid))))
   (t (goblins-sidebar-toggle))))

(defun goblins--sidebar-goblin-at-point ()
  "Return (SOURCE . SESSION-ID) for the goblin on this line."
  (let ((id (get-text-property (line-beginning-position) 'goblins-id)))
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
  (let* ((props (list (get-text-property (line-beginning-position) 'goblins-id)
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
    (goblins-run directory #'goblins--show-in-main)))

(defun goblins-sidebar-run ()
  "Run a new goblin in the project of the goblin or directory at point."
  (interactive)
  (goblins--sidebar-run-in (goblins--sidebar-directory-at-point)))

(defun goblins-sidebar-run-in-project ()
  "Choose a project, then run a new goblin there."
  (interactive)
  (goblins--sidebar-run-in (project-prompt-project-dir)))

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
  "Leave the Goblins workspace."
  (interactive)
  (funcall goblins-workspace-quit-function))

;;;; Mode

(defvar goblins-sidebar-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (dolist (binding '(("j" . goblins-sidebar-next) ("k" . goblins-sidebar-previous)
                       ("<down>" . goblins-sidebar-next) ("<up>" . goblins-sidebar-previous)
                       ("RET" . goblins-sidebar-open) ("p" . goblins-sidebar-permissions)
                       ("i" . goblins-sidebar-invite) ("I" . goblins-sidebar-info)
                       ("r" . goblins-raid) ("?" . describe-mode)
                       ("TAB" . goblins-sidebar-toggle) ("<tab>" . goblins-sidebar-toggle)
                       ("g" . goblins-sidebar-refresh)
                       ("c" . goblins-sidebar-run) ("C" . goblins-sidebar-run-in-project)
                       ("q" . goblins-workspace-quit)))
      (define-key map (kbd (car binding)) (cdr binding)))
    map))

;; Keep Evil optional; its motion state would otherwise shadow j/k/p/RET.
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
    (kbd "?") #'describe-mode
    (kbd "TAB") #'goblins-sidebar-toggle
    (kbd "<tab>") #'goblins-sidebar-toggle
    (kbd "gr") #'goblins-sidebar-refresh
    (kbd "c") #'goblins-sidebar-run
    (kbd "C") #'goblins-sidebar-run-in-project
    (kbd "q") #'goblins-workspace-quit))

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
              revert-buffer-function (lambda (&rest _) (goblins-sidebar-refresh)))
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
      (unless (eq goblins--sidebar-source source)
        (setq goblins--sidebar-source source goblins--sidebar-collapsed nil))
      (setq goblins--directory (buffer-local-value 'goblins--directory source))
      (goblins--sidebar-render))
    buffer))

(defun goblins--initial-main-buffer (source)
  "The most recently used goblin terminal of SOURCE's server, else SOURCE."
  (or (cl-find-if (lambda (buffer)
                    (let ((key (buffer-local-value 'goblins--session-key buffer)))
                      (and key (equal (car key)
                                      (buffer-local-value 'goblins--directory source)))))
                  (buffer-list))
      source))

;;;###autoload
(defun goblins-workspace (&optional directory)
  "Open the Goblins workspace: a sidebar of goblins beside the main view.
The main view is the last used goblin terminal, or the status buffer.
With a prefix argument, prompt for the daemon state directory.
Switching or creating the workspace is delegated to
`goblins-workspace-switch-or-create-function'."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "
                                            (or goblins--directory
                                                goblins-state-directory
                                                (goblins--default-directory))))))
  (let ((source (goblins--status-buffer directory)))
    (unless (goblins--sidebar-window)
      (funcall goblins-workspace-switch-or-create-function goblins-workspace-name))
    (with-current-buffer source
      (unless (or goblins--connection (process-live-p goblins--server-process))
        (goblins-refresh)))
    (let ((sidebar (goblins--sidebar-buffer source)))
      (if (goblins--sidebar-window)
          (when-let* ((window (goblins--main-window))) (select-window window))
        (set-window-buffer (selected-window) (goblins--initial-main-buffer source))
        (let ((window (display-buffer-in-side-window
                       sidebar `((side . left) (slot . 0)
                                 (window-width . ,goblins-sidebar-width)
                                 (window-parameters (no-delete-other-windows . t))))))
          (set-window-dedicated-p window t))))
    (goblins--sidebar-update-active)))

(provide 'goblins-workspace)
;;; goblins-workspace.el ends here
