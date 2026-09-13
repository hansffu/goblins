;;; goblins.el --- Goblins agent status and approvals -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; Run M-x goblins-status, then press s to start the server if needed.
;; Connects directly to the daemon's host socket using JSON-RPC API 1.

;;; Code:

(require 'cl-lib)
(require 'jsonrpc)
(require 'magit-section)
(require 'subr-x)

(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))

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
(defvar-local goblins--notice "Disconnected; s start server, r reconnect")
(defvar-local goblins--decisions nil)

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

(defun goblins--render ()
  "Render state, preserving section identity and folding across updates."
  (let* ((section (magit-current-section))
         (ident (and section (magit-section-ident section)))
         (offset (and section (- (point) (oref section start))))
         (inhibit-read-only t)
         (sessions (append (plist-get goblins--snapshot :sessions) nil))
         (requests (append (plist-get goblins--snapshot :permissions) nil))
         (pending (cl-remove-if-not
                   (lambda (r) (equal (plist-get r :state) "pending")) requests)))
    (erase-buffer)
    (magit-insert-section (goblins-section 'root)
      (insert (propertize "Goblins\n" 'face 'magit-section-heading)
              (goblins--safe goblins--directory) "\n"
              (goblins--safe goblins--notice) "\n\n")
      (magit-insert-section (goblins-section 'agents)
        (magit-insert-heading (format "Agents (%d)" (length sessions)))
        (unless sessions (insert "  No agents\n"))
        (dolist (agent sessions)
          (magit-insert-section (goblins-agent-section (plist-get agent :id) t)
            (magit-insert-heading
              (format "  %-20s %-12s %s"
                      (goblins--safe (plist-get agent :agent_name))
                      (goblins--safe (plist-get agent :state))
                      (goblins--safe (plist-get agent :name))))
            (goblins--field "Session:" (plist-get agent :id))
            (goblins--field "Startup:" (string-join (append (plist-get agent :initial_packages) nil) ", "))
            (goblins--field "Granted:" (string-join (append (plist-get agent :packages) nil) ", "))
            (when-let* ((detail (plist-get agent :detail)))
              (goblins--field "Detail:" detail))))
        (insert "\n"))
      (magit-insert-section (goblins-section 'pending)
        (magit-insert-heading (format "Pending requests (%d)" (length pending)))
        (unless pending (insert "  No pending requests\n\n"))
        (mapc #'goblins--insert-request pending))
      (magit-insert-section (goblins-section 'recent t)
        (magit-insert-heading "Recent requests")
        (mapc #'goblins--insert-request (cl-set-difference requests pending))))
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
                "; s start server, r reconnect"))
  (goblins--disconnect)
  (goblins--render))

(defun goblins--server-command (action)
  "Run server ACTION asynchronously for this buffer's state directory."
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
                                         goblins--notice "Server stopped; s start server")
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
  "Start the server for this status buffer, then connect automatically."
  (interactive)
  (goblins--server-command "start"))

(defun goblins-stop-server ()
  "Stop this buffer's server and all its agents asynchronously."
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
  (unless (and goblins--connection goblins--subscription)
    (user-error "Disconnected; press r to reconnect"))
  (let* ((section (magit-current-section))
         (record (and section
                      (object-of-class-p section 'goblins-request-section)
                      (oref section record)))
         (id (plist-get record :id)))
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
  "Accept the pending request at point."
  (interactive)
  (goblins--decide t))

(defun goblins-deny ()
  "Deny the pending request at point."
  (interactive)
  (goblins--decide nil))

(defun goblins-quit ()
  "Close this frontend, leaving the daemon and its agents running."
  (interactive)
  (quit-window t))

(defvar goblins-status-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map magit-section-mode-map)
    (define-key map (kbd "g") #'goblins-refresh)
    (define-key map (kbd "r") #'goblins-refresh)
    (define-key map (kbd "s") #'goblins-start-server)
    (define-key map (kbd "S") #'goblins-stop-server)
    (define-key map (kbd "a") #'goblins-accept)
    (define-key map (kbd "d") #'goblins-deny)
    (define-key map (kbd "q") #'goblins-quit)
    (define-key map (kbd "RET") #'magit-section-toggle)
    map))

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
    (kbd "RET") #'magit-section-toggle
    (kbd "za") #'magit-section-toggle
    (kbd "a") #'goblins-accept
    (kbd "d") #'goblins-deny
    (kbd "r") #'goblins-refresh
    (kbd "gr") #'goblins-refresh
    (kbd "s") #'goblins-start-server
    (kbd "S") #'goblins-stop-server
    (kbd "q") #'goblins-quit))

(define-derived-mode goblins-status-mode magit-section-mode "Goblins"
  "Status of Goblins agents and permission requests.
\<goblins-status-mode-map>
Use \[goblins-accept] to accept and \[goblins-deny] to deny.
Use \[goblins-refresh] to reconnect."
  (setq-local header-line-format
              '(:eval (if (bound-and-true-p evil-local-mode)
                          " s start   S stop server   a accept   d deny   TAB fold   j/k navigate   gr refresh   q quit"
                        " s start   S stop server   a accept   d deny   TAB fold   n/p navigate   g refresh   q quit")))
  (setq-local revert-buffer-function (lambda (&rest _) (goblins-refresh)))
  (add-hook 'kill-buffer-hook #'goblins--disconnect nil t))

;;;###autoload
(defun goblins-status (&optional directory)
  "Show Goblins status for DIRECTORY.
With a prefix argument, prompt for the daemon state directory."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "
                                            (or goblins-state-directory
                                                (goblins--default-directory))))))
  (let* ((directory (directory-file-name
                     (expand-file-name (or directory goblins-state-directory
                                           (goblins--default-directory)))))
         (buffer (get-buffer-create (format "*Goblins: %s*" directory))))
    (pop-to-buffer buffer)
    (unless (derived-mode-p 'goblins-status-mode)
      (goblins-status-mode)
      (setq goblins--directory directory))
    (unless (or goblins--connection (process-live-p goblins--server-process))
      (goblins-refresh))))

(provide 'goblins)
;;; goblins.el ends here
