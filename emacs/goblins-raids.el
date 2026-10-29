;;; goblins-raids.el --- Raid pages and commands for Goblins -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0"))
;; Keywords: tools, processes

;;; Commentary:
;; A raid is a group of goblins that can find and message each other.
;; `goblins-raid' creates one around a goblin (or adds the goblin to an
;; existing one) and opens its page, which lists the members and the running
;; goblins that could still be invited.  Invitations join immediately; there
;; is no acceptance step.  A raid whose owner left is paused until you make a
;; member its owner or dissolve it.

;;; Code:

(require 'cl-lib)
(require 'subr-x)
(require 'goblins)
(require 'goblins-views)

(declare-function goblins--sidebar-ensure-source "goblins-workspace" ())
(declare-function goblins--main-window "goblins-views" ())
(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))

(defconst goblins--raid-buffer-name "*Goblins Raid*")

;;;; Data

(defun goblins--session-raid-id (session)
  (let ((raid (plist-get session :raid)))
    (and (listp raid) (plist-get raid :id))))

(defun goblins--snapshot-raids (snapshot)
  (append (plist-get snapshot :raids) nil))

(defun goblins--snapshot-raid (snapshot name)
  (cl-find name (goblins--snapshot-raids snapshot)
           :key (lambda (r) (plist-get r :name)) :test #'equal))

(defun goblins--raid-candidates (snapshot)
  "Running sessions in SNAPSHOT that belong to no raid."
  (cl-remove-if-not
   (lambda (s) (and (equal (plist-get s :state) "running")
                    (not (goblins--session-raid-id s))))
   (append (plist-get snapshot :sessions) nil)))

(defun goblins--read-session (source sessions prompt)
  "Choose one of SESSIONS of SOURCE's server, with PROMPT, and return its record."
  (unless sessions (user-error "No running goblin outside a raid"))
  (goblins--snapshot-session (buffer-local-value 'goblins--snapshot source)
                             (cdr (goblins--read-goblin prompt source sessions))))

(defun goblins--raid-invite (source raid-name session-id)
  "Add SESSION-ID to the raid RAID-NAME on SOURCE's server, creating it if absent."
  (goblins--host-call source 'raids.invite
                      (list :raid raid-name :sessions (vector session-id))))

;;;; Page

(defvar-local goblins--raid-source nil)

(defun goblins--raid-member-name (raid snapshot id)
  "The name of session ID, a member of RAID or else a goblin of SNAPSHOT."
  (if-let* ((member (cl-find id (append (plist-get raid :members) nil)
                             :key (lambda (m) (plist-get m :id)) :test #'equal)))
      (goblins--safe (plist-get member :agent_name))
    (goblins--session-label snapshot id)))

(defun goblins--raid-member-line (member session terminal creator)
  "Insert MEMBER of a raid; SESSION is its record and TERMINAL its Emacs buffer.
CREATOR names the owner that ran it, if one did."
  (let ((start (point)))
    (insert "  " (goblins--state-glyph (plist-get member :state)) " "
            (goblins--safe (plist-get member :agent_name))
            (propertize (format " (%s)" (goblins--safe (plist-get member :name)))
                        'face 'goblins-dim)
            (if (eq (plist-get member :owner) t)
                (propertize "  owner" 'face 'goblins-attention) "")
            (if-let* ((role (plist-get member :role)) ((stringp role)))
                (propertize (format "  %s" (goblins--safe role)) 'face 'goblins-key) "")
            (if creator (propertize (format "  created by %s" creator) 'face 'goblins-dim) "")
            (if-let* ((cwd (plist-get session :cwd)) ((stringp cwd)))
                (propertize (format "  %s" (abbreviate-file-name cwd)) 'face 'goblins-dim)
              "")
            (if terminal
                (propertize "  [term]" 'face 'goblins-key
                            'help-echo (format "Open in %s" (buffer-name terminal)))
              "")
            "\n")
    (add-text-properties start (point)
                         (list 'goblins-id (plist-get member :id) 'goblins-raid-line 'member))))

(defun goblins--raid-candidate-line (session)
  (let ((start (point)))
    (insert "  " (goblins--state-glyph (plist-get session :state)) " "
            (goblins--safe (plist-get session :agent_name))
            (propertize (format " (%s)" (goblins--safe (plist-get session :name)))
                        'face 'goblins-dim)
            (if-let* ((cwd (plist-get session :cwd)) ((stringp cwd)))
                (propertize (format "  %s" (abbreviate-file-name cwd)) 'face 'goblins-dim)
              "")
            "\n")
    (add-text-properties start (point)
                         (list 'goblins-id (plist-get session :id) 'goblins-raid-line 'candidate))))

(defun goblins--raid-key-at (pos)
  (save-excursion (goto-char pos)
                  (get-text-property (line-beginning-position) 'goblins-id)))

(defun goblins--raid-render ()
  "Redraw the raid page from the current snapshot, keeping the selected line."
  (let* ((inhibit-read-only t)
         (snapshot (goblins--view-snapshot))
         (name goblins--view-session)
         (raid (and snapshot (goblins--snapshot-raid snapshot name)))
         (members (and raid (append (plist-get raid :members) nil)))
         (candidates (and raid (goblins--raid-candidates snapshot)))
         (key (goblins--raid-key-at (point)))
         (line (line-number-at-pos)))
    (erase-buffer)
    (cond
     ((null snapshot)
      (insert (propertize "Not connected to the server\n" 'face 'goblins-dim)))
     ((null raid)
      (insert (propertize (format "raid %s" (goblins--safe name)) 'face 'bold) "\n\n"
              (propertize "This raid no longer exists. Its goblins keep running.\n"
                          'face 'goblins-dim)))
     (t
      (insert (propertize (format "raid %s" (goblins--safe name)) 'face 'bold)
              (propertize (format "   %d member%s · " (length members)
                                  (if (= (length members) 1) "" "s"))
                          'face 'goblins-dim)
              (if (eq (plist-get raid :paused) t)
                  (concat (propertize "paused" 'face 'goblins-attention)
                          (propertize
                           (if-let* ((former (plist-get raid :former_owner)) ((stringp former)))
                               (format " since %s left · o picks a new owner"
                                       (goblins--raid-member-name raid snapshot former))
                             " · o picks a new owner")
                           'face 'goblins-dim))
                (propertize (format "owner %s" (goblins--raid-member-name
                                                raid snapshot (plist-get raid :owner)))
                            'face 'goblins-dim))
              "\n")
      (goblins--insert-messaging-warning snapshot)
      (insert "\n")
      (insert (propertize (format "Members (%d)" (length members)) 'face 'goblins-heading) "\n")
      (dolist (member members)
        (goblins--raid-member-line
         member (goblins--snapshot-session snapshot (plist-get member :id))
         (with-current-buffer goblins--view-source
           (goblins--terminal-buffer (plist-get member :id)))
         (when-let* ((creator (plist-get member :created_by)) ((stringp creator)))
           (goblins--raid-member-name raid snapshot creator))))
      (insert "\n" (propertize (format "Available to invite (%d)" (length candidates))
                               'face 'goblins-heading)
              "\n")
      (if (null candidates)
          (insert (propertize "  every running goblin is in a raid\n" 'face 'goblins-dim))
        (dolist (session candidates) (goblins--raid-candidate-line session)))))
    (goto-char (point-min))
    (let ((pos (or (and key (text-property-any (point-min) (point-max) 'goblins-id key))
                   (progn (forward-line (1- line)) (point)))))
      (goblins--view-goto (save-excursion (goto-char pos) (line-beginning-position))))))

;;;; Commands

(defun goblins-raid-next (&optional n)
  "Move to the next goblin line, N times."
  (interactive "p")
  (let ((direction (if (< (or n 1) 0) -1 1)))
    (dotimes (_ (abs (or n 1)))
      (let ((start (point)) found)
        (while (and (not found) (zerop (forward-line direction)))
          (when (get-text-property (point) 'goblins-id) (setq found t)))
        (if found (beginning-of-line) (goto-char start))))))

(defun goblins-raid-previous (&optional n)
  "Move to the previous goblin line, N times."
  (interactive "p")
  (goblins-raid-next (- (or n 1))))

(defun goblins--raid-line ()
  "Return (KIND . SESSION-ID) for the goblin line at point, or nil."
  (let ((kind (get-text-property (line-beginning-position) 'goblins-raid-line)))
    (and kind (cons kind (get-text-property (line-beginning-position) 'goblins-id)))))

(defun goblins-raid-open ()
  "Show the terminal of the goblin at point, attaching if necessary."
  (interactive)
  (let ((line (goblins--raid-line)))
    (unless line (user-error "Select a goblin"))
    (goblins-open-session goblins--view-source (cdr line))))

(defun goblins-raid-invite ()
  "Invite a goblin into this raid: the one at point, or one you choose."
  (interactive)
  (let* ((snapshot (goblins--view-snapshot))
         (line (goblins--raid-line))
         (session (if (eq (car line) 'candidate)
                      (goblins--snapshot-session snapshot (cdr line))
                    (goblins--read-session goblins--view-source
                                           (goblins--raid-candidates snapshot)
                                           (format "Invite to %s: " goblins--view-session)))))
    (goblins--raid-invite goblins--view-source goblins--view-session (plist-get session :id))
    (message "Invited %s to raid %s" (plist-get session :agent_name) goblins--view-session)))

(defun goblins-raid-kick ()
  "Remove the member at point from this raid."
  (interactive)
  (let* ((snapshot (goblins--view-snapshot))
         (line (goblins--raid-line))
         (raid (goblins--snapshot-raid snapshot goblins--view-session))
         (session (and (eq (car line) 'member)
                       (goblins--snapshot-session snapshot (cdr line)))))
    (unless session (user-error "Select a member to kick"))
    (let ((owner (equal (plist-get raid :owner) (plist-get session :id))))
      (when (y-or-n-p (format "Kick %s out of raid %s%s? "
                              (plist-get session :agent_name) goblins--view-session
                              (if owner " (the owner: this pauses the raid until you select a new owner)" "")))
        (goblins--host-call goblins--view-source 'raids.kick
                            (list :raid goblins--view-session :session (plist-get session :id)))
        (message "Kicked %s out of raid %s" (plist-get session :agent_name) goblins--view-session)))))

(defun goblins-raid-set-owner ()
  "Make the member at point this raid's owner.  A paused raid resumes.
The daemon refuses it if the raid changed since the page showed it."
  (interactive)
  (let* ((snapshot (goblins--view-snapshot))
         (line (goblins--raid-line))
         (raid (goblins--snapshot-raid snapshot goblins--view-session))
         (member (and (eq (car line) 'member)
                      (cl-find (cdr line) (append (plist-get raid :members) nil)
                               :key (lambda (m) (plist-get m :id)) :test #'equal))))
    (unless member (user-error "Select a member to make owner"))
    (when (eq (plist-get member :owner) t)
      (user-error "%s already owns raid %s" (plist-get member :agent_name) goblins--view-session))
    (goblins--host-call goblins--view-source 'raids.set_owner
                        (list :raid goblins--view-session :session (cdr line)
                              :id (plist-get raid :id) :generation (plist-get raid :generation)))
    (message "%s now owns raid %s%s" (plist-get member :agent_name) goblins--view-session
             (if (eq (plist-get raid :paused) t) "; it has resumed" ""))))

(defun goblins-raid-set-role ()
  "Set the descriptive role of the member at point.
Roles can be changed but not cleared."
  (interactive)
  (let* ((snapshot (goblins--view-snapshot))
         (line (goblins--raid-line))
         (raid (goblins--snapshot-raid snapshot goblins--view-session))
         (member (and (eq (car line) 'member)
                      (cl-find (cdr line) (append (plist-get raid :members) nil)
                               :key (lambda (m) (plist-get m :id)) :test #'equal))))
    (unless member (user-error "Select a member"))
    (let ((role (string-trim (read-string (format "Role for %s: " (plist-get member :agent_name))
                                          (and (stringp (plist-get member :role))
                                               (plist-get member :role))))))
      (when (string-empty-p role) (user-error "Roles can be changed but not cleared"))
      (goblins--host-call goblins--view-source 'raids.set_role
                          (list :raid goblins--view-session :session (cdr line) :role role))
      (message "%s is now %s" (plist-get member :agent_name) role))))

(defun goblins-destroy-raid (source name &optional stop)
  "Dissolve the raid NAME on SOURCE's server after confirming.
Its goblins keep running unless STOP, which stops every member.  The raid's
ID and generation are taken before asking, so a raid that changed while you
decided, or another one by the same name, is refused rather than dissolved."
  (let* ((raid (or (goblins--snapshot-raid (buffer-local-value 'goblins--snapshot source) name)
                   (user-error "No raid %s" name)))
         (generation (plist-get raid :generation))
         (params (append (list :raid name :id (plist-get raid :id))
                         (and (numberp generation) (list :generation generation))
                         (and stop (list :stop_members t)))))
    (when (y-or-n-p (if stop
                        (format "Dissolve raid %s and stop all its members? " name)
                      (format "Dissolve raid %s?  Its goblins keep running. " name)))
      (goblins--host-call source 'raids.destroy params)
      (message "Dissolved raid %s%s" name (if stop " and stopped its members" "")))))

(defun goblins-raid-destroy (&optional stop)
  "Dissolve this raid after confirming.  Its goblins keep running; with a
prefix argument STOP, they are stopped too."
  (interactive "P")
  (goblins-destroy-raid goblins--view-source goblins--view-session stop))

(defun goblins-raid-help ()
  "Describe the raid page keys."
  (interactive)
  (goblins--describe-keys
   "Goblins raid page"
   '(("Move" ("j / k" . "next / previous goblin"))
     ("Members" ("RET" . "show or attach the goblin's terminal")
                ("o" . "make the member owner (resumes a paused raid)")
                ("x" . "kick the member (kicking the owner pauses the raid)")
                ("R" . "set the member's role"))
     ("Raid" ("i" . "invite the goblin at point, or choose one")
             ("D" . "dissolve the raid; its goblins keep running")
             ("C-u D" . "dissolve the raid and stop its members"))
     ("Page" ("g" . "reconnect") ("q" . "back") ("?" . "this help")))))

(defun goblins-raid-refresh ()
  "Reconnect to the server for a fresh snapshot."
  (interactive)
  (with-current-buffer goblins--view-source (goblins-refresh)))

(defvar goblins-raid-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (dolist (binding '(("j" . goblins-raid-next) ("k" . goblins-raid-previous)
                       ("<down>" . goblins-raid-next) ("<up>" . goblins-raid-previous)
                       ("RET" . goblins-raid-open) ("i" . goblins-raid-invite)
                       ("x" . goblins-raid-kick) ("R" . goblins-raid-set-role)
                       ("o" . goblins-raid-set-owner)
                       ("D" . goblins-raid-destroy) ("?" . goblins-raid-help)
                       ("g" . goblins-raid-refresh) ("q" . quit-window)))
      (define-key map (kbd (car binding)) (cdr binding)))
    map))

(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-raid-mode 'motion)
  (evil-define-key* 'motion goblins-raid-mode-map
    (kbd "j") #'goblins-raid-next
    (kbd "k") #'goblins-raid-previous
    (kbd "RET") #'goblins-raid-open
    (kbd "i") #'goblins-raid-invite
    (kbd "x") #'goblins-raid-kick
    (kbd "o") #'goblins-raid-set-owner
    (kbd "R") #'goblins-raid-set-role
    (kbd "D") #'goblins-raid-destroy
    (kbd "?") #'goblins-raid-help
    (kbd "gr") #'goblins-raid-refresh
    (kbd "q") #'quit-window))

(define-derived-mode goblins-raid-mode special-mode "Goblins-Raid"
  "Members of a raid, and the goblins that could join it.
\\<goblins-raid-mode-map>
\\[goblins-raid-invite] invites, \\[goblins-raid-kick] kicks, \\[goblins-raid-set-owner] makes owner, \\[goblins-raid-open] opens the goblin at point."
  (setq-local truncate-lines t
              header-line-format " Raid   i invite   o owner   x kick   R role   D dissolve   ? help")
  (hl-line-mode 1)
  (setq-local revert-buffer-function (lambda (&rest _) (goblins-raid-refresh))))

(defun goblins-raid-buffer (source name)
  "Return the page of the raid called NAME on SOURCE's server."
  (goblins--view-buffer goblins--raid-buffer-name #'goblins-raid-mode
                        #'goblins--raid-render source name))

;;;; Creating and joining

(defun goblins--raid-context ()
  "Return (SOURCE . SESSION-ID) for the goblin in the current context, or nil.
That is the goblin on this sidebar line, else the goblin shown in the main
window, or the goblin whose terminal is the current buffer."
  (cond
   ((derived-mode-p 'goblins-sidebar-mode)
    (let ((shown (and (goblins--main-window)
                      (buffer-local-value 'goblins--session-key
                                          (window-buffer (goblins--main-window))))))
      (cond ((get-text-property (line-beginning-position) 'goblins-id)
             (cons (goblins--sidebar-ensure-source)
                   (get-text-property (line-beginning-position) 'goblins-id)))
            (shown (cons (goblins--sidebar-ensure-source) (nth 2 shown))))))
   (goblins--session-key
    (cons (goblins--status-buffer (car goblins--session-key))
          (nth 2 goblins--session-key)))))

;;;###autoload
(defun goblins-raid ()
  "Create a raid around a goblin, or add it to an existing one, and open the page.
The goblin is the one on this sidebar line or in this terminal buffer;
otherwise choose a running goblin that is not in a raid."
  (interactive)
  (let* ((context (goblins--raid-context))
         (source (or (car context)
                     (and (derived-mode-p 'goblins-sidebar-mode) (goblins--sidebar-ensure-source))
                     (goblins--connect)))
         (snapshot (buffer-local-value 'goblins--snapshot source))
         (session (if context
                      (or (goblins--snapshot-session snapshot (cdr context))
                          (user-error "The daemon no longer retains this goblin"))
                    (goblins--read-session source (goblins--raid-candidates snapshot)
                                           "Create a raid around: "))))
    (goblins--raid-around source session)))

(defun goblins--raid-around (source session)
  "Create a raid around SESSION of SOURCE's server, or add it to one you
choose, and open the raid's page."
  (let ((names (mapcar (lambda (r) (plist-get r :name))
                       (goblins--snapshot-raids (buffer-local-value 'goblins--snapshot source)))))
    (when-let* ((id (goblins--session-raid-id session)))
      (user-error "%s is already in a raid" (plist-get session :agent_name)))
    (let ((name (string-trim (completing-read "Raid (new or existing): " names))))
      (when (string-empty-p name) (user-error "Name the raid"))
      (goblins--raid-invite source name (plist-get session :id))
      (goblins--display-main (goblins-raid-buffer source name)))))

(defun goblins-invite-to-raid (source session-id raid-name)
  "Add SESSION-ID to the existing raid RAID-NAME on SOURCE's server."
  (let* ((snapshot (buffer-local-value 'goblins--snapshot source))
         (session (goblins--snapshot-session snapshot session-id)))
    (unless session (user-error "The daemon no longer retains this goblin"))
    (when (goblins--session-raid-id session)
      (user-error "%s is already in a raid" (plist-get session :agent_name)))
    (goblins--raid-invite source raid-name session-id)
    (message "Added %s to raid %s" (plist-get session :agent_name) raid-name)))

(defun goblins--invite-to-chosen-raid (source session-id)
  "Add SESSION-ID of SOURCE's server to an existing raid you choose."
  (let* ((snapshot (buffer-local-value 'goblins--snapshot source))
         (session (goblins--snapshot-session snapshot session-id))
         (names (mapcar (lambda (r) (plist-get r :name)) (goblins--snapshot-raids snapshot))))
    (unless names (user-error "No raid yet; create one with r"))
    (unless session (user-error "The daemon no longer retains this goblin"))
    (when (goblins--session-raid-id session)
      (user-error "%s is already in a raid" (plist-get session :agent_name)))
    (goblins-invite-to-raid
     source session-id (completing-read (format "Invite %s to raid: "
                                                (plist-get session :agent_name))
                                        names nil t))))

(provide 'goblins-raids)
;;; goblins-raids.el ends here
