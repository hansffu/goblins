;;; emacs-init.el --- Load checkout source in test Emacsen -*- lexical-binding: t; -*-

;; Put only source files ahead of the checkout's potentially stale .elc files.
;; Keep installed compiled dependencies fast to load, including compressed ones.
(let* ((source (expand-file-name "../emacs" (file-name-directory load-file-name)))
       (directory (make-temp-file "goblins-elisp-" t)))
  (dolist (file (directory-files source t "\\.el\\'"))
    (make-symbolic-link file (expand-file-name (file-name-nondirectory file) directory)))
  (add-to-list 'load-path directory)
  (add-hook 'kill-emacs-hook (lambda () (delete-directory directory t))))
(setq load-prefer-newer t)
