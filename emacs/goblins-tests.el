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
(defvar ghostel--process)

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

(ert-deftest goblins-ensure-server-never-displays-status ()
  (goblins-test--outside-status
    (let ((goblins-executable "/nonexistent/goblins-test-executable")
          (windows (window-list))
          echoed)
      (cl-letf (((symbol-function 'message)
                 (lambda (format &rest args) (push (apply #'format format args) echoed))))
        (call-interactively #'goblins-ensure-server))
      (should (eq (current-buffer) source))
      (should (equal (window-list) windows))
      (should (string-match-p "Cannot start server" (car echoed)))
      (with-current-buffer (goblins--status-buffer)
        (should-not goblins--server-process)
        (goblins--cancel-reconnect)
        ;; Connected: nothing to start.
        (setq goblins--subscription "s")
        (cl-letf (((symbol-function 'goblins--server-command)
                   (lambda (&rest _) (error "Started a running server"))))
          (goblins-ensure-server))))))

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
                       (cl-find request (goblins-test--choices choices)
                                :key (lambda (c) (get-text-property 0 'goblins-request c))
                                :test #'equal))))
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
            (should-not (equal goblins--instance instance))
            ;; Ensuring leaves the running server and its connection alone,
            (let ((status (current-buffer))
                  (connection goblins--connection))
              (pop-to-buffer source)
              (goblins-ensure-server)
              (should (eq (current-buffer) source))
              (with-current-buffer status
                (should-not goblins--server-process)
                (should (eq goblins--connection connection))
                ;; and starts a stopped one without showing its status.
                (goblins-stop-server)
                (goblins-test--wait (lambda () (not goblins--server-process))))
              (quit-windows-on status)
              (goblins-ensure-server)
              (should (eq (current-buffer) source))
              (should-not (get-buffer-window status t))
              (with-current-buffer status
                (goblins-test--wait (lambda () goblins--subscription))
                (should (equal goblins--notice "Connected"))))))
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

(defun goblins-test-activity-live ()
  "Report sandbox activity through the CLI and observe subscribed GUI updates."
  (let ((id (getenv "GOBLINS_TEST_AGENT")) source info
        (sidebar (generate-new-buffer " *goblins activity sidebar*")))
    (unwind-protect
        (progn
          (goblins-status (getenv "GOBLINS_TEST_STATE"))
          (setq source (current-buffer))
          (goblins-test--wait (lambda () goblins--subscription))
          (setq info (goblins-info-buffer source id))
          (with-current-buffer sidebar
            (goblins-sidebar-mode)
            (setq goblins--sidebar-source source
                  goblins--directory (getenv "GOBLINS_TEST_STATE"))
            (goblins--sidebar-render))
          (dolist (state '("working" "waiting-input" "idle"))
            (should (= 0 (call-process (getenv "GOBLINS_APP") nil nil nil
                                      "--state-dir" (getenv "GOBLINS_TEST_STATE")
                                      "exec" id "--" "/run/goblins/bin/goblins"
                                      "set-state" state)))
            (with-current-buffer source
              (goblins-test--wait
               (lambda ()
                 (equal state (plist-get
                               (cl-find id (plist-get goblins--snapshot :sessions)
                                        :key (lambda (s) (plist-get s :id)) :test #'equal)
                               :agent_state))))
              (should (string-match-p state (buffer-string))))
            (with-current-buffer info
              (should (string-match-p (concat "Activity +" state) (buffer-string))))
            (with-current-buffer sidebar
              (should (string-match-p (pcase state
                                        ("working" "◐ working · <1m")
                                        ("waiting-input" "◆ needs input · <1m")
                                        ("idle" "○ idle · <1m"))
                                      (buffer-string))))))
      (dolist (buffer (list sidebar info source))
        (when (buffer-live-p buffer) (kill-buffer buffer))))))

;;; Workspace sidebar

(defun goblins-test--choices (collection)
  "The candidates of COLLECTION, a completion table, in order."
  (all-completions "" collection))

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

(defun goblins-test--sidebar-plain ()
  "The sidebar's text, with each rule drawn as \"──\" after its indentation."
  (let ((out nil))
    (dotimes (i (- (point-max) (point-min)))
      (let* ((pos (+ (point-min) i))
             (display (get-text-property pos 'display)))
        (push (cond ((get-text-property pos 'goblins-rule) "──")
                    ((integerp (plist-get (cdr-safe display) :align-to))
                     (make-string (plist-get (cdr display) :align-to) ?\s))
                    (t (string (char-after pos))))
              out)))
    (apply #'concat (nreverse out))))

(ert-deftest goblins-sidebar-groups-by-raid-then-directory ()
  (goblins-test--sidebar goblins-test--raid-sessions
    ;; Raids first, then directories; a blank line before each group, and a
    ;; rule before each child goblin at its indent.
    (should (equal (goblins-test--sidebar-plain)
                   (concat "▾ raid alpha\n  ▾ /p/two/\n    session-d (shell)\n"
                           "\n▾ raid zeta !1\n  ▾ /p/one/ !1\n    session-a (shell)\n"
                           "    ! hello (package)\n    ──\n      session-c (shell)\n"
                           "\n▾ /p/two/\n  session-b (shell)\n"
                           "\n▾ unknown\n  session-e (shell)\n")))))

(ert-deftest goblins-sidebar-shows-each-goblin-as-a-block ()
  (let* ((since (- (* 1000 (float-time)) (* 4 60 1000)))
         (sessions
          (list (goblins-test--session "chief" :cwd "/p" :name "claude"
                                       :agent_state "working" :agent_state_since since
                                       :inbox 2 :shells 2 :execs 1)
                (goblins-test--session "scout" :cwd "/p" :parent "chief" :name "codex"
                                       :agent_state "waiting-approval" :agent_state_since since)
                (goblins-test--session "helper" :cwd "/p" :parent "chief"
                                       :agent_state "idle" :inbox 1)
                (goblins-test--session "beta" :cwd "/p" :state "starting")
                (goblins-test--session "old" :cwd "/p" :name "claude"
                                       :state "stopped" :exit_code 1))))
    (goblins-test--sidebar sessions
      (with-current-buffer source
        (setq goblins--snapshot
              (plist-put goblins--snapshot :permissions
                         (vector (goblins-test--request-in "x" "chief" "pending" "jq")
                                 (goblins-test--request-in "y" "chief" "denied")
                                 (append '(:kind "docker")
                                         (goblins-test--request-in "z" "scout" "pending"
                                                                   "docker"))))))
      (let ((goblins-sidebar-show-finished t))
        (goblins--sidebar-render)
        ;; One rule before every goblin but the first, never two in a row.
        (should (equal (goblins-test--sidebar-plain)
                       (concat "▾ /p/ !2\n  chief (claude)\n  ◐ working · 4m\n"
                               "  ! jq (package)\n  ✉ 2 messages\n  ⇄ 2 shells · Tramp connected\n"
                               "  ──\n    scout (codex)\n    ◆ needs approval · 4m\n"
                               "    ! docker\n"
                               "  ──\n    helper (shell)\n    ○ idle\n    ✉ 1 message\n"
                               "──\n  beta (shell)\n  ◌ starting\n"
                               "──\n  ✗ old (claude) · exited 1\n")))
        ;; Rules before children are fainter.
        (should (equal (cl-loop for pos from (point-min) below (point-max)
                                when (get-text-property pos 'goblins-rule)
                                collect (get-text-property pos 'face))
                       '(goblins-sidebar-child-separator goblins-sidebar-child-separator
                         goblins-sidebar-separator goblins-sidebar-separator)))
        (goto-char (point-min))
        (re-search-forward "working")
        (should (eq (get-text-property (point) 'face) 'goblins-activity-working))
        (should (equal (goblins--sidebar-duration (- (* 1000 (float-time)) 5000)) "<1m"))
        (should (equal (goblins--sidebar-duration (- (* 1000 (float-time)) (* 3 3600 1000)))
                       "3h"))))))

(ert-deftest goblins-sidebar-moves-past-a-goblins-details-in-one-step ()
  (goblins-test--sidebar
      (list (goblins-test--session "chief" :cwd "/p" :agent_state "working" :inbox 1)
            (goblins-test--session "scout" :cwd "/p" :parent "chief" :agent_state "idle")
            (goblins-test--session "beta" :cwd "/p"))
    (goto-char (point-min))
    (let ((stops nil))
      (while (progn (push (goblins--sidebar-key-at (point)) stops)
                    (let ((here (point))) (goblins-sidebar-next) (/= here (point)))))
      (should (equal (nreverse stops)
                     '((group dir nil "/p") (id . "chief") (id . "scout") (id . "beta")))))
    (goblins-sidebar-previous)
    (should (equal (get-text-property (point) 'goblins-id) "scout"))
    ;; The selection covers the name with its details, as one line.
    (goblins-sidebar-previous)
    (let ((range (goblins--sidebar-selection)))
      (should (equal (buffer-substring-no-properties (car range) (cdr range))
                     "  chief (shell)\n  ◐ working\n  ✉ 1 message\n")))
    ;; On a detail line, the goblin's commands still apply, and point
    ;; stays with its goblin across renders.
    (forward-line 1)
    (should (equal (goblins--sidebar-goblin-at-point) (cons source "chief")))
    (should (equal (goblins--sidebar-selection) (progn (forward-line -1)
                                                       (prog1 (goblins--sidebar-selection)
                                                         (forward-line 1)))))
    (goblins--sidebar-render)
    (should (equal (get-text-property (point) 'goblins-id) "chief"))))

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
    (should (eq (key-binding (kbd "RET")) #'goblins-sidebar-dwim))
    (goblins-sidebar-fold)
    (should-not (string-match-p "session-d" (buffer-string)))
    (should (string-match-p "▸ raid alpha" (buffer-string)))
    ;; Folds survive re-renders, and point stays on the same line.
    (goblins--sidebar-render)
    (should-not (string-match-p "session-d" (buffer-string)))
    (should (equal (get-text-property (point) 'goblins-group) '(raid "r0")))))

(ert-deftest goblins-sidebar-dwim-shows-the-terminal-in-the-main-window ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p"))
    (let* ((terminal (generate-new-buffer " *goblins test terminal*"))
           (goblins--terminal-buffers (make-hash-table :test #'equal))
           (shown nil))
      (unwind-protect
          (progn
            (puthash (list "/tmp/goblins-test" "daemon-a" "session-a") terminal
                     goblins--terminal-buffers)
            (goblins-sidebar-next)
            (cl-letf (((symbol-function 'pop-to-buffer)
                       (lambda (buffer &rest _) (setq shown buffer))))
              (goblins-sidebar-next)
              (should (equal (get-text-property (point) 'goblins-id) "session-a"))
              (goblins-sidebar-dwim))
            (should (eq shown terminal)))
        (kill-buffer terminal)))))

(ert-deftest goblins-sidebar-marks-selected-and-active-lines-apart ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p")
                               (goblins-test--session "session-b" :cwd "/p"
                                                      :agent_state "idle"))
    (let ((terminal (generate-new-buffer " *goblins test terminal*")))
      (unwind-protect
          (save-window-excursion
            (rename-buffer goblins--sidebar-buffer-name)
            (with-current-buffer terminal
              (setq-local goblins--session-key '("/tmp/goblins-test" "daemon-a" "session-b")))
            (set-window-buffer (selected-window) terminal)
            (cl-letf (((symbol-function 'goblins--main-window) #'selected-window))
              (goblins--sidebar-update-active))
            ;; No cursor, Evil's included: the line at point is highlighted.
            (should-not cursor-type)
            (should (equal (buffer-local-value 'evil-motion-state-cursor (current-buffer)) '(nil)))
            (should (eq hl-line-face 'goblins-sidebar-selected))
            ;; Only while the sidebar's window is selected.
            (goblins--sidebar-highlight-selection)
            (should-not (overlayp hl-line-overlay))
            (set-window-buffer (selected-window) (current-buffer))
            (goblins--sidebar-highlight-selection)
            (should (overlayp hl-line-overlay))
            (set-window-buffer (selected-window) terminal)
            (let* ((pos (goblins--sidebar-find '(id . "session-b")))
                   (overlays (overlays-at pos))
                   (block (cl-find 'goblins-sidebar-active overlays
                                   :key (lambda (o) (overlay-get o 'face)))))
              ;; The goblin's whole block, with a bar on each of its lines two
              ;; columns before the text, under the selection's highlight.
              (should block)
              (should (equal (buffer-substring-no-properties (overlay-start block)
                                                             (overlay-end block))
                             "  session-b (shell)\n  ○ idle\n"))
              (should (< (overlay-get block 'priority) hl-line-overlay-priority))
              (let ((bars (cl-remove-if-not (lambda (o) (equal (overlay-get o 'display) "▌"))
                                            (overlays-in (overlay-start block)
                                                         (overlay-end block)))))
                (should (= (length bars) 2))
                (should (equal (mapcar (lambda (o) (- (overlay-start o)
                                                      (save-excursion
                                                        (goto-char (overlay-start o))
                                                        (line-beginning-position))))
                                       bars)
                               '(0 0))))
              ;; A GUI draws strips half a column wide instead, the goblin's
              ;; outside a shown shell's with a gap between, and the text keeps
              ;; its column.
              (cl-letf (((symbol-function 'display-graphic-p) #'always))
                (let ((bars (goblins--sidebar-bar pos t t)))
                  (unwind-protect
                      (let ((strips (overlay-get (car bars) 'before-string)))
                        (should (equal (mapcar (lambda (i) (get-text-property i 'display strips))
                                               '(0 1 2))
                                       '((space :width 0.5) (space :width 0.25)
                                         (space :width 0.5))))
                        (should (get-text-property 0 'face strips))
                        (should (eq (get-text-property 1 'face strips) 'default))
                        (should (equal (mapcar (lambda (o) (overlay-get o 'display)) bars)
                                       '((space :align-to 1)))))
                    (mapc #'delete-overlay bars)))))
            ;; Neither highlight reaches the bar's column or left of it.
            (let ((pos (goblins--sidebar-find '(id . "session-b"))))
              (let ((masks (cl-remove-if-not (lambda (o) (overlay-get o 'goblins-margin))
                                             (overlays-at pos))))
                (should (equal (mapcar (lambda (o) (overlay-get o 'face)) masks) '(default)))
                (should (> (overlay-get (car masks) 'priority) hl-line-overlay-priority)))
              (should-not (cl-some (lambda (o) (overlay-get o 'goblins-margin))
                                   (overlays-at (1+ pos)))))
            (should-not (cl-remove-if (lambda (o) (overlay-get o 'goblins-margin))
                                      (overlays-at (goblins--sidebar-find '(id . "session-a"))))))
        (kill-buffer terminal)))))

(ert-deftest goblins-sidebar-bars-mark-shown-and-attached-goblins ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p")
                               (goblins-test--session "session-b" :cwd "/p")
                               (goblins-test--session "session-c" :cwd "/p"))
    (let ((shown (generate-new-buffer " *goblins test terminal b*"))
          (hidden (generate-new-buffer " *goblins test terminal a*"))
          (goblins--terminal-buffers (make-hash-table :test #'equal)))
      (unwind-protect
          (save-window-excursion
            (rename-buffer goblins--sidebar-buffer-name)
            (puthash '("/tmp/goblins-test" "daemon-a" "session-a") hidden
                     goblins--terminal-buffers)
            (with-current-buffer shown
              (setq-local goblins--session-key '("/tmp/goblins-test" "daemon-a" "session-b")))
            (puthash '("/tmp/goblins-test" "daemon-a" "session-b") shown
                     goblins--terminal-buffers)
            (set-window-buffer (selected-window) shown)
            (goblins--sidebar-update-active)
            (let ((bar (lambda (id)
                         (let ((pos (goblins--sidebar-find (cons 'id id))))
                           (cl-some (lambda (o) (and (overlay-get o 'display)
                                                     (get-text-property 0 'face (overlay-get o 'display))))
                                    (overlays-in pos (1+ pos)))))))
              ;; Blue while shown, grey while only its buffer exists, none
              ;; without one.
              (should (eq (funcall bar "session-b") 'goblins-sidebar-bar-shown))
              (should (eq (funcall bar "session-a") 'goblins-sidebar-bar-attached))
              (should-not (funcall bar "session-c"))))
        (kill-buffer shown)
        (kill-buffer hidden)))))

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
        (save-window-excursion
          (goblins-workspace)
          (goblins-test--leave-workspace-windows))
        (should (equal calls '("*goblins*")))))))

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

(defmacro goblins-test--workspace (sessions &rest body)
  "Run BODY in a workspace: the test sidebar in a left side window over SESSIONS.
In BODY, (terminal ID) is a fake agent terminal for session ID."
  (declare (indent 1))
  `(goblins-test--sidebar ,sessions
     (let ((goblins--layouts (make-hash-table :test #'equal))
           (goblins--terminal-buffers (make-hash-table :test #'equal))
           (goblins--last-goblin nil)
           (goblins--previous-goblin nil)
           (terminals nil))
       (save-window-excursion
         (delete-other-windows)
         (rename-buffer goblins--sidebar-buffer-name)
         (display-buffer-in-side-window sidebar '((side . left) (window-width . 20)
                                                  (window-parameters
                                                   (no-delete-other-windows . t))))
         (cl-flet ((terminal (id)
                     (or (alist-get id terminals nil nil #'equal)
                         (let ((buffer (generate-new-buffer (format " *agent %s*" id))))
                           (with-current-buffer buffer
                             (setq-local goblins--session-key
                                         (list "/tmp/goblins-test" "daemon-a" id)))
                           (puthash (list "/tmp/goblins-test" "daemon-a" id) buffer
                                    goblins--terminal-buffers)
                           (push (cons id buffer) terminals)
                           buffer))))
           (unwind-protect (progn ,@body)
             (goblins-test--leave-workspace-windows)
             (mapc (lambda (e) (kill-buffer (cdr e))) terminals)))))))

(defun goblins-test--leave-workspace-windows ()
  "Make every window ordinary again; restoring the window configuration
keeps the parameters of windows that live on."
  (dolist (window (window-list nil 'no-minibuf))
    (goblins--release-agent-window window)
    (set-window-parameter window 'goblins-workspace nil)))

(defun goblins-test--layout ()
  "The buffers beside the sidebar, left to right, then top to bottom."
  (mapcar #'window-buffer
          (sort (goblins--main-windows)
                (lambda (a b) (or (< (window-left-column a) (window-left-column b))
                                  (and (= (window-left-column a) (window-left-column b))
                                       (< (window-top-line a) (window-top-line b))))))))

(ert-deftest goblins-workspace-saves-each-goblins-windows ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p"))
    (let ((dired-a (generate-new-buffer " *dired a*"))
          (notes (generate-new-buffer " *notes*"))
          (scratch (generate-new-buffer " *scratch test*"))
          (view (generate-new-buffer " *view*"))
          (split-width-threshold 20))
      (with-current-buffer view (goblins-info-mode))
      (unwind-protect
          (progn
            ;; Without an agent terminal, the status buffer, alone.
            (goblins--workspace-restore source)
            (should (equal (goblins-test--layout) (list source)))
            ;; The first agent replaces it.
            (goblins--display-new-agent (terminal "session-a"))
            (setq goblins--last-goblin '("/tmp/goblins-test" . "session-a"))
            (should (equal (goblins-test--layout) (list (terminal "session-a"))))
            ;; What Goblins opens goes right of the agent.
            (goblins-select-goblin source "session-a" dired-a)
            (should (equal (goblins-test--layout) (list (terminal "session-a") dired-a)))
            (should (eq (window-buffer) dired-a))
            ;; So does what you show and split there yourself.
            (switch-to-buffer scratch)
            (set-window-buffer (split-window nil nil 'below) notes)
            (should (equal (goblins-test--layout) (list (terminal "session-a") scratch notes)))
            ;; Another goblin with nothing saved: its agent alone.
            (terminal "session-b")
            (goblins-select-goblin source "session-b")
            (should (equal (goblins-test--layout) (list (terminal "session-b"))))
            ;; Back again: everything you left, the agent selected.
            (goblins-select-goblin source "session-a")
            (should (equal (goblins-test--layout) (list (terminal "session-a") scratch notes)))
            (let ((right (mapcar (lambda (b) (get-buffer-window b)) (list scratch notes))))
              (should (> (window-left-column (car right))
                         (window-left-column (goblins--agent-window))))
              (should (= (window-left-column (car right)) (window-left-column (cadr right)))))
            (should (eq (window-buffer) (terminal "session-a")))
            ;; The agent window is the agent's.
            (should-error (switch-to-buffer scratch nil t))
            (should-error (delete-window (goblins--agent-window)) :type 'user-error)
            (select-window (get-buffer-window notes))
            (delete-other-windows)
            (should (equal (goblins-test--layout) (list (terminal "session-a") notes)))
            ;; A view is never saved: leaving the goblin keeps what it replaced,
            ;; and q puts that back too.
            (goblins--display-main view)
            (should (equal (goblins-test--layout) (list (terminal "session-a") view)))
            (goblins-select-goblin source "session-b")
            (goblins-select-goblin source "session-a")
            (should (equal (goblins-test--layout) (list (terminal "session-a") notes)))
            (goblins--display-main view)
            (quit-window)
            (should (equal (goblins-test--layout) (list (terminal "session-a") notes)))
            ;; A saved window whose buffer was killed is dropped.
            (goblins-select-goblin source "session-b")
            (kill-buffer notes)
            (goblins-select-goblin source "session-a")
            (should (equal (goblins-test--layout) (list (terminal "session-a"))))
            ;; Re-entering the workspace shows the goblin selected last, as
            ;; left: `goblins-workspace' saves, then restores.
            (goblins-select-goblin source "session-b" dired-a)
            (goblins--save-layout)
            (delete-other-windows (goblins--agent-window))
            (goblins--workspace-restore source)
            (should (equal (goblins-test--layout) (list (terminal "session-b") dired-a))))
        (mapc (lambda (b) (when (buffer-live-p b) (kill-buffer b)))
              (list dired-a notes scratch view))))))

(ert-deftest goblins-workspace-leaves-other-packages-windows-alone ()
  "A window split off below everything (as shackle or popper do for
*Messages*) is neither used, saved nor deleted for a goblin."
  (goblins-test--workspace (list (goblins-test--session "session-a")
                                 (goblins-test--session "session-b"))
    (let ((dired (generate-new-buffer " *dired*"))
          (file (generate-new-buffer " *file*"))
          (messages (generate-new-buffer " *messages*"))
          (split-width-threshold 20)
          popup)
      (unwind-protect
          (progn
            (mapc #'terminal '("session-a" "session-b"))
            (goblins-select-goblin source "session-a")
            ;; Below everything while the agent is alone...
            (setq popup (split-window (window-main-window) -4 'below))
            (set-window-buffer popup messages)
            (goblins-select-goblin source "session-a" dired)
            (should (eq (window-buffer popup) messages))
            (should (eq (window-buffer (goblins--right-window)) dired))
            ;; ...and while the goblin has windows of its own.
            (delete-window popup)
            (setq popup (split-window (window-main-window) -4 'below))
            (set-window-buffer popup messages)
            (select-window popup)
            (goblins--display-right file)
            (should (eq (window-buffer popup) messages))
            ;; (The layout lists the popup second: it starts at the agent's column.)
            (should (equal (goblins-test--layout) (list (terminal "session-a") messages file)))
            ;; Switching goblins keeps it, and doesn't save it.
            (goblins-select-goblin source "session-b")
            (should (window-live-p popup))
            (should (equal (goblins-test--layout) (list (terminal "session-b") messages)))
            (delete-window popup)
            (goblins-select-goblin source "session-a")
            (should (equal (goblins-test--layout) (list (terminal "session-a") file))))
        (mapc #'kill-buffer (list dired file messages))))))

(ert-deftest goblins-workspace-splits-by-the-split-thresholds ()
  (goblins-test--workspace (list (goblins-test--session "session-a")
                                 (goblins-test--session "session-b")
                                 (goblins-test--session "session-c"))
    (let ((dired (generate-new-buffer " *dired*"))
          (side-by-side (lambda ()
                          (let ((windows (goblins--main-windows)))
                            (/= (window-left-column (car windows))
                                (window-left-column (cadr windows)))))))
      (unwind-protect
          (progn
            (mapc #'terminal '("session-a" "session-b" "session-c"))
            ;; Wide enough: right of the agent.
            (let ((split-width-threshold 20))
              (goblins-select-goblin source "session-a" dired))
            (should (equal (goblins-test--layout) (list (terminal "session-a") dired)))
            (should (funcall side-by-side))
            ;; Too narrow: below it.
            (let ((split-width-threshold 1000) (split-height-threshold 10))
              (goblins-select-goblin source "session-b" dired))
            (should (equal (goblins-test--layout) (list (terminal "session-b") dired)))
            (should-not (funcall side-by-side))
            ;; Too small for two windows: the buffer asked for wins.
            (let ((split-width-threshold 1000) (window-min-height 100))
              (goblins-select-goblin source "session-c" dired))
            (should (equal (goblins-test--layout) (list dired)))
            ;; Restoring keeps the arrangement saved, whatever the thresholds.
            (let ((split-width-threshold 1000))
              (goblins-select-goblin source "session-a"))
            (should (funcall side-by-side)))
        (kill-buffer dired)))))

(ert-deftest goblins-views-quit-with-q ()
  (dolist (mode '(goblins-info-mode goblins-permissions-mode goblins-raid-mode))
    (with-temp-buffer
      (funcall mode)
      (should (eq (key-binding "q") #'quit-window)))))

(ert-deftest goblins-request-popup-follows-the-selected-goblin ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p"))
    (let ((popup (lambda () (get-buffer-window goblins--popup-buffer-name)))
          decided)
      (terminal "session-a")
      (terminal "session-b")
      ;; Selecting a waiting goblin pops up its request, without the focus.
      (goblins-select-goblin source "session-a")
      (should (funcall popup))
      (should (eq (window-buffer) (terminal "session-a")))
      (should (equal (goblins-test--layout) (list (terminal "session-a"))))
      (with-current-buffer goblins--popup-buffer-name
        (should (string-match-p "session-a asks for hello" (buffer-string)))
        (should (string-match-p "Reason: +Try a live grant" (buffer-string)))
        ;; a decides the request shown.
        (cl-letf (((symbol-function 'goblins--decide-record)
                   (lambda (record approved) (setq decided (list (plist-get record :id) approved)))))
          (goblins-request-popup-accept))
        (should (equal decided '("a" t))))
      ;; A goblin with nothing waiting closes it.
      (goblins-select-goblin source "session-b")
      (should-not (funcall popup))
      ;; A request of the selected goblin pops up; another goblin's doesn't.
      (with-current-buffer source
        (goblins--popup-on-request (list :session "session-a")))
      (should-not (funcall popup))
      (with-current-buffer source
        (setq goblins--snapshot
              (plist-put goblins--snapshot :permissions
                         (vector (goblins-test--request "a")
                                 (plist-put (goblins-test--request "b") :session "session-b"))))
        (goblins--popup-on-request (list :session "session-b")))
      (should (funcall popup))
      ;; Once nothing is waiting, it closes by itself.
      (with-current-buffer source
        (setq goblins--snapshot (plist-put goblins--snapshot :permissions
                                           (vector (goblins-test--request "a"))))
        (goblins--views-after-snapshot))
      (should-not (funcall popup))
      ;; Off: never.
      (let ((goblins-request-popups nil))
        (goblins-select-goblin source "session-a")
        (should-not (funcall popup))))))

(ert-deftest goblins-editor-files-wait-for-approval-before-requests ()
  (require 'server)
  (goblins-test--workspace (list (goblins-test--session "session-a" :path "gribble"))
    (let ((goblins--pins (make-hash-table :test #'equal))
          (goblins--pending-edits nil)
          (client (start-process "goblins-test-client" nil "sleep" "60"))
          ;; As the server passes them: the last file first.
          (files '(("/goblins:gribble:/x" . nil) ("/goblins:gribble:/tmp/msg" . (3 . nil))))
          (popup (lambda () (and (get-buffer-window goblins--popup-buffer-name)
                                 (with-current-buffer goblins--popup-buffer-name
                                   (buffer-string)))))
          executed refused)
      (cl-letf (((symbol-function 'server-send-string)
                 (lambda (_proc string) (push string refused)))
                ((symbol-function 'server-delete-client) #'ignore))
        (unwind-protect
            (let ((execute (lambda (&rest args) (push args executed))))
              (terminal "session-a")
              ;; The selected goblin's request shows until an edit arrives.
              (goblins-select-goblin source "session-a")
              (should (string-match-p "asks for hello" (funcall popup)))
              ;; Asked: nothing opens; the edit takes over the popup.
              (goblins-tramp--server-execute execute client files nil)
              (should-not executed)
              (should (string-match-p "gribble wants to open in Emacs\n +/tmp/msg:3\n +/x"
                                      (funcall popup)))
              (with-current-buffer goblins--popup-buffer-name
                (goblins-request-popup-accept))
              (should (equal executed (list (list client files nil))))
              ;; Settled: back to the goblin's request.
              (should (string-match-p "asks for hello" (funcall popup)))
              ;; Refused: emacsclient fails with an explanation.
              (goblins-tramp--server-execute execute client files nil)
              (with-current-buffer goblins--popup-buffer-name
                (goblins-request-popup-deny))
              (should (string-match-p "\\`-error .*refused" (car refused)))
              (should (= (length executed) 1))
              ;; A requester that went away is dropped.
              (let ((gone (start-process "goblins-test-gone" nil "true")))
                (goblins-tramp--server-execute execute gone files nil)
                (goblins-test--wait (lambda () (not (process-live-p gone))))
                (should-not (goblins--live-edits)))
              ;; Policies, and other emacsclient files, don't ask.
              (setq refused nil)
              (let ((goblins-editor-approval 'deny))
                (goblins-tramp--server-execute execute client files nil))
              (should refused)
              (let ((goblins-editor-approval 'approve))
                (goblins-tramp--server-execute execute client files nil))
              (goblins-tramp--server-execute execute client '(("/tmp/local" . nil)) nil)
              (should (= (length executed) 3))
              (should-not goblins--pending-edits))
          (delete-process client))))))

(ert-deftest goblins-workspace-without-a-last-goblin-shows-the-latest-terminal ()
  (goblins-test--workspace (list (goblins-test--session "session-a")
                                 (goblins-test--session "session-b"))
    (let ((older (terminal "session-a"))
          (newer (terminal "session-b")))
      (bury-buffer older)
      (switch-to-buffer newer)
      (goblins--workspace-restore source)
      (should (equal (goblins-test--layout) (list newer))))))

(ert-deftest goblins-sidebar-toggle-hides-it-and-keeps-the-workspace ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p"))
    (let ((dired (generate-new-buffer " *dired*"))
          (split-width-threshold 20)
          (sidebar-window (lambda () (get-buffer-window goblins--sidebar-buffer-name))))
      (unwind-protect
          (progn
            (terminal "session-a")
            (terminal "session-b")
            (goblins-select-goblin source "session-a" dired)
            ;; Shown but not selected: hide it.
            (goblins-sidebar-toggle)
            (should-not (funcall sidebar-window))
            (should (eq (window-buffer) dired))
            ;; Still the workspace: buffers go beside the agent, and each
            ;; goblin keeps its windows.
            (should (goblins--in-workspace-p))
            (goblins-select-goblin source "session-b")
            (should (equal (goblins-test--layout) (list (terminal "session-b"))))
            (goblins-select-goblin source "session-a")
            (should (equal (goblins-test--layout) (list (terminal "session-a") dired)))
            ;; Hidden: show it, selected.
            (goblins-sidebar-toggle)
            (should (eq (selected-window) (funcall sidebar-window)))
            (should (equal (goblins-test--layout) (list (terminal "session-a") dired)))
            ;; Selected: hide it, and select the agent.
            (goblins-sidebar-toggle)
            (should-not (funcall sidebar-window))
            (should (eq (window-buffer) (terminal "session-a")))
            ;; Another tab's windows are not the workspace.
            (goblins--release-agent-window (goblins--agent-window))
            (dolist (window (window-list nil 'no-minibuf))
              (set-window-parameter window 'goblins-workspace nil))
            (should-not (goblins--in-workspace-p)))
        (kill-buffer dired)))))

(ert-deftest goblins-workspace-entry-points-select-the-sidebar ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p"))
    (let* ((sidebar-window (lambda () (get-buffer-window goblins--sidebar-buffer-name)))
           (leave (lambda ()
                    (goblins-test--leave-workspace-windows)
                    (when (funcall sidebar-window) (delete-window (funcall sidebar-window)))
                    (set-frame-parameter nil 'goblins-workspace-saved-config nil))))
      (terminal "session-a")
      (terminal "session-b")
      (setq goblins--last-goblin '("/tmp/goblins-test" . "session-a"))
      (cl-letf (((symbol-function 'goblins--status-buffer) (lambda (&optional _) source))
                ((symbol-function 'goblins-refresh) #'ignore))
        (unwind-protect
            (progn
              ;; Opening the workspace selects the sidebar, beside the goblin
              ;; selected last.
              (funcall leave)
              (should-not (goblins--in-workspace-p))
              (goblins-workspace)
              (should (eq (selected-window) (funcall sidebar-window)))
              (should (equal (goblins-test--layout) (list (terminal "session-a"))))
              ;; dwim elsewhere selects it, showing it if hidden.
              (select-window (goblins--agent-window))
              (goblins-sidebar-dwim)
              (should (eq (selected-window) (funcall sidebar-window)))
              (goblins-sidebar-toggle)
              (should-not (funcall sidebar-window))
              (goblins-sidebar-dwim)
              (should (eq (selected-window) (funcall sidebar-window)))
              ;; In the workspace, goblins-workspace opens the menu instead.
              (select-window (goblins--agent-window))
              (let ((menu nil))
                (cl-letf (((symbol-function 'goblins-workspace-menu) (lambda () (setq menu t))))
                  (goblins-workspace))
                (should menu))
              (should (eq (window-buffer) (terminal "session-a")))
              ;; Showing a goblin in the workspace selects its agent, and opens
              ;; the workspace when needed.
              (goblins-show-in-workspace source "session-b")
              (should (eq (window-buffer) (terminal "session-b")))
              (funcall leave)
              (delete-other-windows)
              (goblins-show-in-workspace source "session-a")
              (should (funcall sidebar-window))
              (should (equal (goblins-test--layout) (list (terminal "session-a"))))
              (should (eq (window-buffer) (terminal "session-a"))))
          (set-frame-parameter nil 'goblins-workspace-saved-config nil))))))

(ert-deftest goblins-workspace-moves-between-windows-and-goblins ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p")
                                 (goblins-test--session "session-c" :cwd "/q")
                                 (goblins-test--session "session-d" :cwd "/q" :state "stopped"))
    (let ((dired (generate-new-buffer " *dired*"))
          (notes (generate-new-buffer " *notes*"))
          (split-width-threshold 20)
          (shown (lambda () (cdr goblins--last-goblin)))
          (goblins-request-popups nil))
      (unwind-protect
          (progn
            (dolist (id '("session-a" "session-b" "session-c")) (terminal id))
            (with-current-buffer source
              (setq goblins--snapshot
                    (plist-put goblins--snapshot :permissions
                               (vector (goblins-test--request-in "x" "session-a" "pending")
                                       (goblins-test--request-in "y" "session-c" "pending")))))
            (goblins--sidebar-render)
            (goblins-select-goblin source "session-a" dired)
            (set-window-buffer (split-window (selected-window) nil 'below) notes)
            ;; Windows: the agent, then those beside it in turn.
            (goblins-workspace-select-agent)
            (should (eq (window-buffer) (terminal "session-a")))
            (goblins-workspace-select-right)
            (should (eq (window-buffer) dired))
            (goblins-workspace-select-right)
            (should (eq (window-buffer) notes))
            (goblins-workspace-select-right)
            (should (eq (window-buffer) dired))
            (goblins-workspace-select-sidebar)
            (should (eq (window-buffer) sidebar))
            ;; Goblins in sidebar order, wrapping, never a stopped one; the
            ;; sidebar's selection follows.
            (goblins-workspace-next-goblin)
            (should (equal (funcall shown) "session-b"))
            (should (eq (window-buffer) (terminal "session-b")))
            (should (equal (with-current-buffer sidebar (get-text-property (point) 'goblins-id))
                           "session-b"))
            (goblins-workspace-next-goblin)
            (should (equal (funcall shown) "session-c"))
            (goblins-workspace-next-goblin)
            (should (equal (funcall shown) "session-a"))
            (should (equal (goblins-test--layout) (list (terminal "session-a") dired notes)))
            (goblins-workspace-previous-goblin)
            (should (equal (funcall shown) "session-c"))
            ;; The goblin selected before, back and forth.
            (cl-letf (((symbol-function 'goblins--status-buffer) (lambda (&optional _) source)))
              (goblins-workspace-last-goblin)
              (should (equal (funcall shown) "session-a"))
              (goblins-workspace-last-goblin)
              (should (equal (funcall shown) "session-c")))
            ;; Waiting goblins only.
            (goblins-workspace-next-waiting)
            (should (equal (funcall shown) "session-a"))
            (goblins-workspace-next-waiting)
            (should (equal (funcall shown) "session-c"))
            (goblins-workspace-previous-waiting)
            (should (equal (funcall shown) "session-a"))
            ;; Even in a folded group.
            (with-current-buffer sidebar
              (goblins--sidebar-set-collapsed '((dir nil "/q")))
              (goblins--sidebar-render))
            (goblins-workspace-previous-waiting)
            (should (equal (funcall shown) "session-c"))
            (should (equal (with-current-buffer sidebar (get-text-property (point) 'goblins-id))
                           "session-c")))
        (kill-buffer dired)
        (kill-buffer notes)))))

(ert-deftest goblins-sidebar-moves-back-to-the-previous-waiting-goblin ()
  (goblins-test--sidebar goblins-test--raid-sessions
    (with-current-buffer source
      (setq goblins--snapshot
            (plist-put goblins--snapshot :permissions
                       (vector (goblins-test--request-in "x" "session-a" "pending")
                               (goblins-test--request-in "y" "session-e" "pending")))))
    (goblins--sidebar-render)
    (should (eq (key-binding (kbd "N")) #'goblins-sidebar-previous-pending))
    (goto-char (point-min))
    (goblins-sidebar-previous-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-e"))
    (goblins-sidebar-previous-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-a"))
    (goblins-sidebar-previous-pending)
    (should (equal (get-text-property (point) 'goblins-id) "session-e"))))

(ert-deftest goblins-request-popup-decides-from-other-windows ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :cwd "/p"))
    (let (decided)
      (terminal "session-a")
      (cl-letf (((symbol-function 'goblins--decide-record)
                 (lambda (record approved) (setq decided (list (plist-get record :id) approved)))))
        (goblins-select-goblin source "session-a")
        (should (eq (window-buffer) (terminal "session-a")))
        (goblins-request-popup-deny)
        (should (equal decided '("a" nil)))
        (goblins--popup-close)
        (should-error (goblins-request-popup-accept) :type 'user-error)))))

(ert-deftest goblins-workspace-quit-leaves-no-agent-window ()
  (goblins-test--workspace (list (goblins-test--session "session-a"))
    (terminal "session-a")
    (goblins-select-goblin source "session-a")
    (should (window-parameter (goblins--agent-window) 'goblins-agent))
    (let ((goblins-workspace-quit-function #'ignore))
      (goblins-workspace-quit))
    (should-not (cl-some (lambda (w) (or (window-parameter w 'goblins-workspace)
                                         (window-parameter w 'goblins-agent)
                                         (window-dedicated-p w)))
                         (goblins--main-windows)))))

(ert-deftest goblins-editor-files-open-right-of-their-goblin ()
  (goblins-test--workspace (list (goblins-test--session "session-a" :path "gribble"))
    (let ((goblins--pins (make-hash-table :test #'equal))
          (file (generate-new-buffer " *goblin file*"))
          (name (buffer-name source)))
      (unwind-protect
          (progn
            (with-current-buffer source (rename-buffer "*Goblins: /tmp/goblins-test*"))
            (with-current-buffer file (setq default-directory "/goblins:gribble:/tmp/"))
            (goblins-pin-host "gribble" "/tmp/goblins-test" "daemon-a" "session-a")
            (terminal "session-a")
            (goblins-tramp--server-switch-buffer
             (lambda (next &rest _) (funcall server-window next)) file)
            (should (equal (goblins-test--layout) (list (terminal "session-a") file))))
        (with-current-buffer source (rename-buffer name))
        (kill-buffer file)))))

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
        (should (equal call '("/p/one" nil)))
        (goblins-sidebar-next)
        (setq call nil)
        (goblins-sidebar-run)
        (should (equal (car call) "/p/one"))
        (goblins-sidebar-run-in-project)
        (should (equal call '("/chosen/" nil)))
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
        (goblins-sidebar-dwim)
        (should (eq (buffer-local-value 'goblins--view-source shown) source))
        (should (equal (buffer-local-value 'goblins--view-session shown) "crew"))
        ;; Directory headings still fold.
        (goblins-sidebar-next)
        (goblins-sidebar-dwim)
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
                         (lambda (_prompt choices &rest _) (car (goblins-test--choices choices)))))
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
                         (progn (setq asked (goblins-test--choices choices)) (car asked))
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
      (cl-letf (((symbol-function 'completing-read)
                 (lambda (_p choices &rest _) (car (goblins-test--choices choices)))))
        (goblins-sidebar-invite))
      (should (equal (pop calls) '(raids.invite :raid "crew" :sessions ["session-c"]))))))

(defvar goblins-test-shadow-mode-map (make-sparse-keymap))
(define-minor-mode goblins-test-shadow-mode
  "A minor mode binding motion-state keys, as evil-snipe or a leader does.")

(ert-deftest goblins-sidebar-keys-win-over-evil-minor-modes ()
  (skip-unless (require 'evil nil t))
  (evil-define-key* 'motion goblins-test-shadow-mode-map
    "t" #'ignore "j" #'ignore (kbd "SPC") #'ignore)
  (with-temp-buffer
    (goblins-sidebar-mode)
    (evil-local-mode 1)
    (goblins-test-shadow-mode 1)
    (evil-normalize-keymaps)
    (should (eq evil-state 'motion))
    ;; The sidebar's own keys win; others still reach Evil and minor modes.
    (should (eq (key-binding "t") #'goblins-sidebar-terminal))
    (should (eq (key-binding "j") #'goblins-sidebar-next))
    (should (eq (key-binding (kbd "SPC")) #'ignore))
    (should (eq (key-binding "gg") #'evil-goto-first-line))
    (should (eq (key-binding "gr") #'goblins-sidebar-refresh))
    (evil-local-mode -1)))

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
    (goblins-sidebar-fold)
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
    (goblins-sidebar-fold)
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
      (should (string-match-p "previous goblin waiting for a decision" (buffer-string)))
      (should (string-match-p "dissolve" (buffer-string))))
    (save-window-excursion (goblins-raid-help))
    (with-current-buffer "*Goblins Keys*"
      (should (string-match-p "set the member's role" (buffer-string)))
      (kill-buffer))))

;;; Completion: commands on a chosen goblin, Embark, Marginalia, Consult

(defmacro goblins-test--terminals (sessions &rest body)
  "Run BODY in `goblins-test--sidebar' over SESSIONS, with an agent terminal
for session-a, `terminal', and an unrelated buffer, `other'."
  (declare (indent 1))
  `(goblins-test--sidebar ,sessions
     (let ((goblins--terminal-buffers (make-hash-table :test #'equal))
           (terminal (generate-new-buffer "*Goblin: session-a*"))
           (other (generate-new-buffer " *not a goblin*")))
       (unwind-protect
           (progn
             (with-current-buffer terminal
               (setq-local goblins--session-key (list "/tmp/goblins-test" "daemon-a" "session-a")))
             (puthash (list "/tmp/goblins-test" "daemon-a" "session-a") terminal
                      goblins--terminal-buffers)
             (cl-letf (((symbol-function 'goblins--status-buffer) (lambda (&optional _) source))
                       ((symbol-function 'goblins--connect) (lambda (&optional _) source)))
               ,@body))
         (kill-buffer terminal)
         (kill-buffer other)))))

(ert-deftest goblins-commands-act-on-a-chosen-goblin ()
  (goblins-test--terminals (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p"))
    (let (shown)
      (cl-letf (((symbol-function 'goblins--display-main) (lambda (b) (setq shown b)))
                ((symbol-function 'completing-read) (lambda (&rest _) "session-b")))
        (call-interactively #'goblins-goblin-info)
        (should (eq shown (get-buffer goblins--info-buffer-name)))
        (should (equal (buffer-local-value 'goblins--view-session shown) "session-b")))
      ;; An agent terminal's buffer name chooses its goblin, as an Embark
      ;; action on that buffer injects it; the table accepts it unlisted.
      (cl-letf (((symbol-function 'goblins--display-main) (lambda (b) (setq shown b)))
                ((symbol-function 'completing-read)
                 (lambda (_prompt table &rest _)
                   (should (test-completion (buffer-name terminal) table))
                   (should-not (member (buffer-name terminal) (goblins-test--choices table)))
                   (should-not (test-completion (buffer-name other) table))
                   (buffer-name terminal))))
        (call-interactively #'goblins-goblin-permissions)
        (should (equal (buffer-local-value 'goblins--view-session shown) "session-a"))))))

(ert-deftest goblins-requests-choose-among-pending-requests ()
  (goblins-test--terminals (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p"))
    (with-current-buffer source
      (setq goblins--snapshot
            (plist-put goblins--snapshot :permissions
                       (vector (goblins-test--request-in "x" "session-a" "pending" "jq")
                               (goblins-test--request-in "y" "session-b" "pending" "git")
                               (goblins-test--request-in "z" "session-b" "denied" "curl")
                               (goblins-test--request-in "w" "session-b" "pending" "git")))))
    (let (offered decided opened shown)
      (cl-letf (((symbol-function 'completing-read)
                 (lambda (_prompt table &rest _)
                   (let* ((metadata (completion-metadata "" table nil))
                          (choices (goblins-test--choices table)))
                     (should (eq (completion-metadata-get metadata 'category) 'goblins-request))
                     (setq offered (mapcar #'substring-no-properties choices))
                     (should (string-match-p "package +because"
                                             (substring-no-properties
                                              (funcall (completion-metadata-get
                                                        metadata 'annotation-function)
                                                       (cadr choices)))))
                     (cadr choices))))
                ((symbol-function 'goblins--decide-record)
                 (lambda (record approved) (setq decided (list (plist-get record :id) approved))))
                ((symbol-function 'goblins-open-session)
                 (lambda (src id) (setq opened (list src id))))
                ((symbol-function 'goblins--display-main) (lambda (b) (setq shown b))))
        (call-interactively #'goblins-requests)
        ;; Alike requests are numbered apart.
        (should (equal offered '("session-a: jq" "session-b: git" "session-b: git <2>")))
        ;; The goblin's requests, that one focused.
        (with-current-buffer shown
          (should (equal goblins--view-session "session-b"))
          (should (equal (oref (magit-current-section) value) "y")))
        (call-interactively #'goblins-request-deny)
        (should (equal decided '("y" nil)))
        (call-interactively #'goblins-request-accept)
        (should (equal decided '("y" t)))
        (call-interactively #'goblins-request-goblin)
        (should (equal opened (list source "session-b")))))))

(ert-deftest goblins-embark-acts-on-goblins-and-their-terminals ()
  (skip-unless (require 'embark nil t))
  (goblins-test--terminals (list (goblins-test--session "session-a" :cwd "/p"))
    (should (equal (alist-get 'goblin embark-keymap-alist) '(goblins-embark-goblin-map)))
    (should (equal (alist-get 'goblins-terminal embark-keymap-alist)
                   '(goblins-embark-terminal-map)))
    ;; A goblin's terminal among buffers is a goblins-terminal target, in
    ;; switch-to-buffer and in consult-buffer alike; other buffers stay buffers.
    (should (equal (funcall (alist-get 'buffer embark-transformer-alist)
                            'buffer (buffer-name terminal))
                   (cons 'goblins-terminal (buffer-name terminal))))
    (should (equal (funcall (alist-get 'buffer embark-transformer-alist)
                            'buffer (buffer-name other))
                   (cons 'buffer (buffer-name other))))
    (should (equal (funcall (alist-get 'multi-category embark-transformer-alist)
                            'multi-category
                            (propertize (buffer-name terminal)
                                        'multi-category (cons 'buffer terminal)))
                   (cons 'goblins-terminal (buffer-name terminal))))
    ;; Goblin actions, and on terminals Embark's buffer actions as well.
    (should (eq (lookup-key goblins-embark-goblin-map (kbd "RET")) #'goblins-goblin-show))
    (should (eq (lookup-key goblins-embark-goblin-map "W") #'goblins-show-in-workspace))
    (should (eq (lookup-key goblins-embark-terminal-map "p") #'goblins-goblin-permissions))
    (should (eq (lookup-key goblins-embark-terminal-map "W") #'goblins-show-in-workspace))
    (should (eq (lookup-key goblins-embark-terminal-map "k") #'kill-buffer))
    (should (eq (lookup-key goblins-embark-terminal-map "w") #'embark-copy-as-kill))
    ;; Requests: accept, deny, their goblin.
    (should (equal (alist-get 'goblins-request embark-keymap-alist) '(goblins-embark-request-map)))
    (should (eq (lookup-key goblins-embark-request-map "a") #'goblins-request-accept))
    (should (eq (lookup-key goblins-embark-request-map "d") #'goblins-request-deny))
    (should (eq (lookup-key goblins-embark-request-map "g") #'goblins-request-goblin))))

(ert-deftest goblins-marginalia-describes-agent-terminals-like-goblins ()
  (skip-unless (require 'marginalia nil t))
  (goblins-test--terminals (list (goblins-test--session "session-a" :cwd "/p" :agent_state "idle"))
    (let ((annotate (car (alist-get 'buffer (if (boundp 'marginalia-annotators)
                                                 marginalia-annotators
                                               marginalia-annotator-registry)))))
      (should (eq annotate #'goblins-marginalia-annotate-buffer))
      (should (string-match-p "shell +○ idle +/p !1 \\[term\\]"
                              (substring-no-properties (funcall annotate (buffer-name terminal)))))
      (should (string-match-p "shell +○ idle"
                              (substring-no-properties (funcall annotate terminal))))
      (should-not (string-match-p "○ idle" (or (funcall annotate (buffer-name other)) ""))))))

(ert-deftest goblins-switch-with-consult-lists-terminals-and-other-goblins ()
  (skip-unless (require 'consult nil t))
  (goblins-test--terminals (list (goblins-test--session "session-a" :cwd "/p")
                                 (goblins-test--session "session-b" :cwd "/p")
                                 (goblins-test--session "session-c" :state "stopped"))
    (let (sources opened)
      (cl-letf (((symbol-function 'consult-buffer) (lambda (s) (setq sources s)))
                ((symbol-function 'goblins-open-session)
                 (lambda (src id) (setq opened (list src id)))))
        (goblins-switch)
        (let ((terminals (car sources))
              (others (cadr sources)))
          (should (equal (mapcar (lambda (s) (plist-get s :narrow)) sources) '(?t ?a)))
          (should (equal (funcall (plist-get terminals :items))
                         (list (cons (buffer-name terminal) terminal))))
          (should (string-match-p "/p" (funcall (plist-get terminals :annotate) terminal)))
          (funcall (plist-get terminals :action) terminal)
          (should (equal opened (list source "session-a")))
          ;; Running goblins without a terminal, in the goblin category.
          (should (eq (plist-get others :category) 'goblin))
          (let ((items (funcall (plist-get others :items))))
            (should (equal items '("session-b")))
            (funcall (plist-get others :action) (car items)))
          (should (equal opened (list source "session-b"))))))))

(ert-deftest goblins-consult-buffer-narrows-to-agent-terminals ()
  (skip-unless (require 'consult nil t))
  (goblins-test--terminals (list (goblins-test--session "session-a" :cwd "/p"))
    (should (memq 'goblins-consult-source consult-buffer-sources))
    (should (eq (plist-get goblins-consult-source :narrow) ?g))
    (should (plist-get goblins-consult-source :hidden))
    (should (equal (funcall (plist-get goblins-consult-source :items))
                   (list (cons (buffer-name terminal) terminal))))))

;;; Switching and attention

(ert-deftest goblins-switch-chooses-among-running-goblins ()
  (goblins-test--raid-sidebar
    (let (offered category annotation opened (goblins-switch-use-consult nil))
      (cl-letf (((symbol-function 'goblins--connect) (lambda (&optional _) source))
                ((symbol-function 'goblins-open-session)
                 (lambda (src id) (setq opened (list src id))))
                ((symbol-function 'completing-read)
                 (lambda (_prompt table &rest _)
                   (let ((choices (goblins-test--choices table)))
                     (setq offered choices
                           category (completion-metadata-get
                                     (completion-metadata "" table nil) 'category)
                           annotation (funcall (completion-metadata-get
                                                (completion-metadata "" table nil)
                                                'annotation-function)
                                               (car (member "session-a" choices)))))
                   "session-a")))
        (goblins-switch))
      (should (equal (sort offered #'string<) '("session-a" "session-b" "session-c")))
      (should (eq category 'goblin))
      (should (string-match-p "shell.*raid crew.*/p/one !1" annotation))
      (should (equal opened (list source "session-a"))))))

(ert-deftest goblins-goblin-candidates-describe-each-goblin ()
  (goblins-test--sidebar
      (list (goblins-test--session "session-a" :cwd "/p" :name "claude" :agent_state "waiting-input"
                                   :inbox 2 :raid '(:id "r1" :name "crew"))
            (goblins-test--session "session-b" :cwd "/q" :agent_state "idle")
            (goblins-test--session "session-c" :state "stopped"))
    (let ((goblins--terminal-buffers (make-hash-table :test #'equal))
          (terminal (generate-new-buffer " *agent a*")))
      (unwind-protect
          (progn
            (puthash (list "/tmp/goblins-test" "daemon-a" "session-a") terminal
                     goblins--terminal-buffers)
            (cl-letf (((symbol-function 'completing-read)
                       (lambda (_prompt table &rest _)
                         (let* ((metadata (completion-metadata "" table nil))
                                (annotate (completion-metadata-get metadata 'annotation-function)))
                           (should (eq (completion-metadata-get metadata 'category) 'goblin))
                           (should (equal (goblins-test--choices table) '("session-a" "session-b")))
                           (should (string-match-p
                                    "claude +◆ needs input +raid crew +/p !1 ✉2 \\[term\\]"
                                    (substring-no-properties
                                     (funcall annotate (car (goblins-test--choices table))))))
                           (should (string-match-p
                                    "shell +○ idle +/q$"
                                    (substring-no-properties
                                     (funcall annotate (cadr (goblins-test--choices table))))))
                           "session-b"))))
              (should (equal (goblins--read-goblin "Goblin: " source) (cons source "session-b")))))
        (kill-buffer terminal)))))

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

;; The sandbox has no display, so the agents' own Ctrl+V cannot reach the
;; clipboard; Emacs writes the image into the goblin and pastes its path.
(ert-deftest goblin-paste-image-writes-into-the-goblin-and-pastes-its-path ()
  (let ((terminal (generate-new-buffer " *goblins test terminal*"))
        (png (unibyte-string #x89 ?P ?N ?G #x0d #x0a #x1a #x0a #x00 #xff))
        directories written pasted redraws)
    (unwind-protect
        (with-current-buffer terminal
          (setq-local goblins--session-key (list "/tmp/goblins-test" "daemon-a" "session-c"))
          (goblin-mode 1)
          (should (eq (key-binding (kbd "C-c g v")) #'goblin-paste-image))
          (cl-letf (((symbol-function 'goblins--status-buffer) (lambda (&optional _) terminal))
                    ((symbol-function 'goblins--pinned-terminal-host) (lambda () "gribble"))
                    ((symbol-function 'gui-get-selection)
                     (lambda (_selection type)
                       (pcase type
                         ('TARGETS [TARGETS UTF8_STRING image/png])
                         ('image/png png))))
                    ((symbol-function 'make-directory)
                     (lambda (dir &optional _parents) (push dir directories)))
                    ((symbol-function 'write-region)
                     (lambda (_start _end file &rest _)
                       (push (list file (buffer-string) enable-multibyte-characters
                                   coding-system-for-write)
                             written)))
                    ((symbol-function 'ghostel-paste-string) (lambda (s) (push s pasted)))
                    ((symbol-function 'run-with-timer)
                     (lambda (delay &rest _) (push delay redraws))))
            (goblin-paste-image))
          (should (equal (sort redraws #'<) '(0.2 1)))
          (should (equal directories '("/goblins:gribble:/tmp/goblins-clipboard/")))
          (pcase-let ((`((,file ,bytes ,multibyte ,coding)) written))
            (should (string-match-p "\\`/goblins:gribble:/tmp/goblins-clipboard/[0-9-]+\\.png\\'" file))
            (should (equal bytes png))
            (should-not multibyte)
            (should (eq coding 'no-conversion))
            (should (equal pasted (list (string-remove-prefix "/goblins:gribble:" file))))))
      (kill-buffer terminal)))
  (should (equal (goblins--image-extension 'image/jpeg) "jpg"))
  (should (equal (goblins--image-extension 'image/svg+xml) "svg"))
  (should (equal (goblins--image-extension 'image/webp) "webp"))
  (with-temp-buffer
    (should-error (goblin-paste-image) :type 'user-error)))

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
    (goblins-test--wait (lambda () (string-match-p "gamma (shell)\n  ! hello (package)"
                                                   (goblins-test--sidebar-text))))
    (let ((text (goblins-test--sidebar-text)))
      ;; A rule (blank in plain text) parts the goblins of a directory.
      (should (string-match-p (concat (regexp-quote project-one) "\n  alpha (shell)\n +\n  beta (shell)")
                              text))
      ;; The directory of the waiting goblin carries its count too.
      (should (string-match-p "/ !1\n  delta (shell)\n +\n  gamma (shell)\n  ! hello (package)\n"
                              text)))
    ;; Selecting the waiting goblin pops up its request at the bottom.
    (goblins-select-goblin source (plist-get (goblins-test--live-session source "gamma") :id))
    (should (window-parameter (get-buffer-window goblins--popup-buffer-name) 'window-side))
    (should (string-match-p "gamma asks for hello"
                            (with-current-buffer goblins--popup-buffer-name (buffer-string))))
    ;; Attention: the mode line counts the request.
    (goblins-attention-mode 1)
    (should (string-match-p "Goblins!1" goblins--mode-line-string))
    ;; n finds the waiting goblin, p shows its request focused, d denies it.
    (goblins-test--in-sidebar)
    (goblins-sidebar-next-pending)
    (should (looking-at-p "  gamma"))
    (goblins-sidebar-permissions)
    ;; Views open right of the status buffer, which holds the agent's place.
    (should (eq (window-buffer (goblins--main-window)) source))
    (with-current-buffer (window-buffer (goblins--right-window))
      (should (eq major-mode 'goblins-permissions-mode))
      (should (equal (plist-get (oref (magit-current-section) record) :package) "hello"))
      (goblins-permissions-deny)
      (goblins-test--wait (lambda () (string-match-p "History (1)" (buffer-string))))
      (should (string-match-p "hello  denied" (buffer-string))))
    (goblins-test--wait (lambda () (equal goblins--mode-line-string "")))
    ;; Decided: the popup is gone.
    (should-not (get-buffer-window goblins--popup-buffer-name))
    ;; I shows the goblin's info.
    (goblins-test--in-sidebar "^  alpha")
    (goblins-sidebar-info)
    (with-current-buffer (window-buffer (goblins--right-window))
      (should (string-match-p (concat "Directory +" (regexp-quote (directory-file-name project-one)))
                              (buffer-string))))
    ;; q in a view puts back what its window showed before: the
    ;; permissions view, then (that window was opened for it) nothing.
    (select-window (goblins--right-window))
    (call-interactively (key-binding "q"))
    (should (equal (mapcar #'window-buffer (goblins--main-windows))
                   (list source (get-buffer goblins--permissions-buffer-name))))
    (select-window (goblins--right-window))
    (call-interactively (key-binding "q"))
    (should (equal (mapcar #'window-buffer (goblins--main-windows)) (list source)))
    ;; r creates a raid around alpha and opens its page.
    (goblins-test--in-sidebar "^  alpha")
    (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "crew")))
      (goblins-raid))
    (setq page (get-buffer goblins--raid-buffer-name))
    (should (eq (window-buffer (goblins--right-window)) page))
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
    (goblins-test--in-sidebar "^  beta")
    (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "crew")))
      (goblins-sidebar-invite))
    (goblins-test--wait
     (lambda () (equal (plist-get (plist-get (goblins-test--live-session source "beta") :raid) :name)
                       "crew")))
    ;; K stops delta, which then leaves the sidebar.
    (goblins-test--in-sidebar "^  delta")
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
        (let ((goblins-switch-use-consult nil)) (goblins-switch)))
      (let ((terminal (window-buffer (goblins--main-window))))
        ;; The agent replaces the status buffer, alone beside the sidebar.
        (should (equal (mapcar #'window-buffer (goblins--main-windows)) (list terminal)))
        (with-current-buffer terminal
          (should (derived-mode-p 'ghostel-mode))
          (should goblin-mode)
          (should (eq (key-binding (kbd "C-c g p")) #'goblin-permissions)))
        (goblins-test--in-sidebar "^  alpha")
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

(ert-deftest goblins-editor-approval-applies-before-tramp-loads ()
  "The first edit in a fresh Emacs waits too: visiting it is what loads TRAMP."
  (with-temp-buffer
    (let ((code (call-process
                 (expand-file-name invocation-name invocation-directory) nil t nil
                 "--batch" "-Q" "-L" (file-name-directory (locate-library "goblins"))
                 "--eval" "(progn (require 'goblins) (require 'server))"
                 "--eval" (concat "(let ((client (start-process \"client\" nil \"sleep\" \"10\")))"
                                  " (server-execute client '((\"/goblins:g:/x\")) nil nil nil nil nil nil)"
                                  " (kill-emacs (if (and goblins--pending-edits"
                                  " (not (get-file-buffer \"/goblins:g:/x\"))) 0 1)))"))))
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
    (goblins-sidebar-fold)
    (goto-char (point-min))
    (re-search-forward "^▾ /p/two/")
    (goblins-sidebar-fold)
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


;;; TRAMP

(require 'goblins-tramp)
(defvar goblins-test--pwned nil)

(ert-deftest goblins-tramp-hosts-and-osc7-directories ()
  (should (equal (goblins-tramp-host "gribble/snaga") "gribble.snaga"))
  (should (equal (goblins--osc7-directory "file://sandbox/tmp/a%20b" "gribble")
                 "/goblins:gribble:/tmp/a b/"))
  (should (equal (goblins--osc7-directory "kitty-shell-cwd://sandbox/w" "a.b")
                 "/goblins:a.b:/w/"))
  (should-not (goblins--osc7-directory "file://sandbox/x%0Ay" "gribble"))
  (should-not (goblins--osc7-directory "/plain" "gribble")))

(ert-deftest goblins-terminals-track-directories-as-goblins-paths ()
  (with-temp-buffer
    (setq-local ghostel-identity '((kind . goblins-shell) (host . "gribble.snaga")
                                   (session . "i-s3") (instance . "i")))
    (setq-local goblins--directory "/state")
    (let ((goblins--pins (make-hash-table :test #'equal))
          (called nil))
      (goblins--ghostel-directory (lambda (_) (setq called t)) "file://sandbox/tmp/x")
      (should-not called)
      (should (equal default-directory "/goblins:gribble.snaga:/tmp/x/"))
      ;; Pinned from the terminal's identity, before any TRAMP use.
      (should (equal (goblins-pin "gribble.snaga") '("/state" "i" "i-s3")))
      ;; A host pinned to another goblin falls back to the session ID.
      (puthash "gribble.snaga" '("/state" "i" "i-s1") goblins--pins)
      (goblins--ghostel-directory #'ignore "file://sandbox/w")
      (should (equal default-directory "/goblins:i-s3:/w/")))
    (setq-local ghostel-identity '((kind . exec)))
    (let ((called nil))
      (goblins--ghostel-directory (lambda (_) (setq called t)) "file://host/tmp/y")
      (should called))))

(ert-deftest goblins-agent-terminals-stay-in-their-project ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p/one" :path "gribble")
                               (goblins-test--session "session-b" :path "snaga"))
    (let ((goblins--pins (make-hash-table :test #'equal)))
      (with-temp-buffer
        (setq-local goblins--directory "/tmp/goblins-test")
        ;; The agent's reports don't move it: it stays in the project.
        (setq-local ghostel-identity '((kind . goblins) (session . "session-a")
                                       (instance . "daemon-a") (host . "gribble")))
        (goblins--ghostel-directory #'ignore "file://sandbox/tmp/x")
        (should (equal default-directory "/p/one/"))
        ;; Without a project, its own /workspace, inside the goblin.
        (setq-local ghostel-identity '((kind . goblins) (session . "session-b")
                                       (instance . "daemon-a") (host . "snaga")))
        (goblins--ghostel-directory #'ignore "file://sandbox/tmp/x")
        (should (equal default-directory "/goblins:snaga:/workspace/"))
        (should (equal (goblins-pin "snaga") '("/tmp/goblins-test" "daemon-a" "session-b")))))))

(ert-deftest goblins-find-file-visits-any-path-inside-the-goblin ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p/one" :path "gribble"))
    (let ((goblins--pins (make-hash-table :test #'equal))
          (file (generate-new-buffer " *goblin file*"))
          read shown)
      (unwind-protect
          (cl-letf (((symbol-function 'read-file-name)
                     (lambda (&rest _) (setq read default-directory) "/goblins:gribble:/etc/hosts"))
                    ((symbol-function 'find-file-noselect)
                     (lambda (name &rest _) (should (equal name "/goblins:gribble:/etc/hosts")) file))
                    ((symbol-function 'goblins-select-goblin)
                     (lambda (src id right &rest _) (setq shown (list src id right)))))
            (goblins-find-file (goblins--snapshot-session
                                (buffer-local-value 'goblins--snapshot source) "session-a")
                               source)
            ;; Reading starts in its working directory, inside the goblin.
            (should (equal read "/goblins:gribble:/p/one/"))
            (should (equal shown (list source "session-a" file))))
        (kill-buffer file))
      (should (eq (key-binding "f") #'goblins-sidebar-find-file)))
    (with-temp-buffer
      (goblin-mode 1)
      (should (eq (key-binding (kbd "C-c g f")) #'goblins-find-file)))))

(ert-deftest goblins-exec-summary-counts-shells-and-tramp ()
  (should (equal (goblins--exec-summary (list :shells 0 :execs 0)) ""))
  (should (equal (substring-no-properties (goblins--exec-summary (list :shells 1 :execs 3)))
                 " · 1 shell · tramp"))
  (should (equal (substring-no-properties (goblins--exec-summary (list :shells 2)))
                 " · 2 shells")))

(ert-deftest goblins-tramp-resolves-names-like-the-daemon ()
  (let ((sessions (list (list :id "s1" :path "gribble" :agent_name "gribble" :state "stopped")
                        (list :id "s2" :path "gribble" :agent_name "gribble" :state "running")
                        (list :id "s3" :path "gribble/snaga" :agent_name "snaga" :state "running")
                        (list :id "s4" :path "other/snaga" :agent_name "snaga" :state "running"))))
    (should (equal (plist-get (goblins-tramp--match "gribble" sessions) :id) "s2"))
    (should (equal (plist-get (goblins-tramp--match "gribble.snaga" sessions) :id) "s3"))
    (should (equal (plist-get (goblins-tramp--match "s1" sessions) :id) "s1"))
    (should-error (goblins-tramp--match "snaga" sessions))
    (should-not (goblins-tramp--match "nobody" sessions))))

(ert-deftest goblins-tramp-files-get-only-safe-local-variables ()
  "The policy applies when local variables are processed, before any hook."
  (setq goblins-test--pwned nil)
  (dolist (name '("/goblins:gribble:/tmp/f.txt" "/tmp/goblins-local.txt"))
    (with-temp-buffer
      (insert "-*- eval: (setq goblins-test--pwned t); fill-column: 33; "
              "goblins-test--unsafe: 7 -*-\n")
      (setq buffer-file-name name)
      (let ((enable-local-variables t)
            (enable-local-eval t)
            (remote (goblins-tramp-file-p name)))
        (cl-letf (((symbol-function 'hack-local-variables-confirm) (lambda (&rest _) t)))
          (hack-local-variables))
        (should (= fill-column 33))
        (should (eq (local-variable-p 'goblins-test--unsafe) (not remote)))
        (should (eq goblins-test--pwned (not remote))))
      (setq buffer-file-name nil)))
  (setq goblins-test--pwned nil))

(ert-deftest goblins-tramp-buffers-are-never-saved-in-desktops ()
  (require 'desktop)
  (should-not (desktop-save-buffer-p "/goblins:gribble:/tmp/f" "f" 'text-mode))
  (should (desktop-save-buffer-p "/tmp/f" "f" 'text-mode))
  ;; Dired and vc-dir buffers visit no file: their directory decides.
  (with-current-buffer (get-buffer-create "goblins-test-dired")
    (unwind-protect
        (progn
          (setq default-directory "/goblins:gribble:/tmp/")
          (should-not (desktop-save-buffer-p nil (buffer-name) 'dired-mode))
          (setq default-directory "/tmp/")
          (should (desktop-save-buffer-p nil (buffer-name) 'dired-mode)))
      (kill-buffer))))

;;; Shells, Eshell and Dired

(defvar ghostel--process)

(defun goblins-test--shell (id number &optional process)
  "A buffer like goblin ID's open shell NUMBER of the test daemon, running PROCESS."
  (let ((buffer (generate-new-buffer " *goblins test shell*")))
    (with-current-buffer buffer
      (setq-local ghostel-identity `((kind . goblins-shell) (session . ,id)
                                     (instance . "daemon-a")))
      (setq-local ghostel--process process)
      (setq-local goblins--directory "/tmp/goblins-test")
      (setq-local goblins--shell-number number)
      (add-hook 'kill-buffer-hook #'goblins-tramp--shell-closed nil t))
    buffer))

(ert-deftest goblins-sidebar-lists-shells-below-their-goblin ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p" :execs 1)
                               (goblins-test--session "session-b" :cwd "/p" :parent "session-a"))
    (let* ((process (start-process "goblins-test-shell" nil "sleep" "60"))
           (first (goblins-test--shell "session-a" 1 process))
           (second (goblins-test--shell "session-a" 2))
           (elsewhere (goblins-test--shell "session-a" 3 process))
           shown)
      (unwind-protect
          (progn
            ;; Another daemon's shell with the same session ID is not listed.
            (with-current-buffer elsewhere (setq goblins--directory "/elsewhere"))
            (goblins--sidebar-render-all)
            (should (equal (goblins-test--sidebar-plain)
                           (concat "▾ /p/ !1\n  session-a (shell)\n  ! hello (package)\n"
                                   "  ⇄ Tramp connected\n  › shell\n  › shell 2 · exited\n"
                                   "  ──\n    session-b (shell)\n")))
            (should (eq (goblins-tramp--shell-number "/tmp/goblins-test" "session-a") 3))
            ;; Movement stops on shells; the goblin's commands apply there too.
            (goto-char (point-min))
            (goblins-sidebar-next 2)
            (should (eq (get-text-property (point) 'goblins-terminal) first))
            (should (equal (goblins--sidebar-goblin-at-point) (cons source "session-a")))
            (should (equal (goblins--sidebar-directory-at-point) "/p"))
            (cl-letf (((symbol-function 'goblins-select-goblin)
                       (lambda (src id right &rest _) (setq shown (list src id right)))))
              (goblins-sidebar-dwim))
            (should (equal shown (list source "session-a" first)))
            ;; Point stays on its shell; a killed shell leaves the sidebar.
            (goblins--sidebar-render)
            (should (eq (get-text-property (point) 'goblins-terminal) first))
            (kill-buffer second)
            (should-not (string-match-p "shell 2" (buffer-string)))
            (should (eq (goblins-tramp--shell-number "/tmp/goblins-test" "session-a") 2)))
        (delete-process process)
        (mapc #'kill-buffer (seq-filter #'buffer-live-p (list first second elsewhere)))))))

(ert-deftest goblins-t-e-and-d-open-a-terminal-eshell-and-dired ()
  (with-temp-buffer
    (goblins-sidebar-mode)
    (dolist (binding '(("t" . goblins-sidebar-terminal) ("e" . goblins-sidebar-eshell)
                       ("d" . goblins-sidebar-dired)))
      (should (eq (key-binding (kbd (car binding))) (cdr binding))))
    (should (memq (key-binding (kbd "S")) '(nil undefined))))
  (with-temp-buffer
    (goblin-mode 1)
    (dolist (binding '(("C-c g t" . goblins-shell) ("C-c g e" . goblins-eshell)
                       ("C-c g d" . goblins-dired)))
      (should (eq (key-binding (kbd (car binding))) (cdr binding)))))
  ;; The sidebar passes the goblin at point and its own daemon's status buffer.
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p"))
    (let (calls)
      (cl-letf (((symbol-function 'goblins-shell) (lambda (&rest args) (push (cons 't args) calls)))
                ((symbol-function 'goblins-eshell) (lambda (&rest args) (push (cons 'e args) calls)))
                ((symbol-function 'goblins-dired) (lambda (&rest args) (push (cons 'd args) calls))))
        (goto-char (point-min))
        (should-error (goblins-sidebar-dired) :type 'user-error)
        (goblins-sidebar-next)
        (goblins-sidebar-terminal)
        (goblins-sidebar-eshell)
        (goblins-sidebar-dired))
      (should (equal (mapcar #'car calls) '(d e t)))
      (dolist (call calls)
        (should (equal (plist-get (nth 1 call) :id) "session-a"))
        (should-not (nth 2 call))
        (should (eq (nth 3 call) source))))))

(ert-deftest goblins-eshell-and-dired-open-in-the-goblins-pinned-directory ()
  (require 'eshell)
  (goblins-test--sidebar (list (goblins-test--session "session-a" :cwd "/p/one" :path "gribble")
                               (goblins-test--session "session-b" :path "snaga")
                               (goblins-test--session "session-c" :path "gone" :state "stopped"))
    (let ((goblins--pins (make-hash-table :test #'equal))
          (snapshot (buffer-local-value 'goblins--snapshot source))
          eshell listed shown)
      (cl-letf (((symbol-function 'eshell-mode) #'ignore)
                ((symbol-function 'dired-noselect)
                 (lambda (directory) (push directory listed) (current-buffer)))
                ((symbol-function 'goblins-select-goblin) (lambda (_src _id b) (setq shown b))))
        (unwind-protect
            (progn
              ;; Eshell starts in the goblin's directory, pinned to it, and is reused.
              (setq eshell (goblins-eshell (goblins--snapshot-session snapshot "session-a")
                                           nil source))
              (should (eq shown eshell))
              (should (equal (buffer-local-value 'default-directory eshell)
                             "/goblins:gribble:/p/one/"))
              (should (equal (goblins-pin "gribble") '("/tmp/goblins-test" "daemon-a" "session-a")))
              (should (eq (goblins-eshell (goblins--snapshot-session snapshot "session-a") nil source)
                          eshell))
              ;; Dired uses /workspace without a known directory, and the
              ;; session ID when the name is pinned to another goblin.
              (puthash "snaga" '("/tmp/goblins-test" "daemon-a" "old") goblins--pins)
              (goblins-dired (goblins--snapshot-session snapshot "session-b") nil source)
              (should (equal (car listed) "/goblins:session-b:/workspace/"))
              ;; From a /goblins: buffer, its pinned goblin.
              (with-temp-buffer
                (setq default-directory "/goblins:gribble:/tmp/")
                (goblins-dired nil nil source))
              (should (equal (car listed) "/goblins:gribble:/p/one/"))
              (should-error (goblins-dired (goblins--snapshot-session snapshot "session-c") nil source)
                            :type 'user-error))
          (when (buffer-live-p eshell) (kill-buffer eshell)))))))

(ert-deftest goblins-tramp-closes-connections-nothing-uses ()
  (let ((process (start-process "goblins-test-connection" nil "sleep" "60"))
        (buffers nil) closed)
    (cl-letf (((symbol-function 'tramp-get-connection-process) (lambda (_) process))
              ((symbol-function 'tramp-cleanup-connection)
               (lambda (vec &rest _) (push (tramp-file-name-host vec) closed))))
      (unwind-protect
          (let ((use (lambda (name directory)
                       (let ((buffer (get-buffer-create name)))
                         (with-current-buffer buffer (setq default-directory directory))
                         (push buffer buffers)
                         buffer))))
            ;; Hidden, connection and terminal buffers do not keep it open.
            (funcall use " *goblins hidden*" "/goblins:gribble:/tmp/")
            (funcall use "*tramp/goblins gribble*" "/goblins:gribble:/")
            (with-current-buffer (funcall use "goblins-test-term" "/goblins:gribble:/")
              (setq-local ghostel-identity '((kind . goblins-shell))))
            (let ((one (funcall use "goblins-test-one" "/goblins:gribble:/tmp/"))
                  (two (funcall use "goblins-test-two" "/goblins:gribble:/w/")))
              (funcall use "goblins-test-other" "/goblins:snaga:/")
              (should (goblins-tramp--host-in-use-p "gribble"))
              ;; Killing the last buffer that uses the host closes it at once.
              (kill-buffer one)
              (should-not closed)
              (should-not (goblins-tramp--host-in-use-p "gribble" two))
              (kill-buffer two)
              (should (equal closed '("gribble")))
              ;; Never while TRAMP is using the connection.
              (setq closed nil)
              (cl-letf (((symbol-function 'tramp-get-connection-property)
                         (lambda (_ property &rest _) (equal property "locked"))))
                (goblins-tramp--close-unused "gribble"))
              (should-not closed)))
        (delete-process process)
        (mapc #'kill-buffer (seq-filter #'buffer-live-p buffers))))))

(ert-deftest goblins-process-file-passes-what-differs-from-emacss-environment ()
  (let ((process-environment (append '("GIT_LITERAL_PATHSPECS=1" "TERM=xterm" "PATH=/host/bin"
                                       "UNSET_ME" "GIT_LITERAL_PATHSPECS=0")
                                     process-environment))
        (tramp-propagate-emacsclient-tramp nil))
    (let ((env (goblins-tramp--process-environment (tramp-dissect-file-name "/goblins:g:/"))))
      (should (member "GIT_LITERAL_PATHSPECS=1" env))
      (should-not (member "GIT_LITERAL_PATHSPECS=0" env))
      (should (member "TERM=xterm" env))
      (should (member "PAGER=cat" env))
      (should (cl-find-if (lambda (e) (string-prefix-p "INSIDE_EMACS=" e)) env))
      (should-not (cl-find-if (lambda (e) (string-prefix-p "PATH=" e)) env))
      (should-not (cl-find-if (lambda (e) (string-prefix-p "UNSET_ME" e)) env))
      ;; Emacs's own environment stays on the host.
      (should-not (cl-intersection (seq-remove (lambda (e) (string-match-p "\\`\\(TERM\\|PAGER\\)=" e))
                                               env)
                                   (default-toplevel-value 'process-environment)
                                   :test #'equal)))))

(ert-deftest goblins-process-file-uses-an-exec-only-while-tramp-is-busy ()
  (let ((default-directory "/goblins:gribble:/tmp/")
        (busy nil) used)
    (cl-letf (((symbol-function 'goblins-tramp--busy-p) (lambda (_) busy))
              ((symbol-function 'goblins-tramp--process-file)
               (lambda (&rest _) (push 'exec used) 0)))
      (goblins-tramp--sh-process-file (lambda (&rest _) (push 'tramp used) 0) "true")
      (setq busy t)
      (goblins-tramp--sh-process-file (lambda (&rest _) (push 'tramp used) 0) "true"))
    (should (equal used '(exec tramp)))))

(ert-deftest goblins-vc-never-looks-inside-goblins ()
  (should (string-match-p vc-ignore-dir-regexp "/goblins:gribble:/home/x/"))
  (should-not (string-match-p vc-ignore-dir-regexp "/home/x/"))
  (should-not (vc-find-root "/goblins:gribble:/home/x/" ".git")))

(ert-deftest goblins-shell-runs-goblins-shell-program ()
  (goblins-test--sidebar (list (goblins-test--session "session-a" :path "gribble"))
    (let ((goblins--pins (make-hash-table :test #'equal))
          (session (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source)
                                              "session-a"))
          argvs shells)
      (cl-letf (((symbol-function 'goblins--require-ghostel) #'ignore)
                ((symbol-function 'ghostel-exec)
                 (lambda (_buffer _program args &rest _) (push args argvs))))
        (unwind-protect
            (dolist (program '(nil "fish" ("bash" "--norc")))
              (let ((goblins-shell-program program))
                (push (goblins-shell session #'ignore source) shells)))
          (mapc #'kill-buffer shells)))
      (should (equal (mapcar (lambda (args) (cdr (member "gribble" args))) (reverse argvs))
                     '(nil ("--" "fish") ("--" "bash" "--norc")))))))

(ert-deftest goblins-disconnect-closes-the-connection-without-waiting ()
  (let* ((socket (make-temp-name (expand-file-name "goblins-test-" temporary-file-directory)))
         (server (make-network-process :name "goblins-test-server" :server t
                                       :family 'local :service socket :noquery t))
         warnings)
    (unwind-protect
        (with-temp-buffer
          (setq goblins--connection
                (make-instance 'jsonrpc-process-connection
                               :name "goblins-test"
                               :process (make-network-process :name "goblins" :family 'local
                                                              :service socket :noquery t)))
          (let ((process (jsonrpc--process goblins--connection))
                (start (float-time)))
            (cl-letf (((symbol-function 'jsonrpc--warn)
                       (lambda (&rest args) (push args warnings))))
              (goblins--disconnect))
            (should-not warnings)
            (should-not (process-live-p process))
            (should (< (- (float-time) start) 0.3))
            (should-not goblins--connection)))
      (delete-process server)
      (when (file-exists-p socket) (delete-file socket)))))

(ert-deftest goblins-pins-belong-to-one-goblin ()
  (let ((goblins--pins (make-hash-table :test #'equal)))
    (should (equal (goblins-pin-host "g" "/s" "i" "i-s1") "g"))
    (should (equal (goblins-pin-host "g" "/s" "i" "i-s1") "g"))
    (should-error (goblins-pin-host "g" "/other" "j" "j-s1"))
    (should-error (goblins-pin-host "g" "/s" "i" nil))
    (should (equal (goblins--pin-or-id "g" "/s" "i" "i-s2") "i-s2"))
    ;; A stopped goblin's pins are released from its daemon's snapshot.
    (with-temp-buffer
      (setq goblins--directory "/s" goblins--instance "i"
            goblins--snapshot (list :sessions (vector (list :id "i-s1" :state "stopped")
                                                      (list :id "i-s2" :state "running"))))
      (cl-letf (((symbol-function 'goblins-tramp-release)
                 (lambda (host _) (remhash host goblins--pins))))
        (goblins--release-stopped-pins)))
    (should-not (goblins-pin "g"))
    (should (goblins-pin "i-s2"))))

(defun goblins-test--host-pids (marker)
  "PIDs of host processes whose command line contains MARKER."
  (cl-remove-if-not
   (lambda (pid)
     (ignore-errors
       (with-temp-buffer
         (insert-file-contents-literally (format "/proc/%s/cmdline" pid))
         (search-forward marker nil t))))
   (directory-files "/proc" nil "\\`[0-9]+\\'")))

(defun goblins-test--in (directory goblin script)
  "Run SCRIPT in GOBLIN of DIRECTORY's daemon with the CLI; return its output."
  (with-temp-buffer
    (let ((default-directory temporary-file-directory))
      (call-process goblins-executable nil '(t nil) nil "--state-dir" directory
                    "exec" goblin "--" "sh" "-c" script))
    (buffer-string)))

(defun goblins-test--start-goblin (source name)
  "Start goblin NAME in SOURCE's daemon and return its session ID once running."
  (let (id)
    (goblins-test--wait
     (lambda ()
       (setq id (or id (ignore-errors
                         (plist-get (goblins--host-call
                                     source 'sessions.start
                                     (list :key (format "%s-%s" name (random 100000000))
                                           :name "plain" :agent_name name
                                           :configuration (getenv "GOBLINS_TEST_MANIFEST")
                                           :rows 24 :cols 80 :detached t))
                                    :session))))
       (and id (equal (plist-get (goblins--snapshot-session
                                  (buffer-local-value 'goblins--snapshot source) id)
                                 :state)
                      "running")))
     90)
    id))

(defun goblins-test--stop-goblin (source id)
  (goblins--host-call source 'sessions.stop (list :session id))
  (goblins-test--wait
   (lambda () (equal (plist-get (goblins--snapshot-session
                                 (buffer-local-value 'goblins--snapshot source) id)
                                :state)
                     "stopped"))))

(defun goblins-test--pin (host)
  (goblins-pin host))

(defun goblins-test--tramp-scenario ()
  (let* ((directory (getenv "GOBLINS_TEST_STATE"))
         (other (getenv "GOBLINS_TEST_OTHER_STATE"))
         (goblins-state-directory directory)
         (goblins-executable (getenv "GOBLINS_APP"))
         (ghostel-query-before-killing nil)
         (source (goblins--connect))
         (first (goblins-test--start-goblin source "gribble"))
         (file "/goblins:gribble:/tmp/notes.txt")
         (tmp "/goblins:gribble:/tmp/")
         notes ghostel-used)
    ;; Running goblins complete after /goblins:, like hosts after /ssh:.
    (should (equal (let ((non-essential t))
                     (tramp-completion-handle-file-name-all-completions "goblins:gr" "/"))
                   '("goblins:gribble:")))
    ;; Visit, save and list files; the connection is pinned to the session.
    (setq notes (find-file file))
    (insert "hello from emacs\n")
    (save-buffer)
    (should (equal (goblins-test--in directory "gribble" "cat /tmp/notes.txt")
                   "hello from emacs\n"))
    (should (equal (nth 2 (goblins-test--pin "gribble")) first))
    (with-current-buffer (dired tmp)
      (goto-char (point-min))
      (should (search-forward "notes.txt" nil t)))
    ;; Large files travel uncompressed, without a warning about gzip.
    (goblins-test--in directory "gribble" "seq 1 5000 > /tmp/big.txt")
    (with-current-buffer (find-file-noselect "/goblins:gribble:/tmp/big.txt")
      (should (= (count-lines (point-min) (point-max)) 5000))
      (goto-char (point-max))
      (insert "end\n")
      (save-buffer)
      (kill-buffer))
    (should (equal (goblins-test--in directory "gribble" "tail -n 1 /tmp/big.txt") "end\n"))
    (should-not (and (get-buffer "*Warnings*")
                     (string-match-p "compress" (with-current-buffer "*Warnings*" (buffer-string)))))
    ;; Eshell runs external commands inside the sandbox.
    (require 'eshell)
    (let ((default-directory tmp))
      (should (equal (string-trim (eshell-command-result "id -u")) "1000"))
      (should (equal (string-trim (eshell-command-result "cat notes.txt")) "hello from emacs")))
    ;; goblins-eshell and goblins-dired open in the goblin's directory.
    (let ((session (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source) first)))
      (with-current-buffer (goblins-eshell session #'ignore source)
        (should (equal default-directory "/goblins:gribble:/workspace/"))
        (goto-char (point-max))
        (insert "cat /tmp/notes.txt")
        (eshell-send-input)
        (goblins-test--wait (lambda () (string-match-p "^hello from emacs$" (buffer-string)))))
      (with-current-buffer (goblins-dired session #'ignore source)
        (should (derived-mode-p 'dired-mode))
        (should (equal default-directory "/goblins:gribble:/workspace/"))))
    ;; While TRAMP's connection is busy, process-file runs as its own exec,
    ;; with the environment, stdin and stderr redirections it asks for.
    (let ((default-directory tmp)
          (process-environment (cons "GOBLINS_TEST_VAR=from-emacs" process-environment)))
      (tramp-set-connection-property (tramp-get-connection-process (goblins-tramp--vec tmp))
                                     "locked" t)
      (unwind-protect
          (with-temp-buffer
            (should (eq (process-file "sh" nil t nil "-c"
                                      "echo $GOBLINS_TEST_VAR; pwd; echo err >&2; exit 3")
                        3))
            (should (equal (buffer-string) "from-emacs\n/tmp\nerr\n"))
            (erase-buffer)
            (should (eq (process-file "cat" (concat tmp "notes.txt") t) 0))
            (should (equal (buffer-string) "hello from emacs\n"))
            (erase-buffer)
            (should (eq (process-file "sh" nil (list t (concat tmp "stderr.txt")) nil "-c"
                                      "echo out; echo bad >&2")
                        0))
            (should (equal (buffer-string) "out\n")))
        (tramp-flush-connection-property
         (tramp-get-connection-process (goblins-tramp--vec tmp)) "locked"))
      (should (equal (goblins-test--in directory "gribble" "cat /tmp/stderr.txt") "bad\n")))
    ;; Killing a compilation, or deleting a process, leaves nothing behind.
    (let ((default-directory tmp)
          (compilation-ask-about-save nil))
      (with-current-buffer (compile "sleep 4242421; echo finished")
        (goblins-test--wait (lambda () (goblins-test--host-pids "sleep\0004242421")))
        (kill-compilation)
        (goblins-test--wait (lambda () (null (goblins-test--host-pids "sleep\0004242421")))))
      (let ((process (start-file-process "sleeper" nil "sleep" "4242422")))
        (goblins-test--wait (lambda () (goblins-test--host-pids "sleep\0004242422")))
        (delete-process process)
        (goblins-test--wait (lambda () (null (goblins-test--host-pids "sleep\0004242422")))))
      ;; Closing stdin ends a pipe process, as eglot's shutdown does.
      (with-temp-buffer
        (let ((process (make-process :name "cat" :buffer (current-buffer) :command '("cat")
                                     :connection-type 'pipe :file-handler t)))
          (process-send-string process "line\n")
          (process-send-eof process)
          (goblins-test--wait (lambda () (not (process-live-p process))))
          (should (eq (process-exit-status process) 0))
          (should (equal (buffer-string) "line\n")))))
    ;; The first visit to a goblin-written file ignores eval: and unsafe variables.
    (goblins-test--in directory "gribble"
                      (concat "printf '%s\\n' '-*- eval: (setq goblins-test--pwned t); "
                              "fill-column: 33; goblins-test--unsafe: 7 -*-' > /tmp/unsafe.txt"))
    (setq goblins-test--pwned nil)
    (let ((enable-local-variables t))
      (with-current-buffer (find-file-noselect "/goblins:gribble:/tmp/unsafe.txt")
        (should (= fill-column 33))
        (should-not (local-variable-p 'goblins-test--unsafe))))
    (should-not goblins-test--pwned)
    ;; goblins-editor asks first: the popup names the goblin and its files,
    ;; and nothing opens until you approve.  A refusal fails the editor.
    (let ((editor (start-process "refused-editor" (generate-new-buffer " *refused*")
                                 goblins-executable "--state-dir" directory "exec" "gribble"
                                 "--" "goblins-editor" "/tmp/r1")))
      (goblins-test--wait (lambda () goblins--pending-edits))
      (should (get-buffer-window goblins--popup-buffer-name))
      (with-current-buffer goblins--popup-buffer-name
        (should (string-match-p "gribble wants to open in Emacs\n +/tmp/r1" (buffer-string)))
        (goblins-request-popup-deny))
      (goblins-test--wait (lambda () (not (process-live-p editor))))
      (should-not (eq (process-exit-status editor) 0))
      (should (string-match-p "refused" (with-current-buffer (process-buffer editor) (buffer-string))))
      (should-not (get-file-buffer "/goblins:gribble:/tmp/r1"))
      (should-not (get-buffer-window goblins--popup-buffer-name)))
    ;; goblins-editor blocks until every file is done with C-x #.
    (let* ((editor (start-process "goblins-editor" (generate-new-buffer " *editor*")
                                  goblins-executable "--state-dir" directory "exec" "gribble"
                                  "--" "goblins-editor" "+2" "/tmp/m1" "/tmp/m2"))
           (m1 "/goblins:gribble:/tmp/m1") (m2 "/goblins:gribble:/tmp/m2"))
      (goblins-test--wait (lambda () goblins--pending-edits))
      (should-not (get-file-buffer m1))
      (with-current-buffer goblins--popup-buffer-name
        (should (string-match-p "/tmp/m1:2\n +/tmp/m2" (buffer-string)))
        (goblins-request-popup-accept))
      (goblins-test--wait (lambda () (and (get-file-buffer m1) (get-file-buffer m2))))
      (dolist (name (list m1 m2))
        (with-current-buffer (get-file-buffer name)
          (insert (file-name-nondirectory name))
          (save-buffer)
          (should (process-live-p editor))
          (server-edit)))
      (goblins-test--wait (lambda () (not (process-live-p editor))))
      (should (eq (process-exit-status editor) 0))
      (should (equal (goblins-test--in directory "gribble" "cat /tmp/m1 /tmp/m2") "m1m2")))
    ;; A shell inside the goblin, alongside its agent terminal.
    (when (and (require 'ghostel nil t) (require 'ghostel-module nil t))
      (let ((shell (goblins-shell (goblins--snapshot-session
                                   (buffer-local-value 'goblins--snapshot source) first))))
        (with-current-buffer shell
          (should (equal default-directory "/goblins:gribble:/"))
          (goblins-test--wait (lambda () (process-live-p ghostel--process)))
          (ghostel-send-string "echo SHELL-$((6*7))\n")
          (goblins-test--wait (lambda () (string-match-p "SHELL-42" (buffer-string))))
          ;; A pasted image lands byte for byte in the goblin's /tmp, its
          ;; path pasted after the command.
          (let ((image (apply #'concat (make-list 256 (apply #'unibyte-string
                                                             (number-sequence 0 255))))))
            (ghostel-send-string "echo PASTED:")
            (goblins--yank-media-image 'image/png image)
            (ghostel-send-string "\n")
            (goblins-test--wait
             (lambda () (string-match-p "^PASTED:/tmp/goblins-clipboard/[0-9-]+\\.png$"
                                        (buffer-string))))
            (should (equal (goblins-test--in directory "gribble"
                                             "cat /tmp/goblins-clipboard/*.png | sha256sum")
                           (format "%s  -\n" (secure-hash 'sha256 image))))))
        ;; The sidebar lists it under its goblin.
        (require 'goblins-workspace)
        (should (equal (gethash first (goblins--sidebar-shells source)) (list shell)))
        (goblins-test--wait (lambda () (eq (plist-get (goblins--snapshot-session
                                                       (buffer-local-value 'goblins--snapshot source)
                                                       first)
                                                      :shells)
                                           1)))
        (setq ghostel-used t)))
    ;; Stopping the goblin kills its buffers before the pin is released, so
    ;; a later goblin with the same name is pinned afresh.
    (goblins-test--stop-goblin source first)
    (goblins-test--wait (lambda () (not (buffer-live-p notes))))
    (should-not (goblins-test--pin "gribble"))
    (should-not (cl-some (lambda (b) (goblins-tramp--buffer-vec b))
                         (cl-remove-if #'goblins-tramp--terminal-p (buffer-list))))
    (let ((second (goblins-test--start-goblin source "gribble")))
      (setq notes (find-file-noselect "/goblins:gribble:/tmp/second.txt"))
      (with-current-buffer notes (insert "x") (save-buffer))
      (should (equal (nth 2 (goblins-test--pin "gribble")) second))
      ;; With the stop notification missed, the pin fails hard instead of
      ;; reaching the next namesake.
      (remove-hook 'goblins-snapshot-hook #'goblins--after-snapshot)
      (remove-hook 'goblins-snapshot-hook #'goblins-tramp--after-snapshot)
      (goblins-test--stop-goblin source second)
      (goblins-test--start-goblin source "gribble")
      (with-current-buffer notes
        (should-not (ignore-errors (eq 0 (process-file "sh" nil nil nil "-c"
                                                       "echo leaked > /tmp/leak")))))
      (should (equal (goblins-test--in directory "gribble" "cat /tmp/leak 2>/dev/null") ""))
      (add-hook 'goblins-snapshot-hook #'goblins--after-snapshot)
      (add-hook 'goblins-snapshot-hook #'goblins-tramp--after-snapshot)
      (with-current-buffer source (goblins--render))
      (goblins-test--wait (lambda () (not (buffer-live-p notes)))))
    ;; Pointing goblins.el at another daemon does not move existing connections.
    (goblins-test--in directory "gribble" "echo first-daemon > /tmp/which")
    (goblins-test--in other "gribble" "echo other-daemon > /tmp/which")
    (with-current-buffer (find-file-noselect "/goblins:gribble:/tmp/which")
      (let ((goblins-state-directory other))
        (revert-buffer t t)
        (should (equal (buffer-string) "first-daemon\n"))))
    ;; An edit from the other daemon's namesake never opens this daemon's
    ;; file: refused while the name is pinned here, its own once released.
    (let* ((goblins-editor-approval 'approve)
           (edit (lambda ()
                  (let ((process (start-process "other-editor" (generate-new-buffer " *other*")
                                                goblins-executable "--state-dir" other "exec"
                                                "gribble" "--" "goblins-editor" "--no-wait"
                                                "/tmp/which")))
                    (goblins-test--wait (lambda () (not (process-live-p process))))
                    (process-exit-status process)))))
      (should-not (eq (funcall edit) 0))
      (goblins-tramp-release "gribble")
      (should-not (get-file-buffer "/goblins:gribble:/tmp/which"))
      (should (eq (funcall edit) 0))
      (should (equal (car (goblins-test--pin "gribble")) other))
      (with-current-buffer (get-file-buffer "/goblins:gribble:/tmp/which")
        (should (equal (buffer-string) "other-daemon\n")))
      (goblins-tramp-release "gribble"))
    ;; TRAMP's connection shell closes with the last buffer that uses it.
    (let* ((id (goblins-test--start-goblin source "snaga"))
           (execs (lambda () (or (plist-get (goblins--snapshot-session
                                             (buffer-local-value 'goblins--snapshot source) id)
                                            :execs)
                                 0)))
           (session (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source) id))
           (listing (goblins-dired session #'ignore source)))
      (goblins-test--wait (lambda () (> (funcall execs) 0)))
      (kill-buffer listing)
      (goblins-test--wait (lambda () (= (funcall execs) 0)))
      (should (equal (nth 2 (goblins-test--pin "snaga")) id))
      ;; A fresh shell runs goblins-shell-program from the sandbox's PATH,
      ;; and leaves the sandbox when its goblin stops.
      (when ghostel-used
        (let ((shell (let ((goblins-shell-program '("sh" "-c" "echo PROGRAM-$((6*7)); sleep 600")))
                       (goblins-shell session))))
          (should (equal (buffer-local-value 'default-directory shell) "/goblins:snaga:/"))
          (with-current-buffer shell
            (goblins-test--wait (lambda () (string-match-p "PROGRAM-42" (buffer-string)))))
          (goblins-test--stop-goblin source id)
          (goblins-test--wait (lambda () (not (string-prefix-p "/goblins:" (buffer-local-value
                                                                           'default-directory shell)))))
          (should-not (goblins-test--pin "snaga")))))
    (if ghostel-used "ok ghostel" "ok")))

(defvar goblins-test--backtrace nil)

(defun goblins-test-tramp-live ()
  "Run the TRAMP scenario, write ok or the failure to the result file, exit."
  (let ((result (getenv "GOBLINS_TEST_RESULT")))
    (condition-case err
        (progn
          (setq server-name (getenv "GOBLINS_TEST_SERVER_NAME")
                server-socket-dir (getenv "GOBLINS_TEST_SERVER_DIR"))
          (server-start)
          (write-region (handler-bind ((error (lambda (_) (setq goblins-test--backtrace
                                                                (backtrace-to-string)))))
                          (goblins-test--tramp-scenario))
                        nil result))
      (error (write-region (format "%S\n%s\n%s" err goblins-test--backtrace
                                   (with-current-buffer (messages-buffer)
                                     (buffer-substring (max (point-min) (- (point-max) 3000))
                                                       (point-max))))
                           nil result)))
    (kill-emacs 0)))

;;; goblins-tests.el ends here

(ert-deftest goblins-activity-is-visible-with-old-snapshot-fallback ()
  (with-temp-buffer
    (goblins--insert-agent-fields (list :id "a" :state "running" :name "shell"))
    (should (string-match-p "Activity:.*unknown" (buffer-string))))
  (with-temp-buffer
    (goblins--insert-agent-fields (list :id "a" :state "running" :name "shell"
                                       :agent_state "waiting-input"))
    (should (string-match-p "Activity:.*waiting-input" (buffer-string)))))

;;; Messages

(require 'goblins-messages)

(defun goblins-test--message (id sender recipient body conversation created &optional reply-to)
  (list :id id :sender sender :sender_path (if (equal sender "a") "alpha" "beta")
        :recipient recipient :recipient_path (if (equal recipient "a") "alpha" "beta")
        :body body :conversation conversation :created_at created :in_reply_to reply-to
        :state "queued" :failure nil :claim_generation 0))

(defun goblins-test--message-events ()
  "Two threads: m1 from alpha in raid r1, which beta claimed and answered with
m2, and m3, which failed when beta stopped."
  (list
   (list :kind "raid.created" :actor "host" :timestamp 1000
         :data (list :raid "i-r1" :name "test" :owner "a"))
   (list :kind "message.accepted" :actor "a" :timestamp 2000
         :data (list :change (list :message (goblins-test--message
                                             "i-m1" "a" "b" "Hello beta\nsecond line" "i-c1" 2000)
                                   :raid "i-r1" :route "raid")))
   (list :kind "message.claimed" :actor "b" :timestamp 3000
         :data (list :change (list :message "i-m1" :claim_generation 1)))
   (list :kind "message.replied" :actor "b" :timestamp 4000
         :data (list :change (list :message (goblins-test--message
                                             "i-m2" "b" "a" "Hi alpha" "i-c1" 4000 "i-m1")
                                   :completed "i-m1")))
   (list :kind "message.accepted" :actor "a" :timestamp 5000
         :data (list :change (list :message (goblins-test--message
                                             "i-m3" "a" "b" "Only one" "i-c3" 5000)
                                   :raid nil :route "tree")))
   (list :kind "recipient.stopped" :actor "host" :timestamp 6000
         :data (list :recipient "b" :messages ["i-m3"]))))

(defmacro goblins-test--messages (&rest body)
  "Run BODY in a message list of a fake server holding the test events."
  (declare (indent 0))
  `(let ((source (generate-new-buffer " *goblins messages source*")))
     (unwind-protect
         (progn
           (with-current-buffer source
             (goblins-status-mode)
             (setq goblins--directory "/tmp/goblins-test" goblins--instance "i"
                   goblins--subscription "s"
                   goblins--snapshot
                   (list :instance "i"
                         :sessions [(:id "a" :path "alpha" :agent_name "alpha" :name "claude"
                                     :state "running")
                                    (:id "b" :path "beta" :agent_name "beta" :name "codex"
                                     :state "running")]
                         :raids [(:id "i-r1" :name "test")])))
           (cl-letf (((symbol-function 'goblins--messages-fetch) #'ignore))
             (with-current-buffer (goblins--messages-open source nil)
               (goblins--messages-reset source "i")
               (mapc #'goblins--messages-ingest (goblins-test--message-events))
               (goblins--messages-render)
               ,@body)))
       (dolist (name (list goblins--messages-buffer-name goblins--message-buffer-name))
         (when (get-buffer name) (kill-buffer name)))
       (kill-buffer source))))

(defun goblins-test--lines ()
  (split-string (string-trim-right (buffer-substring-no-properties (point-min) (point-max))) "\n"))

(ert-deftest goblins-messages-list-threads-folded ()
  (goblins-test--messages
    (let ((lines (goblins-test--lines)))
      (should (= (length lines) 2))
      (should (string-match-p "\\`▸ [0-9:]\\{8\\}  alpha → beta +Hello beta +\\+1\\'" (car lines)))
      ;; A thread of one message has nothing to fold, and m3 failed.
      (should (string-match-p "\\`  [0-9:]\\{8\\}  alpha → beta +✗ Only one\\'" (cadr lines))))
    (goto-char (point-min))
    (goblins-messages-fold)
    (let ((lines (goblins-test--lines)))
      (should (= (length lines) 3))
      (should (string-prefix-p "▾ " (car lines)))
      (should (string-match-p "beta → alpha +└ Hi alpha" (nth 1 lines))))
    (forward-line 1)
    (should (equal (goblins--messages-id-at) "i-m2"))
    ;; Folding from a reply goes back to the thread's first message.
    (goblins-messages-fold)
    (should (equal (goblins--messages-id-at) "i-m1"))
    (should (= (length (goblins-test--lines)) 2))
    (goblins-messages-fold-all)
    (should (= (length (goblins-test--lines)) 3))
    (goblins-messages-fold-all)
    (should (= (length (goblins-test--lines)) 2))
    ;; With every thread that has replies unfolded, S-TAB folds at once:
    ;; a thread of one message has nothing to fold.
    (goto-char (point-min))
    (goblins-messages-fold)
    (goblins-messages-fold-all)
    (should (= (length (goblins-test--lines)) 2))
    (should (string-match-p "Time.*From → To.*Message.*3 messages" (format "%s" header-line-format)))))

(ert-deftest goblins-messages-filters-narrow-the-list ()
  (goblins-test--messages
    (setq goblins--messages-filters (list :from "b"))
    (goblins--messages-render)
    (should (= (length (goblins-test--lines)) 1))
    (should (string-match-p "beta → alpha +Hi alpha" (buffer-string)))
    (should (string-match-p "from beta.*1 of 3" (format "%s" header-line-format)))
    (setq goblins--messages-filters (list :raid "i-r1"))
    (goblins--messages-render)
    (should (= (length (goblins-test--lines)) 1))
    (setq goblins--messages-filters (list :thread "i-c1"))
    (goblins--messages-render)
    ;; The thread filter shows its thread unfolded.
    (should (= (length (goblins-test--lines)) 2))
    (should-error (goblins-messages-fold) :type 'user-error)
    (setq goblins--messages-filters (list :failed t))
    (goblins--messages-render)
    (should (string-match-p "Only one" (buffer-string)))
    (should (= (length (goblins-test--lines)) 1))
    ;; Nobody claimed the reply yet.
    (setq goblins--messages-filters (list :awaiting t))
    (goblins--messages-render)
    (should (= (length (goblins-test--lines)) 1))
    (should (string-match-p "Hi alpha" (buffer-string)))
    (setq goblins--messages-filters (list :awaiting t :from "a"))
    (goblins--messages-render)
    (should (string-match-p "No message passes the filters" (buffer-string)))
    (setq goblins--messages-filters (list :goblin "a" :before 3.0))
    (goblins--messages-render)
    (should (= (length (goblins-test--lines)) 1))
    (goblins-messages-widen)
    (should-not goblins--messages-filters)
    (should (= (length (goblins-test--lines)) 2))))

(ert-deftest goblins-messages-reader-follows-point ()
  (goblins-test--messages
    (goto-char (point-min))
    (goblins-messages-fold)
    (forward-line 1)
    (goblins--messages-follow)
    (with-current-buffer goblins--message-buffer-name
      (let ((text (buffer-string)))
        (should (string-match-p "^From     beta (codex)$" text))
        (should (string-match-p "^To       alpha (claude)$" text))
        (should (string-match-p "^Thread   c1   message 2 of 2$" text))
        (should (string-match-p "^Raid     test$" text))
        (should (string-match-p "^Re       m1   [0-9:]\\{8\\}  alpha  Hello beta$" text))
        (should (string-match-p "^Hi alpha$" text))))
    (forward-line -1)
    (goblins--messages-follow)
    (with-current-buffer goblins--message-buffer-name
      (should (string-match-p "claimed by beta [0-9:]\\{8\\}   ·   replied to by m2" (buffer-string)))
      (should (string-match-p "^Hello beta\nsecond line$" (buffer-string)))
      ;; RET on the reply's ID shows it in the list.
      (goto-char (point-min))
      (search-forward "by m2")
      (backward-char 1)
      (goblins-message-visit))
    (should (equal (goblins--messages-id-at) "i-m2"))
    (forward-line 1)
    (goblins--messages-follow)
    (with-current-buffer goblins--message-buffer-name
      (should (string-match-p "failed: recipient stopped" (buffer-string)))
      (should-not (string-match-p "^Raid" (buffer-string))))))

(ert-deftest goblins-messages-embark-targets-and-actions ()
  (goblins-test--messages
    (goto-char (point-min))
    (should (equal (goblins--messages-embark-target)
                   `(goblins-raid-message "m1" ,(line-beginning-position) . ,(line-end-position))))
    (forward-line 1)
    (should (equal (car (goblins--messages-embark-target)) 'goblins-message))
    (forward-line -1)
    (goblins--messages-follow)
    (with-current-buffer goblins--message-buffer-name
      (goto-char (point-min))
      (search-forward "alpha")
      (should (equal (goblins--messages-embark-target)
                     `(goblin "alpha" ,(- (point) 5) . ,(point))))
      (search-forward "c1")
      (should (equal (car (goblins--messages-embark-target)) 'goblins-thread))
      (should (equal (cadr (goblins--messages-embark-target)) "c1"))
      (goto-char (point-max))
      (should (equal (goblins--messages-embark-target) '(goblins-raid-message . "m1"))))
    ;; Actions on a message filter by what it names; in the list they add up.
    (goblins-message-to "i-m1")
    (should (equal goblins--messages-filters '(:to "b")))
    (goblins-message-thread "i-m1")
    (should (equal goblins--messages-filters '(:to "b" :thread "i-c1")))
    (goblins-message-raid "i-m1")
    (should (equal (plist-get goblins--messages-filters :raid) "i-r1"))
    (should-error (goblins-message-raid "i-m3") :type 'user-error)
    (with-temp-buffer
      ;; Elsewhere they start afresh.
      (goblins-message-from "i-m2"))
    (with-current-buffer goblins--messages-buffer-name
      (should (equal goblins--messages-filters '(:from "b"))))
    (with-temp-buffer
      (goblins-thread-messages "i-c3"))
    (with-current-buffer goblins--messages-buffer-name
      (should (equal goblins--messages-filters '(:thread "i-c3"))))
    (should (eq (lookup-key goblins-embark-raid-message-map "r") #'goblins-message-raid))
    (should-not (lookup-key goblins-embark-message-map "r"))
    (should (eq (lookup-key goblins-embark-message-map "F") #'goblins-message-from))
    (should (eq (lookup-key goblins-embark-message-map "T") #'goblins-message-to))
    (should (eq (lookup-key goblins-embark-message-map "t") #'goblins-message-thread))
    (should (eq (lookup-key goblins-embark-goblin-messages-map "f") #'goblins-goblin-messages-from))))

(ert-deftest goblins-messages-read-choices ()
  (goblins-test--messages
    (cl-letf (((symbol-function 'completing-read)
               (lambda (_prompt table &rest _)
                 (should (eq (cdr (assq 'category (cdr (funcall table "" nil 'metadata))))
                             'goblins-message))
                 "m2")))
      (should (equal (goblins--read-message "Message: ") "i-m2")))
    (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "c3")))
      (should (equal (goblins--read-thread "Thread: ") "i-c3")))
    (should (equal (goblins--messages-participants) '(("alpha" . "a") ("beta" . "b"))))
    (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "beta")))
      (goblins-messages-filter-to)
      (should (equal goblins--messages-filters '(:to "b")))
      ;; Again clears it.
      (goblins-messages-filter-to)
      (should-not (plist-get goblins--messages-filters :to)))
    (should (string-match-p "to +any" (goblins--messages-filter-description "to" :to)))
    (goblins-messages-filter-awaiting)
    (should (string-match-p "awaiting reply +on"
                            (goblins--messages-filter-description "awaiting reply" :awaiting)))
    ;; The time filters say what they accept, and read it with Org's prompt.
    (require 'org)
    (should (string-match-p "sent after +off  15:30, -1h, -2d 9:00"
                            (goblins--messages-filter-description "sent after" :after)))
    (cl-letf (((symbol-function 'org-read-date)
               (lambda (with-time to-time _from prompt &rest _)
                 (should (and with-time to-time))
                 (should (equal prompt "Sent after, like 15:30, -1h, -2d 9:00. "))
                 (encode-time (list 0 30 15 15 10 2026 nil -1 nil)))))
      (goblins-messages-filter-after))
    (should (equal (format-time-string "%F %H:%M" (plist-get goblins--messages-filters :after))
                   "2026-10-15 15:30"))
    (should (string-match-p "after 2026-10-15 15:30:00\\|after 15:30:00"
                            (format "%s" header-line-format)))
    ;; Again clears it.
    (goblins-messages-filter-after)
    (should-not (plist-get goblins--messages-filters :after))))

(ert-deftest goblins-messages-from-the-sidebar ()
  (goblins-test--messages
    (let ((source goblins--view-source))
      (with-temp-buffer
        (goblins-sidebar-mode)
        (setq goblins--sidebar-source source)
        (let ((inhibit-read-only t))
         (insert (propertize "▾ raid test\n" 'goblins-raid "test")
                (propertize "alpha (claude)\n" 'goblins-id "a")
                (propertize "○ idle\n" 'goblins-detail "a")
                (propertize "› shell\n" 'goblins-terminal (current-buffer) 'goblins-parent "a")))
        (goto-char (point-min))
        (should (eq (key-binding (kbd "m m")) #'goblins-sidebar-messages))
        (save-current-buffer (goblins-sidebar-messages))
        (should (equal (buffer-local-value 'goblins--messages-filters
                                           (get-buffer goblins--messages-buffer-name))
                       '(:raid "i-r1")))
        (forward-line 2)
        (save-current-buffer (goblins-sidebar-messages-from))
        (should (equal (buffer-local-value 'goblins--messages-filters
                                           (get-buffer goblins--messages-buffer-name))
                       '(:from "a")))
        (forward-line 1)
        ;; Nothing for shells.
        (should-error (goblins-sidebar-messages-to) :type 'user-error)))))

(defun goblins-test-messages-live ()
  "Read a real daemon's messages, then see a message and an answer to you arrive."
  (let (source list)
    (unwind-protect
        (progn
          (goblins-status (getenv "GOBLINS_TEST_STATE"))
          (setq source (current-buffer))
          (goblins-test--wait (lambda () goblins--subscription))
          (setq list (goblins-messages (getenv "GOBLINS_TEST_STATE")))
          (with-current-buffer list
            (goblins-test--wait
             (lambda () (string-match-p "host → chief +First question +\\+1" (buffer-string))))
            (goto-char (point-min))
            (goblins-messages-fold)
            (should (string-match-p "chief → host +└ First answer" (buffer-string)))
            (forward-line 1)
            (goblins--messages-follow))
          (with-current-buffer goblins--message-buffer-name
            (should (string-match-p "^From     chief (shell)$" (buffer-string)))
            (should (string-match-p "^Re       m[0-9]+ .* host  First question$" (buffer-string)))
            (should (string-match-p "^First answer$" (buffer-string))))
          (goblins--host-call source 'messages.send
                              (list :key (md5 "second question") :to "chief"
                                    :body "Second question"))
          ;; A new message reaches the list by itself.
          (with-current-buffer list
            (goblins-test--wait
             (lambda () (string-match-p "host → chief +Second question" (buffer-string))))
            (should (string-match-p "3 messages" (format "%s" header-line-format))))
          ;; The goblin's answer to you changes no snapshot: the list reads
          ;; the log by itself.
          (write-region "" nil (getenv "GOBLINS_TEST_READY"))
          (with-current-buffer list
            (goblins-test--wait
             (lambda () (string-match-p "host → chief +Second question +\\+1" (buffer-string))))
            (should (string-match-p "4 messages" (format "%s" header-line-format)))))
      (dolist (buffer (list list (get-buffer goblins--message-buffer-name) source))
        (when (buffer-live-p buffer) (kill-buffer buffer))))))

(ert-deftest goblins-messages-take-the-window-and-read-on-ret ()
  (save-window-excursion
    (delete-other-windows)
    (goblins-test--messages
      ;; The list takes the selected window; the reader waits for RET.
      (should (eq (window-buffer (selected-window)) (current-buffer)))
      (should (one-window-p t))
      (should-not (get-buffer-window goblins--message-buffer-name))
      (goto-char (point-min))
      (goblins-messages-read)
      (should (equal (buffer-name) goblins--message-buffer-name))
      (should (string-match-p "^Hello beta$" (buffer-string)))
      (should (= (length (window-list)) 2)))))

(ert-deftest goblins-messages-reader-goblins-keep-their-identity ()
  (goblins-test--messages
    (goto-char (point-min))
    (goblins--messages-follow)
    (with-current-buffer goblins--message-buffer-name
      (goto-char (point-min))
      (search-forward "alpha")
      (should (equal (car (goblins--messages-embark-target)) 'goblin))
      ;; alpha stopped, and a new goblin took its name: alpha's own target
      ;; offers only its messages, by its session.
      (with-current-buffer goblins--view-source
        (setq goblins--snapshot
              (plist-put goblins--snapshot :sessions
                         [(:id "a" :path "alpha" :agent_name "alpha" :name "claude" :state "stopped")
                          (:id "b" :path "beta" :agent_name "beta" :name "codex" :state "running")
                          (:id "c" :path "alpha" :agent_name "alpha" :name "claude"
                           :state "running")])))
      (should (equal (butlast (goblins--messages-embark-target) 0)
                     `(goblins-participant "alpha" ,(- (point) 5) . ,(point))))
      (should-not (lookup-key goblins-embark-participant-map "K"))
      (should (eq (lookup-key goblins-embark-participant-map "mt")
                  #'goblins-participant-messages-to))
      (cl-letf (((symbol-function 'completing-read) (lambda (&rest _) "alpha")))
        (call-interactively #'goblins-participant-messages-to)))
    (with-current-buffer goblins--messages-buffer-name
      (should (equal (plist-get goblins--messages-filters :to) "a")))))

(ert-deftest goblins-messages-redraws-keep-the-scroll-position ()
  (save-window-excursion
    (delete-other-windows)
    (goblins-test--messages
      ;; Enough threads to scroll.
      (dotimes (i 60)
        (goblins--messages-ingest
         (list :kind "message.accepted" :actor "a" :timestamp 7000
               :data (list :change (list :message (goblins-test--message
                                                   (format "i-x%d" i) "a" "b" (format "Note %d" i)
                                                   (format "i-cx%d" i) (+ 7000 i)))))))
      (goblins--messages-render)
      (goto-char (goblins--messages-position 'goblins-message "i-x40"))
      (set-window-start (selected-window)
                        (goblins--messages-position 'goblins-message "i-x30"))
      (goblins--messages-ingest
       (list :kind "message.claimed" :actor "b" :timestamp 9000
             :data (list :change (list :message "i-x5" :claim_generation 1))))
      (goblins--messages-render)
      (should (equal (goblins--messages-id-at) "i-x40"))
      (should (= (window-start) (goblins--messages-position 'goblins-message "i-x30")))
      ;; An ID made anew, not the one the list stored, still shows its message.
      (goblins-message-show (copy-sequence "i-x2"))
      (should (equal (goblins--messages-id-at) "i-x2")))))

(ert-deftest goblins-messages-tick-reads-the-log-and-expires-the-last-minutes ()
  (goblins-test--messages
    (let ((fetched 0))
      (cl-letf (((symbol-function 'goblins--messages-fetch) (lambda () (cl-incf fetched))))
        (setq goblins--messages-filters (list :last 10))
        (goblins--messages-render)
        (should (string-match-p "No message passes" (buffer-string)))
        ;; m2 was just sent: the next tick shows it without any log event.
        (goblins--messages-update "i-m2" :created (* 1000 (float-time)))
        (goblins--messages-tick)
        (should (= fetched 1))
        (should (string-match-p "Hi alpha" (buffer-string)))
        (goblins--messages-update "i-m2" :created 4000)
        (goblins--messages-tick)
        (should (string-match-p "No message passes" (buffer-string)))))))
