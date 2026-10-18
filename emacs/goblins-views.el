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
(declare-function evil-make-intercept-map "evil-core" (keymap &optional state aux))

(defface goblins-heading '((t :inherit font-lock-keyword-face :weight bold))
  "Headings in the Goblins views." :group 'goblins)
(defface goblins-state-running '((t :inherit success))
  "Running goblins." :group 'goblins)
(defface goblins-state-failed '((t :inherit error))
  "Failed goblins and denied requests." :group 'goblins)
(defface goblins-activity-working '((t :inherit success))
  "A goblin's agent at work." :group 'goblins)
(defface goblins-activity-waiting '((t :inherit warning :weight bold))
  "A goblin's agent waiting for input or approval." :group 'goblins)
(defface goblins-activity-idle '((t :inherit shadow))
  "A goblin's idle agent." :group 'goblins)
(defface goblins-inbox '((t :inherit font-lock-constant-face))
  "Messages queued in a goblin's inbox." :group 'goblins)

(defconst goblins--info-buffer-name "*Goblins Info*")
(defconst goblins--permissions-buffer-name "*Goblins Permissions*")

(defvar-local goblins--view-source nil
  "The status buffer whose snapshot this view shows.")
(defvar-local goblins--view-render nil
  "Function that redraws this view when the snapshot changes.")
(defvar-local goblins--view-session nil
  "Immutable ID of the goblin this view is about.")

;;;; Shared

(defun goblins--sidebar-duration (since)
  "How long ago SINCE, in Unix milliseconds, was: \"<1m\", \"4m\", \"2h\" or \"3d\"."
  (when (numberp since)
    (let ((seconds (max 0 (floor (- (float-time) (/ since 1000.0))))))
      (cond ((< seconds 60) "<1m")
            ((< seconds 3600) (format "%dm" (/ seconds 60)))
            ((< seconds 86400) (format "%dh" (/ seconds 3600)))
            (t (format "%dd" (/ seconds 86400)))))))

(defun goblins--sidebar-status (session)
  "SESSION's activity line, or its lifecycle state while not running, or nil."
  (let ((state (plist-get session :state)))
    (if (not (equal state "running"))
        (propertize (format "◌ %s" (goblins--safe state)) 'face 'goblins-dim)
      (when-let* ((activity (pcase (plist-get session :agent_state)
                              ("working" '("◐ working" goblins-activity-working))
                              ("waiting-input" '("◆ needs input" goblins-activity-waiting))
                              ("waiting-approval" '("◆ needs approval" goblins-activity-waiting))
                              ("idle" '("○ idle" goblins-activity-idle)))))
        (propertize (concat (car activity)
                            (when-let* ((age (goblins--sidebar-duration
                                              (plist-get session :agent_state_since))))
                              (concat " · " age)))
                    'face (cadr activity))))))

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

(defun goblins--session-label (snapshot id)
  "Name the session ID from SNAPSHOT, or ID itself if it is gone."
  (if-let* ((session (goblins--snapshot-session snapshot id)))
      (goblins--safe (plist-get session :agent_name))
    (goblins--safe id)))

(defconst goblins--sidebar-buffer-name "*Goblins Sidebar*")

(defun goblins-stop-goblin (source id)
  "Stop goblin ID of SOURCE's server after confirming."
  (let ((session (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source) id)))
    (unless session (user-error "The daemon no longer retains this goblin"))
    (when (y-or-n-p (format "Stop %s? " (goblins--safe (plist-get session :agent_name))))
      (goblins--host-call source 'sessions.stop (list :session id))
      (message "Stopping %s" (goblins--safe (plist-get session :agent_name))))))

(defun goblins-kill-goblin-execs (source id)
  "Kill every shell and command inside goblin ID of SOURCE's server, after
confirming."
  (let ((session (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source) id)))
    (unless session (user-error "The daemon no longer retains this goblin"))
    (when (y-or-n-p (format "Kill all shells and commands in %s? "
                            (goblins--safe (plist-get session :agent_name))))
      (goblins--host-call source 'sessions.kill_execs (list :session id)))))

;;;; Workspace windows
;;
;; Beside the sidebar the workspace has an agent window on the left, dedicated
;; to the selected goblin's agent terminal (the status buffer stands in until
;; the first agent opens).  Everything else goes right of it (or below it, in
;; a narrow frame; see `split-width-threshold'): what Goblins opens, and
;; whatever you show, split and arrange there yourself.  Each goblin keeps
;; those windows: leaving it saves them, selecting it again brings them back.
;; The info, permissions and raid views are never saved; they are shared
;; between goblins.

(defvar goblins--layouts (make-hash-table :test #'equal)
  "(DIRECTORY . SESSION-ID) to that goblin's saved windows, see
`goblins--layout-tree'.")

(defvar goblins--last-goblin nil
  "(DIRECTORY . SESSION-ID) of the selected goblin, whose windows show.")

(defvar goblins--previous-goblin nil
  "(DIRECTORY . SESSION-ID) of the goblin selected before `goblins--last-goblin'.")

(defun goblins--note-selected (key)
  "Make KEY, (DIRECTORY . SESSION-ID), the selected goblin."
  (unless (equal key goblins--last-goblin)
    (setq goblins--previous-goblin goblins--last-goblin
          goblins--last-goblin key)))

(defun goblins--in-workspace-p ()
  "Whether this frame shows the workspace: its sidebar, or with the sidebar
hidden, the agent window.  Window parameters belong to the window
configuration, so another tab of the frame is not in the workspace."
  (and (or (get-buffer-window goblins--sidebar-buffer-name)
           (cl-some (lambda (w) (window-parameter w 'goblins-workspace))
                    (window-list nil 'no-minibuf)))
       t))

(defun goblins--main-windows ()
  "This frame's windows other than side windows and the minibuffer."
  (cl-remove-if (lambda (w) (or (window-parameter w 'window-side) (window-minibuffer-p w)))
                (window-list)))

(defun goblins--agent-window ()
  "The workspace's agent window: the one claimed for it, else the leftmost."
  (let ((windows (goblins--main-windows)))
    (or (cl-find-if (lambda (w) (window-parameter w 'goblins-agent)) windows)
        (car (sort windows
                   (lambda (a b) (or (< (window-left-column a) (window-left-column b))
                                     (and (= (window-left-column a) (window-left-column b))
                                          (< (window-top-line a) (window-top-line b))))))))))

(defun goblins--window-children (window)
  (cl-loop for child = (window-child window) then (window-next-sibling child)
           while child collect child))

(defun goblins--area (agent)
  "The goblin's windows: the outermost combination around AGENT made for them,
or AGENT alone.  Windows outside it, such as popups another package split
off below everything, are never used, saved or deleted for a goblin."
  (let ((window agent) area)
    (while (setq window (window-parent window))
      (when (window-parameter window 'goblins-area) (setq area window)))
    (or area
        ;; Deleting other windows rebuilds the combination without its tag;
        ;; the agent's side by side partner is still its own.
        (let ((parent (window-parent agent)))
          (when (and parent (window-combined-p agent t)
                     (not (cl-some (lambda (w) (window-parameter w 'window-side))
                                   (goblins--window-children parent))))
            (set-window-parameter parent 'goblins-area t)
            parent))
        agent)))

(defun goblins--area-windows (agent)
  "The live windows of AGENT's area, AGENT included."
  (let (windows)
    (walk-window-subtree (lambda (w) (push w windows)) (goblins--area agent))
    windows))

(defun goblins--claim-area (agent)
  "Tag the combination holding AGENT that was just made for the goblin's windows."
  (let ((parent (window-parent agent)))
    (when (and parent (not (window-parameter parent 'goblins-area)))
      (set-window-parameter parent 'goblins-area t))))

(defun goblins--right-window ()
  "The most recently used window of the goblin's area other than the agent's."
  (let ((agent (goblins--agent-window)))
    (car (sort (remq agent (goblins--area-windows agent))
               (lambda (a b) (> (window-use-time a) (window-use-time b)))))))

(defun goblins--main-window ()
  "The agent window in the workspace, else the most recently used window."
  (if (goblins--in-workspace-p)
      (goblins--agent-window)
    (car (sort (goblins--main-windows)
               (lambda (a b) (> (window-use-time a) (window-use-time b)))))))

(defun goblins--refuse-agent-window-delete (_window)
  "The `delete-window' parameter of the agent window."
  (user-error "The agent window stays; select another goblin to change it"))

(defun goblins--release-agent-window (window)
  "Make WINDOW an ordinary window again."
  (set-window-dedicated-p window nil)
  (dolist (parameter '(goblins-agent no-delete-other-windows delete-window))
    (set-window-parameter window parameter nil)))

(defun goblins--set-agent-buffer (window buffer)
  "Show BUFFER in WINDOW and dedicate WINDOW to the agent.
Deleting the other windows keeps it, and deleting it is refused.  WINDOW also
marks the frame as the workspace, while the sidebar is hidden too."
  (goblins--release-agent-window window)
  (set-window-buffer window buffer)
  (set-window-dedicated-p window t)
  (set-window-parameter window 'goblins-workspace t)
  (set-window-parameter window 'goblins-agent t)
  (set-window-parameter window 'no-delete-other-windows t)
  (set-window-parameter window 'delete-window #'goblins--refuse-agent-window-delete))

(defun goblins--agent-buffer-killed ()
  "Keep an agent window whose buffer dies, showing the status buffer instead.
Emacs would delete the dedicated window otherwise."
  (let ((dying (current-buffer)))
    (dolist (window (get-buffer-window-list dying nil t))
      (when (window-parameter window 'goblins-agent)
        (goblins--set-agent-buffer
         window
         (let ((source (and (get-buffer goblins--sidebar-buffer-name)
                            (buffer-local-value 'goblins--sidebar-source
                                                (get-buffer goblins--sidebar-buffer-name)))))
           (if (and (buffer-live-p source) (not (eq source dying)))
               source
             (get-scratch-buffer-create))))))))
(add-hook 'kill-buffer-hook #'goblins--agent-buffer-killed)

(defun goblins--only-agent-window ()
  "Delete the windows of the goblin's area other than the agent window.
Return the agent window."
  (let ((agent (goblins--agent-window)))
    (dolist (window (goblins--area-windows agent))
      (unless (eq window agent) (delete-window window)))
    agent))

(defun goblins--split-agent-window (agent)
  "Split a window off AGENT for the other buffers and return it, or nil.
Right of the agent when AGENT is at least `split-width-threshold' wide, else
below it when at least `split-height-threshold' high, else below it if it
fits at all.  In a frame too small for two windows, nil."
  ;; Always a new combination, never a sibling in one with other windows.
  (let* ((window-combination-limit t)
         (window (cond ((window-splittable-p agent t) (split-window agent nil 'right))
                       ((window-splittable-p agent) (split-window agent nil 'below))
                       ((let ((split-height-threshold 0)) (window-splittable-p agent))
                        (split-window agent nil 'below)))))
    (when window (goblins--claim-area agent))
    window))

;;;;; Saving and restoring a goblin's windows

(defun goblins--view-buffer-p (buffer)
  "Whether BUFFER is one of the views shared between goblins."
  (with-current-buffer buffer
    (derived-mode-p 'goblins-info-mode 'goblins-permissions-mode 'goblins-raid-mode
                    'goblins-messages-mode 'goblins-message-mode)))

(defun goblins--layout-leaf (window)
  "WINDOW's saved state, as a leaf of `goblins--layout-tree', or nil.
A view is saved as the buffer it replaced there, if any."
  (let ((buffer (window-buffer window)))
    (if (not (goblins--view-buffer-p buffer))
        (list :buffer buffer :start (window-start window) :point (window-point window))
      (let ((quit (window-parameter window 'quit-restore)))
        (when-let* (((eq (car quit) 'other))
                    (previous (nth 1 quit))
                    ((buffer-live-p (car previous))))
          (list :buffer (car previous) :start (nth 1 previous) :point (nth 2 previous)))))))

(defun goblins--layout-tree (window agent)
  "The saved form of WINDOW, a window tree with the agent window AGENT.
A leaf is (:agent t) or (:buffer B :start S :point P); a combination is
\(:horizontal H :children ((SHARE . TREE)...)), SHARE its children's sizes."
  (if (window-live-p window)
      (if (eq window agent) (list :agent t) (goblins--layout-leaf window))
    (let* ((horizontal (window-combined-p (window-child window) t))
           (children (cl-loop for child = (window-child window) then (window-next-sibling child)
                              while child
                              for tree = (goblins--layout-tree child agent)
                              when tree
                              collect (cons (window-total-size child horizontal) tree))))
      (if (cdr children)
          (list :horizontal horizontal :children children)
        (cdar children)))))

(defun goblins--layout-live (tree)
  "TREE without the windows whose buffer was killed."
  (cond ((plist-get tree :agent) tree)
        ((plist-get tree :buffer) (and (buffer-live-p (plist-get tree :buffer)) tree))
        (t (let ((children (cl-loop for (share . child) in (plist-get tree :children)
                                    for live = (goblins--layout-live child)
                                    when live collect (cons share live))))
             (if (cdr children)
                 (list :horizontal (plist-get tree :horizontal) :children children)
               (cdar children))))))

(defun goblins--layout-put (window tree)
  "Rebuild TREE in the live WINDOW.  Return the window for the agent, if placed."
  (cond
   ((plist-get tree :agent) window)
   ((plist-get tree :buffer)
    (set-window-buffer window (plist-get tree :buffer))
    (set-window-start window (plist-get tree :start) t)
    (set-window-point window (plist-get tree :point))
    nil)
   (t
    (let* ((horizontal (plist-get tree :horizontal))
           (children (plist-get tree :children))
           (total (float (apply #'+ (mapcar #'car children))))
           (size (window-total-size window horizontal))
           (current window)
           placed)
      ;; Split off each child's share in turn; one that doesn't fit is left out.
      (while (and children current)
        (let* ((child (pop children))
               (next (and children
                          (ignore-errors
                            (let ((window-combination-limit t))
                              (split-window current (max 1 (round (* size (/ (car child) total))))
                                            (if horizontal 'right 'below)))))))
          (push (cons current (cdr child)) placed)
          (setq current next)))
      (let (agent)
        (dolist (entry (nreverse placed) agent)
          (setq agent (or (goblins--layout-put (car entry) (cdr entry)) agent))))))))

(defun goblins--save-layout ()
  "Save the selected goblin's windows, if the workspace shows them."
  (when (and goblins--last-goblin (goblins--in-workspace-p))
    (let ((agent (goblins--agent-window)))
      (puthash goblins--last-goblin
               (goblins--layout-tree (goblins--area agent) agent)
               goblins--layouts))))

(defun goblins--restore-layout (key buffer)
  "Rebuild goblin KEY's saved windows, with BUFFER in the agent window.
Return the agent window."
  (let* ((window (goblins--only-agent-window))
         (outer (window-parent window))
         (tree (goblins--layout-live (gethash key goblins--layouts))))
    (goblins--release-agent-window window)
    (let ((agent (or (and tree (goblins--layout-put window tree))
                     window)))
      ;; Tag the combination built in WINDOW's place as the goblin's.
      (let ((root agent))
        (while (and (window-parent root) (not (eq (window-parent root) outer)))
          (setq root (window-parent root)))
        (unless (window-live-p root)
          (set-window-parameter root 'goblins-area t)))
      (goblins--set-agent-buffer agent buffer)
      agent)))

(defun goblins--forget-stopped-layouts ()
  "Forget the saved windows of this status buffer's goblins that stopped."
  (let ((directory goblins--directory)
        (sessions (append (plist-get goblins--snapshot :sessions) nil)))
    (maphash (lambda (key _)
               (when (and (equal (car key) directory)
                          (member (plist-get (cl-find (cdr key) sessions
                                                      :key (lambda (s) (plist-get s :id))
                                                      :test #'equal)
                                             :state)
                                  '(nil "stopped" "failed")))
                 (remhash key goblins--layouts)))
             goblins--layouts)))
(add-hook 'goblins-snapshot-hook #'goblins--forget-stopped-layouts)

;;;;; Showing buffers

(defun goblins--display-buffer-right (buffer alist)
  "A `display-buffer' action: BUFFER in the right window used last, else in
one split off the agent window."
  (if-let* ((window (goblins--right-window)))
      (window--display-buffer buffer window 'reuse alist)
    (when-let* ((window (goblins--split-agent-window (goblins--agent-window))))
      (window--display-buffer buffer window 'window alist))))

(defun goblins--display-right (buffer)
  "Show BUFFER beside the agent window and select it, splitting if needed.
In a frame too small for two windows it replaces the agent for now.
Outside the workspace, pop it up."
  (if (not (goblins--in-workspace-p))
      (pop-to-buffer buffer)
    (select-window
     (or (goblins--display-buffer-right buffer nil)
         (let ((agent (goblins--agent-window)))
           (goblins--release-agent-window agent)
           (set-window-buffer agent buffer)
           agent))))
  buffer)

(defun goblins--display-main (buffer)
  "Show BUFFER, a view such as info or permissions, beside the agent.
\\[quit-window] there puts back what the window showed before.  Outside the
workspace, pop it up."
  (goblins--display-right buffer))

(defun goblins--show-in-main (buffer)
  "Like `goblins--display-main'."
  (goblins--display-right buffer))

(defun goblins--display-new-agent (buffer)
  "Show BUFFER, a new agent terminal, alone beside the sidebar and select it.
That also hides the status buffer the workspace showed before any agent.
Outside the workspace, pop it up."
  (if (goblins--in-workspace-p)
      (let ((window (goblins--only-agent-window)))
        (goblins--save-layout)
        (goblins--set-agent-buffer window buffer)
        (select-window window))
    (pop-to-buffer buffer))
  buffer)

(defun goblins--exec-summary (session)
  "SESSION's live shells and commands, like \" · 1 shell · tramp\", or \"\"."
  (let ((shells (or (plist-get session :shells) 0))
        (execs (or (plist-get session :execs) 0)))
    (propertize
     (concat (if (> shells 0) (format " · %d shell%s" shells (if (= shells 1) "" "s")) "")
             (if (> execs 0) " · tramp" ""))
     'face 'goblins-dim)))

(defun goblins--enter-goblin (source id &optional attach)
  "Show goblin ID of SOURCE's server in the workspace: its agent terminal,
attached if ATTACH and needed, and its saved windows.  Without a terminal
the status buffer holds the agent's place."
  (let* ((key (cons (buffer-local-value 'goblins--directory source) id))
         (terminal (with-current-buffer source (goblins--terminal-buffer id))))
    (goblins--note-selected key)
    (let ((agent (goblins--restore-layout key (or terminal source))))
      (when (and attach (not terminal))
        (goblins--attach source id (lambda (b) (goblins--set-agent-buffer agent b))))
      (select-window agent))))

(defun goblins-select-goblin (source id &optional right attach)
  "Show goblin ID of SOURCE's daemon, and return the buffer shown for it.
In the workspace the windows of the goblin shown so far are saved, and this
goblin's agent terminal and saved windows come back; RIGHT then shows beside
the agent.  Without an Emacs terminal for the goblin, ATTACH attaches one.
Outside the workspace RIGHT, or the terminal, is popped up."
  (let* ((key (cons (buffer-local-value 'goblins--directory source) id))
         (terminal (with-current-buffer source (goblins--terminal-buffer id))))
    (cond
     ((not (goblins--in-workspace-p))
      (cond (right (pop-to-buffer right))
            (terminal (pop-to-buffer terminal))
            (attach (goblins--attach source id #'pop-to-buffer))))
     (t
      (if (equal key goblins--last-goblin)
          (let ((agent (goblins--agent-window)))
            (cond ((and terminal (not (eq (window-buffer agent) terminal)))
                   (goblins--set-agent-buffer agent terminal))
                  ((and attach (not terminal))
                   (goblins--attach source id (lambda (b) (goblins--set-agent-buffer agent b)))))
            (select-window agent))
        (goblins--save-layout)
        (goblins--enter-goblin source id attach))
      (when right (goblins--display-right right))
      (goblins--popup-follow source id)
      (or right (window-buffer (goblins--agent-window)))))))

(defun goblins--attach (source id display)
  "Attach a new terminal to running goblin ID of SOURCE's server, shown by DISPLAY."
  (with-current-buffer source
    (let ((session (goblins--snapshot-session goblins--snapshot id)))
      (unless session (user-error "The daemon no longer retains this goblin"))
      (when (member (plist-get session :state) '("stopped" "failed"))
        (user-error "This goblin has finished"))
      (goblins--open-terminal
       id (plist-get session :agent_name)
       (if (stringp (plist-get session :cwd))
           (file-name-as-directory (plist-get session :cwd))
         default-directory)
       display))))

(defun goblins-open-session (source id)
  "Show the terminal of session ID of SOURCE's server, attaching if needed.
In the workspace the goblin's saved windows come back beside it."
  (goblins-select-goblin source id nil t))

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
      ;; Never a /goblins: directory inherited from where it was opened.
      (setq default-directory (expand-file-name "~/"))
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

(defcustom goblins-info-section-functions
  '(goblins--info-packages goblins--info-permissions goblins--info-raid)
  "Functions that insert sections of the info view.
Each is called with the session record and the snapshot, with point at the
end of the buffer.  Add your own to show more."
  :type '(repeat function) :group 'goblins)

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
        (granted (append (plist-get session :packages) nil))
        (auto (append (plist-get session :auto_approve) nil)))
    (goblins--info-heading "Packages")
    (goblins--info-row "Startup" (string-join startup ", "))
    (goblins--info-row "Granted" (string-join granted ", "))
    ;; Granted on request without asking: copied from the goblin's creator.
    (goblins--info-row "Auto" (string-join auto ", "))
    (unless (or startup granted auto) (insert "  none\n"))))

(defun goblins--info-permissions (session snapshot)
  (let ((records (cl-remove-if-not
                  (lambda (r) (equal (plist-get r :session) (plist-get session :id)))
                  (append (plist-get snapshot :permissions) nil))))
    (goblins--info-heading (format "Permission requests (%d)" (length records)))
    (if (null records)
        (insert "  none\n")
      (dolist (record (reverse records))
        (insert "  " (goblins--permission-glyph record) " "
                (propertize (format "%-18s" (goblins--request-subject record))
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
    (let ((record (cl-find id (append (plist-get snapshot :raids) nil)
                           :key (lambda (r) (plist-get r :id)) :test #'equal)))
      (if (or (eq (plist-get raid :paused) t) (eq (plist-get record :paused) t))
          (goblins--info-row "Owner"
                             (format "none: paused%s"
                                     (if-let* ((former (plist-get record :former_owner))
                                               ((stringp former)))
                                         (format " since %s left"
                                                 (goblins--session-label snapshot former))
                                       "")))
        (goblins--info-row "Owner" (goblins--session-label snapshot (plist-get raid :owner))))
      (when record
        (goblins--info-row "Members"
                           (string-join
                            (mapcar (lambda (m) (goblins--safe (plist-get m :agent_name)))
                                    (append (plist-get record :members) nil))
                            ", "))))))

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
      (goblins--info-row "Activity" (or (plist-get session :agent_state) "unknown"))
      (goblins--info-row "Session" (plist-get session :id))
      (goblins--info-row "Directory" (let ((cwd (plist-get session :cwd)))
                                       (and (stringp cwd) (abbreviate-file-name cwd))))
      (goblins--info-row "Configuration" (plist-get session :name))
      (goblins--info-row "Manifest" (plist-get session :configuration))
      (goblins--info-row "Created by" (goblins--created-by session))
      (goblins--info-row "Dev shell" (plist-get session :dev_shell))
      (goblins--info-row "Docker" (if (eq (plist-get session :docker_enabled) t)
                                      (format "enabled%s" (if-let* ((scope (plist-get session :scope)))
                                                              (format " (scope %s)" scope) ""))
                                    "disabled"))
      (goblins--info-row "Resumed from" (plist-get session :recreated_from))
      (goblins--info-row "Skipped"
                         (mapconcat (lambda (s) (format "%s (%s)" (plist-get s :name) (plist-get s :reason)))
                                    (plist-get session :skipped) "; "))
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
    (define-key map (kbd "q") #'quit-window)
    map))

(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-info-mode 'motion)
  (evil-define-key* 'motion goblins-info-mode-map (kbd "q") #'quit-window))

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
          (propertize (goblins--request-subject record) 'font-lock-face 'goblins-key)
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

(defun goblins--review-request (source record)
  "Show RECORD, a pending request of SOURCE's server, with all its details,
for a decision there."
  (let ((buffer (goblins-permissions-buffer source (plist-get record :session))))
    (goblins--display-main buffer)
    (with-current-buffer buffer
      (when-let* ((section (goblins--find-request-section (plist-get record :id))))
        (magit-section-show section)
        (goblins--view-goto (oref section start))))))

(defun goblins-permissions-decide (approved)
  "Decide the request at point, else the only pending one.
Accept it when APPROVED is non-nil, otherwise deny it."
  (let* ((section (magit-current-section))
         (at-point (and section (object-of-class-p section 'goblins-request-section)
                        (equal (plist-get (oref section record) :state) "pending")
                        section))
         (record (or (and at-point (oref at-point record))
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
      (goblins--decide-record record approved
                              (and at-point (goblins--section-shows-p at-point))))))

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

;;;; Request popup
;;
;; A small window at the bottom with what waits for your decision.  First
;; files a goblin's editor asks to open here (see `goblins-editor-approval'),
;; from any goblin, wherever you are.  Then the selected goblin's pending
;; permission request: what it asks for and why, which opens when you select a
;; goblin that is waiting and when the goblin on the left asks for something.
;; It never takes the focus, isn't remembered, and closes once nothing waits.

(defcustom goblins-request-popups t
  "Whether the workspace pops up the selected goblin's pending request."
  :type 'boolean :group 'goblins)

(defconst goblins--popup-buffer-name "*Goblins Request*")

(defvar goblins--pending-edits nil
  "Files goblins' editors asked to open, waiting for approval, oldest first.
Each is a plist: :goblin (a label), :files (file names), :approve and :deny
\(functions that settle it) and :live (a function, nil once the requester
has gone).")

(defun goblins--live-edits ()
  "`goblins--pending-edits' without those whose requester has gone."
  (setq goblins--pending-edits
        (cl-remove-if-not (lambda (edit) (funcall (plist-get edit :live)))
                          goblins--pending-edits)))

(defun goblins--pending-requests (snapshot id)
  "The pending permission requests of session ID in SNAPSHOT, oldest first."
  (cl-remove-if-not (lambda (r) (and (equal (plist-get r :session) id)
                                     (equal (plist-get r :state) "pending")))
                    (append (plist-get snapshot :permissions) nil)))

(defun goblins--popup-preview (record)
  "One line about RECORD's preview, or nil."
  (cond
   ((goblins--awaiting-preview-p record) "evaluating the refresh; its preview follows")
   ((when-let* ((preview (plist-get record :preview))
                (text (or (plist-get preview :description) (plist-get preview :error))))
      (car (split-string text "\n"))))))

(defun goblins--waiting (count)
  (if (> count 1)
      (propertize (format "   1 of %d waiting" count) 'face 'goblins-attention)
    ""))

(defun goblins--popup-insert-edit (edit count)
  (insert (propertize (goblins--safe (plist-get edit :goblin)) 'face 'bold)
          " wants to open in Emacs" (goblins--waiting count) "\n")
  (dolist (file (plist-get edit :files))
    (insert "    " (propertize (goblins--safe file) 'face 'goblins-key) "\n"))
  (insert (propertize "a open   d refuse   q later" 'face 'goblins-dim)))

(defun goblins--popup-insert-host-command (session pending)
  "The popup for a host command: each part on its own field, the command
wrapped instead of one long heading.  The fields come from the preview's
structured values, which the daemon escaped: no request text can add a
line or a field."
  (let* ((record (car pending))
         (preview (plist-get record :preview))
         (description (or (plist-get preview :description) "")))
    (insert (propertize (goblins--safe (or (plist-get session :agent_name)
                                           (plist-get record :agent_name)))
                        'face 'bold)
            " asks to run a command on the host"
            (goblins--waiting (length pending))
            "\n")
    (goblins--wrapped-field "Command:" (plist-get preview :command))
    (goblins--wrapped-field "Directory:" (plist-get preview :directory))
    (seq-doseq (variable (plist-get preview :environment))
      (goblins--wrapped-field "Variable:" variable))
    (goblins--wrapped-field "Time limit:" (plist-get preview :time_limit))
    (goblins--wrapped-field "Reason:" (plist-get record :reason))
    ;; The daemon's warning, its first line.
    (insert "    " (propertize (goblins--safe (car (split-string description "\n")))
                               'face 'goblins-dim)
            "\n")
    (insert (propertize "a accept   d deny   p all requests   q close" 'face 'goblins-dim))))

(defun goblins--popup-insert-raid-owner (pending)
  "The popup for a paused raid: the raid, its former owner and its members,
from the daemon's preview, and the keys that resume or dissolve it."
  (let* ((record (car pending))
         (preview (plist-get record :preview)))
    (insert (propertize (format "raid %s" (goblins--safe (plist-get preview :name)))
                        'face 'bold)
            " is paused"
            (goblins--waiting (length pending))
            "\n")
    (goblins--field "Reason:" (plist-get record :reason))
    (goblins--insert-raid-owner-fields preview)
    (insert (propertize "o new owner   x dissolve   k dissolve and stop members   q close"
                        'face 'goblins-dim))))

(defun goblins--popup-insert-request (session pending)
  (let ((record (car pending)))
    (cond ((equal (plist-get record :kind) "raid-owner")
           (goblins--popup-insert-raid-owner pending))
          ((and (equal (plist-get record :kind) "host-command")
                (plist-get (plist-get record :preview) :command))
           (goblins--popup-insert-host-command session pending))
          (t (goblins--popup-insert-permission session pending)))))

(defun goblins--popup-insert-permission (session pending)
  (let ((record (car pending)))
    (insert (propertize (goblins--safe (or (plist-get session :agent_name)
                                           (plist-get record :agent_name)))
                        'face 'bold)
            " asks for "
            (propertize (goblins--safe (plist-get record :package)) 'face 'goblins-key)
            (let ((kind (plist-get record :kind)))
              (if (member kind '(nil "package")) ""
                (propertize (format " (%s)" (goblins--safe kind)) 'face 'goblins-dim)))
            (goblins--waiting (length pending))
            "\n")
    (goblins--field "Reason:" (plist-get record :reason))
    (when-let* ((preview (goblins--popup-preview record)))
      (goblins--field "Preview:" preview))
    (when-let* ((message (plist-get record :message)))
      (goblins--field "Message:" message))
    (insert (propertize "a accept   d deny   p all requests   q close" 'face 'goblins-dim))))

(defun goblins--popup-requests ()
  "The selected goblin's pending requests the popup shows, or nil."
  (and goblins-request-popups
       (goblins--pending-requests (goblins--view-snapshot) goblins--view-session)))

(defun goblins--popup-render ()
  "Redraw the popup: an edit waiting for approval first, else the goblin's
request.  Close it once nothing waits."
  (let ((inhibit-read-only t)
        (edits (goblins--live-edits))
        (pending (goblins--popup-requests)))
    (erase-buffer)
    (cond (edits (goblins--popup-insert-edit (car edits) (length edits)))
          (pending (goblins--popup-insert-request (goblins--view-session-record) pending)))
    (goto-char (point-min))
    (when-let* ((window (get-buffer-window (current-buffer) t)))
      (if (or edits pending)
          (fit-window-to-buffer window 12 2)
        (delete-window window)))))

(defun goblins--popup-buffer ()
  "The popup buffer, created if needed."
  (let ((buffer (get-buffer-create goblins--popup-buffer-name)))
    (with-current-buffer buffer
      (unless (derived-mode-p 'goblins-request-popup-mode)
        (goblins-request-popup-mode)
        (setq default-directory (expand-file-name "~/")))
      (setq goblins--view-render #'goblins--popup-render))
    buffer))

(defun goblins--popup-display (buffer)
  "Render BUFFER and pop it up at the bottom, without selecting it.
Return BUFFER, or nil and close the popup when nothing waits."
  (with-current-buffer buffer (goblins--popup-render))
  (if (zerop (buffer-size buffer))
      (progn (goblins--popup-close) nil)
    (display-buffer buffer
                    '((display-buffer-in-side-window)
                      (side . bottom) (slot . 0)
                      (window-height . (lambda (w) (fit-window-to-buffer w 12 2)))
                      (window-parameters (no-delete-other-windows . t))))
    buffer))

(defun goblins--popup-show (source id)
  "Pop up what waits, with goblin ID of SOURCE's daemon as the selected goblin."
  (goblins--view-buffer goblins--popup-buffer-name #'goblins-request-popup-mode
                        #'goblins--popup-render source id)
  (goblins--popup-display (goblins--popup-buffer)))

(defun goblins--popup-edits-changed ()
  "Pop up an edit that arrived, or move on after one was settled."
  (goblins--popup-display (goblins--popup-buffer)))

(defun goblins--popup-close ()
  "Close the request popup if it is shown."
  (when-let* ((window (get-buffer-window goblins--popup-buffer-name t)))
    (delete-window window)))

(defun goblins--popup-follow (source id)
  "Show what waits after goblin ID was selected, or close the popup."
  (goblins--popup-show source id))

(defun goblins--popup-on-request (record)
  "Pop up RECORD, a request that became pending, if the selected goblin asks."
  (when (and goblins-request-popups (goblins--in-workspace-p)
             (equal goblins--last-goblin (cons goblins--directory (plist-get record :session))))
    (goblins--popup-show (current-buffer) (plist-get record :session))))
(add-hook 'goblins-request-hook #'goblins--popup-on-request)

(defun goblins-request-popup ()
  "Pop up what waits for your decision: files a goblin's editor wants to open,
else the pending request of this goblin terminal's goblin, or of the goblin
selected in the workspace."
  (interactive)
  (let ((goblin (cond (goblins--session-key
                       (cons (goblins--status-buffer (car goblins--session-key))
                             (nth 2 goblins--session-key)))
                      (goblins--last-goblin
                       (cons (goblins--status-buffer (car goblins--last-goblin))
                             (cdr goblins--last-goblin))))))
    (unless (if goblin
                (goblins--popup-show (car goblin) (cdr goblin))
              (goblins--popup-edits-changed))
      (message "Nothing waiting for a decision"))))

(defun goblins--shown-popup ()
  "The request popup: this buffer, else the shown one; or a user error."
  (cond ((derived-mode-p 'goblins-request-popup-mode) (current-buffer))
        ((get-buffer-window goblins--popup-buffer-name) (get-buffer goblins--popup-buffer-name))
        (t (user-error "Nothing waiting for a decision"))))

(defun goblins--popup-decide (approved)
  (with-current-buffer (goblins--shown-popup)
    (if-let* ((edit (car (goblins--live-edits))))
        (funcall (plist-get edit (if approved :approve :deny)))
      (let ((record (car (goblins--popup-requests))))
        (unless record (user-error "Nothing waiting for a decision"))
        (with-current-buffer goblins--view-source
          ;; The popup shows a host command whole from its structured fields.
          (goblins--decide-record record approved
                                  (and (plist-get (plist-get record :preview) :command) t)))))))

(defun goblins-request-popup-accept ()
  "Open the files in the popup, or accept its request.
Elsewhere, decide what the shown popup asks."
  (interactive)
  (goblins--popup-decide t))

(defun goblins-request-popup-deny ()
  "Refuse the files in the popup, or deny its request.
Elsewhere, decide what the shown popup asks."
  (interactive)
  (goblins--popup-decide nil))

(defun goblins--popup-raid-owner ()
  "(SOURCE . RECORD) for the paused raid the shown popup asks about, or a
user error."
  (with-current-buffer (goblins--shown-popup)
    (let ((record (and (not (goblins--live-edits)) (car (goblins--popup-requests)))))
      (unless (equal (plist-get record :kind) "raid-owner")
        (user-error "The popup shows no paused raid"))
      (cons goblins--view-source record))))

(defun goblins-raid-owner-pick (source record)
  "Resume the paused raid of RECORD, a pending raid-owner request of
SOURCE's server, with a member you choose as its owner.  The daemon refuses
it if the raid changed since the prompt it answers."
  (let* ((preview (plist-get record :preview))
         (members (cl-remove-if
                   (lambda (m) (member (plist-get m :state) '("stopped" "failed")))
                   (append (plist-get preview :members) nil)))
         (choices (mapcar (lambda (m) (cons (goblins--raid-owner-member-line m) m)) members))
         (name (goblins--safe (plist-get preview :name))))
    (unless choices (user-error "Raid %s has no running member to own it" name))
    (let ((member (cdr (assoc (completing-read (format "New owner of raid %s: " name)
                                               choices nil t)
                              choices))))
      (unless member (user-error "Select a member"))
      (goblins--host-call source 'raids.set_owner
                          (list :raid (plist-get preview :name)
                                :session (plist-get member :id)
                                :id (plist-get preview :raid)
                                :generation (plist-get preview :generation)))
      (message "Raid %s resumed with owner %s" name
               (goblins--safe (plist-get member :agent_name))))))

(defun goblins-raid-owner-dissolve (source record &optional stop)
  "Dissolve the paused raid of RECORD, a pending raid-owner request of
SOURCE's server, after confirming.  STOP also stops each of its members."
  (let* ((preview (plist-get record :preview))
         (name (goblins--safe (plist-get preview :name)))
         (count (length (append (plist-get preview :members) nil))))
    (when (y-or-n-p (if stop
                        (format "Dissolve raid %s and stop its %d member%s? "
                                name count (if (= count 1) "" "s"))
                      (format "Dissolve raid %s?  Its goblins keep running. " name)))
      (goblins--host-call source 'raids.destroy
                          (append (list :raid (plist-get preview :name)
                                        :id (plist-get preview :raid)
                                        :generation (plist-get preview :generation))
                                  (and stop (list :stop_members t))))
      (message "Dissolved raid %s%s" name (if stop " and stopped its members" "")))))

(defun goblins-request-popup-owner ()
  "Resume the popup's paused raid with a member you choose as its owner."
  (interactive)
  (let ((request (goblins--popup-raid-owner)))
    (goblins-raid-owner-pick (car request) (cdr request))))

(defun goblins-request-popup-dissolve ()
  "Dissolve the popup's paused raid after confirming; its goblins keep running."
  (interactive)
  (let ((request (goblins--popup-raid-owner)))
    (goblins-raid-owner-dissolve (car request) (cdr request))))

(defun goblins-request-popup-dissolve-stop ()
  "Dissolve the popup's paused raid and stop its members, after confirming."
  (interactive)
  (let ((request (goblins--popup-raid-owner)))
    (goblins-raid-owner-dissolve (car request) (cdr request) t)))

(defun goblins-request-popup-all ()
  "Show all of the popup's goblin's requests, then close the popup."
  (interactive)
  (let* ((popup (goblins--shown-popup))
         (source (buffer-local-value 'goblins--view-source popup))
         (id (buffer-local-value 'goblins--view-session popup)))
    (unless (and (buffer-live-p source) id) (user-error "No goblin selected"))
    (goblins--popup-close)
    (goblins--display-main (goblins-permissions-buffer source id))))

(defvar goblins-request-popup-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (define-key map "a" #'goblins-request-popup-accept)
    (define-key map "d" #'goblins-request-popup-deny)
    (define-key map "p" #'goblins-request-popup-all)
    (define-key map "o" #'goblins-request-popup-owner)
    (define-key map "x" #'goblins-request-popup-dissolve)
    (define-key map "k" #'goblins-request-popup-dissolve-stop)
    (define-key map (kbd "RET") #'goblins-request-popup-all)
    (define-key map "q" #'goblins--popup-close)
    map))

(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-request-popup-mode 'motion)
  (evil-define-key* 'motion goblins-request-popup-mode-map
    "a" #'goblins-request-popup-accept
    "d" #'goblins-request-popup-deny
    "p" #'goblins-request-popup-all
    "o" #'goblins-request-popup-owner
    "x" #'goblins-request-popup-dissolve
    "k" #'goblins-request-popup-dissolve-stop
    (kbd "RET") #'goblins-request-popup-all
    "q" #'goblins--popup-close)
  (evil-make-intercept-map goblins-request-popup-mode-map 'motion t))

(define-derived-mode goblins-request-popup-mode special-mode "Goblins-Request"
  "What waits for your decision: files to open, or the goblin's request.
\\<goblins-request-popup-mode-map>\\[goblins-request-popup-accept] opens or accepts, \\[goblins-request-popup-deny] refuses or denies, \\[goblins-request-popup-all] shows all requests, \\[goblins--popup-close] closes.
A paused raid takes a new owner with \\[goblins-request-popup-owner], or
dissolves with \\[goblins-request-popup-dissolve]; \\[goblins-request-popup-dissolve-stop] also stops its members."
  (setq-local mode-line-format nil
              truncate-lines nil))

;;;; Switching

(defun goblins--goblin-description (source session)
  "Describe SESSION of SOURCE's server for completion: its type, activity,
raid and directory, its pending requests and queued messages, and whether
it has a terminal here."
  (let* ((snapshot (buffer-local-value 'goblins--snapshot source))
         (id (plist-get session :id))
         (pending (cl-count-if (lambda (r) (and (equal (plist-get r :session) id)
                                                (equal (plist-get r :state) "pending")))
                               (append (plist-get snapshot :permissions) nil)))
         (inbox (or (plist-get session :inbox) 0))
         (raid (plist-get session :raid))
         (raid-name (and (listp raid) (plist-get raid :name)))
         (cwd (plist-get session :cwd)))
    (concat (propertize " " 'display '(space :align-to 32))
            (propertize (format "%-8s" (goblins--safe (plist-get session :name))) 'face 'goblins-dim)
            " " (truncate-string-to-width (or (goblins--sidebar-status session) "") 20 nil ?\s)
            (if raid-name (propertize (format " raid %-10s" (goblins--safe raid-name)) 'face 'goblins-key)
              (make-string 16 ?\s))
            (if (stringp cwd) (propertize (concat " " (abbreviate-file-name cwd)) 'face 'goblins-dim) "")
            (if (> pending 0) (propertize (format " !%d" pending) 'face 'goblins-attention) "")
            (if (> inbox 0) (propertize (format " ✉%d" inbox) 'face 'goblins-inbox) "")
            (if (with-current-buffer source (goblins--terminal-buffer id))
                (propertize " [term]" 'face 'goblins-dim)
              ""))))

(defun goblins--default-source ()
  "The status buffer, connected, of the server at hand: this goblin buffer's,
the workspace's, else that of `goblins--resolve-directory'."
  (goblins--connect (or (car goblins--session-key)
                        (and (goblins--in-workspace-p) (car goblins--last-goblin)))))

(defun goblins--running-sessions (snapshot)
  "The sessions of SNAPSHOT that have not finished."
  (cl-remove-if (lambda (s) (member (plist-get s :state) '("stopped" "failed")))
                (append (plist-get snapshot :sessions) nil)))

(defun goblins--goblin-candidate (source session)
  "SESSION of SOURCE's server as a completion candidate: its name, with its
identity as the `goblins-goblin' property, (DIRECTORY . SESSION-ID)."
  (propertize (goblins--safe (plist-get session :agent_name))
              'goblins-goblin (cons (buffer-local-value 'goblins--directory source)
                                    (plist-get session :id))))

(defun goblins--goblin-annotation (source)
  "An annotation function for goblin candidates of SOURCE's server."
  (lambda (candidate)
    (when-let* ((id (cdr (get-text-property 0 'goblins-goblin candidate)))
                (session (goblins--snapshot-session
                          (buffer-local-value 'goblins--snapshot source) id)))
      (goblins--goblin-description source session))))

(defun goblins--terminal-session-named (source name ids)
  "The session ID of IDS whose agent terminal of SOURCE's server is the
buffer called NAME, or nil."
  (when-let* ((buffer (get-buffer name))
              (key (buffer-local-value 'goblins--session-key buffer))
              ((equal (car key) (buffer-local-value 'goblins--directory source))))
    (car (member (nth 2 key) ids))))

(defun goblins--goblin-table (source candidates)
  "A completion table of goblin CANDIDATES of SOURCE's server, in the
`goblin' category.  The name of a candidate's agent terminal buffer matches
too, unlisted, so an Embark action on that buffer can choose its goblin."
  (let ((annotate (goblins--goblin-annotation source))
        (ids (mapcar (lambda (c) (cdr (get-text-property 0 'goblins-goblin c))) candidates)))
    (lambda (string predicate action)
      (cond ((eq action 'metadata)
             `(metadata (category . goblin) (annotation-function . ,annotate)))
            ((and (memq action '(nil lambda))
                  (goblins--terminal-session-named source string ids))
             t)
            (t (complete-with-action action candidates string predicate))))))

(defun goblins--read-goblin (prompt source &optional sessions)
  "Choose one of SESSIONS of SOURCE's server, by default the running ones.
Return (SOURCE . SESSION-ID)."
  (let* ((sessions (or sessions (goblins--running-sessions
                                 (buffer-local-value 'goblins--snapshot source))))
         (candidates (mapcar (lambda (s) (goblins--goblin-candidate source s)) sessions)))
    (unless candidates (user-error "No running goblins"))
    (let* ((choice (completing-read prompt (goblins--goblin-table source candidates) nil t))
           (found (car (member choice candidates))))
      (cons source
            (or (and found (cdr (get-text-property 0 'goblins-goblin found)))
                (goblins--terminal-session-named
                 source choice (mapcar (lambda (s) (plist-get s :id)) sessions))
                (user-error "Select a goblin"))))))

;;;; Attention

(defcustom goblins-attention-notify-function #'goblins-notify-request
  "Function called with a permission request record when it becomes pending,
while `goblins-attention-mode' is on.  It runs in the server's status buffer.
`goblins-notify-message' announces in the echo area only."
  :type 'function :group 'goblins)

(defcustom goblins-attention-approval-function #'goblins-notify-approval
  "Function called with a goblin's session record when its agent begins
waiting at its own permission prompt, while `goblins-attention-mode' is on.
It runs in the server's status buffer.  nil announces nothing."
  :type '(choice function (const :tag "Nothing" nil)) :group 'goblins)

(defvar goblins--mode-line-string ""
  "Mode line text of `goblins-attention-mode'.")
(put 'goblins--mode-line-string 'risky-local-variable t)

(defun goblins--raid-owner-notice (record)
  "(TITLE . BODY) announcing RECORD, a paused raid's prompt."
  (let ((reason (goblins--safe (let ((reason (plist-get record :reason)))
                                 (if (and (stringp reason) (not (string-empty-p reason)))
                                     reason "the owner left")))))
    (cons (format "Goblins: raid %s is paused" (goblins--safe (plist-get record :package)))
          (format "%s%s; pick a new owner or dissolve the raid."
                  (upcase (substring reason 0 1)) (substring reason 1)))))

(defun goblins-notify-message (record)
  "Announce the permission request RECORD in the echo area."
  (if (equal (plist-get record :kind) "raid-owner")
      (let ((notice (goblins--raid-owner-notice record)))
        (message "%s — %s" (car notice) (cdr notice)))
    (message "Goblins: %s asks for %s — %s"
             (goblins--safe (plist-get record :agent_name))
             (goblins--request-subject record)
             (goblins--safe (plist-get record :reason)))))

(defun goblins--notify (title body action)
  "Show TITLE and BODY as a desktop notification that calls ACTION when
clicked, or in the echo area when Emacs has no notification service.
Both are shown as plain text."
  (unless (condition-case nil
              (and (require 'notifications)
                   (notifications-notify
                    :title title
                    ;; A server with body markup would read the goblin's text
                    ;; as markup; the title is never markup.
                    :body (if (memq :body-markup (notifications-get-capabilities))
                              (replace-regexp-in-string
                               "[&<>]" (lambda (c) (pcase c ("&" "&amp;") ("<" "&lt;") (_ "&gt;")))
                               body t t)
                            body)
                    :app-name "Goblins" :actions '("default" "Show")
                    :on-action (lambda (_id _key)
                                 (ignore-errors (select-frame-set-input-focus (selected-frame)))
                                 (funcall action))))
            (error nil))
    (message "%s — %s" title body)))

(defun goblins-notify-request (record)
  "Announce the permission request RECORD with `goblins--notify'; clicking
it shows the request."
  (let ((source (current-buffer)))
    (if (equal (plist-get record :kind) "raid-owner")
        ;; Its popup has the keys that resume or dissolve the raid.
        (let ((notice (goblins--raid-owner-notice record)))
          (goblins--notify (car notice) (cdr notice)
                           (lambda () (when (buffer-live-p source)
                                        (goblins--popup-show source (plist-get record :session))))))
      (goblins--notify (format "Goblins: %s asks for %s"
                               (goblins--safe (plist-get record :agent_name))
                               (goblins--request-subject record))
                       (goblins--safe (plist-get record :reason))
                       (lambda () (when (buffer-live-p source) (goblins-requests source record)))))))

(defun goblins-notify-approval (session)
  "Announce that SESSION's agent waits at its own permission prompt with
`goblins--notify'; clicking it shows the goblin's terminal."
  (let ((source (current-buffer))
        (id (plist-get session :id)))
    (goblins--notify (format "Goblins: %s needs your approval"
                             (goblins--safe (plist-get session :agent_name)))
                     "It is waiting at its own permission prompt."
                     (lambda () (when (buffer-live-p source) (goblins-open-session source id))))))

(defun goblins--attention-total ()
  "What waits for your approval over every connected server: pending
requests and goblins at their agent's own permission prompt."
  (let ((total 0))
    (dolist (buffer (buffer-list))
      (when (eq (buffer-local-value 'major-mode buffer) 'goblins-status-mode)
        (let ((snapshot (buffer-local-value 'goblins--snapshot buffer)))
          (setq total (+ total
                         (cl-count-if (lambda (r) (equal (plist-get r :state) "pending"))
                                      (append (plist-get snapshot :permissions) nil))
                         (length (goblins--waiting-approval snapshot)))))))
    total))

(defun goblins--update-mode-line ()
  (let ((total (goblins--attention-total)))
    (setq goblins--mode-line-string
          (if (= total 0) ""
            (propertize (format " Goblins!%d" total)
                        'face 'goblins-attention
                        'help-echo "Goblins waiting for your approval; mouse-1: open the workspace"
                        'mouse-face 'mode-line-highlight
                        'local-map (make-mode-line-mouse-map 'mouse-1 #'goblins-workspace))))
    (force-mode-line-update t)))

(defun goblins--attention-request (record)
  (funcall goblins-attention-notify-function record))

(defun goblins--attention-approval (session)
  (when goblins-attention-approval-function
    (funcall goblins-attention-approval-function session)))

;;;###autoload
(define-minor-mode goblins-attention-mode
  "Show in the mode line how much waits for your approval, and announce it.
Counts pending requests and goblins waiting at their agent's own permission
prompt.  New ones are passed to `goblins-attention-notify-function' and
`goblins-attention-approval-function', which show desktop notifications."
  :global t :group 'goblins
  (if goblins-attention-mode
      (progn
        (unless (memq 'goblins--mode-line-string global-mode-string)
          (setq global-mode-string (append global-mode-string '(goblins--mode-line-string))))
        (add-hook 'goblins-snapshot-hook #'goblins--update-mode-line)
        (add-hook 'goblins-request-hook #'goblins--attention-request)
        (add-hook 'goblins-approval-hook #'goblins--attention-approval)
        (goblins--update-mode-line))
    (setq global-mode-string (delq 'goblins--mode-line-string global-mode-string))
    (remove-hook 'goblins-snapshot-hook #'goblins--update-mode-line)
    (remove-hook 'goblins-request-hook #'goblins--attention-request)
    (remove-hook 'goblins-approval-hook #'goblins--attention-approval)))

(declare-function notifications-notify "notifications" (&rest params))
(declare-function notifications-get-capabilities "notifications" (&optional bus))
(declare-function goblins-requests "goblins-completion" (source record))
(declare-function goblins-workspace "goblins-workspace" (&optional directory))

(provide 'goblins-views)
;;; goblins-views.el ends here
