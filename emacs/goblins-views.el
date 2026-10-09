;;; goblins-views.el --- Per-goblin info and permission views -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; Two buffers scoped to one goblin of a status buffer's server, refreshed
;; whenever the daemon sends a new snapshot:
;;
;; - `goblins-info-mode': what the daemon knows about the goblin.  Extend it
;;   by adding functions to `goblins-info-section-functions'.
;; - `goblins-permissions-mode': the current request, focused and ready to
;;   accept or deny, above the history of earlier requests and their outcome.

;;; Code:

(require 'cl-lib)
(require 'subr-x)
(require 'goblins)

(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))

(defface goblins-heading '((t :inherit font-lock-keyword-face :weight bold))
  "Headings in the Goblins views." :group 'goblins)
(defface goblins-state-running '((t :inherit success))
  "Running goblins." :group 'goblins)
(defface goblins-state-failed '((t :inherit error))
  "Failed goblins and denied requests." :group 'goblins)

(defconst goblins--info-buffer-name "*Goblins Info*")
(defconst goblins--permissions-buffer-name "*Goblins Permissions*")

(defvar-local goblins--view-source nil
  "The status buffer whose snapshot this view shows.")
(defvar-local goblins--view-render nil
  "Function that redraws this view when the snapshot changes.")
(defvar-local goblins--view-session nil
  "Immutable ID of the goblin this view is about.")

;;;; Shared

(defun goblins--state-glyph (state)
  "A one-character marker for the session STATE."
  (cond ((equal state "running") (propertize "●" 'face 'goblins-state-running))
        ((equal state "failed") (propertize "✗" 'face 'goblins-state-failed))
        ((equal state "stopped") (propertize "○" 'face 'goblins-dim))
        (t (propertize "◌" 'face 'goblins-dim))))

(defun goblins--view-snapshot ()
  (and (buffer-live-p goblins--view-source)
       (buffer-local-value 'goblins--snapshot goblins--view-source)))

(defun goblins--snapshot-session (snapshot id)
  (cl-find id (append (plist-get snapshot :sessions) nil)
           :key (lambda (s) (plist-get s :id)) :test #'equal))

(defun goblins--view-session-record ()
  (goblins--snapshot-session (goblins--view-snapshot) goblins--view-session))

(defun goblins--session-path (session)
  "The tree path of SESSION, or its name when the daemon sent no path."
  (let ((path (plist-get session :path)))
    (if (and (stringp path) (not (string-empty-p path))) path (plist-get session :agent_name))))

(defun goblins--live-descendants (snapshot id)
  "Sessions in SNAPSHOT below ID that have not finished."
  (let ((children (cl-remove-if-not (lambda (s) (equal (plist-get s :parent) id))
                                    (append (plist-get snapshot :sessions) nil))))
    (append (cl-remove-if (lambda (s) (member (plist-get s :state) '("stopped" "failed"))) children)
            (apply #'append (mapcar (lambda (c) (goblins--live-descendants
                                                 snapshot (plist-get c :id)))
                                    children)))))

(defun goblins--session-label (snapshot id)
  "Name the session ID from SNAPSHOT, or ID itself if it is gone."
  (if-let* ((session (goblins--snapshot-session snapshot id)))
      (goblins--safe (or (plist-get session :path) (plist-get session :agent_name)))
    (goblins--safe id)))

(defconst goblins--sidebar-buffer-name "*Goblins Sidebar*")

(defun goblins--main-window ()
  "Return the most recently used window that is not the sidebar."
  (let ((windows (cl-remove-if (lambda (w)
                                 (or (window-parameter w 'window-side)
                                     (window-minibuffer-p w)))
                               (window-list))))
    (car (sort windows (lambda (a b) (> (window-use-time a) (window-use-time b)))))))

(defun goblins--show-in-main (buffer)
  "Display BUFFER in the main window and select it."
  (let ((window (or (goblins--main-window)
                    (user-error "No window for the main view"))))
    (set-window-buffer window buffer)
    (select-window window)
    buffer))

(defun goblins--display-main (buffer)
  "Show BUFFER in the workspace's main window, or pop it up outside a workspace."
  (if (get-buffer-window goblins--sidebar-buffer-name)
      (goblins--show-in-main buffer)
    (pop-to-buffer buffer)))

(defun goblins-open-session (source id)
  "Show the terminal of session ID of SOURCE's server, attaching if needed."
  (with-current-buffer source
    (let ((session (goblins--snapshot-session goblins--snapshot id)))
      (if-let* ((buffer (goblins--terminal-buffer id)))
          (goblins--display-main buffer)
        (unless session (user-error "The daemon no longer retains this goblin"))
        (when (member (plist-get session :state) '("stopped" "failed"))
          (user-error "This goblin has finished"))
        (goblins--open-terminal
         id (plist-get session :agent_name)
         (if (stringp (plist-get session :cwd))
             (file-name-as-directory (plist-get session :cwd))
           default-directory)
         #'goblins--display-main)))))

(defun goblins--describe-keys (title groups)
  "Show TITLE and GROUPS of (HEADING (KEY . DESCRIPTION)...) in a help window."
  (with-help-window "*Goblins Keys*"
    (with-current-buffer standard-output
      (insert (propertize title 'face 'bold) "\n")
      (dolist (group groups)
        (insert "\n" (propertize (car group) 'face 'goblins-heading) "\n")
        (dolist (key (cdr group))
          (insert "  " (propertize (format "%-8s" (car key)) 'face 'help-key-binding)
                  " " (cdr key) "\n"))))))

(defun goblins--view-goto (position)
  "Move point, and the point of every window showing this buffer, to POSITION."
  (goto-char position)
  (dolist (window (get-buffer-window-list (current-buffer) nil t))
    (set-window-point window position)))

(defun goblins--view-buffer (name mode render source session)
  "Return buffer NAME in MODE, rendered by RENDER, for SESSION on SOURCE."
  (let ((buffer (get-buffer-create name)))
    (with-current-buffer buffer
      (unless (derived-mode-p mode) (funcall mode))
      (unless (and (eq goblins--view-source source)
                   (equal goblins--view-session session))
        (setq goblins--view-source source goblins--view-session session)
        (setq-local goblins--view-fresh t))
      (setq goblins--view-render render)
      (setq goblins--directory (buffer-local-value 'goblins--directory source))
      (funcall render))
    buffer))

(defvar-local goblins--view-fresh nil
  "Non-nil until the view has rendered once for its current goblin.")

(defun goblins--views-after-snapshot ()
  "Re-render the views of the status buffer that just changed."
  (let ((source (current-buffer)))
    (dolist (buffer (buffer-list))
      (when (eq (buffer-local-value 'goblins--view-source buffer) source)
        (with-current-buffer buffer
          (when goblins--view-render
            (condition-case err (funcall goblins--view-render)
              (error (message "goblins view: %s" (error-message-string err))))))))))
(add-hook 'goblins-snapshot-hook #'goblins--views-after-snapshot)

;;;; Info

(defvar goblins-info-section-functions
  '(goblins--info-packages goblins--info-permissions goblins--info-raid
    goblins--info-children)
  "Functions that insert sections of the info view.
Each is called with the session record and the snapshot, with point at the
end of the buffer.  Add your own to show more.")

(defun goblins--info-heading (text)
  (insert "\n" (propertize text 'face 'goblins-heading) "\n"))

(defun goblins--info-row (label value)
  "Insert LABEL and VALUE unless VALUE is empty."
  (unless (or (null value) (equal value "") (eq value :null))
    (insert "  " (propertize (format "%-15s" label) 'face 'goblins-key)
            (goblins--safe value) "\n")))

(defun goblins--yes-no (value)
  (if (eq value t) "yes" "no"))

(defun goblins--info-terminal (session)
  (let ((flags (delq nil
                     (list (and (eq (plist-get session :terminal_attached) t) "attached")
                           (and (eq (plist-get session :terminal_detached) t) "launched detached")
                           (and (eq (plist-get session :terminal_complete) t) "output complete")
                           (and (eq (plist-get session :terminal_interrupted) t) "interrupted")
                           (and (eq (plist-get session :pty_eof) t) "pty closed")))))
    (if flags (string-join flags ", ") "idle")))

(defun goblins--info-packages (session _snapshot)
  (let ((startup (append (plist-get session :initial_packages) nil))
        (granted (append (plist-get session :packages) nil)))
    (goblins--info-heading "Packages")
    (goblins--info-row "Startup" (string-join startup ", "))
    (goblins--info-row "Granted" (string-join granted ", "))
    (unless (or startup granted) (insert "  none\n"))))

(defun goblins--info-permissions (session snapshot)
  (let ((records (cl-remove-if-not
                  (lambda (r) (equal (plist-get r :session) (plist-get session :id)))
                  (append (plist-get snapshot :permissions) nil))))
    (goblins--info-heading (format "Permission requests (%d)" (length records)))
    (if (null records)
        (insert "  none\n")
      (dolist (record (reverse records))
        (insert "  " (goblins--permission-glyph record) " "
                (propertize (format "%-18s" (goblins--safe (plist-get record :package)))
                            'face 'goblins-key)
                (goblins--safe (plist-get record :state))
                (let ((kind (plist-get record :kind)))
                  (if (member kind '(nil "package")) ""
                    (format "  (%s)" (goblins--safe kind))))
                "\n")))))

(defun goblins--info-raid (session snapshot)
  (when-let* ((raid (plist-get session :raid))
              ((listp raid))
              (id (plist-get raid :id)))
    (goblins--info-heading "Raid")
    (goblins--info-row "Name" (plist-get raid :name))
    (goblins--info-row "Role" (plist-get raid :role))
    (goblins--info-row "Owner" (goblins--session-label snapshot (plist-get raid :owner)))
    (when-let* ((record (cl-find id (append (plist-get snapshot :raids) nil)
                                 :key (lambda (r) (plist-get r :id)) :test #'equal)))
      (goblins--info-row "Members"
                         (string-join
                          (mapcar (lambda (m) (goblins--safe (plist-get m :path)))
                                  (append (plist-get record :members) nil))
                          ", ")))))

(defun goblins--info-children (session snapshot)
  (let ((children (cl-remove-if-not
                   (lambda (s) (equal (plist-get s :parent) (plist-get session :id)))
                   (append (plist-get snapshot :sessions) nil))))
    (when children
      (goblins--info-heading (format "Children (%d)" (length children)))
      (dolist (child children)
        (insert "  " (goblins--state-glyph (plist-get child :state)) " "
                (goblins--safe (plist-get child :agent_name))
                (propertize (format "  %s" (goblins--safe (plist-get child :state)))
                            'face 'goblins-dim)
                "\n")))))

(defun goblins--info-render ()
  (let* ((inhibit-read-only t)
         (line (line-number-at-pos))
         (snapshot (goblins--view-snapshot))
         (session (goblins--view-session-record)))
    (erase-buffer)
    (cond
     ((null snapshot)
      (insert (propertize "Not connected to the server\n" 'face 'goblins-dim)))
     ((null session)
      (insert (propertize "The daemon no longer retains this goblin\n" 'face 'goblins-dim)))
     (t
      (let ((state (plist-get session :state)))
        (insert (propertize (goblins--safe (plist-get session :agent_name)) 'face 'bold)
                "  " (goblins--state-glyph state) " " (goblins--safe state) "\n"))
      (when-let* ((description (plist-get session :description))
                  ((not (equal description ""))))
        (insert (propertize (goblins--safe description) 'face 'goblins-dim) "\n"))
      (insert "\n")
      (goblins--info-row "Session" (plist-get session :id))
      (goblins--info-row "Path" (plist-get session :path))
      (goblins--info-row "Directory" (let ((cwd (plist-get session :cwd)))
                                       (and (stringp cwd) (abbreviate-file-name cwd))))
      (goblins--info-row "Configuration" (plist-get session :name))
      (goblins--info-row "Manifest" (plist-get session :configuration))
      (goblins--info-row "Parent" (and (stringp (plist-get session :parent))
                                       (goblins--session-label snapshot (plist-get session :parent))))
      (goblins--info-row "Dev shell" (plist-get session :dev_shell))
      (goblins--info-row "Docker" (if (eq (plist-get session :docker_enabled) t)
                                      (format "enabled%s" (if-let* ((scope (plist-get session :scope)))
                                                              (format " (scope %s)" scope) ""))
                                    "disabled"))
      (goblins--info-row "Terminal" (goblins--info-terminal session))
      (goblins--info-row "Emacs buffer"
                         (when-let* ((buffer (with-current-buffer goblins--view-source
                                               (goblins--terminal-buffer goblins--view-session))))
                           (buffer-name buffer)))
      (goblins--info-row "Exit code" (and (integerp (plist-get session :exit_code))
                                          (number-to-string (plist-get session :exit_code))))
      (goblins--info-row "Stop reason" (plist-get session :stop_reason))
      (goblins--info-row "Detail" (plist-get session :detail))
      (dolist (function goblins-info-section-functions)
        (funcall function session snapshot))))
    (goto-char (point-min))
    (forward-line (1- line))))

(defvar goblins-info-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (define-key map (kbd "g") #'goblins-info-refresh)
    map))

(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-info-mode 'motion))

(define-derived-mode goblins-info-mode special-mode "Goblins-Info"
  "Everything the daemon knows about one goblin."
  (setq-local header-line-format " Goblin info   q back")
  (visual-line-mode 1)
  (setq-local revert-buffer-function (lambda (&rest _) (goblins-info-refresh))))

(defun goblins-info-refresh ()
  "Redraw the info view from the current snapshot."
  (interactive)
  (goblins--info-render))

(defun goblins-info-buffer (source session)
  "Return the info buffer for SESSION, an ID in SOURCE's snapshot."
  (goblins--view-buffer goblins--info-buffer-name #'goblins-info-mode
                        #'goblins--info-render source session))

;;;; Permissions

(defun goblins--permission-glyph (record)
  (pcase (plist-get record :state)
    ("pending" (propertize "?" 'face 'goblins-attention))
    ("ready" (propertize "✓" 'face 'goblins-state-running))
    ((or "denied" "failed") (propertize "✗" 'face 'goblins-state-failed))
    ((or "cancelled" "withdrawn") (propertize "⊘" 'face 'goblins-dim))
    (_ (propertize "…" 'face 'goblins-dim))))

(defun goblins--permission-outcome (record)
  (pcase (plist-get record :state)
    ("ready" "accepted, ready")
    ("realizing" "accepted, provisioning")
    (state (goblins--safe state))))

(defun goblins--permission-heading (record)
  (format "  %s %s  %s%s"
          (goblins--permission-glyph record)
          (propertize (goblins--safe (plist-get record :package)) 'font-lock-face 'goblins-key)
          (propertize (goblins--permission-outcome record) 'font-lock-face 'goblins-dim)
          (let ((kind (plist-get record :kind)))
            (if (member kind '(nil "package")) ""
              (propertize (format "  (%s)" (goblins--safe kind)) 'font-lock-face 'goblins-dim)))))

(defun goblins--find-request-section (id)
  (cl-labels ((walk (section)
                (or (and (object-of-class-p section 'goblins-request-section)
                         (equal (oref section value) id)
                         section)
                    (cl-some #'walk (oref section children)))))
    (and magit-root-section (walk magit-root-section))))

(defvar-local goblins--permissions-seen nil
  "IDs of the pending requests this view has already focused.")

(defun goblins--permissions-render ()
  "Redraw the permissions view, focusing a new or the first pending request."
  (let* ((inhibit-read-only t)
         (snapshot (goblins--view-snapshot))
         (session (goblins--view-session-record))
         (section (and magit-root-section (magit-current-section)))
         (ident (and section (magit-section-ident section)))
         (offset (and section (- (point) (oref section start))))
         (records (cl-remove-if-not
                   (lambda (r) (equal (plist-get r :session) goblins--view-session))
                   (append (plist-get snapshot :permissions) nil)))
         (pending (cl-remove-if-not (lambda (r) (equal (plist-get r :state) "pending")) records))
         (history (reverse (cl-remove-if (lambda (r) (equal (plist-get r :state) "pending"))
                                         records)))
         (arrived (cl-find-if-not (lambda (r) (member (plist-get r :id) goblins--permissions-seen))
                                  pending)))
    (erase-buffer)
    (magit-insert-section (goblins-section 'permissions)
      (insert (propertize (if session (goblins--safe (plist-get session :agent_name))
                            (if snapshot "Goblin no longer retained" "Not connected to the server"))
                          'font-lock-face 'bold)
              (if session
                  (concat "  " (goblins--state-glyph (plist-get session :state))
                          " " (goblins--safe (plist-get session :state)))
                "")
              "\n\n")
      (magit-insert-section (goblins-section 'current)
        (magit-insert-heading
          (propertize (if (> (length pending) 1)
                          (format "Current requests (%d)" (length pending))
                        "Current request")
                      'font-lock-face 'magit-section-heading))
        (if (null pending)
            (insert (propertize "  Nothing waiting for a decision\n" 'font-lock-face 'goblins-dim))
          (dolist (record pending)
            (goblins--insert-request record t nil (goblins--permission-heading record)))
          (insert (propertize "  a accept   d deny\n" 'font-lock-face 'goblins-dim)))
        (insert "\n"))
      (magit-insert-section (goblins-section 'history)
        (magit-insert-heading
          (propertize (format "History (%d)" (length history))
                      'font-lock-face 'magit-section-heading))
        (if (null history)
            (insert (propertize "  No earlier requests\n" 'font-lock-face 'goblins-dim))
          (dolist (record history)
            (goblins--insert-request record t t (goblins--permission-heading record))))))
    ;; Apply the folded flags of the history entries to display overlays.
    (magit-section-show magit-root-section)
    (goto-char (point-min))
    (let ((focus (or (and arrived (goblins--find-request-section (plist-get arrived :id)))
                     (and goblins--view-fresh pending
                          (goblins--find-request-section (plist-get (car pending) :id))))))
      (cond
       (focus (goblins--view-goto (oref focus start)))
       ((and ident (magit-get-section ident))
        (let ((restored (magit-get-section ident)))
          (goblins--view-goto (min (+ (oref restored start) offset)
                                   (max (point-min) (1- (oref restored end))))))))
      ;; Settled requests never become pending again, so only pending IDs matter.
      (setq goblins--permissions-seen (mapcar (lambda (r) (plist-get r :id)) pending)
            goblins--view-fresh nil))))

(defun goblins-permissions-decide (approved)
  "Decide the request at point, else the only pending one.
Accept it when APPROVED is non-nil, otherwise deny it."
  (let* ((section (magit-current-section))
         (record (or (and section (object-of-class-p section 'goblins-request-section)
                          (equal (plist-get (oref section record) :state) "pending")
                          (oref section record))
                     (let ((pending (cl-remove-if-not
                                     (lambda (r)
                                       (and (equal (plist-get r :session) goblins--view-session)
                                            (equal (plist-get r :state) "pending")))
                                     (append (plist-get (goblins--view-snapshot) :permissions) nil))))
                       (if (cdr pending)
                           (user-error "Place point on the request to decide")
                         (car pending))))))
    (unless record (user-error "No pending request for this goblin"))
    (with-current-buffer goblins--view-source
      (goblins--decide-record record approved))))

(defun goblins-permissions-accept ()
  "Accept the current request."
  (interactive)
  (goblins-permissions-decide t))

(defun goblins-permissions-deny ()
  "Deny the current request."
  (interactive)
  (goblins-permissions-decide nil))

(defun goblins-permissions-toggle ()
  "Fold or unfold the section at point."
  (interactive)
  (when-let* ((section (magit-current-section)))
    (magit-section-toggle section)))

(defvar goblins-permissions-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map magit-section-mode-map)
    (define-key map (kbd "a") #'goblins-permissions-accept)
    (define-key map (kbd "d") #'goblins-permissions-deny)
    (define-key map (kbd "RET") #'goblins-permissions-toggle)
    (define-key map (kbd "g") #'goblins-permissions-refresh)
    (define-key map (kbd "q") #'quit-window)
    map))

(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-permissions-mode 'motion)
  (evil-define-key* 'motion goblins-permissions-mode-map
    (kbd "j") #'magit-section-forward
    (kbd "k") #'magit-section-backward
    (kbd "a") #'goblins-permissions-accept
    (kbd "d") #'goblins-permissions-deny
    (kbd "RET") #'goblins-permissions-toggle
    (kbd "TAB") #'goblins-permissions-toggle
    (kbd "<tab>") #'goblins-permissions-toggle
    (kbd "gr") #'goblins-permissions-refresh
    (kbd "q") #'quit-window))

(define-derived-mode goblins-permissions-mode magit-section-mode "Goblins-Permissions"
  "Requests of one goblin: the current one focused, then the history.
\\<goblins-permissions-mode-map>
\\[goblins-permissions-accept] accepts and \\[goblins-permissions-deny] denies the request at point.
\\[goblins-permissions-toggle] folds a request, \\[quit-window] goes back."
  (setq-local header-line-format " Permissions   a accept   d deny   RET fold   q back")
  (setq-local revert-buffer-function (lambda (&rest _) (goblins-permissions-refresh))))

(defun goblins-permissions-refresh ()
  "Redraw the permissions view from the current snapshot."
  (interactive)
  (goblins--permissions-render))

(defun goblins-permissions-buffer (source session)
  "Return the permissions buffer for SESSION, an ID in SOURCE's snapshot."
  (goblins--view-buffer goblins--permissions-buffer-name #'goblins-permissions-mode
                        #'goblins--permissions-render source session))

;;;; Switching

(defun goblins--switch-annotation (session pending)
  "Describe SESSION, with PENDING requests, for completion."
  (let* ((raid (plist-get session :raid))
         (raid-name (and (listp raid) (plist-get raid :name)))
         (cwd (plist-get session :cwd)))
    (concat (propertize " " 'display '(space :align-to 32))
            (propertize (format "%-10s" (goblins--safe (plist-get session :name))) 'face 'goblins-dim)
            (if raid-name (propertize (format " raid %-10s" (goblins--safe raid-name)) 'face 'goblins-key)
              (make-string 16 ?\s))
            (if (stringp cwd) (propertize (concat " " (abbreviate-file-name cwd)) 'face 'goblins-dim) "")
            (if (> pending 0) (propertize (format " !%d" pending) 'face 'goblins-attention) ""))))

;;;###autoload
(defun goblins-switch (&optional directory)
  "Choose a running goblin and show its terminal, attaching if necessary.
With a prefix argument, prompt for the daemon state DIRECTORY."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "))))
  (let* ((source (goblins--connect directory))
         (snapshot (buffer-local-value 'goblins--snapshot source))
         (permissions (append (plist-get snapshot :permissions) nil))
         (table (mapcar (lambda (s) (cons (goblins--safe (goblins--session-path s)) s))
                        (cl-remove-if (lambda (s) (member (plist-get s :state) '("stopped" "failed")))
                                      (append (plist-get snapshot :sessions) nil))))
         (completion-extra-properties
          (list :annotation-function
                (lambda (candidate)
                  (when-let* ((session (cdr (assoc candidate table))))
                    (goblins--switch-annotation
                     session
                     (cl-count-if (lambda (r) (and (equal (plist-get r :session) (plist-get session :id))
                                                   (equal (plist-get r :state) "pending")))
                                  permissions)))))))
    (unless table (user-error "No running goblins"))
    (let ((choice (assoc (completing-read "Goblin: " table nil t) table)))
      (unless choice (user-error "Select a goblin"))
      (goblins-open-session source (plist-get (cdr choice) :id)))))

;;;; Attention

(defcustom goblins-attention-notify-function #'goblins-notify-message
  "Function called with a permission request record when it becomes pending,
while `goblins-attention-mode' is on."
  :type 'function :group 'goblins)

(defvar goblins--mode-line-string ""
  "Mode line text of `goblins-attention-mode'.")
(put 'goblins--mode-line-string 'risky-local-variable t)

(defun goblins-notify-message (record)
  "Announce the permission request RECORD in the echo area."
  (message "Goblins: %s asks for %s — %s"
           (goblins--safe (plist-get record :agent_name))
           (goblins--safe (plist-get record :package))
           (goblins--safe (plist-get record :reason))))

(defun goblins--pending-total ()
  "Pending requests over every connected server."
  (let ((total 0))
    (dolist (buffer (buffer-list))
      (when (eq (buffer-local-value 'major-mode buffer) 'goblins-status-mode)
        (setq total (+ total (cl-count-if
                              (lambda (r) (equal (plist-get r :state) "pending"))
                              (append (plist-get (buffer-local-value 'goblins--snapshot buffer)
                                                 :permissions)
                                      nil))))))
    total))

(defun goblins--update-mode-line ()
  (let ((total (goblins--pending-total)))
    (setq goblins--mode-line-string
          (if (= total 0) ""
            (propertize (format " Goblins!%d" total)
                        'face 'goblins-attention
                        'help-echo "Goblins waiting for a decision; mouse-1: open the workspace"
                        'mouse-face 'mode-line-highlight
                        'local-map (make-mode-line-mouse-map 'mouse-1 #'goblins-workspace))))
    (force-mode-line-update t)))

(defun goblins--attention-request (record)
  (funcall goblins-attention-notify-function record))

;;;###autoload
(define-minor-mode goblins-attention-mode
  "Show pending Goblins requests in the mode line and announce new ones.
New requests are passed to `goblins-attention-notify-function'."
  :global t :group 'goblins
  (if goblins-attention-mode
      (progn
        (unless (memq 'goblins--mode-line-string global-mode-string)
          (setq global-mode-string (append global-mode-string '(goblins--mode-line-string))))
        (add-hook 'goblins-snapshot-hook #'goblins--update-mode-line)
        (add-hook 'goblins-request-hook #'goblins--attention-request)
        (goblins--update-mode-line))
    (setq global-mode-string (delq 'goblins--mode-line-string global-mode-string))
    (remove-hook 'goblins-snapshot-hook #'goblins--update-mode-line)
    (remove-hook 'goblins-request-hook #'goblins--attention-request)))

(declare-function goblins-workspace "goblins-workspace" (&optional directory))

(provide 'goblins-views)
;;; goblins-views.el ends here
