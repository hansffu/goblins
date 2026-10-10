;;; goblins-terminal.el --- goblin-mode for Goblins terminal buffers -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; `goblin-mode' is a minor mode that Goblins turns on in every Ghostel
;; buffer attached to a goblin, so you can give those buffers keys of your
;; own:
;;
;;   (define-key goblin-mode-map (kbd "C-c C-g") #'goblins-workspace)
;;   (with-eval-after-load 'evil
;;     (evil-define-key 'normal goblin-mode-map (kbd "SPC g") #'goblin-info))
;;
;; It is a minor mode rather than a mode derived from `ghostel-mode':
;; `ghostel-exec' always puts the buffer in `ghostel-mode' itself, and Ghostel
;; refuses major-mode changes while the terminal runs.  A minor mode composes
;; with Ghostel and with evil-ghostel, which is a minor mode too.  Its keymap
;; takes precedence over `ghostel-mode-map', and `C-c' is one of Ghostel's
;; `ghostel-keymap-exceptions', so `C-c' keys reach Emacs in every input mode.

;;; Code:

(require 'goblins)
(require 'goblins-views)
(require 'goblins-raids)

(declare-function evil-normalize-keymaps "evil-core" (&optional state))
(declare-function goblins-workspace "goblins-workspace" (&optional directory))

(defun goblins--terminal-goblin ()
  "Return (SOURCE . SESSION-ID) for this goblin terminal."
  (unless goblins--session-key
    (user-error "Not a goblin terminal"))
  (cons (goblins--status-buffer (car goblins--session-key))
        (nth 2 goblins--session-key)))

(defun goblin-info ()
  "Show what the daemon knows about this goblin."
  (interactive)
  (let ((goblin (goblins--terminal-goblin)))
    (goblins--display-main (goblins-info-buffer (car goblin) (cdr goblin)))))

(defun goblin-permissions ()
  "Show this goblin's permission requests, the current one focused."
  (interactive)
  (let ((goblin (goblins--terminal-goblin)))
    (goblins--display-main (goblins-permissions-buffer (car goblin) (cdr goblin)))))

(defun goblin-stop ()
  "Stop this goblin, and its running descendants, after confirming."
  (interactive)
  (let* ((goblin (goblins--terminal-goblin))
         (snapshot (buffer-local-value 'goblins--snapshot (car goblin)))
         (session (goblins--snapshot-session snapshot (cdr goblin)))
         (children (goblins--live-descendants snapshot (cdr goblin))))
    (unless session (user-error "The daemon no longer retains this goblin"))
    (when (y-or-n-p (format "Stop %s%s? " (goblins--session-path session)
                            (if children (format " and %d running descendant%s"
                                                 (length children) (if (cdr children) "s" ""))
                              "")))
      (goblins--host-call (car goblin) 'sessions.stop
                          (append (list :session (cdr goblin))
                                  (and children (list :kill_children t)))))))

(defun goblin-find-file ()
  "Visit a file inside this goblin's sandbox, over /goblins: TRAMP.
In the workspace it opens right of the agent and is remembered for the goblin."
  (interactive)
  (let* ((goblin (goblins--terminal-goblin))
         (session (goblins--snapshot-session
                   (buffer-local-value 'goblins--snapshot (car goblin)) (cdr goblin)))
         (default-directory
          (if (string-prefix-p "/goblins:" default-directory)
              default-directory
            (format "/goblins:%s:%s" (string-replace "/" "." (goblins--session-path session))
                    (file-name-as-directory (or (plist-get session :cwd) "/workspace")))))
         (file (read-file-name "Find file in goblin: ")))
    (goblins-select-goblin (car goblin) (cdr goblin) (find-file-noselect file))))

(defvar goblin-mode-command-map
  (let ((map (make-sparse-keymap)))
    (define-key map "w" #'goblins-workspace)
    (define-key map "s" #'goblins-switch)
    (define-key map "i" #'goblin-info)
    (define-key map "p" #'goblin-permissions)
    (define-key map "r" #'goblins-raid)
    (define-key map "K" #'goblin-stop)
    (define-key map "t" #'goblins-shell)
    (define-key map "e" #'goblins-eshell)
    (define-key map "d" #'goblins-dired)
    (define-key map "f" #'goblin-find-file)
    map)
  "Goblin commands, on \\`C-c g' in `goblin-mode-map'.")

(defvar goblin-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-c g") goblin-mode-command-map)
    map)
  "Keymap of `goblin-mode'.  Add your own goblin terminal keys here.")

;;;###autoload
(define-minor-mode goblin-mode
  "Minor mode of Ghostel buffers attached to a goblin.
Goblins turns it on in every terminal it opens; bind your own keys in
`goblin-mode-map', and run code in `goblin-mode-hook'.

\\{goblin-mode-map}"
  :lighter " Goblin"
  :keymap goblin-mode-map
  ;; Evil's state maps for minor modes need refreshing when one turns on.
  (when (fboundp 'evil-normalize-keymaps)
    (evil-normalize-keymaps)))

(provide 'goblins-terminal)
;;; goblins-terminal.el ends here
