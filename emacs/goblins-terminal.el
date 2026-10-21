;;; goblins-terminal.el --- goblins-goblin-mode for Goblins terminal buffers -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; `goblins-goblin-mode' is a minor mode that Goblins turns on in every
;; Ghostel buffer attached to a goblin, so you can give those buffers keys of
;; your own:
;;
;;   (define-key goblins-goblin-mode-map (kbd "C-c C-g") #'goblins-workspace)
;;   (define-key goblins-goblin-mode-map (kbd "C-v") #'goblins-paste-image)
;;   (with-eval-after-load 'evil
;;     (evil-define-key 'normal goblins-goblin-mode-map (kbd "SPC g") #'goblins-info))
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

(declare-function goblins-switch "goblins-completion" (&optional directory))
(declare-function evil-normalize-keymaps "evil-core" (&optional state))
(declare-function goblins-workspace "goblins-workspace" (&optional directory))
(declare-function ghostel-paste-string "ghostel" (string))
(declare-function ghostel-force-redraw "ghostel" ())
(declare-function yank-media-handler "yank-media" (types handler))
(declare-function yank-media "yank-media" (&optional noselect))

(defun goblins--terminal-goblin ()
  "Return (SOURCE . SESSION-ID) for this goblin terminal."
  (unless goblins--session-key
    (user-error "Not a goblin terminal"))
  (cons (goblins--status-buffer (car goblins--session-key))
        (nth 2 goblins--session-key)))

(defun goblins-info ()
  "Show what the daemon knows about this goblin."
  (interactive)
  (let ((goblin (goblins--terminal-goblin)))
    (goblins--display-main (goblins-info-buffer (car goblin) (cdr goblin)))))

(defun goblins-permissions ()
  "Show this goblin's permission requests, the current one focused."
  (interactive)
  (let ((goblin (goblins--terminal-goblin)))
    (goblins--display-main (goblins-permissions-buffer (car goblin) (cdr goblin)))))

(defun goblins-stop ()
  "Stop this goblin after confirming."
  (interactive)
  (let ((goblin (goblins--terminal-goblin)))
    (goblins-stop-goblin (car goblin) (cdr goblin))))

(defconst goblins--clipboard-directory "/tmp/goblins-clipboard/"
  "Where pasted clipboard images go inside a goblin.
The sandbox's own /tmp: the host never sees them, and they go with the goblin.")

(defun goblins--image-extension (type)
  "The file name extension for image MIME TYPE, a symbol like `image/png'."
  (pcase (cadr (split-string (symbol-name type) "/"))
    ("jpeg" "jpg")
    ("svg+xml" "svg")
    (subtype subtype)))

(defun goblins--yank-media-image (type data)
  "Paste image DATA of MIME TYPE into this goblin terminal.
Write it under `goblins--clipboard-directory' inside the goblin and paste
its path: Claude Code and Codex attach an image whose path arrives as a
paste.  Their own clipboard paste cannot work, as the sandbox has no
display, and this way the goblin only sees the images you paste."
  (let* ((host (or (goblins--pinned-terminal-host) (user-error "Not a goblin terminal")))
         (path (format "%s%s.%s" goblins--clipboard-directory
                       (format-time-string "%Y%m%d-%H%M%S-%3N")
                       (goblins--image-extension type)))
         (file (format "/goblins:%s:%s" host path))
         ;; TRAMP's progress messages would hide the paste in the echo area.
         (inhibit-message t))
    (make-directory (file-name-directory file) t)
    (with-temp-buffer
      (set-buffer-multibyte nil)
      (insert data)
      (let ((coding-system-for-write 'no-conversion))
        (write-region nil nil file nil 'silent)))
    (ghostel-paste-string path)
    (goblins--redraw-after-paste (current-buffer))))

(defun goblins--redraw-after-paste (buffer)
  "Redraw terminal BUFFER shortly after a paste, once the agent has answered.
In a graphical frame Ghostel can leave Codex's answer to a pasted image
path unrendered until the next key, though `ghostel-force-redraw' shows it."
  (dolist (delay '(0.2 1))
    (run-with-timer delay nil
                    (lambda ()
                      (when (and (buffer-live-p buffer) (get-buffer-window buffer t))
                        (with-current-buffer buffer
                          (ghostel-force-redraw)))))))

(defun goblins-paste-image ()
  "Paste the clipboard's image into this goblin's agent.
The image is written inside the goblin and its path pasted, see
`goblins--yank-media-image'.  \[yank-media] does the same."
  (interactive)
  (goblins--terminal-goblin)
  (yank-media))

(defvar goblins-goblin-mode-command-map
  (let ((map (make-sparse-keymap)))
    (define-key map "w" #'goblins-workspace)
    (define-key map "s" #'goblins-switch)
    (define-key map "i" #'goblins-info)
    (define-key map "p" #'goblins-permissions)
    (define-key map "r" #'goblins-raid)
    (define-key map "K" #'goblins-stop)
    (define-key map "t" #'goblins-shell)
    (define-key map "e" #'goblins-eshell)
    (define-key map "d" #'goblins-dired)
    (define-key map "f" #'goblins-find-file)
    (define-key map "v" #'goblins-paste-image)
    map)
  "Goblin commands, on \\`C-c g' in `goblins-goblin-mode-map'.")

(defvar goblins-goblin-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-c g") goblins-goblin-mode-command-map)
    map)
  "Keymap of `goblins-goblin-mode'.  Add your own goblin terminal keys here.")

;;;###autoload
(define-minor-mode goblins-goblin-mode
  "Minor mode of Ghostel buffers attached to a goblin.
Goblins turns it on in every terminal it opens; bind your own keys in
`goblins-goblin-mode-map', and run code in `goblins-goblin-mode-hook'.

\\{goblins-goblin-mode-map}"
  :lighter " Goblin"
  :keymap goblins-goblin-mode-map
  (when (and goblins-goblin-mode (fboundp 'yank-media-handler))
    (yank-media-handler "image/.*" #'goblins--yank-media-image))
  ;; Evil's state maps for minor modes need refreshing when one turns on.
  (when (fboundp 'evil-normalize-keymaps)
    (evil-normalize-keymaps)))

(provide 'goblins-terminal)
;;; goblins-terminal.el ends here
