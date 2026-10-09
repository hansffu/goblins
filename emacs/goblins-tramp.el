;;; goblins-tramp.el --- /goblins: TRAMP method and shells for running goblins -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "29.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; Files and processes inside running goblin sandboxes:
;;
;;   C-x C-f /goblins:gribble:/tmp/diagram.svg
;;   C-x C-f /goblins:gribble.snaga:/workspace/      (a child's tree path)
;;   M-x goblins-shell
;;
;; The method runs `goblins exec' as its login program, so dired, eshell,
;; compile, grep, magit and eglot run inside the sandbox.  The host part is an
;; agent name, a tree path with `.' for `/', or a session ID; names complete
;; after "/goblins:".
;;
;; Names are reusable, so each connection is pinned to the session it first
;; reached: (state directory, daemon instance, session ID).  Every later
;; command of that connection passes the pin to `goblins exec', which fails
;; rather than reaching another goblin with the same name.  When a goblin
;; stops, its /goblins: buffers and processes are killed before the pin is
;; released.  Files a goblin wrote are visited with only safe local
;; variables and no `eval:'.

;;; Code:

(require 'cl-lib)
(require 'tramp)
(require 'goblins)

(declare-function ghostel-exec "ghostel" (buffer program &optional args identity))
(declare-function goblin-mode "goblins-terminal" (&optional arg))
(declare-function desktop-save-buffer-p "desktop" (filename bufname mode &rest rest))
(defvar ghostel-kill-buffer-on-exit)
(defvar ghostel-identity)

(defconst goblins-tramp-method "goblins"
  "The TRAMP method for files and processes inside running goblins.")

(defun goblins-tramp-host (path)
  "The TRAMP host for a goblin's tree PATH: `/' is not allowed in hosts."
  (string-replace "/" "." path))

(defun goblins-tramp-file-p (name)
  "Whether NAME is a /goblins: file name."
  (and (stringp name)
       (string-prefix-p (concat "/" goblins-tramp-method ":") name)))

(defun goblins-tramp--state-directory ()
  "The daemon /goblins: means: the one goblins.el is configured for."
  (goblins--resolve-directory
   (or goblins-state-directory (goblins--default-directory))))

(defun goblins-tramp--live-source (directory)
  "DIRECTORY's status buffer if it is connected and has a snapshot."
  (let ((buffer (get-buffer (format "*Goblins: %s*" directory))))
    (and buffer
         (buffer-local-value 'goblins--subscription buffer)
         (buffer-local-value 'goblins--snapshot buffer)
         buffer)))

(defun goblins-tramp--cli (directory &rest args)
  "Run the Goblins CLI for DIRECTORY with ARGS and parse its JSON output.
Always runs locally, whatever `default-directory' is."
  (with-temp-buffer
    (let* ((default-directory temporary-file-directory)
           (code (apply #'call-process goblins-executable nil '(t nil) nil
                        "--state-dir" directory args)))
      (unless (eq code 0)
        (error "Goblins: %s failed" (string-join args " ")))
      (goto-char (point-min))
      (json-parse-buffer :object-type 'plist :array-type 'list))))

(defun goblins-tramp--sessions (directory)
  "Return (INSTANCE . SESSIONS) for DIRECTORY's daemon."
  (if-let* ((source (goblins-tramp--live-source directory)))
      (with-current-buffer source
        (cons goblins--instance (append (plist-get goblins--snapshot :sessions) nil)))
    (cons (plist-get (goblins-tramp--cli directory "rpc" "server.status" "{}") :instance)
          (goblins-tramp--cli directory "list"))))

(defun goblins-tramp--running-p (session)
  (equal (plist-get session :state) "running"))

(defun goblins-tramp--match (host sessions)
  "The running session in SESSIONS that HOST names, as the daemon resolves it."
  (let* ((path (string-replace "." "/" host))
         (running (cl-remove-if-not #'goblins-tramp--running-p sessions)))
    (or (cl-find host sessions :key (lambda (s) (plist-get s :id)) :test #'equal)
        (cl-find path running :key (lambda (s) (plist-get s :path)) :test #'equal)
        (let ((named (cl-remove-if-not
                      (lambda (s) (equal (plist-get s :agent_name) host)) running)))
          (if (cdr named)
              (error "Goblins: ambiguous agent name %s; use its tree path" host)
            (car named))))))

(defun goblins-tramp--resolve (host)
  "Resolve HOST once to the pin (DIRECTORY INSTANCE SESSION)."
  (let* ((directory (goblins-tramp--state-directory))
         (state (goblins-tramp--sessions directory))
         (session (goblins-tramp--match host (cdr state))))
    (unless (and session (goblins-tramp--running-p session))
      (error "Goblins: no running goblin %s" host))
    (list directory (car state) (plist-get session :id))))

(defvar goblins-tramp--pins (make-hash-table :test #'equal)
  "Host to its pin (DIRECTORY INSTANCE SESSION).
Kept apart from TRAMP's connection properties, which TRAMP flushes when a
connection fails: the next use must fail again, not resolve the name anew.
Only `goblins-tramp-release' removes a pin.")

(defun goblins-tramp-pin (host)
  "HOST's pin, or nil before its first connection."
  (gethash host goblins-tramp--pins))

(defun goblins-tramp--pin (vec)
  "The pin of VEC's connection, resolving its host on first use."
  (let ((host (tramp-file-name-host vec)))
    (or (gethash host goblins-tramp--pins)
        (puthash host (goblins-tramp--resolve host) goblins-tramp--pins))))

(defun goblins-tramp--pin-field (n)
  "Field N of the current connection's pin, for `tramp-extra-expand-args'."
  (nth n (goblins-tramp--pin (car tramp-current-connection))))

(defun goblins-tramp--program ()
  "The local Goblins executable the login program runs."
  (or (executable-find goblins-executable) goblins-executable))

(defun goblins-tramp--completion-function (_method)
  "Running goblins for host completion after /goblins:."
  (ignore-errors
    (let ((sessions (cdr (goblins-tramp--sessions (goblins-tramp--state-directory)))))
      (mapcar (lambda (session) (list nil (goblins-tramp-host (plist-get session :path))))
              (cl-remove-if-not #'goblins-tramp--running-p sessions)))))

(defconst goblins-tramp-connection-local-variables
  '(;; Evaluated by `tramp-expand-args' for every connection and process.
    (tramp-extra-expand-args
     ?G (goblins-tramp--program)
     ?D (goblins-tramp--pin-field 0)
     ?I (goblins-tramp--pin-field 1)
     ?S (goblins-tramp--pin-field 2))
    ;; Each async process is its own `goblins exec', without the shell.
    (tramp-direct-async-process . t)
    ;; The sandbox's live PATH includes packages granted later.
    (tramp-remote-path . (tramp-own-remote-path tramp-default-remote-path)))
  "Connection-local variables for /goblins: connections.")

(add-to-list 'tramp-methods
             `(,goblins-tramp-method
               (tramp-login-program "%G")
               ;; A terminal only when TRAMP gives the client one: the shell
               ;; connection, not pipe processes such as eglot's.
               (tramp-login-args (("--state-dir" "%D") ("exec") ("--tty=auto")
                                  ("--instance" "%I") ("--session" "%S")
                                  ("%h") ("--") ("%l")))
               (tramp-direct-async ("/bin/sh" "-c"))
               (tramp-remote-shell "/bin/sh")
               (tramp-remote-shell-login ("-l"))
               (tramp-remote-shell-args ("-i" "-c"))
               (tramp-completion-use-cache nil)))

(tramp-set-completion-function
 goblins-tramp-method '((goblins-tramp--completion-function "goblins")))

(connection-local-set-profile-variables
 'goblins-tramp-connection-local-profile goblins-tramp-connection-local-variables)
(connection-local-set-profiles
 `(:application tramp :protocol ,goblins-tramp-method)
 'goblins-tramp-connection-local-profile)

;;; Lifetime

(defun goblins-tramp--vec (name)
  "The /goblins: connection of file NAME, or nil."
  (and (goblins-tramp-file-p name)
       (ignore-errors (tramp-dissect-file-name name))))

(defun goblins-tramp--buffer-vec (buffer)
  "The /goblins: connection BUFFER visits or runs in, or nil."
  (with-current-buffer buffer
    (goblins-tramp--vec (or buffer-file-name
                            (and (stringp default-directory) default-directory)))))

(defun goblins-tramp--terminal-p (buffer)
  "Whether BUFFER is a Goblins terminal; it outlives the connection."
  (with-current-buffer buffer
    (memq (alist-get 'kind (bound-and-true-p ghostel-identity)) '(goblins goblins-shell))))

(defun goblins-tramp-release (host &optional session)
  "Kill HOST's /goblins: buffers and processes, then release its pin.
With SESSION, only when the pin, if any, belongs to SESSION."
  (let* ((vec (make-tramp-file-name :method goblins-tramp-method :host host))
         (pin (goblins-tramp-pin host)))
    (when (or (null session) (null pin) (equal (nth 2 pin) session))
      (dolist (process (process-list))
        (when-let* ((v (process-get process 'tramp-vector)))
          (when (and (equal (tramp-file-name-method v) goblins-tramp-method)
                     (equal (tramp-file-name-host v) host))
            (delete-process process))))
      (dolist (buffer (buffer-list))
        (when-let* ((v (goblins-tramp--buffer-vec buffer)))
          (when (equal (tramp-file-name-host v) host)
            (if (goblins-tramp--terminal-p buffer)
                ;; A terminal keeps its output; it just leaves the sandbox.
                (with-current-buffer buffer
                  (setq default-directory (expand-file-name "~/")))
              (with-current-buffer buffer
                (set-buffer-modified-p nil))
              (let ((kill-buffer-query-functions nil))
                (kill-buffer buffer))))))
      ;; Only now can a later visit pin to whichever goblin has the name.
      (remhash host goblins-tramp--pins)
      (tramp-cleanup-connection vec t))))

(defun goblins-tramp--after-snapshot ()
  "Release the connections of goblins that stopped, from a status buffer.
Also those pinned to another instance of this daemon, which restarted."
  (when (and goblins--snapshot goblins--instance)
    (let ((directory goblins--directory)
          (sessions (append (plist-get goblins--snapshot :sessions) nil)))
      (maphash
       (lambda (host pin)
         (when (and (equal (car pin) directory)
                    (or (not (equal (nth 1 pin) goblins--instance))
                        (not (goblins-tramp--running-p
                              (cl-find (nth 2 pin) sessions
                                       :key (lambda (s) (plist-get s :id))
                                       :test #'equal)))))
           (goblins-tramp-release host (nth 2 pin))))
       (copy-hash-table goblins-tramp--pins))
      ;; Buffers that never connected still name a stopped goblin.
      (let ((hosts (make-hash-table :test #'equal)))
        (dolist (buffer (buffer-list))
          (when-let* ((v (goblins-tramp--buffer-vec buffer)))
            (puthash (tramp-file-name-host v) t hosts)))
        (maphash
         (lambda (host _)
           (let ((session (ignore-errors (goblins-tramp--match host sessions))))
             (when (and session (not (goblins-tramp--running-p session))
                        (not (goblins-tramp-pin host)))
               (goblins-tramp-release host))))
         hosts)))))

(add-hook 'goblins-snapshot-hook #'goblins-tramp--after-snapshot)

;; Never persist connection data: a name may mean another goblin next time.
(defun goblins-tramp--forget-persistent (&rest _)
  (when (hash-table-p tramp-cache-data)
    (maphash (lambda (key _)
               (when (and (tramp-file-name-p key)
                          (equal (tramp-file-name-method key) goblins-tramp-method))
                 (remhash key tramp-cache-data)))
             tramp-cache-data)))
(advice-add 'tramp-dump-connection-properties :before #'goblins-tramp--forget-persistent)

;; A restored buffer would pin to whichever goblin has the name by then.
(defun goblins-tramp--desktop-save-p (filename &rest _)
  (not (goblins-tramp-file-p filename)))
(with-eval-after-load 'desktop
  (advice-add 'desktop-save-buffer-p :before-while #'goblins-tramp--desktop-save-p))

;;; Files a goblin wrote

(defun goblins-tramp--hack-local-variables (function &rest args)
  "Apply only safe local variables, and no `eval:', in /goblins: buffers.
This runs before the file's local variables are processed, also for files
emacsclient visits.  The major mode is still chosen from the file name, and
the user's own mode hooks still run."
  (if (goblins-tramp-file-p (or buffer-file-name
                                (and (stringp default-directory) default-directory)))
      (let ((enable-local-variables (and enable-local-variables :safe))
            (enable-local-eval nil))
        (apply function args))
    (apply function args)))
(advice-add 'hack-local-variables :around #'goblins-tramp--hack-local-variables)

;;; Shells

(defun goblins-tramp--choose-session (source prompt)
  "Choose a running session of SOURCE's snapshot with PROMPT."
  (with-current-buffer source
    (goblins--choose prompt
                     (mapcar (lambda (s) (cons (plist-get s :path) s))
                             (cl-remove-if-not
                              #'goblins-tramp--running-p
                              (append (plist-get goblins--snapshot :sessions) nil)))
                     "No running goblins")))

(defun goblins-tramp--current-session (source)
  "The session of the current goblin terminal or /goblins: buffer, if any."
  (let ((sessions (append (plist-get (buffer-local-value 'goblins--snapshot source)
                                     :sessions)
                          nil))
        (id (alist-get 'session (bound-and-true-p ghostel-identity)))
        (vec (goblins-tramp--vec default-directory)))
    (cl-find-if #'goblins-tramp--running-p
                (list (and id (cl-find id sessions :key (lambda (s) (plist-get s :id))
                                       :test #'equal))
                      (and vec (ignore-errors
                                 (goblins-tramp--match (tramp-file-name-host vec) sessions)))))))

;;;###autoload
(defun goblins-shell (&optional session display)
  "Open a shell inside a running goblin, in a new Ghostel buffer.
It runs the sandbox's own $SHELL with its dev shell environment, alongside
the agent's terminal.  SESSION is a session record; interactively, the
goblin of the current terminal or /goblins: buffer, else (or with a prefix
argument) one you choose.  DISPLAY, default `pop-to-buffer', shows the
buffer."
  (interactive)
  (goblins--require-ghostel)
  (let* ((source (goblins--connect))
         (session (or session
                      (and (not current-prefix-arg)
                           (goblins-tramp--current-session source))
                      (goblins-tramp--choose-session source "Shell in goblin: ")))
         (directory (buffer-local-value 'goblins--directory source))
         (instance (buffer-local-value 'goblins--instance source))
         (id (plist-get session :id))
         (host (goblins-tramp-host (plist-get session :path)))
         (buffer (generate-new-buffer (format "*Goblin shell: %s*" (plist-get session :path)))))
    (condition-case err
        (progn
          ;; Spawn from a local directory: Ghostel would otherwise run the
          ;; client itself through TRAMP.
          (with-current-buffer buffer (setq default-directory (expand-file-name "~/")))
          (funcall (or display #'pop-to-buffer) buffer)
          (let ((ghostel-kill-buffer-on-exit nil))
            (ghostel-exec buffer goblins-executable
                          (list "--state-dir" directory "exec" "-t" "--instance" instance
                                "--session" id host)
                          `((kind . goblins-shell) (session . ,id) (instance . ,instance)
                            (host . ,host))))
          (with-current-buffer buffer
            (setq-local goblins--directory directory)
            (setq default-directory (format "/%s:%s:/" goblins-tramp-method host)))
          buffer)
      (error
       (when (buffer-live-p buffer) (kill-buffer buffer))
       (signal (car err) (cdr err))))))

(provide 'goblins-tramp)
;;; goblins-tramp.el ends here
