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
;; - Consult: `consult-buffer' lists agent terminals under the narrowing key g.

;;; Code:

(require 'cl-lib)
(require 'subr-x)
(require 'goblins)
(require 'goblins-views)
(require 'goblins-raids)

(declare-function goblins-shell "goblins-tramp" (&optional session display source))
(declare-function goblins-eshell "goblins-tramp" (&optional session display source))
(declare-function goblins-dired "goblins-tramp" (&optional session display source))
(declare-function goblins-find-file "goblins-tramp" (&optional session source))
(declare-function goblins-show-in-workspace "goblins-workspace" (source id))
(declare-function marginalia-annotate-buffer "marginalia" (cand))
(declare-function consult--buffer-state "consult" ())
(defvar embark-general-map)
(defvar embark-buffer-map)
(defvar embark-keymap-alist)
(defvar embark-transformer-alist)
(defvar embark-exporters-alist)
(defvar marginalia-annotators)
(defvar marginalia-annotator-registry)
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
  "Stop goblin ID of SOURCE's server and its children, after confirming."
  (interactive (goblins--choose-goblin "Stop goblin: "))
  (goblins-stop-goblin source id))

(defun goblins-goblin-kill-execs (source id)
  "Kill the shells and commands inside goblin ID of SOURCE's server."
  (interactive (goblins--choose-goblin "Kill shells and commands in goblin: "))
  (goblins-kill-goblin-execs source id))

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
    map)
  "Embark actions on a goblin, also on its agent terminal among buffers.")

(defvar goblins-embark-goblin-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map goblins-embark-terminal-actions)
    (define-key map (kbd "RET") #'goblins-goblin-show)
    (define-key map (kbd "o") #'goblins-goblin-show)
    map)
  "Embark actions for `goblin' targets; Embark's general actions too.")

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

(with-eval-after-load 'marginalia
  (let ((registry (if (boundp 'marginalia-annotators)
                      'marginalia-annotators
                    'marginalia-annotator-registry)))
    (unless (memq #'goblins-marginalia-annotate-buffer
                  (alist-get 'buffer (symbol-value registry)))
      (setf (alist-get 'buffer (symbol-value registry))
            (cons #'goblins-marginalia-annotate-buffer
                  (alist-get 'buffer (symbol-value registry)))))))

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
