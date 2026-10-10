;;; goblins-tramp.el --- /goblins: TRAMP method and shells for running goblins -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "29.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; Files and processes inside running goblin sandboxes:
;;
;;   C-x C-f /goblins:gribble:/tmp/diagram.svg
;;   C-x C-f /goblins:gribble.snaga:/workspace/      (a child's tree path)
;;   M-x goblins-shell     (a Ghostel terminal; also goblins-eshell, goblins-dired)
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
(declare-function goblins--sidebar-render-all "goblins-workspace" ())
(defvar server-window)
(declare-function eshell-mode "esh-mode" ())
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

(defun goblins-tramp--pin (vec)
  "The pin of VEC's connection, resolving its host on first use.
Pins live in `goblins--pins', apart from TRAMP's connection properties,
which TRAMP flushes when a connection fails: the next use must fail again,
not resolve the name anew."
  (let ((host (tramp-file-name-host vec)))
    (or (goblins-pin host)
        (puthash host (goblins-tramp--resolve host) goblins--pins))))

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

(defvar goblins-tramp--releasing nil
  "Non-nil while `goblins-tramp-release' kills a host's buffers.")

(defun goblins-tramp-release (host &optional session)
  "Kill HOST's /goblins: buffers and processes, then release its pin.
With SESSION, only when the pin, if any, belongs to SESSION.  Pins of
goblins.el's daemon are released when their goblin stops; use this command
for a pin of another daemon whose goblin is gone."
  (interactive (list (completing-read "Release /goblins: host: "
                                      (hash-table-keys goblins--pins) nil t)))
  (let* ((vec (make-tramp-file-name :method goblins-tramp-method :host host))
         (pin (goblins-pin host))
         (goblins-tramp--releasing t))
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
      (remhash host goblins--pins)
      (tramp-cleanup-connection vec t))))

(defun goblins-tramp--after-snapshot ()
  "Release unpinned /goblins: buffers that name a stopped goblin.
Pinned hosts are released by `goblins--release-stopped-pins'.  A buffer that
never connected has no pin, but its host still names a stopped goblin by tree
path, name or ID: release it unless a running goblin now has that name."
  (when goblins--snapshot
    (let ((sessions (append (plist-get goblins--snapshot :sessions) nil))
          (hosts (make-hash-table :test #'equal)))
      (dolist (buffer (buffer-list))
        (when-let* ((v (goblins-tramp--buffer-vec buffer)))
          (puthash (tramp-file-name-host v) t hosts)))
      (maphash
       (lambda (host _)
         (let* ((path (string-replace "." "/" host))
                (named (cl-remove-if-not
                        (lambda (s) (member host (list (plist-get s :id) (plist-get s :agent_name)
                                                       (and (equal (plist-get s :path) path) host))))
                        sessions)))
           (when (and named (not (goblins-pin host))
                      (not (cl-some #'goblins-tramp--running-p named)))
             (goblins-tramp-release host))))
       hosts))))

(add-hook 'goblins-snapshot-hook #'goblins-tramp--after-snapshot)

;; TRAMP keeps a shell open in the goblin for each connection: close it with
;; the last buffer that works there, so the goblin shows no idle exec.
(defun goblins-tramp--host-in-use-p (host &optional except)
  "Whether a buffer other than EXCEPT still works in /goblins:HOST: through TRAMP.
Hidden buffers, TRAMP's own connection and debug buffers, and goblin
terminals, which reach the goblin through their own exec, do not count."
  (cl-some (lambda (buffer)
             (unless (eq buffer except)
               (when-let* ((v (goblins-tramp--buffer-vec buffer)))
                 (and (equal (tramp-file-name-host v) host)
                      (not (string-prefix-p " " (buffer-name buffer)))
                      (not (member (buffer-name buffer)
                                   (list (tramp-buffer-name v) (tramp-debug-buffer-name v))))
                      (not (goblins-tramp--terminal-p buffer))))))
           (buffer-list)))

(defun goblins-tramp--close-unused (host &optional except)
  "Close the TRAMP connection to HOST unless a buffer other than EXCEPT
still works there, or TRAMP is using it right now.  The pin stays: the
goblin still runs, and the next use reconnects to it."
  (let* ((vec (make-tramp-file-name :method goblins-tramp-method :host host))
         (process (tramp-get-connection-process vec)))
    (when (and (process-live-p process)
               (not (tramp-get-connection-property process "locked"))
               (not (goblins-tramp--host-in-use-p host except)))
      (tramp-cleanup-connection vec 'keep-debug nil 'keep-processes))))

(defun goblins-tramp--buffer-killed ()
  "Close this buffer's /goblins: connection if no other buffer uses it.
At once: a deferred close could hit a connection TRAMP is opening again."
  (unless (or goblins-tramp--releasing
              (string-prefix-p " " (buffer-name))
              (goblins-tramp--terminal-p (current-buffer)))
    (when-let* ((vec (goblins-tramp--buffer-vec (current-buffer))))
      (with-demoted-errors "Goblins: closing a connection: %S"
        (goblins-tramp--close-unused (tramp-file-name-host vec) (current-buffer))))))
(add-hook 'kill-buffer-hook #'goblins-tramp--buffer-killed)

;; Keep VC out of goblins.  VC runs git asynchronously (diff-hl and dirvish
;; ask for it in Dired), and the process sentinel then runs git synchronously
;; while TRAMP waits for another reply on the same shell connection: TRAMP
;; refuses the reentrant call and drops the connection.  Magit doesn't use VC.
(defconst goblins-tramp-vc-ignore-regexp (concat "\\`/" goblins-tramp-method ":")
  "Added to `vc-ignore-dir-regexp' so that VC never looks inside a goblin.")

(unless (string-search goblins-tramp-vc-ignore-regexp vc-ignore-dir-regexp)
  (setq vc-ignore-dir-regexp
        (concat vc-ignore-dir-regexp "\\|" goblins-tramp-vc-ignore-regexp)))

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
(defun goblins-tramp--desktop-save-p (filename bufname &rest _)
  "Refuse /goblins: buffers, also Dired and vc-dir ones that visit no file."
  (not (or (goblins-tramp-file-p filename)
           (when-let* ((buffer (get-buffer bufname)))
             (goblins-tramp-file-p (buffer-local-value 'default-directory buffer))))))
(with-eval-after-load 'desktop
  (advice-add 'desktop-save-buffer-p :before-while #'goblins-tramp--desktop-save-p))

;;; Commands Emacs waits for

;; A process sentinel or timer that runs `process-file' (vc, magit,
;; `shell-command'...) while TRAMP waits for a reply on its shell connection
;; would be refused as a reentrant call, and the connection dropped.  Such a
;; command runs as its own `goblins exec' instead, like asynchronous
;; processes.  Otherwise TRAMP's connection stays: an exec costs about 15 ms
;; per command, the connection about 2 ms, and magit runs dozens.

(defun goblins-tramp--same-host-p (vec name)
  "Whether NAME is a /goblins: file name on VEC's host."
  (when-let* ((v (goblins-tramp--vec name)))
    (equal (tramp-file-name-host v) (tramp-file-name-host vec))))

(defun goblins-tramp--process-environment (vec)
  "The environment `process-file' passes into the goblin, as TRAMP does:
what differs from Emacs's own environment, without PATH, plus TERM and PAGER."
  (let ((ours (seq-remove #'tramp-local-environment-variable-p process-environment))
        env)
    (dolist (elt (append ours
                         (list (concat "INSIDE_EMACS=" (tramp-inside-emacs))
                               "TERM=dumb" "PAGER=cat")
                         (and tramp-propagate-emacsclient-tramp
                              (list (concat "EMACSCLIENT_TRAMP="
                                            (tramp-make-tramp-file-name vec 'noloc))))))
      ;; The first setting of a variable wins, as in `process-environment'.
      (when-let* ((name (and (string-search "=" elt)
                             (substring elt 0 (string-search "=" elt)))))
        (unless (or (equal name "PATH") (length> elt 256) (assoc name env))
          (push (cons name elt) env))))
    (nreverse (mapcar #'cdr env))))

(defun goblins-tramp--process-file (function program &optional infile destination display
                                             &rest args)
  "Run `process-file' in a /goblins: directory as its own `goblins exec'.
FUNCTION is TRAMP's own handler, used for an INFILE or standard error file
on another host."
  (let* ((vec (goblins-tramp--vec default-directory))
         (stderr (and (consp destination) (stringp (cadr destination)) (cadr destination)))
         (remote-in (and infile (file-remote-p infile)))
         (remote-err (and stderr (file-remote-p stderr))))
    (if (or (null vec)
            (and remote-in (not (goblins-tramp--same-host-p vec infile)))
            (and remote-err (not (goblins-tramp--same-host-p vec stderr))))
        (apply function program infile destination display args)
      (let* ((pin (goblins-tramp--pin vec))
             (command
              (if (or remote-in remote-err)
                  ;; Files inside the goblin are opened there.
                  (append (list "/bin/sh" "-c"
                                (concat "in=$1 err=$2; shift 2; "
                                        "if [ -n \"$in\" ]; then exec <\"$in\"; fi; "
                                        "if [ -n \"$err\" ]; then exec 2>\"$err\"; fi; "
                                        "exec \"$@\"")
                                "sh"
                                (if remote-in (tramp-file-local-name (expand-file-name infile)) "")
                                (if remote-err (tramp-file-local-name (expand-file-name stderr)) "")
                                program)
                          args)
                (cons program args)))
             (arguments
              (append (list "--state-dir" (nth 0 pin) "exec" "--instance" (nth 1 pin)
                            "--session" (nth 2 pin)
                            "--workdir" (tramp-file-local-name
                                         (expand-file-name default-directory)))
                      (mapcan (lambda (elt) (list "--env" elt))
                              (goblins-tramp--process-environment vec))
                      (list (tramp-file-name-host vec) "--")
                      command))
             (destination (if remote-err (list (car destination) nil) destination))
             (infile (and infile (not remote-in) (expand-file-name infile))))
        (let ((default-directory temporary-file-directory))
          (apply #'call-process (goblins-tramp--program) infile destination display
                 arguments))))))

(defun goblins-tramp--busy-p (vec)
  "Whether TRAMP is waiting for a reply on VEC's connection right now."
  (when-let* ((process (tramp-get-connection-process vec)))
    (tramp-get-connection-property process "locked")))

(defun goblins-tramp--sh-process-file (function program &rest rest)
  "Around `tramp-sh-handle-process-file': run /goblins: commands as an exec
while TRAMP's connection is busy, instead of failing as a reentrant call."
  (let ((vec (goblins-tramp--vec default-directory)))
    (if (and vec (goblins-tramp--busy-p vec))
        (apply #'goblins-tramp--process-file function program rest)
      (apply function program rest))))
(advice-add 'tramp-sh-handle-process-file :around #'goblins-tramp--sh-process-file)

;;; Files the goblin's editor opens

(defun goblins-tramp--editor-goblin (buffer)
  "(SOURCE . SESSION-ID) of the goblin whose /goblins: file BUFFER visits.
From its host's pin, when Emacs has a status buffer for the pin's daemon."
  (when-let* ((vec (goblins-tramp--buffer-vec buffer))
              (pin (goblins-pin (tramp-file-name-host vec)))
              (source (get-buffer (format "*Goblins: %s*" (car pin)))))
    (cons source (nth 2 pin))))

(defun goblins-tramp--server-switch-buffer (function &optional next-buffer &rest args)
  "Around `server-switch-buffer': in the workspace, files from a goblin's
editor (emacsclient) open right of its agent and are remembered for it."
  (let ((goblin (and (goblins--in-workspace-p) (buffer-live-p next-buffer)
                     (goblins-tramp--editor-goblin next-buffer))))
    (if (not goblin)
        (apply function next-buffer args)
      (let ((server-window (lambda (buffer)
                             (goblins-select-goblin (car goblin) (cdr goblin) buffer))))
        (apply function next-buffer args)))))
(with-eval-after-load 'server
  (advice-add 'server-switch-buffer :around #'goblins-tramp--server-switch-buffer))

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

;;; Shells, Eshell and Dired

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
  "The session of the current goblin terminal or /goblins: buffer, if any.
A /goblins: buffer's goblin is its host's pin, when SOURCE's daemon has it."
  (let* ((sessions (append (plist-get (buffer-local-value 'goblins--snapshot source)
                                      :sessions)
                           nil))
         (find (lambda (id) (and id (cl-find id sessions :key (lambda (s) (plist-get s :id))
                                             :test #'equal))))
         (id (alist-get 'session (bound-and-true-p ghostel-identity)))
         (vec (goblins-tramp--vec default-directory))
         (pin (and vec (goblins-pin (tramp-file-name-host vec)))))
    (cl-find-if #'goblins-tramp--running-p
                (list (funcall find id)
                      (and pin (equal (car pin) (buffer-local-value 'goblins--directory source))
                           (funcall find (nth 2 pin)))
                      (and vec (not pin)
                           (ignore-errors
                             (goblins-tramp--match (tramp-file-name-host vec) sessions)))))))

(defun goblins-tramp--goblin (source session prompt)
  "Return (SOURCE . SESSION), reading what is missing, for a running goblin.
Interactively the goblin is that of the current goblin terminal or /goblins:
buffer, else (or with a prefix argument) one chosen with PROMPT."
  (let* ((source (or source
                     (goblins--connect (and (local-variable-p 'goblins--directory)
                                            goblins--directory))))
         (session (or session
                      (and (not current-prefix-arg) (goblins-tramp--current-session source))
                      (goblins-tramp--choose-session source prompt))))
    (unless (goblins-tramp--running-p session)
      (user-error "%s is not running" (goblins--session-path session)))
    (cons source session)))

(defun goblins-tramp--session-host (source session)
  "The /goblins: host of SOURCE's SESSION, pinned to it now.
Its tree path, or its ID when that path is pinned to another goblin.  Pinned
before any TRAMP use, so it never reaches a later goblin with the same name."
  (goblins--pin-or-id (goblins-tramp-host (plist-get session :path))
                      (buffer-local-value 'goblins--directory source)
                      (buffer-local-value 'goblins--instance source)
                      (plist-get session :id)))

(defun goblins-tramp--session-directory (source session)
  "SESSION's working directory as a pinned /goblins: directory name."
  (let ((cwd (plist-get session :cwd)))
    (format "/%s:%s:%s" goblins-tramp-method (goblins-tramp--session-host source session)
            (file-name-as-directory
             (if (and (stringp cwd) (not (string-empty-p cwd))) cwd "/workspace")))))

(defcustom goblins-shell-program nil
  "The program `goblins-shell' runs inside the goblin.
nil runs the sandbox's own $SHELL.  A string names a program, such as
\"fish\", looked up on the sandbox's PATH; a list is a program and its
arguments.  The program must be available inside the sandbox."
  :type '(choice (const :tag "The sandbox's $SHELL" nil)
                 (string :tag "Program")
                 (repeat :tag "Program and arguments" string))
  :group 'goblins)

(defun goblins-tramp--shell-number (directory id)
  "The lowest number no open shell of goblin ID of DIRECTORY's daemon has."
  (let ((taken (delq nil (mapcar (lambda (buffer)
                                   (with-current-buffer buffer
                                     (and goblins--shell-number
                                          (equal goblins--directory directory)
                                          (equal (alist-get 'session (bound-and-true-p ghostel-identity)) id)
                                          goblins--shell-number)))
                                 (buffer-list))))
        (n 1))
    (while (memq n taken) (setq n (1+ n)))
    n))

(defun goblins-tramp--shells-changed ()
  "Redraw the sidebars, which list each goblin's shells."
  (when (fboundp 'goblins--sidebar-render-all)
    (goblins--sidebar-render-all)))

(defun goblins-tramp--shell-closed ()
  "Take this shell out of the sidebar as it is killed."
  (setq goblins--shell-number nil)
  (goblins-tramp--shells-changed))

(defun goblins-tramp--shell-exited (&rest _)
  (goblins-tramp--shells-changed))

;;;###autoload
(defun goblins-shell (&optional session display source)
  "Open a shell inside a running goblin, in a new Ghostel buffer.
It runs `goblins-shell-program', by default the sandbox's own $SHELL, with
its dev shell environment, alongside the agent's terminal, and the sidebar
lists it under its goblin.  SESSION is
a session record of SOURCE's daemon, by default that of
`goblins-state-directory'; interactively, the goblin of the current terminal
or /goblins: buffer, else (or with a prefix argument) one you choose.
DISPLAY shows the buffer; by default `goblins-select-goblin' shows it
right of the goblin's agent and remembers it for the goblin."
  (interactive)
  (goblins--require-ghostel)
  (let* ((goblin (goblins-tramp--goblin source session "Shell in goblin: "))
         (source (car goblin))
         (session (cdr goblin))
         (directory (buffer-local-value 'goblins--directory source))
         (instance (buffer-local-value 'goblins--instance source))
         (id (plist-get session :id))
         (host (goblins-tramp--session-host source session))
         (number (goblins-tramp--shell-number directory id))
         (buffer (generate-new-buffer (format "*Goblin shell: %s*" (plist-get session :path)))))
    (condition-case err
        (progn
          ;; Spawn from a local directory: Ghostel would otherwise run the
          ;; client itself through TRAMP.
          (with-current-buffer buffer (setq default-directory (expand-file-name "~/")))
          (funcall (or display (goblins-tramp--show-for source id)) buffer)
          (let ((ghostel-kill-buffer-on-exit nil))
            (ghostel-exec buffer goblins-executable
                          (append (list "--state-dir" directory "exec" "-t" "--instance" instance
                                        "--session" id host)
                                  (and goblins-shell-program
                                       (cons "--" (ensure-list goblins-shell-program))))
                          `((kind . goblins-shell) (session . ,id) (instance . ,instance)
                            (directory . ,directory) (host . ,host))))
          (with-current-buffer buffer
            (setq-local goblins--directory directory)
            (setq-local goblins--shell-number number)
            (add-hook 'kill-buffer-hook #'goblins-tramp--shell-closed nil t)
            (add-hook 'ghostel-exit-functions #'goblins-tramp--shell-exited nil t)
            (setq default-directory (format "/%s:%s:/" goblins-tramp-method host)))
          (goblins-tramp--shells-changed)
          buffer)
      (error
       (when (buffer-live-p buffer) (kill-buffer buffer))
       (signal (car err) (cdr err))))))

(defvar-local goblins-tramp--eshell-goblin nil
  "(DIRECTORY SESSION-ID) of the goblin this Eshell buffer was opened in.")

;;;###autoload
(defun goblins-eshell (&optional session display source)
  "Open Eshell inside a running goblin, in its working directory.
External commands run in the sandbox over /goblins: TRAMP.  The goblin's
Eshell buffer is reused; it is killed when the goblin stops.  SESSION,
DISPLAY and SOURCE are as for `goblins-shell'."
  (interactive)
  (require 'eshell)
  (let* ((goblin (goblins-tramp--goblin source session "Eshell in goblin: "))
         (source (car goblin))
         (session (cdr goblin))
         (key (list (buffer-local-value 'goblins--directory source) (plist-get session :id)))
         (directory (goblins-tramp--session-directory source session))
         (buffer (or (cl-find key (buffer-list) :test #'equal
                              :key (lambda (b) (buffer-local-value 'goblins-tramp--eshell-goblin b)))
                     (let ((buffer (generate-new-buffer
                                    (format "*Goblin eshell: %s*" (plist-get session :path)))))
                       (condition-case err
                           (with-current-buffer buffer
                             (setq default-directory directory)
                             (eshell-mode)
                             (setq goblins-tramp--eshell-goblin key)
                             buffer)
                         (error (kill-buffer buffer)
                                (signal (car err) (cdr err))))))))
    (funcall (or display (goblins-tramp--show-for source (plist-get session :id))) buffer)
    buffer))

;;;###autoload
(defun goblins-dired (&optional session display source)
  "Open Dired inside a running goblin, in its working directory, over TRAMP.
SESSION, DISPLAY and SOURCE are as for `goblins-shell'."
  (interactive)
  (let* ((goblin (goblins-tramp--goblin source session "Dired in goblin: "))
         (buffer (dired-noselect (goblins-tramp--session-directory (car goblin) (cdr goblin)))))
    (funcall (or display (goblins-tramp--show-for (car goblin) (plist-get (cdr goblin) :id)))
             buffer)
    buffer))

(defun goblins-tramp--show-for (source id)
  "A function that shows a buffer for goblin ID of SOURCE's daemon.
It goes right of the goblin's agent, and is remembered for the goblin."
  (lambda (buffer) (goblins-select-goblin source id buffer)))

(provide 'goblins-tramp)
;;; goblins-tramp.el ends here
