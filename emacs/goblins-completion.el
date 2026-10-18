;;; goblins-completion.el --- Goblins in Consult, Embark and Marginalia -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; Commands on a goblin you choose, and what Consult, Embark and Marginalia
;; make of goblins.  None of those packages is required: each part takes
;; effect once its package loads.
;;
;; - Commands such as `goblins-goblin-info' and `goblins-goblin-shell' act on
;;   a goblin chosen in the `goblin' completion category.
;; - Embark: goblin candidates, and goblins' agent terminals among buffers
;;   (type `goblins-terminal'), get those commands as actions.
;; - Marginalia: agent terminals among buffers are described like goblins.
;; - Consult: `consult-buffer' lists agent terminals under the narrowing key g,
;;   and `goblins-switch' lists terminals and other goblins apart.
;; - `goblins-requests' chooses among pending permission requests, which
;;   Embark can accept, deny or follow to their goblin.
;; - `goblins-resume' chooses among recorded conversations (category
;;   `goblins-record'), which Marginalia describes and Embark resumes.

;;; Code:

(require 'cl-lib)
(require 'subr-x)
(require 'goblins)
(require 'goblins-views)
(require 'goblins-raids)
(require 'goblins-messages)

(declare-function goblins-shell "goblins-tramp" (&optional session display source))
(declare-function goblins-eshell "goblins-tramp" (&optional session display source))
(declare-function goblins-dired "goblins-tramp" (&optional session display source))
(declare-function goblins-find-file "goblins-tramp" (&optional session source))
(declare-function goblins-show-in-workspace "goblins-workspace" (source id))
(declare-function marginalia-annotate-buffer "marginalia" (cand))
(declare-function consult--buffer-state "consult" ())
(declare-function consult-buffer "consult" (&optional sources))
(defvar embark-general-map)
(defvar embark-buffer-map)
(defvar embark-keymap-alist)
(defvar embark-transformer-alist)
(defvar embark-exporters-alist)
(defvar marginalia-annotators)
(defvar marginalia-annotator-registry)
(defvar marginalia-separator)
(defvar consult-buffer-sources)

;;;; Agent terminals

(defun goblins--agent-terminal (buffer-or-name)
  "BUFFER-OR-NAME's buffer if it is a goblin's agent terminal, else nil."
  (when-let* ((buffer (and buffer-or-name (get-buffer buffer-or-name))))
    (and (buffer-live-p buffer)
         (buffer-local-value 'goblins--session-key buffer)
         buffer)))

(defun goblins--agent-terminals (&optional directory)
  "The live agent terminals, most recently used first; of DIRECTORY's server
only, if given."
  (cl-remove-if-not (lambda (buffer)
                      (when-let* ((key (buffer-local-value 'goblins--session-key buffer)))
                        (or (null directory) (equal (car key) directory))))
                    (buffer-list)))

(defun goblins--agent-terminal-goblin (buffer-or-name)
  "(SOURCE . SESSION-ID) of the goblin whose agent terminal is BUFFER-OR-NAME."
  (when-let* ((buffer (goblins--agent-terminal buffer-or-name))
              (key (buffer-local-value 'goblins--session-key buffer)))
    (cons (goblins--status-buffer (car key)) (nth 2 key))))

(defun goblins--terminal-description (buffer-or-name)
  "Describe the goblin of the agent terminal BUFFER-OR-NAME, or nil."
  (when-let* ((goblin (goblins--agent-terminal-goblin buffer-or-name))
              (session (goblins--snapshot-session
                        (buffer-local-value 'goblins--snapshot (car goblin)) (cdr goblin))))
    (goblins--goblin-description (car goblin) session)))

;;;; Commands on a goblin you choose

(defun goblins--choose-goblin (prompt)
  "Read a running goblin of the server at hand with PROMPT; return
\(SOURCE SESSION-ID), for an `interactive' form."
  (let ((goblin (goblins--read-goblin prompt (goblins--default-source))))
    (list (car goblin) (cdr goblin))))

(defun goblins--chosen-session (source id)
  (or (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source) id)
      (user-error "The daemon no longer retains this goblin")))

(defun goblins-goblin-show (source id)
  "Show the terminal of goblin ID of SOURCE's server, attaching if necessary.
In the workspace its saved windows come back beside it."
  (interactive (goblins--choose-goblin "Show goblin: "))
  (goblins-open-session source id))

(defun goblins-goblin-permissions (source id)
  "Show the permission requests of goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Permissions of goblin: "))
  (goblins--display-main (goblins-permissions-buffer source id)))

(defun goblins-goblin-info (source id)
  "Show what the daemon knows about goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Info on goblin: "))
  (goblins--display-main (goblins-info-buffer source id)))

(defun goblins-goblin-shell (source id)
  "Open a shell terminal inside goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Shell in goblin: "))
  (goblins-shell (goblins--chosen-session source id) nil source))

(defun goblins-goblin-eshell (source id)
  "Open Eshell inside goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Eshell in goblin: "))
  (goblins-eshell (goblins--chosen-session source id) nil source))

(defun goblins-goblin-dired (source id)
  "Open Dired inside goblin ID of SOURCE's server, in its working directory."
  (interactive (goblins--choose-goblin "Dired in goblin: "))
  (goblins-dired (goblins--chosen-session source id) nil source))

(defun goblins-goblin-find-file (source id)
  "Visit a file inside goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Find file in goblin: "))
  (goblins-find-file (goblins--chosen-session source id) source))

(defun goblins-goblin-raid (source id)
  "Create a raid around goblin ID of SOURCE's server, or add it to one."
  (interactive (goblins--choose-goblin "Raid around goblin: "))
  (goblins--raid-around source (goblins--chosen-session source id)))

(defun goblins-goblin-invite (source id)
  "Invite goblin ID of SOURCE's server into an existing raid you choose."
  (interactive (goblins--choose-goblin "Invite goblin: "))
  (goblins--invite-to-chosen-raid source id))

(defun goblins-goblin-stop (source id)
  "Stop goblin ID of SOURCE's server after confirming."
  (interactive (goblins--choose-goblin "Stop goblin: "))
  (goblins-stop-goblin source id))

(defun goblins-goblin-kill-execs (source id)
  "Kill the shells and commands inside goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Kill shells and commands in goblin: "))
  (goblins-kill-goblin-execs source id))

;;;; Switching

(defcustom goblins-switch-use-consult t
  "Whether `goblins-switch' uses Consult, when it is installed.
It then lists the agent terminals and the goblins without one apart, with
the narrowing keys t and a."
  :type 'boolean :group 'goblins)

(defun goblins--switch-sources (source)
  "Consult sources for `goblins-switch' on SOURCE's server: its agent
terminals, then its running goblins without one."
  (let ((directory (buffer-local-value 'goblins--directory source)))
    (list
     (list :name "Terminal" :narrow ?t :category 'buffer
           :face 'consult-buffer :history 'buffer-name-history
           :annotate #'goblins--terminal-description
           :action (lambda (buffer)
                     (let ((goblin (goblins--agent-terminal-goblin buffer)))
                       (goblins-open-session (car goblin) (cdr goblin))))
           :items (lambda () (mapcar (lambda (b) (cons (buffer-name b) b))
                                     (goblins--agent-terminals directory))))
     (list :name "Not attached" :narrow ?a :category 'goblin
           :annotate (goblins--goblin-annotation source)
           :action (lambda (candidate)
                     (goblins-open-session
                      source (cdr (get-text-property 0 'goblins-goblin candidate))))
           :items (lambda ()
                    (mapcar (lambda (session) (goblins--goblin-candidate source session))
                            (cl-remove-if
                             (lambda (session)
                               (with-current-buffer source
                                 (goblins--terminal-buffer (plist-get session :id))))
                             (goblins--running-sessions
                              (buffer-local-value 'goblins--snapshot source)))))))))

;;;###autoload
(defun goblins-switch (&optional directory)
  "Choose a goblin and show its terminal, attaching if necessary.
With Consult (see `goblins-switch-use-consult') choose among the agent
terminals and the running goblins without one; else among the running
goblins.  In the workspace the goblin's saved windows come back beside it.
With a prefix argument, prompt for the daemon state DIRECTORY."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "))))
  (let ((source (goblins--connect directory)))
    (if (and goblins-switch-use-consult (require 'consult nil t))
        (consult-buffer (goblins--switch-sources source))
      (let ((goblin (goblins--read-goblin "Goblin: " source)))
        (goblins-open-session (car goblin) (cdr goblin))))))

;;;; Permission requests

(defun goblins--choose-request (prompt)
  "Read a pending request of the server at hand with PROMPT; return
\(SOURCE RECORD), for an `interactive' form."
  (let ((source (goblins--default-source)))
    (list source (goblins--read-request prompt source))))

;;;###autoload
(defun goblins-requests (source record)
  "Choose a pending permission request and show its goblin's requests, it
focused.  RECORD is the request of SOURCE's server.  With Embark, act on it
instead: accept, deny or show its goblin."
  (interactive (goblins--choose-request "Request: "))
  (let ((buffer (goblins-permissions-buffer source (plist-get record :session))))
    (goblins--display-main buffer)
    (with-current-buffer buffer
      (when-let* ((section (goblins--find-request-section (plist-get record :id))))
        (goblins--view-goto (oref section start))))))

(defun goblins-request-accept (source record)
  "Accept a pending permission request you choose: RECORD of SOURCE's server."
  (interactive (goblins--choose-request "Accept request: "))
  (with-current-buffer source (goblins--decide-record record t)))

(defun goblins-request-deny (source record)
  "Deny a pending permission request you choose: RECORD of SOURCE's server."
  (interactive (goblins--choose-request "Deny request: "))
  (with-current-buffer source (goblins--decide-record record nil)))

(defun goblins-request-goblin (source record)
  "Show the goblin asking for a pending request you choose: RECORD of
SOURCE's server."
  (interactive (goblins--choose-request "Goblin of request: "))
  (goblins-open-session source (plist-get record :session)))

;;;; Resuming

(defvar goblins-resume-history nil
  "Minibuffer history of `goblins-resume'.")

(defvar goblins--resume-records nil
  "While a record to resume is read: (CANDIDATE . RECORD) for each candidate.")

(defvar-local goblins--resuming nil
  "Goblins resumed from this status buffer whose start is not reported yet.")

(defun goblins--history-records (source)
  "The session history of SOURCE's server: its records, newest first."
  (append (plist-get (goblins--host-call source 'history.list (make-hash-table))
                     :records)
          nil))

(defun goblins--record-label (record)
  "RECORD's agent name and its conversation's name, if the agent named it."
  (let ((name (plist-get (plist-get record :variables) :session_name)))
    (goblins--safe (if (and (stringp name) (not (string-empty-p name)))
                       (format "%s: %s" (plist-get record :agent_name) name)
                     (plist-get record :agent_name)))))

(defun goblins--record-candidates (records)
  "RECORDS as completion candidates like \"nikka: Fix the tests\", their
record ID added as \"<k3f9x2ab>\" when they would read alike.  Each carries
its record as the `goblins-record' property."
  (let ((counts (make-hash-table :test #'equal)))
    (dolist (record records)
      (cl-incf (gethash (goblins--record-label record) counts 0)))
    (mapcar (lambda (record)
              (let ((label (goblins--record-label record)))
                (propertize (if (> (gethash label counts) 1)
                                (format "%s <%s>" label (plist-get record :id))
                              label)
                            'goblins-record record)))
            records)))

(defun goblins--record-of (candidate)
  "The record of CANDIDATE, a candidate to resume, or nil."
  (and (stringp candidate)
       (or (get-text-property 0 'goblins-record candidate)
           (cdr (assoc candidate goblins--resume-records)))))

(defun goblins--record-by-id (records id)
  "The record of RECORDS that ID names, as `goblins resume' takes it: its
record ID, a goblin it ran as, or its agent's conversation ID."
  (and (stringp id) (not (string-empty-p id))
       (cl-find-if (lambda (record)
                     (or (equal (plist-get record :id) id)
                         (member id (append (plist-get record :sessions) nil))
                         (equal (plist-get (plist-get record :variables) :session_id) id)))
                   records)))

(defun goblins--record-fields (record)
  "What sets RECORD apart besides its label: its ID, age, configuration,
directory (as a warning when it is gone), Docker, the packages granted to it
and those skipped when it was last resumed."
  (let* ((cwd (plist-get record :cwd))
         (packages (append (plist-get (plist-get record :packages) :approved) nil))
         (skipped (mapcar (lambda (s) (plist-get s :name))
                          (append (plist-get record :skipped) nil))))
    (delq nil
          (list (propertize (goblins--safe (plist-get record :id)) 'face 'goblins-key)
                (format "%4s" (or (goblins--sidebar-duration (plist-get record :updated)) "—"))
                (propertize (format "%-8s" (goblins--safe (plist-get record :configuration)))
                            'face 'goblins-dim)
                (and (stringp cwd)
                     (propertize (truncate-string-to-width
                                  (goblins--safe (abbreviate-file-name cwd)) 40 nil nil "…")
                                 'face (if (file-directory-p cwd) 'goblins-dim 'goblins-attention)))
                (and (eq (plist-get record :docker_enabled) t) "docker")
                (and packages (goblins--safe (concat "+" (string-join packages ","))))
                (and skipped (propertize (goblins--safe (concat "skipped " (string-join skipped ",")))
                                         'face 'goblins-attention))))))

(defun goblins--record-description (record)
  "Describe RECORD, a recorded conversation, for completion."
  (concat (propertize " " 'display '(space :align-to 40))
          (string-join (goblins--record-fields record) "  ")))

(defun goblins--record-table (records candidates)
  "A completion table of CANDIDATES for RECORDS, newest first, in the
`goblins-record' category.  A record ID, a goblin ID it ran as or its
conversation ID matches too, unlisted, as on the command line."
  (let ((annotate (lambda (candidate)
                    (when-let* ((record (goblins--record-of candidate)))
                      (goblins--record-description record)))))
    (lambda (string predicate action)
      (cond ((eq action 'metadata)
             `(metadata (category . goblins-record) (annotation-function . ,annotate)
                        (display-sort-function . identity) (cycle-sort-function . identity)))
            ((and (memq action '(nil lambda)) (goblins--record-by-id records string))
             t)
            (t (complete-with-action action candidates string predicate))))))

(defun goblins--read-record (prompt source)
  "Choose with PROMPT a conversation SOURCE's server recorded and does not
run now; return its record."
  (let* ((records (cl-remove-if (lambda (record) (eq (plist-get record :running) t))
                                (goblins--history-records source)))
         (candidates (goblins--record-candidates records))
         (goblins--resume-records (cl-mapcar #'cons candidates records)))
    (unless records (user-error "No recorded conversations to resume"))
    (let ((choice (completing-read prompt (goblins--record-table records candidates)
                                   nil t nil 'goblins-resume-history)))
      (or (goblins--record-of (car (member choice candidates)))
          (goblins--record-by-id records choice)
          (user-error "Select a conversation")))))

(defun goblins--choose-record (prompt)
  "Read a record to resume with PROMPT; return (SOURCE RECORD), for an
`interactive' form.  With a prefix argument, prompt for the daemon state
directory first."
  (let ((source (if current-prefix-arg
                    (goblins--connect (read-directory-name "Goblins state directory: "))
                  (goblins--default-source))))
    (list source (goblins--read-record prompt source))))

;;;###autoload
(defun goblins-resume (source record &optional display)
  "Resume a recorded conversation in a new goblin with the same environment.
RECORD, from the session history of SOURCE's server, is chosen among those
not running now.  The goblin starts with the record's configuration in its
directory, with the packages granted to it and its Docker, and its agent
continues the conversation; what cannot come back is skipped, and reported
once the goblin has started.  DISPLAY, default `goblins--display-new-agent',
shows the new terminal.  With a prefix argument, prompt for the daemon state
directory."
  (interactive (goblins--choose-record "Resume conversation: "))
  (goblins--require-ghostel)
  (let* ((configs (let ((default-directory (expand-file-name "~/")))
                    (goblins--configurations)))
         (launch (goblins--host-call
                  source 'sessions.start
                  (list :key (goblins--launch-key)
                        :configuration (plist-get configs :configuration)
                        :recreate (plist-get record :id) :rows 24 :cols 80)))
         (session (plist-get launch :session))
         (cwd (plist-get record :cwd)))
    (with-current-buffer source
      (push session goblins--resuming)
      (goblins--report-resumed)
      (condition-case err
          (goblins--open-terminal session (plist-get launch :agent_name)
                                  (file-name-as-directory (expand-file-name (if (stringp cwd) cwd "~")))
                                  display)
        (error
         (ignore-errors
           (jsonrpc-request goblins--connection 'sessions.stop (list :session session) :timeout 3))
         (signal (car err) (cdr err)))))))

(defun goblins--resumed-message (session)
  "What resuming gave SESSION, a goblin that has started or failed."
  (let ((record (goblins--safe (plist-get session :recreated_from)))
        (detail (plist-get session :detail)))
    (if (equal (plist-get session :state) "failed")
        (format "Resuming %s failed: %s" record (goblins--safe detail))
      (string-join
       (delq nil
             (list (format "Resumed %s (%s)" (goblins--safe (plist-get session :agent_name)) record)
                   (when-let* ((skipped (append (plist-get session :skipped) nil)))
                     (concat "skipped "
                             (mapconcat (lambda (s)
                                          (format "%s (%s)" (goblins--safe (plist-get s :name))
                                                  (goblins--safe (plist-get s :reason))))
                                        skipped ", ")))
                   (and (stringp detail) (goblins--safe detail))))
       "; "))))

(defun goblins--report-resumed ()
  "In a status buffer, report each goblin resumed from it once it is no
longer starting."
  (when goblins--resuming
    (setq goblins--resuming
          (cl-remove-if
           (lambda (id)
             (when-let* ((session (goblins--snapshot-session goblins--snapshot id))
                         ((not (equal (plist-get session :state) "starting"))))
               (message "%s" (goblins--resumed-message session))
               t))
           goblins--resuming))))
(add-hook 'goblins-snapshot-hook #'goblins--report-resumed)

;;;; Embark

(defvar goblins-embark-terminal-actions
  (let ((map (make-sparse-keymap)))
    (dolist (binding '(("W" . goblins-show-in-workspace) ("p" . goblins-goblin-permissions)
                       ("I" . goblins-goblin-info) ("t" . goblins-goblin-shell)
                       ("e" . goblins-goblin-eshell) ("d" . goblins-goblin-dired)
                       ("f" . goblins-goblin-find-file) ("R" . goblins-goblin-raid)
                       ("+" . goblins-goblin-invite) ("K" . goblins-goblin-stop)
                       ("X" . goblins-goblin-kill-execs)))
      (define-key map (kbd (car binding)) (cdr binding)))
    (define-key map (kbd "m") goblins-embark-goblin-messages-map)
    map)
  "Embark actions on a goblin, also on its agent terminal among buffers.")

(defvar goblins-embark-goblin-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map goblins-embark-terminal-actions)
    (define-key map (kbd "RET") #'goblins-goblin-show)
    (define-key map (kbd "o") #'goblins-goblin-show)
    map)
  "Embark actions for `goblin' targets; Embark's general actions too.")

(defvar goblins-embark-request-map
  (let ((map (make-sparse-keymap)))
    (dolist (binding '(("RET" . goblins-requests) ("p" . goblins-requests)
                       ("a" . goblins-request-accept) ("d" . goblins-request-deny)
                       ("g" . goblins-request-goblin)))
      (define-key map (kbd (car binding)) (cdr binding)))
    map)
  "Embark actions for `goblins-request' targets: pending permission requests.")

(defvar goblins-embark-record-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'goblins-resume)
    map)
  "Embark actions for `goblins-record' targets: recorded conversations.")

(defvar goblins-embark-terminal-map (make-sparse-keymap)
  "Embark actions for `goblins-terminal' targets: a goblin's agent terminal.
Embark's buffer actions too, under the goblin actions.")

(defun goblins--embark-retype (target)
  "Make TARGET, a (TYPE . NAME) buffer target, a `goblins-terminal' one when
it names an agent terminal."
  (if (and (eq (car-safe target) 'buffer) (goblins--agent-terminal (cdr target)))
      (cons 'goblins-terminal (cdr target))
    target))

(with-eval-after-load 'embark
  (set-keymap-parent goblins-embark-terminal-actions embark-general-map)
  (set-keymap-parent goblins-embark-terminal-map
                     (make-composed-keymap goblins-embark-terminal-actions embark-buffer-map))
  (add-to-list 'embark-keymap-alist '(goblin goblins-embark-goblin-map))
  (add-to-list 'embark-keymap-alist '(goblins-terminal goblins-embark-terminal-map))
  (set-keymap-parent goblins-embark-request-map embark-general-map)
  (add-to-list 'embark-keymap-alist '(goblins-request goblins-embark-request-map))
  (set-keymap-parent goblins-embark-record-map embark-general-map)
  (add-to-list 'embark-keymap-alist '(goblins-record goblins-embark-record-map))
  (when-let* ((export (alist-get 'buffer embark-exporters-alist)))
    (add-to-list 'embark-exporters-alist (cons 'goblins-terminal export)))
  ;; A buffer in `consult-buffer' arrives as a multi-category target.
  (dolist (type '(buffer multi-category))
    (when (alist-get type embark-transformer-alist)
      (add-function :filter-return (alist-get type embark-transformer-alist)
                    #'goblins--embark-retype))))

;;;; Marginalia

(defun goblins-marginalia-annotate-buffer (cand)
  "Describe CAND like a goblin if it is an agent terminal, else as any buffer."
  (or (goblins--terminal-description cand)
      (marginalia-annotate-buffer cand)))

(defun goblins-marginalia-annotate-record (cand)
  "Describe CAND, a recorded conversation, in Marginalia's columns."
  (when-let* ((record (goblins--record-of cand)))
    (concat (propertize " " 'marginalia--align t) marginalia-separator
            (string-join (goblins--record-fields record) marginalia-separator))))

(with-eval-after-load 'marginalia
  (let ((registry (if (boundp 'marginalia-annotators)
                      'marginalia-annotators
                    'marginalia-annotator-registry)))
    (unless (memq #'goblins-marginalia-annotate-buffer
                  (alist-get 'buffer (symbol-value registry)))
      (setf (alist-get 'buffer (symbol-value registry))
            (cons #'goblins-marginalia-annotate-buffer
                  (alist-get 'buffer (symbol-value registry)))))
    (unless (alist-get 'goblins-record (symbol-value registry))
      (setf (alist-get 'goblins-record (symbol-value registry))
            '(goblins-marginalia-annotate-record none)))))

;;;; Consult

(defvar goblins-consult-source
  (list :name "Goblin" :narrow ?g :hidden t :category 'buffer
        :face 'consult-buffer :history 'buffer-name-history
        :state #'consult--buffer-state
        :annotate #'goblins--terminal-description
        :items (lambda () (mapcar (lambda (b) (cons (buffer-name b) b))
                                  (goblins--agent-terminals))))
  "Goblins' agent terminals for `consult-buffer', on the narrowing key g.")

(with-eval-after-load 'consult
  (add-to-list 'consult-buffer-sources 'goblins-consult-source t))

(provide 'goblins-completion)
;;; goblins-completion.el ends here
