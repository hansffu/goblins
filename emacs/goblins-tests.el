;;; goblins-tests.el --- Goblins frontend tests -*- lexical-binding: t; -*-

(require 'ert)
(require 'goblins)
(defvar ghostel-identity)

(defun goblins-test--request (id &optional state)
  (list :id id :session "session-a" :approval (concat "approval-" id)
        :agent_name "snikk" :package "hello" :reason "Try a live grant 🐟"
        :state (or state "pending")))

(defun goblins-test--snapshot (&rest requests)
  (list :instance "daemon-a"
        :sessions [(:id "session-a" :agent_name "snikk" :name "shell"
                    :state "running" :initial_packages ["fish"] :packages [])]
        :permissions (vconcat requests)))

(defmacro goblins-test--buffer (&rest body)
  (declare (indent 0))
  `(with-temp-buffer
     (goblins-status-mode)
     (setq goblins--directory "/tmp/goblins-test"
           goblins--instance "daemon-a"
           goblins--subscription "subscription-a"
           goblins--sequence 1
           goblins--snapshot (goblins-test--snapshot (goblins-test--request "a"))
           goblins--decisions (make-hash-table :test #'equal))
     (goblins--render)
     ,@body))

(defun goblins-test--goto (id)
  (goto-char (point-min))
  (let (found)
    (while (and (not found) (< (point) (point-max)))
      (let ((s (magit-current-section)))
        (when (and (object-of-class-p s 'goblins-request-section)
                   (equal (oref s value) id))
          (setq found t)))
      (unless found (forward-line 1)))
    (should found)))

(ert-deftest goblins-details-are-separate-from-tree-expansion ()
  (goblins-test--buffer
    (should buffer-read-only)
    (should (string-match-p "Agents (1)" (buffer-string)))
    (should-not (string-match-p "Session:\\|Startup:\\|Reason:\\|Details" (buffer-string)))
    (goblins-test--goto-agent "session-a")
    (call-interactively (key-binding (kbd "TAB")))
    (should-not (string-match-p "Session:" (buffer-string)))
    (call-interactively (key-binding (kbd "RET")))
    (should goblins--details)
    (should (string-match-p "Session:.*session-a" (buffer-string)))
    (should (string-match-p "Startup:.*fish" (buffer-string)))
    (should-not (string-match-p "Pending requests" (buffer-string)))
    (let ((text (buffer-string)))
      (call-interactively (key-binding (kbd "TAB")))
      (should (equal text (buffer-string))))
    (goblins-quit)
    (should-not goblins--details)
    (should (equal (oref (magit-current-section) value) "session-a"))
    (should-not (string-match-p "Session:" (buffer-string)))
    (goblins-test--goto "a")
    (call-interactively (key-binding (kbd "TAB")))
    (should-not (string-match-p "Try a live grant" (buffer-string)))
    (call-interactively (key-binding (kbd "RET")))
    (should (string-match-p "Try a live grant 🐟" (buffer-string)))
    (should (equal (oref (magit-current-section) value) "a"))
    (goblins-back)
    (should (equal (oref (magit-current-section) value) "a"))
    (should-not (string-match-p "Try a live grant" (buffer-string)))))

(ert-deftest goblins-live-details-and-approval-use-the-displayed-identity ()
  (goblins-test--buffer
    (goblins-test--goto-agent "session-a")
    (goblins-details)
    (plist-put (aref (plist-get goblins--snapshot :sessions) 0) :packages ["cowsay"])
    (goblins--render)
    (should (string-match-p "Granted:.*cowsay" (buffer-string)))
    (goblins-back)
    (goblins-test--goto "a")
    (goblins-details)
    (search-forward "Reason:")
    (let ((goblins--connection 'test) sent)
      (cl-letf (((symbol-function 'goblins--request)
                 (lambda (method params _callback) (setq sent (list method params)))))
        (goblins-deny))
      (should (equal sent '(permissions.decide
                            (:session "session-a" :request "a" :approval "approval-a"
                             :approved :json-false)))))))

(ert-deftest goblins-docker-approval-shows-authority-description ()
  (goblins-test--buffer
    (let ((request (goblins-test--request "docker")))
      (setq request (plist-put request :kind "docker")
            request (plist-put request :package "Docker engine")
            request (plist-put request :preview '(:description "Private engine; delete data on exit"))
            goblins--snapshot (goblins-test--snapshot request)))
    (goblins--render)
    (goblins-test--goto "docker")
    (goblins-details)
    (should (string-match-p "Docker engine" (buffer-string)))
    (should (string-match-p "Private engine; delete data on exit" (buffer-string)))))

(ert-deftest goblins-details-retain-identity-and-never-target-a-replacement ()
  (goblins-test--buffer
    (goblins-test--goto "a")
    (goblins-details)
    (setq goblins--snapshot (goblins-test--snapshot (goblins-test--request "b")))
    (goblins--render)
    (should (string-match-p "no longer retained" (buffer-string)))
    (let ((goblins--connection 'test))
      (should-error (goblins-accept) :type 'user-error))
    (goblins-back)
    (should (= (point) (point-min)))))

(ert-deftest goblins-default-visibility ()
  (goblins-test--buffer
    (setq goblins--snapshot (goblins-test--snapshot
                             (goblins-test--request "a")
                             (goblins-test--request "old" "denied")))
    (goblins--render)
    (let* ((groups (oref magit-root-section children))
           (agents (nth 0 groups))
           (pending (nth 1 groups))
           (history (nth 2 groups)))
      (should-not (oref agents hidden))
      (should-not (oref pending hidden))
      (should (oref history hidden))
      (should-not (string-match-p "Session:\\|Startup:\\|Reason:" (buffer-string))))
    ;; Newly arriving requests also start folded, without reopening history.
    (setq goblins--snapshot (goblins-test--snapshot
                             (goblins-test--request "a")
                             (goblins-test--request "b")
                             (goblins-test--request "old" "denied")))
    (goblins--render)
    (goblins-test--goto "b")
    (should-not (string-match-p "Reason:" (buffer-string)))
    (should (oref (nth 2 (oref magit-root-section children)) hidden))))

(ert-deftest goblins-evil-bindings ()
  (skip-unless (require 'evil nil t))
  (goblins-test--buffer
    (evil-local-mode 1)
    (unwind-protect
        (progn
          (should (evil-normal-state-p))
          (should-not (eq (key-binding (kbd "s")) #'goblins-start-server))
          (should-not (eq (key-binding (kbd "S")) #'goblins-stop-server))
          (dolist (binding '(("j" . magit-section-forward)
                             ("k" . magit-section-backward)
                             ("h" . magit-section-hide)
                             ("l" . magit-section-show)
                             ("TAB" . goblins-toggle-tree)
                             ("<tab>" . goblins-toggle-tree)
                             ("RET" . goblins-details)
                             ("t" . goblins-visit)
                             ("b" . goblins-back)
                             ("za" . goblins-toggle-tree)
                             ("a" . goblins-accept)
                             ("d" . goblins-deny)
                             ("r" . goblins-refresh)
                             ("gr" . goblins-refresh)
                             ("R" . goblins-run)
                             ("q" . goblins-quit)
                             ("gg" . evil-goto-first-line)
                             ("G" . evil-goto-line)))
            (should (eq (key-binding (kbd (car binding))) (cdr binding)))))
      (evil-local-mode -1))))

(ert-deftest goblins-stopped-agents-have-collapsed-history ()
  (goblins-test--buffer
    (setq goblins--snapshot
          (plist-put goblins--snapshot :sessions
                     (vconcat (mapcar (lambda (state)
                                        (list :id state :agent_name state
                                              :name "shell" :state state))
                                      '("starting" "running" "stopping" "stopped" "failed")))))
    (goblins--render)
    (let* ((groups (oref magit-root-section children))
           (agents (nth 0 groups))
           (stopped (nth 3 groups)))
      (should (equal (mapcar (lambda (s) (oref s value)) (oref agents children))
                     '("starting" "running" "stopping")))
      (should (equal (mapcar (lambda (s) (oref s value)) (oref stopped children))
                     '("stopped" "failed")))
      (should (oref stopped hidden))
      (should (invisible-p (marker-position (oref (car (oref stopped children)) start))))
      (goblins-test--goto-agent "running")
      (plist-put (aref (plist-get goblins--snapshot :sessions) 1) :state "stopped")
      (goblins--render)
      (should (= (point) (point-min)))
      (setq stopped (nth 3 (oref magit-root-section children)))
      (should (= 3 (length (oref stopped children))))
      (should (oref stopped hidden))
      (magit-section-show stopped)
      (goblins--render)
      (should-not (oref (nth 3 (oref magit-root-section children)) hidden)))))

(ert-deftest goblins-decisions-use-displayed-identities ()
  (dolist (approved '(t nil))
    (goblins-test--buffer
      (goblins-test--goto "a")
      (let (sent)
        (cl-letf (((symbol-function 'goblins--request)
                   (lambda (method params _callback) (setq sent (list method params)))))
          (let ((goblins--connection 'test))
            (goblins--decide approved)
            (should-error (goblins--decide approved) :type 'user-error)))
        (should (equal sent
                       (list 'permissions.decide
                             (list :session "session-a" :request "a"
                                   :approval "approval-a"
                                   :approved (if approved t :json-false)))))))))

(ert-deftest goblins-update-cannot-retarget-a-decision ()
  (goblins-test--buffer
    (goblins-test--goto "a")
    ;; A is evicted; B occupies the same row in a replacement snapshot.
    (goblins--changed nil 'state.changed
                     (list :subscription "subscription-a" :sequence 2
                           :snapshot (goblins-test--snapshot (goblins-test--request "b"))))
    (let ((goblins--connection 'test))
      (should-error (goblins-accept) :type 'user-error))
    (should (= (point) (point-min)))
    (goblins-test--goto "b")
    ;; A newly inserted row before B must preserve selection of B.
    (setq goblins--snapshot (goblins-test--snapshot
                             (goblins-test--request "c") (goblins-test--request "b")))
    (goblins--render)
    (should (equal (oref (magit-current-section) value) "b"))
    ;; A completed request moves to history and must not select its successor.
    (setq goblins--snapshot (goblins-test--snapshot
                             (goblins-test--request "b" "denied")
                             (goblins-test--request "c")))
    (goblins--render)
    (should (= (point) (point-min)))))

(ert-deftest goblins-refuses-stale-state-stream ()
  (dolist (change '((:subscription "subscription-a" :sequence 3)
                    (:subscription "other" :sequence 2)
                    (:subscription "subscription-a" :sequence 2
                     :snapshot (:instance "other"))))
    (goblins-test--buffer
      (goblins--changed nil 'state.changed change)
      (should-not goblins--subscription)
      (should (string-match-p "lost events" goblins--notice))
      (should-error (goblins-accept) :type 'user-error))))

(ert-deftest goblins-escapes-untrusted-display-text ()
  (should (equal (goblins--safe "line\n\t\e\u202e")
                 "line\\u000a\\u0009\\u001b\\u202e")))

(defun goblins-test--goto-agent (id)
  (cl-labels ((find-agent (section)
                (if (and (object-of-class-p section 'goblins-agent-section)
                         (equal (oref section value) id))
                    section
                  (cl-some #'find-agent (oref section children)))))
    (let ((section (find-agent magit-root-section)))
      (should section)
      (goto-char (oref section start)))))

(ert-deftest goblins-hierarchy-folds-preserve-identity-and-stopped-ancestry ()
  (goblins-test--buffer
    (setq goblins--snapshot
          '(:sessions [(:id "leaf" :parent "kid" :agent_name "leaf" :name "shell" :state "running")
                       (:id "parent" :agent_name "parent" :name "shell" :state "running")
                       (:id "other" :agent_name "other" :name "shell" :state "running")
                       (:id "kid" :parent "parent" :agent_name "kid" :name "shell" :state "running")
                       (:id "other-kid" :parent "other" :agent_name "kid" :name "shell" :state "running")]
            :permissions []))
    (goblins--render)
    (should (string-match-p "  host\n  ├─ parent" (buffer-string)))
    (should (string-match-p "  │  └─ kid" (buffer-string)))
    (should (string-match-p "  │     └─ leaf" (buffer-string)))
    (should (string-match-p "  └─ other" (buffer-string)))
    (goblins-test--goto-agent "leaf")
    (should-not (invisible-p (point)))
    (should (equal (oref (oref (magit-current-section) parent) value) "kid"))
    (goblins-test--goto-agent "kid")
    (should-not (oref (magit-current-section) hidden))
    (magit-section-hide (magit-current-section))
    ;; A sibling inserted ahead of the selected branch cannot move selection.
    (plist-put goblins--snapshot :sessions
               (vconcat [(:id "new" :parent "parent" :agent_name "new" :name "shell" :state "running")]
                        (plist-get goblins--snapshot :sessions)))
    (goblins--render)
    (should (equal (oref (magit-current-section) value) "kid"))
    (should (oref (magit-current-section) hidden))
    (goblins-test--goto-agent "leaf")
    (should (invisible-p (point)))
    ;; A stopped child stays in its live parent's branch.
    (plist-put (aref (plist-get goblins--snapshot :sessions) 1) :state "stopped")
    (goblins--render)
    (goblins-test--goto-agent "leaf")
    (should (equal (oref (oref (magit-current-section) parent) value) "kid"))
    ;; Once the whole branch stops it moves intact to collapsed history.
    (seq-doseq (agent (plist-get goblins--snapshot :sessions))
      (when (member (plist-get agent :id) '("parent" "kid" "new"))
        (plist-put agent :state "stopped")))
    (goblins--render)
    (goblins-test--goto-agent "parent")
    (should (equal (oref (oref (magit-current-section) parent) value) 'stopped))
    (should (invisible-p (point)))
    (goblins-test--goto-agent "kid")
    (should (equal (oref (oref (magit-current-section) parent) value) "parent"))))

(ert-deftest goblins-partial-tree-and-request-paths ()
  (let* ((records '((:id "orphan" :parent "missing") (:id "a" :parent "b") (:id "b" :parent "a")))
         (trees (goblins--agent-forest records)))
    (should (= 3 (apply #'+ (mapcar #'goblins--tree-count trees)))))
  (goblins-test--buffer
    (plist-put (aref (plist-get goblins--snapshot :sessions) 0) :path "parent/snikk")
    (goblins--render)
    (should (string-match-p "parent/snikk  hello" (buffer-string)))))

(ert-deftest goblins-terminal-links-use-session-identities ()
  (goblins-test--buffer
    (let* ((status (current-buffer))
           (terminal (generate-new-buffer " *goblins test terminal*"))
           (goblins--terminal-buffers (make-hash-table :test #'equal))
           (key (list goblins--directory goblins--instance "session-a")))
      (unwind-protect
          (progn
            (goblins-test--goto-agent "session-a")
            (should-error (goblins-visit) :type 'user-error)
            (puthash key terminal goblins--terminal-buffers)
            (goblins--render)
            (should (string-match-p "t: terminal" (buffer-string)))
            (goblins-visit)
            (should (eq (current-buffer) terminal))
            (set-buffer status)
            (let ((goblins--instance "new-daemon"))
              (should-not (goblins--terminal-buffer "session-a")))
            (let ((goblins--directory "/another/server"))
              (should-not (goblins--terminal-buffer "session-a")))
            (kill-buffer terminal)
            (should-not (goblins--terminal-buffer "session-a")))
        (when (buffer-live-p terminal) (kill-buffer terminal))))))

(ert-deftest goblins-directory-reports-stay-in-the-sandbox ()
  (let (called)
    (let ((ghostel-identity '((kind . goblins))))
      (goblins--ghostel-directory (lambda (dir) (setq called dir))
                                 "file://sandbox/workspace"))
    (should-not called)
    (let ((ghostel-identity '((kind . exec))))
      (goblins--ghostel-directory (lambda (dir) (setq called dir))
                                 "file://ordinary-host/tmp"))
    (should (equal called "file://ordinary-host/tmp"))))

(ert-deftest goblins-completion-enter-selects-displayed-default ()
  (let ((completing-read-function #'completing-read-default))
    ;; Exercise Emacs' completion/default handling, not a mocked choice.
    (cl-letf (((symbol-function 'read-from-minibuffer)
               (lambda (&rest _) "")))
      (should (equal (goblins--read-configuration '("shell")) "shell"))
      (should (equal (goblins--read-configuration '("codex" "shell")) "codex")))
    (cl-letf (((symbol-function 'read-from-minibuffer)
               (lambda (&rest _) "shell")))
      (should (equal (goblins--read-configuration '("codex" "shell")) "shell")))))

(defun goblins-test-run-live ()
  "Launch real goblins in the installed Ghostel, with no package downloads."
  (require 'ghostel)
  (let* ((goblins-executable (getenv "GOBLINS_APP"))
        (goblins-state-directory (getenv "GOBLINS_TEST_STATE"))
        (default-directory (file-name-as-directory (getenv "GOBLINS_TEST_WORKSPACE")))
        (ghostel-query-before-killing nil)
        (completing-read-function #'completing-read-default)
        (source (generate-new-buffer " *goblins launch notes*"))
        terminals status)
    (unwind-protect
        (progn
          (goblins-status)
          (setq status (current-buffer))
          (goblins-test--wait (lambda () goblins--subscription))
          (pop-to-buffer source)
          (insert "Unrelated launch notes")
          (setq buffer-read-only t)
          ;; Press Enter without typing: the configured default must launch.
          (cl-letf (((symbol-function 'read-from-minibuffer)
                     (lambda (&rest _) "")))
            (dotimes (_ 2)
              (push (goblins-run) terminals)))
          (should (equal (with-current-buffer source (buffer-string)) "Unrelated launch notes"))
          (should-not (equal (buffer-local-value 'goblins--session-key (car terminals))
                             (buffer-local-value 'goblins--session-key (cadr terminals))))
          (with-current-buffer (car terminals)
            (should (derived-mode-p 'ghostel-mode))
            (ghostel-send-string "printf ghostel-run-ok > emacs-launch.txt\n"))
          (goblins-test--wait
           (lambda () (file-exists-p (expand-file-name "emacs-launch.txt"
                                                       (getenv "GOBLINS_TEST_WORKSPACE")))) 60)
          (pop-to-buffer status)
          (goblins-test--wait (lambda () (= 3 (length (plist-get goblins--snapshot :sessions)))))
          (dolist (terminal terminals)
            (let ((id (nth 2 (buffer-local-value 'goblins--session-key terminal))))
              (goblins-test--goto-agent id)
              (call-interactively (key-binding (kbd "t")))
              (should (eq (current-buffer) terminal))
              (pop-to-buffer status)))
          (goblins-test--goto-agent (getenv "GOBLINS_TEST_EXTERNAL"))
          (should-error (goblins-visit) :type 'user-error)
          ;; Closing/reopening the status view retains the internal association.
          (kill-buffer status)
          (goblins-status)
          (setq status (current-buffer))
          (goblins-test--wait (lambda () goblins--subscription))
          (let* ((terminal (car terminals))
                 (key (buffer-local-value 'goblins--session-key terminal)))
            (should (eq (goblins--terminal-buffer (nth 2 key)) terminal))
            (kill-buffer terminal)
            (should-not (gethash key goblins--terminal-buffers))))
      (dolist (terminal terminals)
        (when (buffer-live-p terminal) (kill-buffer terminal)))
      (when (buffer-live-p status) (kill-buffer status))
      (kill-buffer source))))

(ert-deftest goblins-disconnected-actions-and-decision-outcomes ()
  (goblins-test--buffer
    (goblins--fail "Disconnected")
    (should (string-match-p "r to reconnect" goblins--notice))
    (should-not (string-match-p "outcome unknown" goblins--notice))
    (puthash "a" t goblins--decisions)
    (goblins--fail "Disconnected")
    (should (string-match-p "outcome unknown" goblins--notice))
    (puthash "a" 'acknowledged goblins--decisions)
    (goblins--fail "Disconnected")
    (should-not (string-match-p "outcome unknown" goblins--notice))))

(ert-deftest goblins-server-start-missing-executable ()
  (goblins-test--buffer
    (let ((goblins-executable "/nonexistent/goblins-test-executable"))
      (goblins-start-server)
      (should-not goblins--server-process)
      (should-not goblins--connection)
      (should (string-match-p "Cannot start server" goblins--notice))
      (should (string-match-p "r to reconnect" goblins--notice)))))

(defmacro goblins-test--outside-status (&rest body)
  (declare (indent 0))
  `(let ((directory (make-temp-file "goblins-outside-" t)))
     (unwind-protect
         (save-window-excursion
           (with-temp-buffer
             (text-mode)
             (insert "Keep my notes\n")
             (set-buffer-modified-p nil)
             (setq buffer-read-only t)
             (let ((source (current-buffer))
                   (goblins-state-directory directory))
               ,@body
               (should (buffer-live-p source))
               (with-current-buffer source
                 (should (equal (buffer-string) "Keep my notes\n"))
                 (should (eq major-mode 'text-mode))
                 (should-not (buffer-modified-p))
                 (should buffer-read-only)))))
       (when-let* ((status (get-buffer (format "*Goblins: %s*" directory))))
         (kill-buffer status))
       (delete-directory directory))))

(ert-deftest goblins-server-commands-preserve-unrelated-buffers ()
  (dolist (command '(goblins-start-server goblins-stop-server goblins-refresh goblins-status))
    (goblins-test--outside-status
      (let ((goblins-executable "/nonexistent/goblins-test-executable"))
        (call-interactively command)
        (should (derived-mode-p 'goblins-status-mode))
        (should (equal goblins--directory directory))
        (should-not (eq (current-buffer) source))
        (should-not (string-match-p "stringp, nil" (buffer-string)))))))

(ert-deftest goblins-quit-outside-status-only-closes-status ()
  (goblins-test--outside-status
    (let ((status (goblins--status-buffer)))
      (goblins-quit)
      (should-not (buffer-live-p status))
      (should (eq (current-buffer) source))
      ;; Repeating quit without any status view must also leave the caller alone.
      (goblins-quit))))

(ert-deftest goblins-visit-outside-status-chooses-local-terminals ()
  (goblins-test--outside-status
    (let ((terminal (generate-new-buffer " *goblins chosen terminal*"))
          (goblins--terminal-buffers (make-hash-table :test #'equal)))
      (unwind-protect
          (progn
            (puthash (list directory "daemon-a" "session-a") terminal goblins--terminal-buffers)
            (puthash '("/other-server" "daemon-b" "session-b") source goblins--terminal-buffers)
            (cl-letf (((symbol-function 'completing-read)
                       (lambda (_prompt choices &rest _)
                         (should (= 1 (length choices)))
                         (caar choices))))
              (goblins-visit)
              (should (eq (current-buffer) terminal))))
        (kill-buffer terminal)))))

(defun goblins-test--decide-from-notes (directory request approved)
  "Select REQUEST from outside status and verify that the caller survives."
  (let ((goblins-state-directory directory)
        (source (generate-new-buffer " *goblins approval notes*")))
    (unwind-protect
        (progn
          (pop-to-buffer source)
          (insert "Unrelated notes")
          (setq buffer-read-only t)
          (cl-letf (((symbol-function 'completing-read)
                     (lambda (_prompt choices &rest _)
                       (car (cl-find request choices
                                     :key (lambda (entry) (plist-get (cdr entry) :id))
                                     :test #'equal)))))
            (call-interactively (if approved #'goblins-accept #'goblins-deny)))
          (should (derived-mode-p 'goblins-status-mode))
          (should (equal (with-current-buffer source (buffer-string)) "Unrelated notes")))
      (kill-buffer source))))

(ert-deftest goblins-decision-disconnect-is-not-a-rejection ()
  (dolist (running '(t nil))
    (goblins-test--buffer
      (puthash "a" t goblins--decisions)
      (let ((goblins--connection 'test)
            callback)
        (cl-letf (((symbol-function 'jsonrpc-async-request)
                   (lambda (_connection _method _params &rest args)
                     (setq callback (plist-get args :error-fn))))
                  ((symbol-function 'jsonrpc-running-p) (lambda (_) running))
                  ((symbol-function 'goblins--disconnect) #'ignore))
          (goblins--request 'permissions.decide '(:request "a") #'ignore)
          (funcall callback '(:code -1 :message "Server died"))
          (should (eq (not running)
                      (not (null (string-match-p "outcome unknown" goblins--notice))))))))))

(defun goblins-test-start-live ()
  "Start a real server without first opening its status buffer."
  (let ((goblins-executable (getenv "GOBLINS_APP"))
        (goblins-state-directory (getenv "GOBLINS_TEST_STATE"))
        (source (generate-new-buffer " *goblins server notes*")))
    (unwind-protect
        (progn
          (pop-to-buffer source)
          (insert "Unrelated notes")
          (setq buffer-read-only t)
          (call-interactively #'goblins-start-server)
          (should (derived-mode-p 'goblins-status-mode))
          (should (process-live-p goblins--server-process))
          (should-error (goblins-start-server) :type 'user-error)
          (goblins-test--wait (lambda () goblins--subscription))
          (should-not goblins--server-process)
          (should (equal goblins--notice "Connected"))
          ;; Starting an already running server is harmless and reconnects.
          (let ((instance goblins--instance))
            (goblins-start-server)
            (goblins-test--wait (lambda () (not goblins--server-process)))
            (goblins-test--wait (lambda () goblins--subscription))
            (should (equal goblins--instance instance))
            (pop-to-buffer source)
            (call-interactively #'goblins-stop-server)
            (should (derived-mode-p 'goblins-status-mode))
            (should (equal (with-current-buffer source (buffer-string)) "Unrelated notes"))
            (should-not goblins--connection)
            (should-error (goblins-start-server) :type 'user-error)
            (goblins-test--wait (lambda () (not goblins--server-process)))
            (should (equal goblins--notice "Server stopped"))
            (should-not goblins--snapshot)
            (goblins-start-server)
            (goblins-test--wait (lambda () goblins--subscription))
            (should-not (equal goblins--instance instance))))
      (when (derived-mode-p 'goblins-status-mode) (kill-buffer (current-buffer)))
      (kill-buffer source))))

(defun goblins-test-tree-live ()
  "Check nested status sections against an isolated real daemon."
  (let ((parent (getenv "GOBLINS_TEST_PARENT"))
        (kid (getenv "GOBLINS_TEST_CHILD"))
        (leaf (getenv "GOBLINS_TEST_LEAF")))
    (unwind-protect
        (progn
          (goblins-status (getenv "GOBLINS_TEST_STATE"))
          (goblins-test--wait (lambda () goblins--subscription))
          (should (string-match-p "  host\n  ├─ parent" (buffer-string)))
          (should (string-match-p "  │     └─ leaf" (buffer-string)))
          (goblins-test--goto-agent leaf)
          (should-not (invisible-p (point)))
          (should (equal (oref (oref (magit-current-section) parent) value) kid))
          (goblins-test--goto-agent kid)
          (should (equal (oref (oref (magit-current-section) parent) value) parent))
          (magit-section-hide (magit-current-section))
          (goblins-details)
          (should (string-match-p (concat "Session:.*" (regexp-quote kid)) (buffer-string)))
          (should-not (string-match-p "Startup:" (progn (goblins-back) (buffer-string))))
          (should (oref (magit-current-section) hidden))
          (goblins-details)
          (jsonrpc-request goblins--connection 'sessions.stop (list :session leaf))
          (goblins-test--wait
           (lambda () (equal (plist-get
                             (cl-find leaf (plist-get goblins--snapshot :sessions)
                                      :key (lambda (a) (plist-get a :id)) :test #'equal)
                             :state) "stopped")))
          (should (equal (oref (magit-current-section) value) kid))
          (goblins-back)
          (should (oref (magit-current-section) hidden))
          (goblins-test--goto-agent leaf)
          (should (invisible-p (point)))
          (should (equal (oref (oref (magit-current-section) parent) value) kid))
          (jsonrpc-request goblins--connection 'sessions.stop
                           (list :session parent :kill_children t))
          (goblins-test--wait
           (lambda () (not (cl-some
                            (lambda (a) (and (member (plist-get a :id) (list parent kid leaf))
                                             (not (equal (plist-get a :state) "stopped"))))
                            (append (plist-get goblins--snapshot :sessions) nil)))))
          (goblins-test--goto-agent parent)
          (should (equal (oref (oref (magit-current-section) parent) value) 'stopped))
          (should (invisible-p (point)))
          (goblins-test--goto-agent leaf)
          (should (equal (oref (oref (magit-current-section) parent) value) kid)))
      (when (derived-mode-p 'goblins-status-mode) (kill-buffer (current-buffer))))))

;; Run against an isolated real daemon from tests/test_emacs.py.
(defun goblins-test-live ()
  (let ((directory (getenv "GOBLINS_TEST_STATE")))
    (unwind-protect
        (progn
          (goblins-status directory)
          (goblins-test--wait (lambda () goblins--subscription))
          (should (string-match-p "snikk" (buffer-string)))
          (should-not (string-match-p "Try a live grant 🐟" (buffer-string)))
          (goblins-test--goto (getenv "GOBLINS_TEST_ACCEPT"))
          (goblins-details)
          (should (string-match-p "Try a live grant 🐟" (buffer-string)))
          (goblins-back)
          (goblins-test--decide-from-notes directory (getenv "GOBLINS_TEST_ACCEPT") t)
          (goblins-test--wait
           (lambda () (cl-find "ready" (plist-get goblins--snapshot :permissions)
                               :key (lambda (r) (plist-get r :state)) :test #'equal)))
          (goblins-test--decide-from-notes directory (getenv "GOBLINS_TEST_DENY") nil)
          (goblins-test--wait
           (lambda () (cl-find "denied" (plist-get goblins--snapshot :permissions)
                               :key (lambda (r) (plist-get r :state)) :test #'equal)))
          (goblins-refresh)
          (goblins-test--wait (lambda () goblins--subscription))
          (should (= 2 (length (plist-get goblins--snapshot :sessions))))
          (let ((connection goblins--connection))
            (kill-buffer (current-buffer))
            (should-not (jsonrpc-running-p connection))))
      (when (derived-mode-p 'goblins-status-mode) (kill-buffer (current-buffer))))))

(defun goblins-test--wait (predicate &optional timeout)
  (let ((deadline (+ (float-time) (or timeout 30))))
    (while (and (not (funcall predicate)) (< (float-time) deadline))
      (accept-process-output nil 0.02))
    (should (funcall predicate))))

;;; goblins-tests.el ends here
