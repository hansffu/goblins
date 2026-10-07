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

(defface goblins-sidebar-header '((t :inherit font-lock-keyword-face :weight bold))
  "Raid and directory headings in the sidebar." :group 'goblins)
(defface goblins-sidebar-running '((t :inherit success))
  "Running goblins in the sidebar." :group 'goblins)
(defface goblins-sidebar-failed '((t :inherit error))
  "Failed goblins in the sidebar." :group 'goblins)
(defface goblins-sidebar-active '((t :inherit highlight :extend t))
  "The goblin whose terminal is shown in the main window." :group 'goblins)

(defconst goblins--sidebar-buffer-name "*Goblins Sidebar*")

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

(defun goblins--sidebar-glyph (session)
  (let ((state (plist-get session :state)))
    (cond ((equal state "running")
           (propertize "●" 'font-lock-face 'goblins-sidebar-running))
          ((equal state "failed")
           (propertize "✗" 'font-lock-face 'goblins-sidebar-failed))
          ((equal state "stopped") (propertize "○" 'font-lock-face 'goblins-dim))
          (t (propertize "◌" 'font-lock-face 'goblins-dim)))))

(defun goblins--sidebar-insert-group (key indent label)
  (let ((collapsed (member key goblins--sidebar-collapsed)))
    (insert (propertize
             (concat (make-string indent ?\s) (if collapsed "▸ " "▾ ")
                     (goblins--safe label) "\n")
             'font-lock-face 'goblins-sidebar-header
             'goblins-group key))
    (not collapsed)))

(defun goblins--sidebar-insert-tree (tree depth group-key permissions)
  (let* ((session (car tree))
         (id (plist-get session :id))
         (pending (goblins--sidebar-pending id permissions))
         (name (goblins--safe (plist-get session :agent_name)))
         (finished (member (plist-get session :state) '("stopped" "failed")))
         (start (point)))
    (insert (make-string (* 2 depth) ?\s)
            (goblins--sidebar-glyph session) " "
            (if finished (propertize name 'font-lock-face 'goblins-dim) name)
            (if (> pending 0)
                (propertize (format " !%d" pending) 'font-lock-face 'goblins-attention)
              "")
            "\n")
    (add-text-properties start (point) (list 'goblins-id id 'goblins-in-group group-key))
    (dolist (child (cdr tree))
      (goblins--sidebar-insert-tree child (1+ depth) group-key permissions))))

(defun goblins--sidebar-render ()
  "Redraw the sidebar from its source buffer's snapshot, keeping point."
  (let* ((source goblins--sidebar-source)
         (inhibit-read-only t)
         (windows (get-buffer-window-list (current-buffer) nil t))
         (keys (mapcar (lambda (w) (cons w (goblins--sidebar-key-at (window-point w))))
                       windows))
         (point-key (goblins--sidebar-key-at (point)))
         (line (line-number-at-pos)))
    (erase-buffer)
    (if (not (and (buffer-live-p source)
                  (buffer-local-value 'goblins--snapshot source)))
        (insert (propertize
                 (format "%s\n" (goblins--safe
                                 (if (buffer-live-p source)
                                     (buffer-local-value 'goblins--notice source)
                                   "No server")))
                 'font-lock-face 'goblins-dim))
      (let* ((snapshot (buffer-local-value 'goblins--snapshot source))
             (sessions (append (plist-get snapshot :sessions) nil))
             (permissions (append (plist-get snapshot :permissions) nil)))
        (if (null sessions)
            (insert (propertize "No goblins\n" 'font-lock-face 'goblins-dim))
          (dolist (raid (goblins--sidebar-groups sessions))
            (let* ((rkey (car raid))
                   (indent (if rkey 2 0))
                   (show (or (null rkey)
                             (goblins--sidebar-insert-group
                              (list 'raid rkey) 0
                              (format "raid %s" (or (cadr raid) rkey))))))
              (when show
                (dolist (dir (cddr raid))
                  (let ((gkey (list 'dir rkey (car dir))))
                    (when (goblins--sidebar-insert-group
                           gkey indent
                           (if (string-empty-p (car dir)) "unknown"
                             (abbreviate-file-name (file-name-as-directory (car dir)))))
                      (dolist (tree (goblins--agent-forest (cdr dir)))
                        (goblins--sidebar-insert-tree
                         tree (1+ (/ indent 2)) gkey permissions)))))))))))
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
      (with-current-buffer buffer (goblins--sidebar-render)))))

(defun goblins--sidebar-after-snapshot ()
  (goblins--sidebar-render-for (current-buffer)))
(add-hook 'goblins-snapshot-hook #'goblins--sidebar-after-snapshot)

;;;; Windows

(defun goblins--sidebar-window ()
  (get-buffer-window goblins--sidebar-buffer-name))

(defun goblins--main-window ()
  "Return the most recently used window that is not the sidebar."
  (let ((windows (cl-remove-if (lambda (w)
                                 (or (window-parameter w 'window-side)
                                     (window-minibuffer-p w)))
                               (window-list))))
    (car (sort windows (lambda (a b) (> (window-use-time a) (window-use-time b)))))))

(defun goblins--main-buffer-default-directory ()
  (if-let* ((window (goblins--main-window)))
      (buffer-local-value 'default-directory (window-buffer window))
    default-directory))

(defun goblins--show-in-main (buffer)
  "Display BUFFER in the main window and select it."
  (let ((window (or (goblins--main-window)
                    (user-error "No window for the main view"))))
    (set-window-buffer window buffer)
    (select-window window)
    buffer))

(defun goblins--sidebar-update-active ()
  "Mark the goblin whose terminal is shown in the main window."
  (when (buffer-live-p (get-buffer goblins--sidebar-buffer-name))
    (with-current-buffer goblins--sidebar-buffer-name
      (when goblins--sidebar-active-overlay
        (delete-overlay goblins--sidebar-active-overlay))
      (let* ((window (goblins--main-window))
             (shown (and window (window-buffer window)))
             (key (and shown (buffer-local-value 'goblins--session-key shown)))
             (pos (and key (goblins--sidebar-find (cons 'id (nth 2 key))))))
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
On a group heading, fold or unfold it."
  (interactive)
  (if-let* ((id (get-text-property (line-beginning-position) 'goblins-id)))
      (let* ((source goblins--sidebar-source)
             (session (goblins--sidebar-session id)))
        (unless (buffer-live-p source) (user-error "Goblins server buffer is gone"))
        (with-current-buffer source
          (if-let* ((buffer (goblins--terminal-buffer id)))
              (goblins--show-in-main buffer)
            (when (member (plist-get session :state) '("stopped" "failed"))
              (user-error "This goblin has finished"))
            (goblins--open-terminal
             id (plist-get session :agent_name)
             (if (stringp (plist-get session :cwd))
                 (file-name-as-directory (plist-get session :cwd))
               default-directory)
             #'goblins--show-in-main))))
    (goblins-sidebar-toggle)))

(defun goblins-sidebar-permissions ()
  "Show the Goblins status buffer, with its pending requests, in the main window."
  (interactive)
  (unless (buffer-live-p goblins--sidebar-source)
    (user-error "Goblins server buffer is gone"))
  (goblins--show-in-main goblins--sidebar-source))

(defun goblins-sidebar-run ()
  "Launch a new goblin from the main window's project."
  (interactive)
  (let ((directory (goblins--main-buffer-default-directory)))
    (when-let* ((window (goblins--main-window))) (select-window window))
    (with-current-buffer goblins--sidebar-source
      (let ((default-directory directory)) (goblins-run)))))

(defun goblins-sidebar-refresh ()
  "Reconnect the server buffer for a fresh snapshot."
  (interactive)
  (with-current-buffer goblins--sidebar-source (goblins-refresh)))

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
                       ("TAB" . goblins-sidebar-toggle) ("<tab>" . goblins-sidebar-toggle)
                       ("g" . goblins-sidebar-refresh) ("r" . goblins-sidebar-refresh)
                       ("R" . goblins-sidebar-run) ("q" . goblins-workspace-quit)))
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
    (kbd "TAB") #'goblins-sidebar-toggle
    (kbd "<tab>") #'goblins-sidebar-toggle
    (kbd "gr") #'goblins-sidebar-refresh
    (kbd "r") #'goblins-sidebar-refresh
    (kbd "R") #'goblins-sidebar-run
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
              header-line-format " Goblins   RET open   p permissions"
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
    (if (goblins--sidebar-window)
        (when-let* ((window (goblins--main-window))) (select-window window))
      (let ((sidebar (goblins--sidebar-buffer source)))
        (set-window-buffer (selected-window) (goblins--initial-main-buffer source))
        (let ((window (display-buffer-in-side-window
                       sidebar `((side . left) (slot . 0)
                                 (window-width . ,goblins-sidebar-width)
                                 (window-parameters (no-delete-other-windows . t))))))
          (set-window-dedicated-p window t))))
    (goblins--sidebar-update-active)))

(provide 'goblins-workspace)
;;; goblins-workspace.el ends here
