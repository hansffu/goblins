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
(declare-function ghostel-exec "ghostel" (buffer program &optional args identity))
(defvar ghostel-kill-buffer-on-exit)
(defvar ghostel-identity)

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
(defvar goblins--terminal-buffers (make-hash-table :test #'equal)
  "Emacs-only mapping from (directory instance session) to Ghostel buffers.")
(defvar-local goblins--session-key nil)
(defvar goblins-run-history nil)

(defclass goblins-section (magit-section) ())
(defclass goblins-agent-section (goblins-section) ())
(defclass goblins-request-section (goblins-section)
  ((record :initarg :record :initform nil)))

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

(defun goblins--insert-request (record)
  (magit-insert-section section
      (goblins-request-section (plist-get record :id) t)
    (oset section record record)
    (magit-insert-heading
      (format "  %s  %s  [%s]"
              (goblins--safe (plist-get record :agent_name))
              (goblins--safe (plist-get record :package))
              (goblins--safe (plist-get record :state))))
    (goblins--field "Reason:" (plist-get record :reason))
    (goblins--field "Session:" (plist-get record :session))
    (goblins--field "Request:" (plist-get record :id))
    (when-let* ((preview (plist-get record :preview)))
      (goblins--field
       "Preview:"
       (if-let* ((error (plist-get preview :error)))
           error
         (format "In store: %s; download: %s; build required: %s"
                 (if (eq (plist-get preview :in_store) t) "yes" "no")
                 (or (plist-get preview :download) "unknown")
                 (if (eq (plist-get preview :build_required) t) "yes" "no")))))
    (when-let* ((message (plist-get record :message)))
      (goblins--field "Message:" message))
    (insert "\n")))

(defun goblins--insert-agent (agent)
  (magit-insert-section (goblins-agent-section (plist-get agent :id) t)
    (magit-insert-heading
      (format "  %-20s %-12s %s%s"
              (goblins--safe (plist-get agent :agent_name))
              (goblins--safe (plist-get agent :state))
              (goblins--safe (plist-get agent :name))
              (if (goblins--terminal-buffer (plist-get agent :id))
                  "  [RET: terminal]" "")))
    (goblins--field "Session:" (plist-get agent :id))
    (goblins--field "Startup:" (string-join (append (plist-get agent :initial_packages) nil) ", "))
    (goblins--field "Granted:" (string-join (append (plist-get agent :packages) nil) ", "))
    (when-let* ((detail (plist-get agent :detail)))
      (goblins--field "Detail:" detail))))

(defun goblins--render ()
  "Render state, preserving section identity and folding across updates."
  (let* ((section (magit-current-section))
         (ident (and section (magit-section-ident section)))
         (offset (and section (- (point) (oref section start))))
         (inhibit-read-only t)
         (sessions (append (plist-get goblins--snapshot :sessions) nil))
         (stopped (cl-remove-if-not
                   (lambda (agent) (member (plist-get agent :state) '("stopped" "failed")))
                   sessions))
         (active (cl-set-difference sessions stopped))
         (requests (append (plist-get goblins--snapshot :permissions) nil))
         (pending (cl-remove-if-not
                   (lambda (r) (equal (plist-get r :state) "pending")) requests)))
    (erase-buffer)
    (magit-insert-section (goblins-section 'root)
      (insert (propertize "Goblins\n" 'face 'magit-section-heading)
              (goblins--safe goblins--directory) "\n"
              (goblins--safe goblins--notice) "\n\n")
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
      (jsonrpc-shutdown connection t))))

(defun goblins--fail (message)
  (setq goblins--notice
        (concat message
                (when (and goblins--decisions
                           (let (pending)
                             (maphash (lambda (_id state)
                                        (when (eq state t) (setq pending t)))
                                      goblins--decisions)
                             pending))
                  "; in-flight decision outcome unknown")
                "; r to reconnect"))
  (goblins--disconnect)
  (goblins--render))

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
                             (if (= (process-exit-status process) 0)
                                 (if (equal action "start")
                                     (goblins-refresh)
                                   (setq goblins--snapshot nil goblins--instance nil
                                         goblins--sequence nil goblins--decisions nil
                                         goblins--notice "Server stopped")
                                   (goblins--render))
                               (goblins--fail
                                (format "Server %s failed: %s" action
                                        (with-current-buffer output
                                          (string-trim (buffer-string)))))))))
                     (when (buffer-live-p output) (kill-buffer output)))))))
      (error
       (kill-buffer output)
       (setq goblins--server-process nil)
       (goblins--fail (format "Cannot %s server: %s" action
                              (error-message-string err)))))))

(defun goblins-start-server ()
  "Start the server and connect, opening its status buffer if needed."
  (interactive)
  (goblins--server-command "start"))

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
                          goblins--notice "Connected")
                    (goblins--render))))))))
      (error (goblins--fail (format "Cannot connect: %s"
                                   (error-message-string err)))))))

(defun goblins--decide (approved)
  (let* ((record
          (if (derived-mode-p 'goblins-status-mode)
              (let ((section (magit-current-section)))
                (and section (object-of-class-p section 'goblins-request-section)
                     (oref section record)))
            (goblins--ensure-connected)
            (goblins--choose
             (if approved "Accept request: " "Deny request: ")
             (mapcar (lambda (request)
                       (cons (format "%s: %s — %s [%s]"
                                     (goblins--safe (plist-get request :agent_name))
                                     (goblins--safe (plist-get request :package))
                                     (goblins--safe (plist-get request :reason))
                                     (plist-get request :id))
                             request))
                     (cl-remove-if-not
                      (lambda (request) (equal (plist-get request :state) "pending"))
                      (append (plist-get goblins--snapshot :permissions) nil)))
             "No pending requests")))
         (id (plist-get record :id)))
    (unless (and goblins--connection goblins--subscription)
      (user-error "Disconnected; use M-x goblins-refresh to reconnect"))
    (unless (equal (plist-get record :state) "pending")
      (user-error "Place point on a pending request"))
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
  "Close the relevant status view, leaving other buffers and agents alone."
  (interactive)
  (let ((buffer (if (derived-mode-p 'goblins-status-mode)
                    (current-buffer)
                  (get-buffer (format "*Goblins: %s*" (goblins--resolve-directory))))))
    (when buffer
      (if (eq (window-buffer (selected-window)) buffer)
          (quit-window t)
        (kill-buffer buffer)))))

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

(defun goblins--ghostel-directory (function directory)
  "Keep namespace-local DIRECTORY reports out of host directory tracking."
  ;; The sandbox's hostname is not an SSH destination, and /workspace need
  ;; not correspond to a host path.  Leave ordinary Ghostel buffers alone.
  (unless (eq (alist-get 'kind ghostel-identity) 'goblins)
    (funcall function directory)))

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
(defun goblins-run ()
  "Choose a configured goblin and launch it in a new Ghostel buffer.
Use the current directory and this status buffer's server, or the default
server outside a status buffer.  Only launches made here are linked by RET."
  (interactive)
  (when (file-remote-p default-directory)
    (user-error "Goblins requires a local working directory"))
  ;; Require the installed module directly, bypassing Ghostel's auto-download.
  (unless (and (require 'ghostel nil t) (require 'ghostel-module nil t)
               (fboundp 'ghostel-exec))
    (user-error "goblins-run requires installed Ghostel with its native module"))
  (let* ((cwd (expand-file-name default-directory))
         (directory (or goblins--directory goblins-state-directory
                        (goblins--default-directory)))
         (executable goblins-executable)
         (configs (goblins--configurations))
         (name (goblins--read-configuration (plist-get configs :names))))
    (goblins--ensure-connected directory)
    (let* ((connection goblins--connection)
           (instance goblins--instance)
           (launch (jsonrpc-request
                    connection 'sessions.start
                    (list :key (concat "emacs-" (md5 (format "%s%s%s" (float-time) (random) (emacs-pid))))
                          :name name :configuration (plist-get configs :configuration)
                          :cwd cwd :rows 24 :cols 80)
                    :timeout 3))
           (session (plist-get launch :session))
           (key (list goblins--directory instance session))
           (buffer (generate-new-buffer
                    (format "*Goblin: %s*" (plist-get launch :agent_name)))))
      (condition-case err
          (progn
            (with-current-buffer buffer (setq default-directory cwd))
            (pop-to-buffer buffer)
            (let ((ghostel-kill-buffer-on-exit nil))
              (ghostel-exec buffer executable
                            (list "--state-dir" (car key) "attach" session
                                  "--instance" instance)
                            `((kind . goblins) (session . ,session) (instance . ,instance))))
            (with-current-buffer buffer
              (setq-local goblins--session-key key)
              (setq-local goblins--directory (car key))
              (setq-local ghostel-kill-buffer-on-exit nil)
              (add-hook 'kill-buffer-hook #'goblins--forget-terminal nil t))
            (puthash key buffer goblins--terminal-buffers)
            (goblins--render-status-buffers)
            buffer)
        (error
         (when (buffer-live-p buffer) (kill-buffer buffer))
         (ignore-errors
           (jsonrpc-request connection 'sessions.stop (list :session session) :timeout 3))
         (signal (car err) (cdr err)))))))

(defvar goblins-status-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map magit-section-mode-map)
    (define-key map (kbd "g") #'goblins-refresh)
    (define-key map (kbd "r") #'goblins-refresh)
    (define-key map (kbd "R") #'goblins-run)
    (define-key map (kbd "a") #'goblins-accept)
    (define-key map (kbd "d") #'goblins-deny)
    (define-key map (kbd "q") #'goblins-quit)
    (define-key map (kbd "RET") #'goblins-visit)
    map))

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
    (kbd "TAB") #'magit-section-toggle
    (kbd "<tab>") #'magit-section-toggle
    (kbd "RET") #'goblins-visit
    (kbd "za") #'magit-section-toggle
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
  (setq-local header-line-format
              '(:eval (if (bound-and-true-p evil-local-mode)
                          " R run   RET terminal   a/d decide   TAB fold   j/k navigate   gr refresh   q quit"
                        " R run   RET terminal   a/d decide   TAB fold   n/p navigate   g refresh   q quit")))
  (setq-local revert-buffer-function (lambda (&rest _) (goblins-refresh)))
  (add-hook 'kill-buffer-hook #'goblins--disconnect nil t))

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

(defun goblins--ensure-connected (&optional directory)
  "Open status for DIRECTORY and await its initial snapshot."
  (goblins-status directory)
  (let ((deadline (+ (float-time) 3)))
    (while (and goblins--connection (not goblins--subscription)
                (< (float-time) deadline))
      (accept-process-output nil 0.01)))
  (unless goblins--subscription
    (user-error "Server disconnected; use M-x goblins-start-server")))

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
;;; goblins.el ends here
