;;; goblins-tests.el --- Goblins frontend tests -*- lexical-binding: t; -*-

(require 'ert)
(require 'goblins)
(require 'goblins-workspace)
(require 'goblins-views)
(require 'goblins-raids)
(require 'goblins-terminal)

;; Unit tests check failure notices; reconnection has tests of its own.
(setq goblins-reconnect-delays nil)
(require 'project)
(defvar ghostel-identity)
(defvar ghostel-query-before-killing)

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

;; Dev shell refreshes: the frontend assembles summary and diff sections from
;; structured data.
(defun goblins-test--devshell-request ()
  (let ((request (goblins-test--request "devshell")))
    (setq request (plist-put request :kind "devshell")
          request (plist-put request :package "refresh")
          request (plist-put request :preview
                             (list :kind "devshell" :reference "/src" :from 1 :to 2
                                   :attention [(:kind "new-input" :input "extra" :source "github:a/b")]
                                   :files [(:path "README.org" :added 1 :removed 0)
                                           (:path "flake.nix" :added 1 :removed 1)]
                                   :inputs [(:name "extra" :change "added" :old nil
                                             :new (:source "github:a/b" :id "1a2b3c4" :date nil))]
                                   :packages [(:name "rustc" :change "changed" :old ["1.89.0"] :new ["1.90.1"])]
                                   :env [] :fetch 2 :build 0 :download "1 MiB"
                                   :incoming [(:action "fetch" :name "glibc" :versions ["2.42"])]
                                   :diff (concat "diff --git a/README.org b/README.org\n"
                                                 "@@ -0,0 +1 @@\n+notes\n"
                                                 "diff --git a/flake.nix b/flake.nix\n--- a/flake.nix\n+++ b/flake.nix\n"
                                                 "@@ -1 +1 @@\n-a\n+b\e[2J\n"))))
    request))

(defun goblins-test--sections (class)
  "Alist of (VALUE . HIDDEN) for sections of CLASS in the buffer."
  (let (found)
    (cl-labels ((walk (section)
                  (when (eq (eieio-object-class section) class)
                    (push (cons (oref section value) (oref section hidden)) found))
                  (mapc #'walk (oref section children))))
      (walk magit-root-section))
    found))

(ert-deftest goblins-devshell-summary-and-diff-sections ()
  (goblins-test--buffer
    (setq goblins--snapshot (goblins-test--snapshot (goblins-test--devshell-request)))
    (goblins--render)
    (goblins-test--goto "devshell")
    (goblins-details)
    (let ((text (buffer-string)))
      (should (string-match-p "^Dev shell refresh /src · generation 1 → 2" text))
      (should (string-match-p "Attention ! new input 'extra' from github:a/b" text))
      (should (string-match-p "Source    flake.nix \\+1 -1" text))
      (should (string-match-p "1 other file changed" text))
      (should (string-match-p "Packages  \\[C\\] rustc 1.89.0 → 1.90.1" text))
      (should (string-match-p "Incoming  1 package" text))
      (should (string-match-p "+b\\\\u001b\\[2J" text))
      (should-not (string-match-p "\e" text))
      ;; Nix changes come first; other files form their own collapsed section.
      (should (< (string-match "^Nix changes (1 file)" text)
                 (string-match "^Other changes (1 file)" text))))
    (should (equal (goblins-test--sections 'goblins-diff-section)
                   '(((other-diff "devshell") . t) ((nix-diff "devshell") . nil))))
    (should (equal (cdr (assoc '("devshell" "flake.nix")
                               (goblins-test--sections 'goblins-diff-file-section)))
                   nil))
    ;; Colors: old versions removed, new ones added, keys highlighted.
    (goto-char (point-min))
    (search-forward "1.89.0")
    (should (eq (get-text-property (1- (point)) 'font-lock-face) 'goblins-removed))
    (search-forward "1.90.1")
    (should (eq (get-text-property (1- (point)) 'font-lock-face) 'goblins-added))
    (goto-char (point-min))
    (search-forward "Packages")
    (should (eq (get-text-property (1- (point)) 'font-lock-face) 'goblins-key))
    (search-forward "+b")
    (should (memq (get-text-property (point) 'font-lock-face) '(magit-diff-added diff-added)))
    ;; TAB folds the summary, and expanded sections survive updates.
    (goto-char (point-min))
    (search-forward "Dev shell refresh")
    (goblins-toggle-tree)
    (should (cdar (goblins-test--sections 'goblins-summary-section)))
    (goblins-toggle-tree)
    (goto-char (point-min))
    (re-search-forward "^Other changes")
    (goblins-toggle-tree)
    (goblins--render)
    (should (equal (cdr (assoc '(other-diff "devshell") (goblins-test--sections 'goblins-diff-section)))
                   nil))))

(ert-deftest goblins-diff-from-an-older-daemon-is-still-shown ()
  (goblins-test--buffer
    (let ((request (goblins-test--request "old")))
      (setq request (plist-put request :kind "devshell")
            request (plist-put request :preview
                               (list :description "Dev shell /src · generation 1 → 2"
                                     :diff "diff --git a/flake.nix b/flake.nix\n@@ -1 +1 @@\n-a\n+b\n"))
            goblins--snapshot (goblins-test--snapshot request)))
    (goblins--render)
    (goblins-test--goto "old")
    (goblins-details)
    (should (string-match-p "Dev shell /src" (buffer-string)))
    (should (string-match-p "^Nix changes (1 file)" (buffer-string)))
    (should (string-match-p "^\\+b" (buffer-string)))))

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

(ert-deftest goblins-refresh-approval-waits-for-its-preview ()
  (goblins-test--buffer
    (setq goblins--snapshot
          (goblins-test--snapshot
           (append (list :kind "devshell") (goblins-test--request "a"))))
    (goblins--render)
    (goblins-test--goto "a")
    (let (sent)
      (cl-letf (((symbol-function 'goblins--request)
                 (lambda (method params _callback) (setq sent (list method params)))))
        (let ((goblins--connection 'test))
          (should-error (goblins--decide t) :type 'user-error)
          (should-not sent)
          ;; Denial still cancels the evaluation.
          (goblins--decide nil)
          (should (equal (plist-get (cadr sent) :approved) :json-false)))))))

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

;; Launches start at the project.el root and may use its flake as the dev shell.
(ert-deftest goblins-run-launches-from-the-project-root ()
  (let* ((root (file-name-as-directory (make-temp-file "goblins-project" t)))
         (sub (file-name-as-directory (expand-file-name "sub" root)))
         (outside (file-name-as-directory (make-temp-file "goblins-outside" t)))
         (project-find-functions
          (list (lambda (dir)
                  (and (string-prefix-p root (expand-file-name dir))
                       (cons 'transient root)))))
         prompts answer)
    (unwind-protect
        (cl-letf (((symbol-function 'y-or-n-p)
                   (lambda (prompt) (push prompt prompts) answer)))
          (make-directory sub)
          (let ((default-directory sub))
            ;; No flake.nix at the root: no question, no dev shell.
            (should (equal (goblins--launch-location) (cons root nil)))
            (should-not prompts)
            (write-region "{}" nil (expand-file-name "flake.nix" root))
            (should (equal (goblins--launch-location) (cons root nil)))
            (should (string-match-p "flake\\.nix as the dev shell\\? \\'" (car prompts)))
            (setq answer t)
            (should (equal (goblins--launch-location)
                           (cons root (directory-file-name (file-truename root))))))
          (setq prompts nil)
          ;; Outside a project: the current directory, never a dev shell.
          (write-region "{}" nil (expand-file-name "flake.nix" outside))
          (let ((default-directory outside))
            (should (equal (goblins--launch-location) (cons outside nil)))
            (should-not prompts)))
      (delete-directory root t)
      (delete-directory outside t))))

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
        ;; Launch from the workspace itself, even if a directory above it
        ;; looks like a project (an empty /tmp/.git is enough for project.el).
        (project-find-functions nil)
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

;;; Workspace sidebar

(defun goblins-test--session (id &rest props)
  (append props
          (list :id id :agent_name id :name "shell" :state "running"
                :initial_packages [] :packages [])))

(defmacro goblins-test--sidebar (sessions &rest body)
  "Run BODY in a sidebar rendering SESSIONS from a fake status buffer."
  (declare (indent 1))
  `(let ((source (generate-new-buffer " *goblins test source*"))
         (sidebar (generate-new-buffer " *goblins test sidebar*")))
     (clrhash goblins--sidebar-folds)
     (unwind-protect
         (progn
           (with-current-buffer source
             (goblins-status-mode)
             (setq goblins--directory "/tmp/goblins-test"
                   goblins--instance "daemon-a"
                   goblins--snapshot
                   (list :instance "daemon-a" :sessions (vconcat ,sessions)
                         :permissions (vector (goblins-test--request "a")))))
           (with-current-buffer sidebar
             (goblins-sidebar-mode)
             (setq goblins--sidebar-source source goblins--directory "/tmp/goblins-test")
             (goblins--sidebar-render)
             ,@body))
       (when (buffer-live-p source)
         (with-current-buffer source (setq goblins--connection nil))
         (kill-buffer source))
       (kill-buffer sidebar))))

(defconst goblins-test--raid-sessions
  (list (goblins-test--session "session-a" :cwd "/p/one"
                               :raid '(:id "r1" :name "zeta"))
        (goblins-test--session "session-b" :cwd "/p/two")
        (goblins-test--session "session-c" :cwd "/p/one" :parent "session-a"
                               :raid '(:id "r1" :name "zeta"))
        (goblins-test--session "session-d" :cwd "/p/two"
                               :raid '(:id "r0" :name "alpha"))
        (goblins-test--session "session-e")))

(ert-deftest goblins-sidebar-groups-by-raid-then-directory ()
  (goblins-test--sidebar goblins-test--raid-sessions
    (let ((text (buffer-string)))
      (should (< (string-match "raid alpha" text) (string-match "raid zeta" text)))
      (should (< (string-match "raid zeta" text) (string-match "session-b" text)))
      (should (string-match-p "▾ raid zeta !1\n  ▾ /p/one/ !1\n    . session-a (shell) !1\n      . session-c (shell)"
                              (replace-regexp-in-string "[●○◌✗]" "." text)))
      (should (string-match-p "▾ /p/two/\n  . session-b" text))
      (should (string-match-p "▾ unknown\n  . session-e"
                              (replace-regexp-in-string "[●○◌✗]" "." text))))))

(ert-deftest goblins-sidebar-navigates-and-folds ()
  (goblins-test--sidebar goblins-test--raid-sessions
    (should buffer-read-only)
    (goto-char (point-min))
    (goblins-sidebar-next)
    (should (get-text-property (point) 'goblins-group))
    (goblins-sidebar-next)
    (should (equal (get-text-property (point) 'goblins-id) "session-d"))
    (goblins-sidebar-previous)
    (goblins-sidebar-previous)
    (should (equal (get-text-property (point) 'goblins-group) '(raid "r0")))
    (should (eq (key-binding (kbd "j")) #'goblins-sidebar-next))
    (should (eq (key-binding (kbd "k")) #'goblins-sidebar-previous))
    (should (eq (key-binding (kbd "p")) #'goblins-sidebar-permissions))
    (should (eq (key-binding (kbd "RET")) #'goblins-sidebar-open))
    (goblins-sidebar-toggle)
    (should-not (string-match-p "session-d" (buffer-string)))
    (should (string-match-p "▸ raid alpha" (buffer-string)))
    ;; Folds survive re-renders, and point stays on the same line.
    (goblins--sidebar-render)
    (should-not (string-match-p "session-d" (buffer-string)))
    (should (equal (get-text-property (point) 'goblins-group) '(raid "r0")))))

(ert-deftest goblins-sidebar-open-shows-the-terminal-in-the-main-window ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p"))
    (let* ((terminal (generate-new-buffer " *goblins test terminal*"))
           (goblins--terminal-buffers (make-hash-table :test #'equal))
           (shown nil))
      (unwind-protect
          (progn
            (puthash (list "/tmp/goblins-test" "daemon-a" "session-a") terminal
                     goblins--terminal-buffers)
            (goblins-sidebar-next)
            (cl-letf (((symbol-function 'goblins--display-main)
                       (lambda (buffer) (setq shown buffer))))
              (goblins-sidebar-next)
              (should (equal (get-text-property (point) 'goblins-id) "session-a"))
              (goblins-sidebar-open))
            (should (eq shown terminal)))
        (kill-buffer terminal)))))

(ert-deftest goblins-workspace-switch-function-is-pluggable ()
  (let (calls)
    (cl-letf (((symbol-function 'goblins--status-buffer)
               (lambda (&optional _) (current-buffer)))
              ((symbol-function 'goblins--sidebar-window) (lambda () nil))
              ((symbol-function 'goblins--sidebar-buffer) (lambda (_) (current-buffer)))
              ((symbol-function 'goblins-refresh) #'ignore)
              ((symbol-function 'display-buffer-in-side-window) (lambda (&rest _) (selected-window)))
              ((symbol-function 'set-window-dedicated-p) #'ignore))
      (let ((goblins-workspace-switch-or-create-function
             (lambda (name) (push name calls))))
        (save-window-excursion (goblins-workspace))
        (should (equal calls '("goblins")))))))

(ert-deftest goblins-workspace-default-switch-saves-and-restores-windows ()
  (save-window-excursion
    (delete-other-windows)
    (split-window)
    (should (= (length (window-list)) 2))
    (goblins-workspace-default-switch "goblins")
    (should (= (length (window-list)) 1))
    (goblins-workspace-default-quit)
    (should (= (length (window-list)) 2))
    (should-not (frame-parameter nil 'goblins-workspace-saved-config))))

(ert-deftest goblins-workspace-opens-the-most-recent-goblin-terminal ()
  (goblins-test--sidebar nil
    (let ((older (generate-new-buffer " *older*"))
          (newer (generate-new-buffer " *newer*")))
      (unwind-protect
          (progn
            (dolist (b (list older newer))
              (with-current-buffer b
                (setq-local goblins--session-key (list "/tmp/goblins-test" "daemon-a" "x"))))
            ;; `buffer-list' is most recently used first.
            (bury-buffer older)
            (switch-to-buffer newer)
            (should (eq (goblins--initial-main-buffer source) newer))
            (with-current-buffer newer (setq-local goblins--session-key nil))
            (with-current-buffer older
              (setq-local goblins--session-key (list "/other/server" "daemon-a" "x")))
            (should (eq (goblins--initial-main-buffer source) source)))
        (kill-buffer older)
        (kill-buffer newer)))))

;;; Views

(defun goblins-test--request-in (id session state &optional package)
  (list :id id :session session :approval (concat "approval-" id) :agent_name session
        :package (or package "hello") :reason "because" :state state :kind "package"))

(defmacro goblins-test--view (snapshot-permissions &rest body)
  "Run BODY in a status buffer holding two goblins and SNAPSHOT-PERMISSIONS."
  (declare (indent 1))
  `(let ((source (generate-new-buffer " *goblins test source*")))
     (unwind-protect
         (progn
           (with-current-buffer source
             (goblins-status-mode)
             (setq goblins--directory "/tmp/goblins-test" goblins--instance "daemon-a"
                   goblins--decisions (make-hash-table :test #'equal)
                   goblins--snapshot
                   (list :instance "daemon-a"
                         :sessions (vector (goblins-test--session
                                            "session-a" :cwd "/p/one" :packages ["fish"]
                                            :raid '(:id "r1" :name "crew" :owner "session-b"))
                                           (goblins-test--session "session-b" :cwd "/p/one"))
                         :raids [(:id "r1" :name "crew"
                                  :members [(:path "session-a") (:path "session-b")])]
                         :permissions (vconcat ,snapshot-permissions))))
           ,@body)
       (dolist (name (list goblins--info-buffer-name goblins--permissions-buffer-name))
         (when (get-buffer name) (kill-buffer name)))
       (kill-buffer source))))

(ert-deftest goblins-permissions-view-focuses-the-current-request-and-folds-history ()
  (goblins-test--view (list (goblins-test--request-in "r1" "session-a" "ready" "fish")
                            (goblins-test--request-in "r2" "session-b" "pending" "other")
                            (goblins-test--request-in "r3" "session-a" "denied" "curl")
                            (goblins-test--request-in "r4" "session-a" "pending" "ripgrep"))
    (with-current-buffer (goblins-permissions-buffer source "session-a")
      (let ((text (buffer-string)))
        ;; Scoped to the selected goblin.
        (should-not (string-match-p "other" text))
        (should (string-match-p "Current request" text))
        (should (< (string-match "ripgrep" text) (string-match "History (2)" text)))
        ;; Newest history first.
        (should (< (string-match "curl" text) (string-match "fish" text))))
      (should (equal (oref (magit-current-section) value) "r4"))
      (should (string-match-p "ripgrep" (buffer-substring (line-beginning-position)
                                                          (line-end-position))))
      (dolist (id '("r1" "r3"))
        (should (oref (goblins--find-request-section id) hidden)))
      (should-not (oref (goblins--find-request-section "r4") hidden)))))

(ert-deftest goblins-permissions-view-focuses-newly-arrived-requests ()
  (goblins-test--view (list (goblins-test--request-in "r1" "session-a" "ready"))
    (with-current-buffer (goblins-permissions-buffer source "session-a")
      (should (equal (plist-get (goblins--view-session-record) :id) "session-a"))
      (with-current-buffer source
        (setq goblins--snapshot
              (plist-put (copy-sequence goblins--snapshot) :permissions
                         (vector (goblins-test--request-in "r1" "session-a" "ready")
                                 (goblins-test--request-in "r2" "session-a" "pending")))))
      (goblins--permissions-render)
      (should (equal (oref (magit-current-section) value) "r2")))))

(ert-deftest goblins-permissions-view-decides-through-the-status-buffer ()
  (goblins-test--view (list (goblins-test--request-in "r1" "session-a" "pending"))
    (let (decided)
      (cl-letf (((symbol-function 'goblins--decide-record)
                 (lambda (record approved)
                   (setq decided (list (current-buffer) (plist-get record :id) approved)))))
        (with-current-buffer (goblins-permissions-buffer source "session-a")
          (goblins-permissions-accept)
          (should (equal decided (list source "r1" t)))
          (goblins-permissions-deny)
          (should (equal decided (list source "r1" nil))))))))

(ert-deftest goblins-info-view-shows-what-the-daemon-knows ()
  (goblins-test--view (list (goblins-test--request-in "r1" "session-a" "ready" "fish")
                            (goblins-test--request-in "r2" "session-a" "denied" "curl"))
    (with-current-buffer (goblins-info-buffer source "session-a")
      (let ((text (buffer-string)))
        (should (string-match-p "Directory +/p/one" text))
        (should (string-match-p "Granted +fish" text))
        (should (string-match-p "fish +ready" text))
        (should (string-match-p "curl +denied" text))
        (should (string-match-p "Name +crew" text))
        (should (string-match-p "Owner +session-b" text))
        (should (string-match-p "Members +session-a, session-b" text)))
      ;; Extra sections plug in.
      (let ((goblins-info-section-functions
             (cons (lambda (session _snapshot)
                     (insert "EXTRA " (plist-get session :id)))
                   goblins-info-section-functions)))
        (goblins--info-render)
        (should (string-match-p "EXTRA session-a" (buffer-string)))))))

(ert-deftest goblins-sidebar-runs-in-the-project-at-point ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p/one")
                               (goblins-test--session "session-b"))
    (let (call)
      (cl-letf (((symbol-function 'goblins-run)
                 (lambda (&optional directory display) (setq call (list directory display))))
                ((symbol-function 'project-prompt-project-dir) (lambda () "/chosen/")))
        (goto-char (point-min))
        ;; The directory group heading, then the goblin under it.
        (goblins-sidebar-run)
        (should (equal call '("/p/one" goblins--show-in-main)))
        (goblins-sidebar-next)
        (setq call nil)
        (goblins-sidebar-run)
        (should (equal (car call) "/p/one"))
        (goblins-sidebar-run-in-project)
        (should (equal call '("/chosen/" goblins--show-in-main)))
        ;; A goblin without a directory, and a raid-less unknown group, cannot run.
        (goto-char (point-min))
        (goblins-sidebar-next 3)
        (should-error (goblins-sidebar-run) :type 'user-error)))))

(ert-deftest goblins-sidebar-recovers-when-its-status-buffer-is-killed ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p"))
    (let ((refreshed nil))
      (kill-buffer source)
      (goblins--sidebar-render)
      (should (string-match-p "No server" (buffer-string)))
      (cl-letf (((symbol-function 'goblins-refresh) (lambda () (setq refreshed t))))
        (goblins-sidebar-refresh))
      (should refreshed)
      (should (buffer-live-p goblins--sidebar-source))
      (kill-buffer goblins--sidebar-source))))

;;; Raids

(defmacro goblins-test--raid-sidebar (&rest body)
  "Run BODY in a sidebar over two raid members, a loner and a stopped goblin."
  (declare (indent 0))
  `(goblins-test--sidebar
       (list (goblins-test--session "session-a" :cwd "/p/one"
                                    :raid '(:id "r1" :name "crew" :owner "session-a"))
             (goblins-test--session "session-b" :cwd "/p/one"
                                    :raid '(:id "r1" :name "crew" :owner "session-a"))
             (goblins-test--session "session-c" :cwd "/p/two")
             (goblins-test--session "session-d" :cwd "/p/two" :state "stopped"))
     (with-current-buffer source
       (setq goblins--connection t goblins--subscription "s"
             goblins--snapshot
             (plist-put goblins--snapshot :raids
                        [(:id "r1" :name "crew" :owner "session-a"
                          :members [(:id "session-a" :path "session-a" :name "shell"
                                     :state "running" :owner t)
                                    (:id "session-b" :path "session-b" :name "shell"
                                     :state "running" :owner :json-false :role "reviewer")])])))
     (goblins--sidebar-render)
     ,@body))

(defmacro goblins-test--raid-calls (var &rest body)
  "Run BODY with `goblins--host-call' recording (METHOD . PARAMS) in VAR."
  (declare (indent 1))
  `(let (,var)
     (cl-letf (((symbol-function 'goblins--host-call)
                (lambda (_source method params) (push (cons method params) ,var) nil))
               ((symbol-function 'goblins--display-main) #'ignore))
       ,@body)))

(ert-deftest goblins-sidebar-shows-the-goblin-type-next-to-the-name ()
  (goblins-test--raid-sidebar
    (should (string-match-p "session-a (shell)" (buffer-string)))))

(ert-deftest goblins-sidebar-ret-on-a-raid-opens-its-page ()
  (goblins-test--raid-sidebar
    (let (shown)
      (cl-letf (((symbol-function 'goblins--display-main) (lambda (b) (setq shown b))))
        (goto-char (point-min))
        (should (equal (get-text-property (point) 'goblins-raid) "crew"))
        (goblins-sidebar-open)
        (should (eq (buffer-local-value 'goblins--view-source shown) source))
        (should (equal (buffer-local-value 'goblins--view-session shown) "crew"))
        ;; Directory headings still fold.
        (goblins-sidebar-next)
        (goblins-sidebar-open)
        (should-not (string-match-p "session-a" (buffer-string)))
        (kill-buffer shown)))))

(ert-deftest goblins-raid-page-lists-members-and-who-could-join ()
  (goblins-test--raid-sidebar
    (let ((page (goblins-raid-buffer source "crew")))
      (unwind-protect
          (with-current-buffer page
            (let ((text (substring-no-properties (buffer-string))))
              (should (string-match-p "raid crew +2 members · owner session-a" text))
              (should (string-match-p "Members (2)" text))
              (should (string-match-p "session-a (shell)  owner" text))
              (should (string-match-p "session-b (shell)  reviewer" text))
              (should (string-match-p "Available to invite (1)\n.*session-c" text))
              ;; Stopped goblins and members are not candidates.
              (should-not (string-match-p "Available[^z]*session-d" text)))
            (goblins-raid-next)
            (should (equal (get-text-property (point) 'goblins-id) "session-a"))
            (goblins-raid-next)
            (goblins-raid-next)
            (should (eq (get-text-property (point) 'goblins-raid-line) 'candidate))
            (goblins-raid-previous 2)
            (should (equal (get-text-property (point) 'goblins-id) "session-a"))
            ;; Snapshot changes redraw it; a dissolved raid says so.
            (with-current-buffer source
              (setq goblins--snapshot (plist-put goblins--snapshot :raids [])))
            (with-current-buffer source (goblins--views-after-snapshot))
            (should (string-match-p "no longer exists" (buffer-string))))
        (kill-buffer page)))))

(ert-deftest goblins-raid-page-invites-and-kicks ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (let ((page (goblins-raid-buffer source "crew")))
        (unwind-protect
            (with-current-buffer page
              ;; On a candidate line, i invites that goblin.
              (goto-char (point-min))
              (re-search-forward "session-c")
              (goblins-raid-invite)
              (should (equal (pop calls)
                             '(raids.invite :raid "crew" :sessions ["session-c"])))
              ;; Elsewhere it asks which goblin.
              (goto-char (point-min))
              (cl-letf (((symbol-function 'completing-read)
                         (lambda (_prompt choices &rest _) (caar choices))))
                (goblins-raid-invite))
              (should (equal (car (pop calls)) 'raids.invite))
              ;; x kicks the member at point after confirming; candidates cannot be kicked.
              (goto-char (point-min))
              (re-search-forward "session-b (shell)")
              (cl-letf (((symbol-function 'y-or-n-p) (lambda (prompt)
                                                       (should-not (string-match-p "owner" prompt))
                                                       t)))
                (goblins-raid-kick))
              (should (equal (pop calls) '(raids.kick :raid "crew" :session "session-b")))
              (goto-char (point-min))
              (re-search-forward "session-a (shell)")
              (let (asked)
                (cl-letf (((symbol-function 'y-or-n-p)
                           (lambda (prompt) (setq asked prompt) nil)))
                  (goblins-raid-kick))
                (should (string-match-p "dissolves the raid" asked))
                (should-not calls))
              (goto-char (point-min))
              (re-search-forward "session-c")
              (should-error (goblins-raid-kick) :type 'user-error))
          (kill-buffer page))))))

(ert-deftest goblins-raid-command-creates-or-joins-around-a-goblin ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (goto-char (point-min))
      (re-search-forward "session-c")
      (beginning-of-line)
      (let (page)
        (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "fresh"))
                  ((symbol-function 'goblins-raid-buffer)
                   (lambda (_source name) (setq page name) (current-buffer))))
          (call-interactively #'goblins-raid))
        (should (equal (pop calls) '(raids.invite :raid "fresh" :sessions ["session-c"])))
        (should (equal page "fresh")))
      ;; A goblin already in a raid cannot start another.
      (goto-char (point-min))
      (re-search-forward "session-a")
      (beginning-of-line)
      (should-error (call-interactively #'goblins-raid) :type 'user-error)
      ;; Without a goblin in context the choice is among raid-less running ones.
      (let (page asked)
        (goto-char (point-min))
        (cl-letf (((symbol-function 'completing-read)
                   (lambda (prompt choices &rest _)
                     (if (string-match-p "around" prompt)
                         (progn (setq asked (mapcar #'car choices)) (caar choices))
                       "crew")))
                  ((symbol-function 'goblins-raid-buffer)
                   (lambda (_source name) (setq page name) (current-buffer))))
          (call-interactively #'goblins-raid))
        (should (equal (length asked) 1))
        (should (string-match-p "session-c" (car asked)))
        (should (equal (pop calls) '(raids.invite :raid "crew" :sessions ["session-c"])))
        (should (equal page "crew"))))))

(ert-deftest goblins-raid-command-uses-the-goblin-buffer ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (let ((terminal (generate-new-buffer " *goblins test terminal*")))
        (unwind-protect
            (with-current-buffer terminal
              (setq-local goblins--session-key (list "/tmp/goblins-test" "daemon-a" "session-c"))
              (cl-letf (((symbol-function 'goblins--status-buffer) (lambda (&optional _) source))
                        ((symbol-function 'completing-read) (lambda (&rest _) "crew"))
                        ((symbol-function 'goblins-raid-buffer)
                         (lambda (&rest _) (current-buffer))))
                (call-interactively #'goblins-raid))
              (should (equal (pop calls) '(raids.invite :raid "crew" :sessions ["session-c"]))))
          (kill-buffer terminal))))))

(ert-deftest goblins-sidebar-i-invites-the-goblin-to-a-raid-you-pick ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (goto-char (point-min))
      (re-search-forward "session-c")
      (beginning-of-line)
      (cl-letf (((symbol-function 'completing-read)
                 (lambda (_prompt names &rest _) (should (equal names '("crew"))) "crew")))
        (goblins-sidebar-invite))
      (should (equal (pop calls) '(raids.invite :raid "crew" :sessions ["session-c"])))
      ;; Already in a raid.
      (goto-char (point-min))
      (re-search-forward "session-a")
      (beginning-of-line)
      (should-error (goblins-sidebar-invite) :type 'user-error)
      ;; On a raid heading, pick the goblin instead.
      (goto-char (point-min))
      (cl-letf (((symbol-function 'completing-read) (lambda (_p choices &rest _) (caar choices))))
        (goblins-sidebar-invite))
      (should (equal (pop calls) '(raids.invite :raid "crew" :sessions ["session-c"]))))))

(ert-deftest goblins-sidebar-keys-follow-the-raid-bindings ()
  (with-temp-buffer
    (goblins-sidebar-mode)
    (dolist (binding '(("r" . goblins-raid) ("i" . goblins-sidebar-invite)
                       ("I" . goblins-sidebar-info) ("c" . goblins-sidebar-run)
                       ("C" . goblins-sidebar-run-in-project)
                       ("p" . goblins-sidebar-permissions)))
      (should (eq (key-binding (kbd (car binding))) (cdr binding))))))

(ert-deftest goblins-raid-refusals-do-not-drop-the-connection ()
  (let ((source (generate-new-buffer " *goblins test source*")))
    (unwind-protect
        (with-current-buffer source
          (goblins-status-mode)
          (setq goblins--connection t goblins--subscription "s")
          (cl-letf (((symbol-function 'jsonrpc-request)
                     (lambda (&rest _)
                       (signal 'jsonrpc-error
                               '("request failed" (jsonrpc-error-code . -32009)
                                 (jsonrpc-error-message . "goblin 'x' is already in raid 'y'"))))))
            (let ((err (should-error (goblins--host-call source 'raids.invite nil)
                                     :type 'user-error)))
              (should (string-match-p "already in raid" (cadr err)))))
          (should goblins--connection)
          (setq goblins--connection nil))
      (kill-buffer source))))

;;; Reconnecting and announcing requests

(ert-deftest goblins-reconnects-with-backoff-until-it-succeeds ()
  (goblins-test--buffer
    (let ((goblins-reconnect-delays '(1 3))
          timers connects)
      (cl-letf (((symbol-function 'run-at-time)
                 (lambda (delay _repeat fn) (push (cons delay fn) timers) (list 'timer delay)))
                ((symbol-function 'cancel-timer) #'ignore)
                ((symbol-function 'goblins--connect-now)
                 (lambda () (push goblins--reconnect-attempt connects)
                   (goblins--fail "Cannot connect"))))
        (goblins--fail "Disconnected")
        (should (string-match-p "reconnecting in 1s (r now)" goblins--notice))
        (should (equal (caar timers) 1))
        (funcall (cdar timers))
        (should (equal connects '(1)))
        (should (equal (caar timers) 3))
        (funcall (cdar timers))
        ;; The last delay repeats.
        (should (equal (caar timers) 3))
        (should (equal connects '(2 1)))
        ;; A manual refresh starts the count again.
        (cl-letf (((symbol-function 'goblins--connect-now) (lambda () nil)))
          (goblins-refresh))
        (should (= goblins--reconnect-attempt 0))))))

(ert-deftest goblins-reconnect-gives-up-without-delays-and-stops-when-killed ()
  (goblins-test--buffer
    (let ((goblins-reconnect-delays nil))
      (goblins--fail "Disconnected")
      (should-not goblins--reconnect-timer)
      (should (string-match-p "r to reconnect" goblins--notice))))
  ;; Not `with-temp-buffer': its buffers skip `kill-buffer-hook'.
  (let ((goblins-reconnect-delays '(60))
        (buffer (generate-new-buffer " *goblins test status*"))
        timer)
    (with-current-buffer buffer
      (goblins-status-mode)
      (setq goblins--directory "/tmp/goblins-test")
      (goblins--fail "Disconnected")
      (setq timer goblins--reconnect-timer)
      (should (timerp timer)))
    (kill-buffer buffer)
    (should-not (memq timer timer-list))))

(ert-deftest goblins-request-hook-announces-each-new-pending-request-once ()
  (goblins-test--buffer
    (let (announced)
      (let ((goblins-request-hook (list (lambda (r) (push (plist-get r :id) announced)))))
        (setq goblins--seen-requests 'unset)
        ;; The first snapshot is the baseline.
        (goblins--note-requests)
        (should-not announced)
        (setq goblins--snapshot (goblins-test--snapshot (goblins-test--request "a")
                                                        (goblins-test--request "b")))
        (goblins--note-requests)
        (should (equal announced '("b")))
        (goblins--note-requests)
        (should (equal announced '("b")))
        ;; A failing hook function does not break the stream.
        (let ((goblins-request-hook (list (lambda (_) (error "Boom")))))
          (setq goblins--snapshot (goblins-test--snapshot (goblins-test--request "c")))
          (goblins--note-requests))))))

;;; Sidebar additions

(ert-deftest goblins-sidebar-hides-finished-goblins ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p/one")
                               (goblins-test--session "session-b" :cwd "/p/two" :state "stopped")
                               (goblins-test--session "session-c" :cwd "/p/two" :state "failed"))
    (should-not (string-match-p "session-b\\|session-c\\|/p/two" (buffer-string)))
    (let ((goblins-sidebar-show-finished t))
      (goblins--sidebar-render)
      (should (string-match-p "session-b" (buffer-string))))
    (with-current-buffer source
      (setq goblins--snapshot (plist-put goblins--snapshot :sessions
                                         (vector (goblins-test--session "session-b" :state "stopped")))))
    (goblins--sidebar-render)
    (should (string-match-p "No running goblins" (buffer-string)))))

(ert-deftest goblins-sidebar-folds-outlive-the-sidebar-buffer-but-not-emacs ()
  (goblins-test--sidebar goblins-test--raid-sessions
    (goto-char (point-min))
    (goblins-sidebar-toggle)
    (should (member '(raid "r0") (gethash "/tmp/goblins-test" goblins--sidebar-folds)))
    ;; A new sidebar for the same server starts with the same folds.
    (with-temp-buffer
      (goblins-sidebar-mode)
      (setq goblins--sidebar-source source goblins--directory "/tmp/goblins-test")
      (goblins--sidebar-render)
      (should (string-match-p "▸ raid alpha" (buffer-string))))
    ;; They live only in this variable: nothing is written to disk.
    (should-not (get 'goblins--sidebar-folds 'saved-value))))

(ert-deftest goblins-sidebar-jumps-to-the-next-waiting-goblin ()
  (goblins-test--sidebar goblins-test--raid-sessions
    (with-current-buffer source
      (setq goblins--snapshot
            (plist-put goblins--snapshot :permissions
                       (vector (goblins-test--request-in "x" "session-a" "pending")
                               (goblins-test--request-in "y" "session-e" "pending")))))
    (goblins--sidebar-render)
    (goto-char (point-min))
    (goblins-sidebar-next-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-a"))
    (goblins-sidebar-next-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-e"))
    (goblins-sidebar-next-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-a"))
    ;; Folded groups open to show a waiting goblin.
    (goto-char (point-min))
    (re-search-forward "raid zeta")
    (goblins-sidebar-toggle)
    (should-not (string-match-p "session-a" (buffer-string)))
    (goto-char (point-min))
    (re-search-forward "session-e")
    (goblins-sidebar-next-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-a"))))

(ert-deftest goblins-sidebar-stops-goblins-and-their-children ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (cl-letf (((symbol-function 'y-or-n-p) (lambda (_) t)))
        (goto-char (point-min))
        (re-search-forward "session-c")
        (goblins-sidebar-stop)
        (should (equal (pop calls) '(sessions.stop :session "session-c")))
        (with-current-buffer source
          (setq goblins--snapshot
                (plist-put goblins--snapshot :sessions
                           (vconcat (plist-get goblins--snapshot :sessions)
                                    (vector (goblins-test--session "kid" :parent "session-c")
                                            (goblins-test--session "old" :parent "session-c"
                                                                   :state "stopped"))))))
        (let (asked)
          (cl-letf (((symbol-function 'y-or-n-p) (lambda (p) (setq asked p) t)))
            (goblins-sidebar-stop))
          (should (string-match-p "and its 1 running descendant\\?" asked)))
        (should (equal (pop calls) '(sessions.stop :session "session-c" :kill_children t)))
        ;; Declining sends nothing.
        (cl-letf (((symbol-function 'y-or-n-p) #'ignore))
          (goblins-sidebar-stop))
        (should-not calls)))))

(ert-deftest goblins-sidebar-and-raid-page-dissolve-raids ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (cl-letf (((symbol-function 'y-or-n-p) (lambda (_) t)))
        (goto-char (point-min))
        (goblins-sidebar-destroy-raid)
        (should (equal (pop calls) '(raids.destroy :raid "crew")))
        (forward-line 1)
        (should-error (goblins-sidebar-destroy-raid) :type 'user-error)
        (with-current-buffer (goblins-raid-buffer source "crew")
          (goblins-raid-destroy)
          (should (equal (pop calls) '(raids.destroy :raid "crew")))
          (kill-buffer))))))

(ert-deftest goblins-raid-page-sets-roles-and-marks-emacs-terminals ()
  (goblins-test--raid-sidebar
    (goblins-test--raid-calls calls
      (let ((terminal (generate-new-buffer " *goblins test terminal*"))
            (goblins--terminal-buffers (make-hash-table :test #'equal)))
        (puthash (list "/tmp/goblins-test" "daemon-a" "session-b") terminal goblins--terminal-buffers)
        (unwind-protect
            (with-current-buffer (goblins-raid-buffer source "crew")
              (should (string-match-p "session-b (shell)  reviewer  /p/one  \\[term\\]"
                                      (buffer-string)))
              (should-not (string-match-p "session-a[^\n]*\\[term\\]" (buffer-string)))
              (goto-char (point-min))
              (re-search-forward "session-b (shell)")
              (cl-letf (((symbol-function 'read-string)
                         (lambda (_prompt initial) (should (equal initial "reviewer")) "lead")))
                (goblins-raid-set-role))
              (should (equal (pop calls)
                             '(raids.set_role :raid "crew" :session "session-b" :role "lead")))
              (cl-letf (((symbol-function 'read-string) (lambda (&rest _) " ")))
                (should-error (goblins-raid-set-role) :type 'user-error))
              (re-search-forward "session-c")
              (should-error (goblins-raid-set-role) :type 'user-error)
              (kill-buffer))
          (kill-buffer terminal))))))

(ert-deftest goblins-help-lists-the-keys ()
  (with-temp-buffer
    (goblins-sidebar-mode)
    (should (eq (key-binding (kbd "?")) #'goblins-sidebar-help))
    (save-window-excursion (goblins-sidebar-help))
    (with-current-buffer "*Goblins Keys*"
      (should (string-match-p "next goblin waiting for a decision" (buffer-string)))
      (should (string-match-p "dissolve" (buffer-string))))
    (save-window-excursion (goblins-raid-help))
    (with-current-buffer "*Goblins Keys*"
      (should (string-match-p "set the member's role" (buffer-string)))
      (kill-buffer))))

;;; Switching and attention

(ert-deftest goblins-switch-chooses-among-running-goblins ()
  (goblins-test--raid-sidebar
    (let (offered annotation opened)
      (cl-letf (((symbol-function 'goblins--connect) (lambda (&optional _) source))
                ((symbol-function 'goblins-open-session)
                 (lambda (src id) (setq opened (list src id))))
                ((symbol-function 'completing-read)
                 (lambda (_prompt table &rest _)
                   (setq offered (mapcar #'car table)
                         annotation (funcall (plist-get completion-extra-properties
                                                        :annotation-function)
                                             "session-a"))
                   "session-a")))
        (goblins-switch))
      (should (equal (sort offered #'string<) '("session-a" "session-b" "session-c")))
      (should (string-match-p "shell.*raid crew.*/p/one !1" annotation))
      (should (equal opened (list source "session-a"))))))

(ert-deftest goblins-attention-mode-counts-and-announces-requests ()
  (goblins-test--buffer
    (let ((global-mode-string nil) announced)
      (unwind-protect
          (let ((goblins-attention-notify-function (lambda (r) (push (plist-get r :id) announced))))
            (goblins-attention-mode 1)
            (should (memq 'goblins--mode-line-string global-mode-string))
            (should (string-match-p "Goblins!1" goblins--mode-line-string))
            (setq goblins--seen-requests nil)
            (setq goblins--snapshot (goblins-test--snapshot (goblins-test--request "a")
                                                            (goblins-test--request "b")))
            (goblins--note-requests)
            (goblins--render)
            (should (equal (sort announced #'string<) '("a" "b")))
            (should (string-match-p "Goblins!2" goblins--mode-line-string))
            (setq goblins--snapshot (goblins-test--snapshot))
            (goblins--render)
            (should (equal goblins--mode-line-string "")))
        (goblins-attention-mode -1))
      (should-not (memq 'goblins--mode-line-string global-mode-string)))))

;;; goblin-mode

(ert-deftest goblin-mode-gives-goblin-terminals-their-own-keys ()
  (goblins-test--raid-sidebar
    (let ((terminal (generate-new-buffer " *goblins test terminal*")) shown)
      (unwind-protect
          (with-current-buffer terminal
            (setq-local goblins--session-key (list "/tmp/goblins-test" "daemon-a" "session-c"))
            (goblin-mode 1)
            (should (eq (key-binding (kbd "C-c g p")) #'goblin-permissions))
            (should (eq (key-binding (kbd "C-c g i")) #'goblin-info))
            (should (eq (key-binding (kbd "C-c g r")) #'goblins-raid))
            (cl-letf (((symbol-function 'goblins--status-buffer) (lambda (&optional _) source))
                      ((symbol-function 'goblins--display-main) (lambda (b) (setq shown b))))
              (goblin-info)
              (should (equal (buffer-local-value 'goblins--view-session shown) "session-c"))
              (should (derived-mode-p 'goblin-mode 'fundamental-mode))
              (goblin-permissions)
              (should (eq (buffer-local-value 'major-mode shown) 'goblins-permissions-mode))))
        (kill-buffer terminal)
        (dolist (name (list goblins--info-buffer-name goblins--permissions-buffer-name))
          (when (get-buffer name) (kill-buffer name))))))
  (with-temp-buffer
    (should-error (goblin-info) :type 'user-error)))

;;; Live workspace, from tests/test_emacs.py in a terminal Emacs (emacs -nw)

(defun goblins-test--in-sidebar (&optional text)
  "Select the sidebar window, on the first line matching TEXT if given."
  (select-window (get-buffer-window goblins--sidebar-buffer-name))
  (goto-char (point-min))
  (when text
    (re-search-forward text)
    (beginning-of-line)))

(defun goblins-test--sidebar-text ()
  (with-current-buffer goblins--sidebar-buffer-name
    (substring-no-properties (buffer-string))))

(defun goblins-test--live-session (source name)
  (cl-find name (append (plist-get (buffer-local-value 'goblins--snapshot source) :sessions) nil)
           :key (lambda (s) (plist-get s :agent_name)) :test #'equal))

(defun goblins-test--workspace-scenario ()
  (let* ((goblins-state-directory (getenv "GOBLINS_TEST_STATE"))
         (goblins-executable (or (getenv "GOBLINS_APP") goblins-executable))
         (goblins-reconnect-delays '(0.2))
         (project-one (abbreviate-file-name
                       (file-name-as-directory (getenv "GOBLINS_TEST_PROJECT_ONE"))))
         (ghostel-query-before-killing nil)
         ghostel-used source sidebar page)
    (goblins-workspace)
    (setq sidebar (get-buffer goblins--sidebar-buffer-name)
          source (buffer-local-value 'goblins--sidebar-source sidebar))
    (should (window-parameter (get-buffer-window sidebar) 'window-side))
    ;; The main window shows the status buffer: no goblin terminal yet.
    (should (eq (window-buffer (goblins--main-window)) source))
    (goblins-test--wait (lambda () (string-match-p "gamma (shell) !1" (goblins-test--sidebar-text))))
    (let ((text (goblins-test--sidebar-text)))
      (should (string-match-p (concat (regexp-quote project-one) "\n  ● alpha (shell)\n  ● beta (shell)")
                              text))
      ;; The directory of the waiting goblin carries its count too.
      (should (string-match-p "/ !1\n  ● delta (shell)\n  ● gamma (shell) !1" text)))
    ;; Attention: the mode line counts the request.
    (goblins-attention-mode 1)
    (should (string-match-p "Goblins!1" goblins--mode-line-string))
    ;; n finds the waiting goblin, p shows its request focused, d denies it.
    (goblins-test--in-sidebar)
    (goblins-sidebar-next-pending)
    (should (looking-at-p "  ● gamma"))
    (goblins-sidebar-permissions)
    (with-current-buffer (window-buffer (goblins--main-window))
      (should (eq major-mode 'goblins-permissions-mode))
      (should (equal (plist-get (oref (magit-current-section) record) :package) "hello"))
      (goblins-permissions-deny)
      (goblins-test--wait (lambda () (string-match-p "History (1)" (buffer-string))))
      (should (string-match-p "hello  denied" (buffer-string))))
    (goblins-test--wait (lambda () (equal goblins--mode-line-string "")))
    ;; I shows the goblin's info.
    (goblins-test--in-sidebar "  ● alpha")
    (goblins-sidebar-info)
    (with-current-buffer (window-buffer (goblins--main-window))
      (should (string-match-p (concat "Directory +" (regexp-quote (directory-file-name project-one)))
                              (buffer-string))))
    ;; r creates a raid around alpha and opens its page.
    (goblins-test--in-sidebar "  ● alpha")
    (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "crew")))
      (goblins-raid))
    (setq page (get-buffer goblins--raid-buffer-name))
    (should (eq (window-buffer (goblins--main-window)) page))
    (with-current-buffer page
      (goblins-test--wait (lambda () (string-match-p "Members (1)\n  ● alpha (shell)  owner"
                                                     (buffer-string))))
      ;; i invites the candidate at point; R sets its role; x kicks it.
      (goto-char (point-min))
      (re-search-forward "Available")
      (re-search-forward "● beta")
      (goblins-raid-invite)
      (goblins-test--wait (lambda () (string-match-p "Members (2)" (buffer-string))))
      (goto-char (point-min))
      (re-search-forward "● beta")
      (cl-letf (((symbol-function 'read-string) (lambda (&rest _) "reviewer")))
        (goblins-raid-set-role))
      (goblins-test--wait (lambda () (string-match-p "beta (shell)  reviewer" (buffer-string))))
      (goto-char (point-min))
      (re-search-forward "● beta")
      (cl-letf (((symbol-function 'y-or-n-p) (lambda (_) t)))
        (goblins-raid-kick))
      (goblins-test--wait (lambda () (string-match-p "Members (1)" (buffer-string)))))
    ;; i in the sidebar invites beta again, choosing the raid.
    (goblins-test--wait (lambda () (string-match-p "raid crew" (goblins-test--sidebar-text))))
    (goblins-test--in-sidebar "  ● beta")
    (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "crew")))
      (goblins-sidebar-invite))
    (goblins-test--wait
     (lambda () (equal (plist-get (plist-get (goblins-test--live-session source "beta") :raid) :name)
                       "crew")))
    ;; K stops delta, which then leaves the sidebar.
    (goblins-test--in-sidebar "  ● delta")
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (_) t)))
      (goblins-sidebar-stop))
    (goblins-test--wait (lambda () (not (string-match-p "delta" (goblins-test--sidebar-text)))))
    ;; D on the raid heading dissolves the raid; the page says so.
    (goblins-test--in-sidebar "raid crew")
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (_) t)))
      (goblins-sidebar-destroy-raid))
    (goblins-test--wait (lambda () (not (string-match-p "raid crew" (goblins-test--sidebar-text)))))
    (with-current-buffer page
      (should (string-match-p "no longer exists" (buffer-string))))
    ;; A lost stream reconnects by itself.
    (with-current-buffer source (goblins--fail "Simulated loss"))
    (should (string-match-p "reconnecting" (buffer-local-value 'goblins--notice source)))
    (goblins-test--wait (lambda () (equal (buffer-local-value 'goblins--notice source) "Connected")))
    (goblins-test--wait (lambda () (string-match-p "alpha" (goblins-test--sidebar-text))))
    ;; Switching attaches a terminal in goblin-mode, shown in the main window.
    (when (and (require 'ghostel nil t) (require 'ghostel-module nil t))
      (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "alpha")))
        (goblins-switch))
      (let ((terminal (window-buffer (goblins--main-window))))
        (with-current-buffer terminal
          (should (derived-mode-p 'ghostel-mode))
          (should goblin-mode)
          (should (eq (key-binding (kbd "C-c g p")) #'goblin-permissions)))
        (goblins-test--in-sidebar "  ● alpha")
        (should (overlays-at (point)))
        (kill-buffer terminal)
        (setq ghostel-used t)))
    (goblins-attention-mode -1)
    ;; q leaves the workspace.
    (goblins-test--in-sidebar)
    (goblins-workspace-quit)
    (should-not (get-buffer goblins--sidebar-buffer-name))
    (if ghostel-used "ok ghostel" "ok")))

(defun goblins-test-workspace-live ()
  "Run the workspace scenario, write ok or the failure to the result file, exit."
  (let ((result (getenv "GOBLINS_TEST_RESULT")))
    (condition-case err
        (write-region (goblins-test--workspace-scenario) nil result)
      (error (write-region (format "%S\n%s" err (goblins-test--sidebar-text-safe))
                           nil result)))
    (kill-emacs 0)))

(defun goblins-test--sidebar-text-safe ()
  (if (get-buffer goblins--sidebar-buffer-name) (goblins-test--sidebar-text) "(no sidebar)"))

;;; Review fixes

(ert-deftest goblins-workspace-is-a-command-after-requiring-goblins ()
  "The documented setup only requires `goblins'; the workspace must still load."
  (with-temp-buffer
    (let ((code (call-process
                 (expand-file-name invocation-name invocation-directory) nil t nil
                 "--batch" "-Q" "-L" (file-name-directory (locate-library "goblins"))
                 "--eval" "(require 'goblins)"
                 "--eval" (concat "(kill-emacs (if (and (commandp 'goblins-workspace)"
                                  " (not (featurep 'goblins-workspace))) 0 1))"))))
      (should (eq code 0)))))

(ert-deftest goblins-sidebar-next-pending-reaches-goblins-in-folded-groups ()
  "Two visible waiting goblins must not hide a third one in a folded group."
  (goblins-test--sidebar goblins-test--raid-sessions
    (with-current-buffer source
      (setq goblins--snapshot
            (plist-put goblins--snapshot :permissions
                       (vector (goblins-test--request-in "x" "session-a" "pending")
                               (goblins-test--request-in "y" "session-b" "pending")
                               (goblins-test--request-in "z" "session-e" "pending")))))
    (goblins--sidebar-render)
    ;; Fold /p/two/ (session-b, waiting) and raid alpha (nobody waiting).
    (goto-char (point-min))
    (goblins-sidebar-toggle)
    (goto-char (point-min))
    (re-search-forward "^▾ /p/two/")
    (goblins-sidebar-toggle)
    (should-not (string-match-p "session-b" (buffer-string)))
    (goto-char (point-min))
    (let (visited)
      (dotimes (_ 4)
        (goblins-sidebar-next-pending)
        (push (get-text-property (point) 'goblins-id) visited))
      (should (equal (nreverse visited) '("session-a" "session-b" "session-e" "session-a"))))
    ;; Only the group of the goblin moved to was unfolded.
    (should (string-match-p "▸ raid alpha" (buffer-string)))
    (should (string-match-p "▾ /p/two/" (buffer-string)))
    ;; With nobody else waiting, n stays put.
    (with-current-buffer source
      (setq goblins--snapshot
            (plist-put goblins--snapshot :permissions
                       (vector (goblins-test--request-in "x" "session-a" "pending")))))
    (goblins--sidebar-render)
    (goto-char (point-min))
    (re-search-forward "session-a")
    (beginning-of-line)
    (let ((here (point)))
      (goblins-sidebar-next-pending)
      (should (= (point) here)))))

(ert-deftest goblins-permissions-view-remembers-each-pending-request-once ()
  (goblins-test--view (list (goblins-test--request-in "r1" "session-a" "pending")
                            (goblins-test--request-in "r2" "session-a" "ready"))
    (with-current-buffer (goblins-permissions-buffer source "session-a")
      (dotimes (_ 50) (goblins--permissions-render))
      (should (equal goblins--permissions-seen '("r1")))
      ;; Settled requests are forgotten.
      (with-current-buffer source
        (setq goblins--snapshot
              (plist-put goblins--snapshot :permissions
                         (vector (goblins-test--request-in "r1" "session-a" "denied")))))
      (goblins--permissions-render)
      (should-not goblins--permissions-seen))))


;;; goblins-tests.el ends here
