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

(ert-deftest goblins-render-and-fold ()
  (goblins-test--buffer
    (should buffer-read-only)
    (should (string-match-p "Agents (1)" (buffer-string)))
    (should (string-match-p "snikk" (buffer-string)))
    (should (string-match-p "Try a live grant 🐟" (buffer-string)))
    (goblins-test--goto "a")
    (should (oref (magit-current-section) hidden))
    (should (invisible-p (marker-position (oref (magit-current-section) content))))
    (magit-section-toggle (magit-current-section))
    (should-not (oref (magit-current-section) hidden))
    (goblins--render)
    (should (equal (oref (magit-current-section) value) "a"))
    (should-not (oref (magit-current-section) hidden))
    (magit-section-toggle (magit-current-section))
    (goblins--render)
    (should (oref (magit-current-section) hidden))))

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
      (dolist (group groups)
        (dolist (item (oref group children))
          (should (oref item hidden))
          (should (invisible-p (marker-position (oref item content)))))))
    ;; Newly arriving requests also start folded, without reopening history.
    (setq goblins--snapshot (goblins-test--snapshot
                             (goblins-test--request "a")
                             (goblins-test--request "b")
                             (goblins-test--request "old" "denied")))
    (goblins--render)
    (goblins-test--goto "b")
    (should (oref (magit-current-section) hidden))
    (should (oref (nth 2 (oref magit-root-section children)) hidden))))

(ert-deftest goblins-evil-bindings ()
  (skip-unless (require 'evil nil t))
  (goblins-test--buffer
    (evil-local-mode 1)
    (unwind-protect
        (progn
          (should (evil-normal-state-p))
          (dolist (binding '(("j" . magit-section-forward)
                             ("k" . magit-section-backward)
                             ("h" . magit-section-hide)
                             ("l" . magit-section-show)
                             ("TAB" . magit-section-toggle)
                             ("<tab>" . magit-section-toggle)
                             ("RET" . goblins-visit)
                             ("za" . magit-section-toggle)
                             ("a" . goblins-accept)
                             ("d" . goblins-deny)
                             ("r" . goblins-refresh)
                             ("gr" . goblins-refresh)
                             ("s" . goblins-start-server)
                             ("S" . goblins-stop-server)
                             ("R" . goblins-run)
                             ("q" . goblins-quit)
                             ("gg" . evil-goto-first-line)
                             ("G" . evil-goto-line)))
            (should (eq (key-binding (kbd (car binding))) (cdr binding)))))
      (evil-local-mode -1))))

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
  (let ((section (magit-get-section
                  `((goblins-agent-section . ,id)
                    (goblins-section . agents) (goblins-section . root)))))
    (should section)
    (goto-char (oref section start))))

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
            (should (string-match-p "RET: terminal" (buffer-string)))
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
  (let ((goblins-executable (getenv "GOBLINS_APP"))
        (goblins-state-directory (getenv "GOBLINS_TEST_STATE"))
        (default-directory (file-name-as-directory (getenv "GOBLINS_TEST_WORKSPACE")))
        (ghostel-query-before-killing nil)
        (completing-read-function #'completing-read-default)
        terminals status)
    (unwind-protect
        (progn
          (goblins-status)
          (setq status (current-buffer))
          (goblins-test--wait (lambda () goblins--subscription))
          ;; Press Enter without typing: the configured default must launch.
          (cl-letf (((symbol-function 'read-from-minibuffer)
                     (lambda (&rest _) "")))
            (dotimes (_ 2)
              (push (goblins-run) terminals)))
          (should-not (equal (buffer-local-value 'goblins--session-key (car terminals))
                             (buffer-local-value 'goblins--session-key (cadr terminals))))
          (with-current-buffer (car terminals)
            (should (derived-mode-p 'ghostel-mode))
            (ghostel-send-string "printf ghostel-run-ok > emacs-launch.txt\n"))
          (goblins-test--wait
           (lambda () (file-exists-p (expand-file-name "emacs-launch.txt" default-directory))))
          (pop-to-buffer status)
          (goblins-test--wait (lambda () (= 3 (length (plist-get goblins--snapshot :sessions)))))
          (dolist (terminal terminals)
            (let ((id (nth 2 (buffer-local-value 'goblins--session-key terminal))))
              (goblins-test--goto-agent id)
              (call-interactively (key-binding (kbd "RET")))
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
      (when (buffer-live-p status) (kill-buffer status)))))

(ert-deftest goblins-disconnected-actions-and-decision-outcomes ()
  (goblins-test--buffer
    (goblins--fail "Disconnected")
    (should (string-match-p "s start server, r reconnect" goblins--notice))
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
      (should (string-match-p "s start server" goblins--notice)))))

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
  "Start a real server from the status buffer in an isolated directory."
  (let ((goblins-executable (getenv "GOBLINS_APP")))
    (unwind-protect
        (progn
          (goblins-status (getenv "GOBLINS_TEST_STATE"))
          (should-not goblins--subscription)
          (should (string-match-p "s start server" (buffer-string)))
          (call-interactively (key-binding (kbd "s")))
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
            (call-interactively (key-binding (kbd "S")))
            (should-not goblins--connection)
            (should-error (goblins-start-server) :type 'user-error)
            (goblins-test--wait (lambda () (not goblins--server-process)))
            (should (equal goblins--notice "Server stopped; s start server"))
            (should-not goblins--snapshot)
            (goblins-start-server)
            (goblins-test--wait (lambda () goblins--subscription))
            (should-not (equal goblins--instance instance))))
      (when (derived-mode-p 'goblins-status-mode) (kill-buffer (current-buffer))))))

;; Run against an isolated real daemon from tests/test_emacs.py.
(defun goblins-test-live ()
  (let ((directory (getenv "GOBLINS_TEST_STATE")))
    (unwind-protect
        (progn
          (goblins-status directory)
          (goblins-test--wait (lambda () goblins--subscription))
          (should (string-match-p "snikk" (buffer-string)))
          (should (string-match-p "Try a live grant 🐟" (buffer-string)))
          (goblins-test--goto (getenv "GOBLINS_TEST_ACCEPT"))
          (goblins-accept)
          (goblins-test--wait
           (lambda () (cl-find "ready" (plist-get goblins--snapshot :permissions)
                               :key (lambda (r) (plist-get r :state)) :test #'equal)))
          (goblins-test--goto (getenv "GOBLINS_TEST_DENY"))
          (goblins-deny)
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

(defun goblins-test--wait (predicate)
  (let ((deadline (+ (float-time) 30)))
    (while (and (not (funcall predicate)) (< (float-time) deadline))
      (accept-process-output nil 0.02))
    (should (funcall predicate))))

;;; goblins-tests.el ends here
