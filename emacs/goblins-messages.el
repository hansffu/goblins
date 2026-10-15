;;; goblins-messages.el --- The messages goblins send each other -*- lexical-binding: t; -*-

;; Version: 0.1.0
;; Package-Requires: ((emacs "28.1") (magit-section "4.0.0") (transient "0.4.0"))
;; Keywords: tools, processes

;;; Commentary:
;; `goblins-messages' lists the messages the daemon's communications log
;; holds, one thread per line, folded: the first message, its sender and
;; recipient, and how many replies are hidden.  TAB unfolds a thread; its
;; replies hang below it.  A reader beside the list (below it in a narrow
;; window) shows the message at point in full and follows point.
;;
;; f opens a menu of filters: who sent or received the messages, their raid
;; or thread, whether they still await a reply or failed, and when they were
;; sent.  Embark acts on a message (its sender's or recipient's messages, its
;; thread, its raid), on a thread, and on the goblins named in the reader.
;; The sidebar's m and Embark's m on a goblin open the list filtered too.

;;; Code:

(require 'cl-lib)
(require 'subr-x)
(require 'jsonrpc)
(require 'transient)
(require 'goblins)
(require 'goblins-views)
(require 'goblins-raids)

(declare-function evil-set-initial-state "evil-core" (mode state))
(declare-function evil-define-key* "evil-core" (state keymap key def &rest bindings))
(defvar embark-general-map)
(defvar embark-keymap-alist)
(defvar embark-target-finders)

(defconst goblins--messages-buffer-name "*Goblins Messages*")
(defconst goblins--message-buffer-name "*Goblins Message*")

(defcustom goblins-messages-reader-side-width 160
  "Columns from which the reader goes beside the message list.
In a narrower window the reader goes below the list."
  :type 'integer :group 'goblins)

(defcustom goblins-messages-goblin-faces
  '(font-lock-keyword-face font-lock-warning-face font-lock-string-face
    font-lock-constant-face font-lock-function-name-face font-lock-type-face)
  "Faces that tell goblins apart in the message list, in order of appearance."
  :type '(repeat face) :group 'goblins)

(defface goblins-messages-filter '((t :inherit region))
  "An active filter in the message list's header line." :group 'goblins)
(defface goblins-messages-ref '((t :inherit font-lock-constant-face))
  "A message or thread ID in the reader." :group 'goblins)

;;;; The log

(defvar-local goblins--messages-cache-source nil
  "Status buffer whose server's log the cache holds.")
(defvar-local goblins--messages-instance nil
  "Daemon instance whose log the cache holds.")
(defvar-local goblins--messages-cursor 0
  "Sequence of the last log event read.")
(defvar-local goblins--messages nil
  "Message ID to its record, a plist.")
(defvar-local goblins--messages-order nil
  "Message IDs, newest first.")
(defvar-local goblins--messages-raids nil
  "Raid ID to its name, as the log recorded it.")
(defvar-local goblins--messages-colors nil
  "Session ID to its index in `goblins-messages-goblin-faces'.")
(defvar-local goblins--messages-fetching nil
  "Non-nil while a page of the log is being read.")
(defvar-local goblins--messages-again nil
  "Non-nil to read the log again once the page being read arrives.")
(defvar-local goblins--messages-error nil
  "Why the log could not be read, or nil.")

(defun goblins--messages-short (id)
  "ID without its daemon instance: \"m51\" for \"5363…-m51\"."
  (if (and (stringp id) (string-match "-\\([^-]+\\)\\'" id)) (match-string 1 id) id))

(defun goblins--messages-reset (source instance)
  "Empty the cache, for the log of INSTANCE on SOURCE's server."
  (setq goblins--messages-cache-source source
        goblins--messages-instance instance
        goblins--messages-cursor 0
        goblins--messages (make-hash-table :test #'equal)
        goblins--messages-order nil
        goblins--messages-raids (make-hash-table :test #'equal)
        goblins--messages-colors (make-hash-table :test #'equal)
        goblins--messages-error nil
        goblins--messages-again nil))

(defun goblins--messages-get (id)
  (and goblins--messages (stringp id) (gethash id goblins--messages)))

(defun goblins--messages-add (message raid)
  "Record MESSAGE, a message object from the log, sent within RAID if non-nil.
A reply belongs to the raid of the message it answers."
  (let ((id (plist-get message :id))
        (reply-to (plist-get message :in_reply_to)))
    (when (stringp id)
      (unless (gethash id goblins--messages) (push id goblins--messages-order))
      (puthash id
               (list :id id
                     :conversation (or (plist-get message :conversation) id)
                     :sender (plist-get message :sender)
                     :sender-path (plist-get message :sender_path)
                     :recipient (plist-get message :recipient)
                     :recipient-path (plist-get message :recipient_path)
                     :body (plist-get message :body)
                     :created (plist-get message :created_at)
                     :reply-to (and (stringp reply-to) reply-to)
                     :state (plist-get message :state)
                     :failure (plist-get message :failure)
                     :raid (if (stringp raid) raid
                             (plist-get (goblins--messages-get reply-to) :raid)))
               goblins--messages))))

(defun goblins--messages-update (id &rest properties)
  "Set PROPERTIES of the recorded message ID."
  (when-let* ((record (goblins--messages-get id)))
    (while properties
      (setq record (plist-put record (pop properties) (pop properties))))
    (puthash id record goblins--messages)))

(defun goblins--messages-ingest (event)
  "Apply EVENT, a communications log event, to the cache."
  (let* ((kind (plist-get event :kind))
         (data (plist-get event :data))
         (change (and (listp data) (plist-get data :change))))
    (pcase kind
      ((or "message.accepted" "message.replied")
       (goblins--messages-add (plist-get change :message) (plist-get change :raid))
       (when (stringp (plist-get change :completed))
         (goblins--messages-update (plist-get change :completed) :state "completed")))
      ("message.claimed"
       (goblins--messages-update (plist-get change :message) :state "claimed"
                                 :claimed-by (plist-get event :actor)
                                 :claimed-at (plist-get event :timestamp)))
      ("message.completed"
       (goblins--messages-update (plist-get change :message) :state "completed"))
      ("message.requeued"
       (goblins--messages-update (plist-get change :message) :state "queued"))
      ("recipient.stopped"
       (mapc (lambda (id) (goblins--messages-update id :state "failed"
                                                    :failure "recipient stopped"))
             (plist-get data :messages)))
      ("raid.created"
       (when (stringp (plist-get data :raid))
         (puthash (plist-get data :raid) (plist-get data :name) goblins--messages-raids)))
      (_
       ;; An operation's outcome is delivered as a notice from goblins.
       (let ((notice (and (listp data) (plist-get data :notice))))
         (when (and (listp notice) (plist-get notice :id))
           (goblins--messages-add notice nil)))))))

(defun goblins--messages-records ()
  "Every recorded message, oldest first."
  (mapcar #'goblins--messages-get (reverse goblins--messages-order)))

(defun goblins--messages-fetch ()
  "Read the log events after the cursor from the source's server, then render.
Pages follow each other until the log is read to its end."
  (let ((buffer (current-buffer))
        (source goblins--view-source))
    (cond
     ((not (buffer-live-p source)))
     (goblins--messages-fetching (setq goblins--messages-again t))
     (t
      (let ((connection (buffer-local-value 'goblins--connection source))
            (instance (buffer-local-value 'goblins--instance source)))
        (when (and connection instance (buffer-local-value 'goblins--subscription source))
          (unless (and (eq source goblins--messages-cache-source)
                       (equal instance goblins--messages-instance))
            (goblins--messages-reset source instance))
          (setq goblins--messages-fetching t goblins--messages-again nil)
          (let ((after goblins--messages-cursor)
                (failed (lambda (text)
                          (when (buffer-live-p buffer)
                            (with-current-buffer buffer
                              (setq goblins--messages-fetching nil
                                    goblins--messages-error text)
                              (goblins--messages-render))))))
            (jsonrpc-async-request
             connection 'communications.list (list :after after :limit 100)
             :timeout 10
             :success-fn (lambda (page)
                           (when (buffer-live-p buffer)
                             (with-current-buffer buffer
                               (goblins--messages-page page source instance after))))
             :error-fn (lambda (error)
                         (funcall failed (format "Cannot read the messages: %s"
                                                 (plist-get error :message))))
             :timeout-fn (lambda ()
                           (funcall failed "Cannot read the messages: the server did not answer"))))))))))

(defun goblins--messages-page (page source instance after)
  "Apply PAGE, read AFTER a sequence from INSTANCE on SOURCE's server."
  (setq goblins--messages-fetching nil)
  (if (not (and (eq source goblins--messages-cache-source)
                (equal instance goblins--messages-instance)
                (= after goblins--messages-cursor)))
      ;; The cache was emptied meanwhile: read it again from its start.
      (goblins--messages-fetch)
    (let ((events (append (plist-get page :events) nil))
          (cursor (plist-get page :cursor))
          (latest (plist-get page :latest)))
      (mapc #'goblins--messages-ingest events)
      (when (and (integerp cursor) (> cursor goblins--messages-cursor))
        (setq goblins--messages-cursor cursor))
      (when (or events goblins--messages-error (= after 0))
        (setq goblins--messages-error nil)
        (goblins--messages-render))
      (when (or goblins--messages-again
                (and (integerp latest) (integerp cursor) (> cursor after) (< cursor latest)))
        (goblins--messages-fetch)))))

(defun goblins--messages-refresh ()
  "The list's view function: read what the log gained since it was last read.
The server's every new snapshot calls it."
  (goblins--messages-fetch)
  (when goblins--view-fresh
    (setq goblins--view-fresh nil)
    (goblins--messages-render)))

;;;; Names

(defun goblins--messages-goblin-path (id)
  "The tree path of session ID, from its messages or the snapshot."
  (or (cl-some (lambda (record)
                 (cond ((equal (plist-get record :sender) id) (plist-get record :sender-path))
                       ((equal (plist-get record :recipient) id) (plist-get record :recipient-path))))
               (and goblins--messages (hash-table-values goblins--messages)))
      (let ((session (goblins--snapshot-session (goblins--view-snapshot) id)))
        (and session (goblins--session-path session)))
      (goblins--messages-short id)))

(defun goblins--messages-goblin-face (id)
  "The face that marks session ID, the next free one for a goblin new here."
  (let ((faces goblins-messages-goblin-faces))
    (if (null faces) 'bold
      (let ((index (or (gethash id goblins--messages-colors)
                       (puthash id (hash-table-count goblins--messages-colors)
                                goblins--messages-colors))))
        (list (nth (mod index (length faces)) faces) 'bold)))))

(defun goblins--messages-goblin (id path)
  "PATH, the name of session ID, in its face."
  (propertize (goblins--safe (or path (goblins--messages-goblin-path id)))
              'face (goblins--messages-goblin-face id)))

(defun goblins--messages-raid-name (id)
  "The name of raid ID, from the log or the snapshot."
  (or (and goblins--messages-raids (gethash id goblins--messages-raids))
      (plist-get (cl-find id (goblins--snapshot-raids (goblins--view-snapshot))
                          :key (lambda (r) (plist-get r :id)) :test #'equal)
                 :name)
      (goblins--messages-short id)))

(defun goblins--messages-time (ms &optional long)
  "The time of MS, Unix milliseconds: \"15:36:44\", with the date when LONG
and not today."
  (if (not (numberp ms)) "--:--:--"
    (let ((time (/ ms 1000.0)))
      (if (and long (not (equal (format-time-string "%F" time) (format-time-string "%F"))))
          (format-time-string "%F %H:%M:%S" time)
        (format-time-string "%H:%M:%S" time)))))

(defun goblins--messages-text (value)
  "VALUE, a message body, with its lines kept and other control and bidi
formatting characters escaped."
  (replace-regexp-in-string
   "[\0-\t\v-\37\177  ‪-‮⁦-⁩]"
   (lambda (s) (if (equal s "\t") s (format "\\u%04x" (string-to-char s))))
   (format "%s" (or value "")) t t))

(defun goblins--messages-summary (record)
  "The first line of RECORD's body that is not blank."
  (let ((body (format "%s" (or (plist-get record :body) ""))))
    (goblins--safe (or (cl-find-if (lambda (line) (not (string-blank-p line)))
                                   (split-string body "\n"))
                       ""))))

;;;; Filters

(defvar-local goblins--messages-filters nil
  "The list's filters, a plist: :goblin, :from and :to (session IDs), :raid
\(a raid ID), :thread (a conversation ID), :awaiting and :failed (t), :since
and :until (Unix seconds) and :last (minutes).")

(defun goblins--messages-match-p (record)
  "Whether RECORD passes the list's filters."
  (let* ((filters goblins--messages-filters)
         (sent (/ (or (plist-get record :created) 0) 1000.0))
         (want (lambda (key value)
                 (let ((filter (plist-get filters key)))
                   (or (null filter) (equal filter value))))))
    (and (or (null (plist-get filters :goblin))
             (member (plist-get filters :goblin)
                     (list (plist-get record :sender) (plist-get record :recipient))))
         (funcall want :from (plist-get record :sender))
         (funcall want :to (plist-get record :recipient))
         (funcall want :raid (plist-get record :raid))
         (funcall want :thread (plist-get record :conversation))
         (or (not (plist-get filters :awaiting))
             (member (plist-get record :state) '("queued" "claimed")))
         (or (not (plist-get filters :failed))
             (equal (plist-get record :state) "failed"))
         (or (null (plist-get filters :since)) (>= sent (plist-get filters :since)))
         (or (null (plist-get filters :until)) (<= sent (plist-get filters :until)))
         (or (null (plist-get filters :last))
             (>= sent (- (float-time) (* 60 (plist-get filters :last))))))))

(defun goblins--messages-filter-label (key value)
  "Describe the filter KEY set to VALUE, like \"from drakka\"."
  (pcase key
    (:goblin (format "goblin %s" (goblins--safe (goblins--messages-goblin-path value))))
    (:from (format "from %s" (goblins--safe (goblins--messages-goblin-path value))))
    (:to (format "to %s" (goblins--safe (goblins--messages-goblin-path value))))
    (:raid (format "raid %s" (goblins--safe (goblins--messages-raid-name value))))
    (:thread (format "thread %s" (goblins--messages-short value)))
    (:awaiting "awaiting reply")
    (:failed "failed")
    (:since (format "since %s" (goblins--messages-time (* 1000 value) t)))
    (:until (format "until %s" (goblins--messages-time (* 1000 value) t)))
    (:last (format "last %dm" value))))

(defconst goblins--messages-filter-keys
  '(:goblin :from :to :raid :thread :awaiting :failed :since :until :last))

(defun goblins--messages-chips ()
  "The active filters for the header line."
  (mapconcat (lambda (key)
               (propertize (concat " " (goblins--messages-filter-label
                                        key (plist-get goblins--messages-filters key))
                                   " ")
                           'face 'goblins-messages-filter))
             (cl-remove-if-not (lambda (key) (plist-get goblins--messages-filters key))
                               goblins--messages-filter-keys)
             " "))

;;;; The list

(defvar-local goblins--messages-unfolded nil
  "Conversation IDs of the threads shown unfolded.")
(defvar-local goblins--messages-shown nil
  "ID of the message the reader shows.")

(defun goblins--messages-threads (records)
  "RECORDS, oldest first, as threads ((CONVERSATION RECORD...)...) in the
order of their first message."
  (let ((table (make-hash-table :test #'equal)) order)
    (dolist (record records)
      (let ((conversation (plist-get record :conversation)))
        (unless (gethash conversation table) (push conversation order))
        (puthash conversation (cons record (gethash conversation table)) table)))
    (mapcar (lambda (c) (cons c (nreverse (gethash c table)))) (nreverse order))))

(defun goblins--messages-open-p (conversation)
  (or (equal (plist-get goblins--messages-filters :thread) conversation)
      (member conversation goblins--messages-unfolded)))

(defun goblins--messages-route-width (records)
  "Columns for \"sender → recipient\" of RECORDS."
  (min 40 (apply #'max 9 (mapcar (lambda (r) (string-width (goblins--messages-route r)))
                                 records))))

(defun goblins--messages-route (record)
  (concat (goblins--messages-goblin (plist-get record :sender) (plist-get record :sender-path))
          (propertize " → " 'face 'goblins-dim)
          (goblins--messages-goblin (plist-get record :recipient)
                                    (plist-get record :recipient-path))))

(defun goblins--messages-width ()
  "The text width of a window showing the list."
  (let ((window (get-buffer-window (current-buffer) t)))
    (if window (window-body-width window) 120)))

(defun goblins--messages-insert-row (record conversation route-width arrow glyph hidden)
  "Insert RECORD's row in thread CONVERSATION: its ARROW, time, route padded to
ROUTE-WIDTH, tree GLYPH and first line, and HIDDEN, the replies folded away."
  (let* ((start (point))
         (route (goblins--messages-route record))
         (prefix (concat (propertize arrow 'face 'goblins-dim)
                         (propertize (goblins--messages-time (plist-get record :created))
                                     'face 'goblins-dim)
                         "  "
                         (truncate-string-to-width route route-width nil ?\s "…")
                         "  "
                         (if glyph (propertize glyph 'face 'goblins-dim) "")
                         (if (equal (plist-get record :state) "failed")
                             (propertize "✗ " 'face 'goblins-state-failed)
                           "")))
         (suffix (if hidden (propertize (format "  +%d" hidden) 'face 'goblins-dim) ""))
         (room (max 10 (- (goblins--messages-width) (string-width prefix) (string-width suffix) 1))))
    (insert prefix
            (truncate-string-to-width (goblins--messages-summary record) room nil nil "…")
            suffix "\n")
    (add-text-properties start (point) (list 'goblins-message (plist-get record :id)
                                             'goblins-thread conversation))))

(defun goblins--messages-insert-thread (thread route-width)
  "Insert THREAD, (CONVERSATION RECORD...): its first message, then its
replies when unfolded."
  (let* ((conversation (car thread))
         (head (cadr thread))
         (replies (cddr thread))
         (open (and replies (goblins--messages-open-p conversation))))
    (goblins--messages-insert-row head conversation route-width
                                  (cond ((null replies) "  ") (open "▾ ") (t "▸ "))
                                  nil (and replies (not open) (length replies)))
    (when open
      (let ((left (length replies)))
        (dolist (record replies)
          (setq left (1- left))
          (goblins--messages-insert-row record conversation route-width "  "
                                        (if (= left 0) "└ " "├ ") nil))))))

(defun goblins--messages-header (shown total route-width)
  "Set the header line: column titles, the active filters, SHOWN of TOTAL."
  (let* ((chips (goblins--messages-chips))
         (count (propertize (if (= shown total) (format "%d messages" total)
                              (format "%d of %d" shown total))
                            'face 'goblins-dim))
         (right (concat chips (if (string-empty-p chips) "" "  ") count " ")))
    (setq header-line-format
          (concat (propertize " " 'display '(space :align-to 0))
                  (propertize "  Time" 'face 'bold)
                  (propertize " " 'display '(space :align-to 12))
                  (propertize "From → To" 'face 'bold)
                  (propertize " " 'display `(space :align-to ,(+ 14 route-width)))
                  (propertize "Message" 'face 'bold)
                  (propertize " " 'display `(space :align-to (- right ,(string-width right))))
                  right))))

(defun goblins--messages-render ()
  "Redraw the list from the cache and the filters, keeping the message at point."
  (let* ((inhibit-read-only t)
         (records (and goblins--messages (goblins--messages-records)))
         (shown (cl-remove-if-not #'goblins--messages-match-p records))
         (route-width (goblins--messages-route-width shown))
         (message (get-text-property (line-beginning-position) 'goblins-message))
         (thread (get-text-property (line-beginning-position) 'goblins-thread))
         (line (line-number-at-pos)))
    (erase-buffer)
    (cond
     (goblins--messages-error
      (insert (propertize (goblins--safe goblins--messages-error) 'face 'goblins-state-failed) "\n"))
     ((not (and (buffer-live-p goblins--view-source)
                (buffer-local-value 'goblins--subscription goblins--view-source)))
      (insert (propertize "Not connected to the server\n" 'face 'goblins-dim)))
     ((null goblins--messages-instance)
      (insert (propertize "Reading the messages…\n" 'face 'goblins-dim)))
     ((null records)
      (insert (propertize "No messages yet\n" 'face 'goblins-dim)))
     ((null shown)
      (insert (propertize "No message passes the filters: f changes them, w clears them\n"
                          'face 'goblins-dim))))
    (dolist (thread (goblins--messages-threads shown))
      (goblins--messages-insert-thread thread route-width))
    (goblins--messages-header (length shown) (length records) route-width)
    (goto-char (point-min))
    (let ((pos (or (and message (text-property-any (point-min) (point-max)
                                                   'goblins-message message))
                   (and thread (text-property-any (point-min) (point-max)
                                                  'goblins-thread thread))
                   (progn (forward-line (1- line))
                          (when (and (eobp) (not (bobp))) (forward-line -1))
                          (point)))))
      (goblins--view-goto (save-excursion (goto-char pos) (line-beginning-position))))
    (goblins--messages-follow t)))

(defun goblins--messages-list ()
  "The message list, or an error."
  (or (get-buffer goblins--messages-buffer-name)
      (user-error "No message list; open one with M-x goblins-messages")))

(defun goblins--messages-id-at ()
  (get-text-property (line-beginning-position) 'goblins-message))

(defun goblins--messages-follow (&optional force)
  "Show the message at point in the reader, if it changed or FORCE."
  (when (derived-mode-p 'goblins-messages-mode)
    (let ((id (goblins--messages-id-at)))
      (when (or force (not (equal id goblins--messages-shown)))
        (setq goblins--messages-shown id)
        (goblins--message-render (current-buffer) id)))))

(defun goblins--messages-show-reader (list)
  "Show the reader beside LIST's window when it is wide, else below it."
  (let ((window (get-buffer-window list))
        (reader (goblins--message-buffer)))
    (when (and window (not (get-buffer-window reader)))
      (display-buffer
       reader
       `((display-buffer-in-direction)
         (window . ,window)
         (direction . ,(if (>= (window-total-width window) goblins-messages-reader-side-width)
                           'right
                         'below))
         ;; Half the list's window; a fraction would be of the frame.
         (window-width . ,(/ (window-total-width window) 2))
         (window-height . ,(/ (window-total-height window) 2)))))))

(defun goblins--messages-open (source filters &optional keep)
  "Show the message list of SOURCE's server with FILTERS, a plist, and the reader.
With KEEP, FILTERS join the filters already set; else they replace them."
  (let ((buffer (goblins--view-buffer goblins--messages-buffer-name #'goblins-messages-mode
                                      #'goblins--messages-refresh source nil)))
    (with-current-buffer buffer
      (let ((merged (if keep (copy-sequence goblins--messages-filters) nil)))
        (while filters
          (setq merged (plist-put merged (pop filters) (pop filters))))
        (setq goblins--messages-filters merged))
      (goblins--messages-render))
    (if-let* ((window (get-buffer-window buffer)))
        (select-window window)
      (goblins--display-main buffer))
    (goblins--messages-show-reader buffer)
    buffer))

(defun goblins--messages-source ()
  "The status buffer of the server at hand: the list's, else the default."
  (let ((list (get-buffer goblins--messages-buffer-name)))
    (if (and list (derived-mode-p 'goblins-messages-mode 'goblins-message-mode)
             (buffer-live-p (buffer-local-value 'goblins--view-source list)))
        (buffer-local-value 'goblins--view-source list)
      (goblins--default-source))))

(defun goblins--messages-keep-p ()
  "Whether a filter set now joins the list's filters: in the list or reader."
  (and (derived-mode-p 'goblins-messages-mode 'goblins-message-mode) t))

;;;###autoload
(defun goblins-messages (&optional directory)
  "Show the messages goblins sent each other, with a reader for the one at point.
With a prefix argument, prompt for the daemon state DIRECTORY."
  (interactive (list (when current-prefix-arg
                       (read-directory-name "Goblins state directory: "))))
  (goblins--messages-open (goblins--connect directory) nil))

;;;; The reader

(defvar-local goblins--message-list nil
  "The message list whose message the reader shows.")
(defvar-local goblins--message-id nil
  "ID of the message the reader shows.")

(defun goblins--message-buffer ()
  (let ((buffer (get-buffer-create goblins--message-buffer-name)))
    (with-current-buffer buffer
      (unless (derived-mode-p 'goblins-message-mode) (goblins-message-mode)))
    buffer))

;; The reader's text is made in the list, whose cache and names it shows.

(defun goblins--message-field (label &rest values)
  "A line of the reader: LABEL, then VALUES."
  (concat (propertize (format "%-9s" label) 'face 'goblins-key) (apply #'concat values) "\n"))

(defun goblins--message-target (text kind value)
  "TEXT that Embark sees as a target of KIND, VALUE its ID."
  (propertize text 'goblins-target (cons kind value)))

(defun goblins--message-ref (id)
  "Message ID as a reference: Embark acts on it and RET shows it."
  (goblins--message-target (propertize (goblins--messages-short id) 'face 'goblins-messages-ref)
                           'message id))

(defun goblins--message-goblin-field (label id path)
  (let ((session (goblins--snapshot-session (goblins--view-snapshot) id)))
    (goblins--message-field
     label
     (goblins--message-target (goblins--messages-goblin id path) 'goblin id)
     (if session (propertize (format " (%s)" (goblins--safe (plist-get session :name)))
                             'face 'goblins-dim)
       ""))))

(defun goblins--message-text (record width)
  "RECORD's fields and body for the reader, its rule WIDTH columns at most."
  (let* ((id (plist-get record :id))
         (conversation (plist-get record :conversation))
         (thread (cl-remove-if-not (lambda (r) (equal (plist-get r :conversation) conversation))
                                   (goblins--messages-records)))
         (reply-to (goblins--messages-get (plist-get record :reply-to)))
         (replies (cl-remove-if-not (lambda (r) (equal (plist-get r :reply-to) id)) thread))
         (claimed (plist-get record :claimed-at))
         (raid (plist-get record :raid)))
    (concat
     (goblins--message-goblin-field "From" (plist-get record :sender) (plist-get record :sender-path))
     (goblins--message-goblin-field "To" (plist-get record :recipient)
                                    (plist-get record :recipient-path))
     (goblins--message-field
      "Thread"
      (goblins--message-target (propertize (goblins--messages-short conversation)
                                           'face 'goblins-messages-ref)
                               'thread conversation)
      (propertize (format "   message %d of %d" (1+ (or (cl-position record thread) 0))
                          (length thread))
                  'face 'goblins-dim))
     (if raid (goblins--message-field "Raid" (goblins--safe (goblins--messages-raid-name raid))) "")
     (if (not reply-to) ""
       (let ((prefix (concat
                      (goblins--message-ref (plist-get reply-to :id))
                      (propertize (format "   %s  " (goblins--messages-time
                                                       (plist-get reply-to :created)))
                                  'face 'goblins-dim)
                      (goblins--messages-goblin (plist-get reply-to :sender)
                                                (plist-get reply-to :sender-path))
                      "  ")))
         ;; One line, however narrow the reader.
         (goblins--message-field
          "Re" prefix
          (truncate-string-to-width (goblins--messages-summary reply-to)
                                    (max 10 (- width 9 (string-width prefix))) nil nil "…"))))
     (goblins--message-field
      "Sent"
      (goblins--messages-time (plist-get record :created) t)
      (mapconcat
       (lambda (part) (concat (propertize "   ·   " 'face 'goblins-dim) part))
       (delq nil
             (list (and claimed
                        (propertize (format "claimed by %s %s"
                                            (goblins--safe (plist-get record :recipient-path))
                                            (goblins--messages-time claimed))
                                    'face 'goblins-dim))
                   (and replies
                        (concat (propertize "replied to by " 'face 'goblins-dim)
                                (mapconcat (lambda (r) (goblins--message-ref (plist-get r :id)))
                                           replies ", ")))
                   (pcase (plist-get record :state)
                     ("queued" (propertize "waiting in the inbox" 'face 'goblins-dim))
                     ("failed" (propertize (format "failed: %s"
                                                   (goblins--safe (or (plist-get record :failure)
                                                                      "unknown")))
                                           'face 'goblins-state-failed)))))
       ""))
     (propertize (make-string (max 20 (min 100 width)) ?─) 'face 'goblins-dim) "\n"
     (goblins--messages-text (plist-get record :body)) "\n")))

(defun goblins--message-render (list id)
  "Show message ID of LIST, the message list, in the reader."
  (let* ((reader (goblins--message-buffer))
         (window (get-buffer-window reader t))
         (width (- (if window (window-body-width window) 80) 2))
         (text (with-current-buffer list
                 (if-let* ((record (goblins--messages-get id)))
                     (goblins--message-text record width)
                   (propertize "No message at point\n" 'face 'goblins-dim)))))
    (with-current-buffer reader
      (let ((inhibit-read-only t)
            (same (and (eq list goblins--message-list) (equal id goblins--message-id)))
            (position (point)))
        (setq goblins--message-list list
              goblins--message-id id
              goblins--directory (buffer-local-value 'goblins--directory list)
              goblins--view-source (buffer-local-value 'goblins--view-source list))
        (erase-buffer)
        (insert text)
        (goto-char (if same (min position (point-max)) (point-min)))
        (dolist (window (get-buffer-window-list reader nil t))
          (set-window-point window (point))
          (unless same (set-window-start window (point-min))))))))

(defun goblins--message-target-at (&optional pos)
  "The reader's target at POS or just before it, (KIND . ID), and its
bounds (BEG . END)."
  (let ((pos (or pos (point))))
    ;; Right after a target, as after typing or searching it, still counts.
    (unless (or (get-text-property pos 'goblins-target) (<= pos (point-min)))
      (setq pos (1- pos)))
    (when-let* ((target (get-text-property pos 'goblins-target)))
      (cons target
            (cons (or (previous-single-property-change (1+ pos) 'goblins-target) (point-min))
                  (or (next-single-property-change pos 'goblins-target) (point-max)))))))

;;;; Commands

(defun goblins-messages-next (&optional n)
  "Move to the next message, or the Nth."
  (interactive "p")
  (forward-line (or n 1))
  (when (and (eobp) (not (bobp))) (forward-line -1)))

(defun goblins-messages-previous (&optional n)
  "Move to the previous message, or the Nth."
  (interactive "p")
  (forward-line (- (or n 1))))

(defun goblins-messages-fold ()
  "Fold or unfold the thread at point."
  (interactive)
  (let ((conversation (get-text-property (line-beginning-position) 'goblins-thread)))
    (unless conversation (user-error "No thread here"))
    (when (equal (plist-get goblins--messages-filters :thread) conversation)
      (user-error "The thread filter keeps this thread unfolded"))
    (setq goblins--messages-unfolded
          (if (member conversation goblins--messages-unfolded)
              (delete conversation goblins--messages-unfolded)
            (cons conversation goblins--messages-unfolded)))
    ;; Point stays on the thread, at its first message when folding.
    (unless (member conversation goblins--messages-unfolded)
      (goto-char (text-property-any (point-min) (point-max) 'goblins-thread conversation)))
    (goblins--messages-render)))

(defun goblins-messages-fold-all ()
  "Unfold every thread shown, or fold them all when none is folded."
  (interactive)
  (let ((shown (delete-dups
                (cl-loop for pos = (point-min) then (next-single-property-change pos 'goblins-thread)
                         while pos
                         for conversation = (get-text-property pos 'goblins-thread)
                         when conversation collect conversation))))
    (setq goblins--messages-unfolded
          (if (cl-some (lambda (c) (not (goblins--messages-open-p c))) shown)
              (delete-dups (append shown goblins--messages-unfolded))
            nil))
    (let ((conversation (get-text-property (line-beginning-position) 'goblins-thread)))
      (when (and conversation (not (member conversation goblins--messages-unfolded)))
        (goto-char (text-property-any (point-min) (point-max) 'goblins-thread conversation))))
    (goblins--messages-render)))

(defun goblins-messages-read ()
  "Show the reader with the message at point, and select it."
  (interactive)
  (goblins--messages-follow t)
  (goblins--messages-show-reader (current-buffer))
  (when-let* ((window (get-buffer-window goblins--message-buffer-name)))
    (select-window window)))

(defun goblins-messages-widen ()
  "Clear every filter."
  (interactive)
  (with-current-buffer (goblins--messages-list)
    (setq goblins--messages-filters nil)
    (goblins--messages-render)))

(defun goblins-messages-reload ()
  "Read the whole log again from the server."
  (interactive)
  (with-current-buffer (goblins--messages-list)
    (setq goblins--messages-cache-source nil goblins--messages-instance nil)
    (goblins--messages-render)
    (goblins--messages-fetch)))

(defun goblins-messages-quit ()
  "Close the reader and the list's window."
  (interactive)
  (dolist (window (get-buffer-window-list goblins--message-buffer-name nil t))
    (quit-window nil window))
  (quit-window))

(defun goblins-message-visit ()
  "Show the message whose reference is at point in the list."
  (interactive)
  (let ((target (car (goblins--message-target-at))))
    (unless (eq (car target) 'message) (user-error "No message reference here"))
    (goblins-message-show (goblins--messages-short (cdr target)))))

(defun goblins-messages-help ()
  "Describe the message list's keys."
  (interactive)
  (goblins--describe-keys
   "Goblins messages"
   '(("Move" ("j / k" . "next / previous message")
             ("TAB" . "fold or unfold the thread")
             ("S-TAB" . "unfold every thread, or fold them all"))
     ("Read" ("RET" . "select the reader; it shows the message at point")
             ("RET" . "in the reader, on a message ID: show that message"))
     ("Filter" ("f" . "filters: goblin, sender, recipient, raid, thread, state, time")
               ("w" . "clear every filter"))
     ("Page" ("gr" . "read the whole log again") ("q" . "close") ("?" . "this help")))))

;;;; Choosing messages, threads and goblins

(defun goblins--messages-candidates (records)
  "RECORDS as completion candidates: their short IDs."
  (mapcar (lambda (r) (propertize (goblins--messages-short (plist-get r :id))
                                  'goblins-message (plist-get r :id)))
          records))

(defun goblins--messages-annotation (records)
  "An annotation function for candidates of RECORDS."
  (lambda (candidate)
    (when-let* ((record (cl-find (get-text-property 0 'goblins-message candidate) records
                                 :key (lambda (r) (plist-get r :id)) :test #'equal)))
      (concat (propertize " " 'display '(space :align-to 12))
              (propertize (goblins--messages-time (plist-get record :created)) 'face 'goblins-dim)
              "  " (goblins--messages-route record) "  "
              (truncate-string-to-width (goblins--messages-summary record) 60 nil nil "…")))))

(defun goblins--messages-complete (prompt category candidates annotate)
  "Choose among CANDIDATES with PROMPT, in completion CATEGORY; return the choice."
  (let ((choice (completing-read
                 prompt
                 (lambda (string predicate action)
                   (if (eq action 'metadata)
                       `(metadata (category . ,category) (annotation-function . ,annotate))
                     (complete-with-action action candidates string predicate)))
                 nil t)))
    (or (car (member choice candidates)) (user-error "Choose one of the candidates"))))

(defun goblins--read-message (prompt)
  "Choose a message of the list with PROMPT; return its ID."
  (with-current-buffer (goblins--messages-list)
    (let ((records (reverse (goblins--messages-records))))
      (unless records (user-error "No messages"))
      (get-text-property 0 'goblins-message
                         (goblins--messages-complete prompt 'goblins-message
                                                     (goblins--messages-candidates records)
                                                     (goblins--messages-annotation records))))))

(defun goblins--read-thread (prompt)
  "Choose a thread of the list with PROMPT; return its conversation ID."
  (with-current-buffer (goblins--messages-list)
    (let* ((heads (mapcar #'cadr (reverse (goblins--messages-threads (goblins--messages-records)))))
           (candidates (mapcar (lambda (r)
                                 (propertize (goblins--messages-short (plist-get r :conversation))
                                             'goblins-message (plist-get r :id)))
                               heads)))
      (unless heads (user-error "No threads"))
      (plist-get (goblins--messages-get
                  (get-text-property 0 'goblins-message
                                     (goblins--messages-complete prompt 'goblins-thread candidates
                                                                 (goblins--messages-annotation heads))))
                 :conversation))))

(defun goblins--read-message-goblin (prompt)
  "Choose a goblin of the server at hand with PROMPT, stopped ones too.
Return (SOURCE SESSION-ID), for an `interactive' form."
  (let* ((source (goblins--messages-source))
         (goblin (goblins--read-goblin
                  prompt source
                  (append (plist-get (buffer-local-value 'goblins--snapshot source) :sessions)
                          nil))))
    (list (car goblin) (cdr goblin))))

(defun goblins--messages-participants ()
  "The goblins that sent or received a message in the list's cache, as an
alist of (LABEL . SESSION-ID)."
  (let (ids)
    (dolist (record (goblins--messages-records))
      (dolist (id (list (plist-get record :sender) (plist-get record :recipient)))
        (unless (member id ids) (push id ids))))
    (let ((seen (make-hash-table :test #'equal)))
      (mapcar (lambda (id)
                (let* ((label (goblins--safe (goblins--messages-goblin-path id)))
                       (count (puthash label (1+ (gethash label seen 0)) seen)))
                  (cons (if (> count 1) (format "%s <%d>" label count) label) id)))
              (nreverse ids)))))

;;;; Filtering from anywhere

(defun goblins-message-show (id)
  "Show message ID in the list, unfolding its thread, and in the reader."
  (interactive (list (goblins--read-message "Show message: ")))
  (let ((list (goblins--messages-list)))
    (with-current-buffer list
      (let* ((id (or (cl-find-if (lambda (full) (equal (goblins--messages-short full) id))
                                 goblins--messages-order)
                     id))
             (record (or (goblins--messages-get id) (user-error "No message %s" id))))
        (unless (goblins--messages-match-p record)
          (setq goblins--messages-filters nil)
          (message "Cleared the filters to show %s" (goblins--messages-short id)))
        (cl-pushnew (plist-get record :conversation) goblins--messages-unfolded :test #'equal)
        (goblins--messages-render)
        (goblins--view-goto (text-property-any (point-min) (point-max) 'goblins-message id))
        (goblins--messages-follow)))
    (if-let* ((window (get-buffer-window list)))
        (select-window window)
      (goblins--display-main list))
    (goblins--messages-show-reader list)))

(defun goblins-message-from (id)
  "List the messages from the sender of message ID."
  (interactive (list (goblins--read-message "Messages from the sender of: ")))
  (goblins--message-filter-id id :from :sender))

(defun goblins-message-to (id)
  "List the messages to the recipient of message ID."
  (interactive (list (goblins--read-message "Messages to the recipient of: ")))
  (goblins--message-filter-id id :to :recipient))

(defun goblins-message-thread (id)
  "List the thread of message ID."
  (interactive (list (goblins--read-message "Thread of message: ")))
  (goblins--message-filter-id id :thread :conversation))

(defun goblins-message-raid (id)
  "List the messages in the raid message ID was sent in."
  (interactive (list (goblins--read-message "Raid of message: ")))
  (goblins--message-filter-id id :raid :raid))

(defun goblins--message-filter-id (id key field)
  "Filter the list by KEY, set to FIELD of message ID."
  (let* ((keep (goblins--messages-keep-p))
         (list (goblins--messages-list))
         (value (with-current-buffer list
                  (plist-get (goblins--messages-get id) field))))
    (unless value
      (user-error (if (eq key :raid) "That message was not sent within a raid"
                    "No such message")))
    (goblins--messages-open (buffer-local-value 'goblins--view-source list)
                            (list key value) keep)))

(defun goblins-thread-messages (conversation)
  "List the thread CONVERSATION, unfolded."
  (interactive (list (goblins--read-thread "Thread: ")))
  (let ((keep (goblins--messages-keep-p)))
    (goblins--messages-open (buffer-local-value 'goblins--view-source (goblins--messages-list))
                            (list :thread conversation) keep)))

(defun goblins-goblin-messages (source id)
  "List the messages goblin ID of SOURCE's server sent or received."
  (interactive (goblins--read-message-goblin "Messages with goblin: "))
  (goblins--messages-open source (list :goblin id) (goblins--messages-keep-p)))

(defun goblins-goblin-messages-from (source id)
  "List the messages goblin ID of SOURCE's server sent."
  (interactive (goblins--read-message-goblin "Messages from goblin: "))
  (goblins--messages-open source (list :from id) (goblins--messages-keep-p)))

(defun goblins-goblin-messages-to (source id)
  "List the messages goblin ID of SOURCE's server received."
  (interactive (goblins--read-message-goblin "Messages to goblin: "))
  (goblins--messages-open source (list :to id) (goblins--messages-keep-p)))

(defun goblins-raid-messages (source raid)
  "List the messages sent within RAID, a raid ID, of SOURCE's server."
  (goblins--messages-open source (list :raid raid)))

;;;; Filter menu

(defun goblins--messages-filter-description (label key)
  "LABEL and the value of filter KEY, for the filter menu."
  (with-current-buffer (goblins--messages-list)
      (let ((value (plist-get goblins--messages-filters key)))
        (concat (format "%-16s" label)
                (if value
                    (propertize (pcase key
                                  ((or :goblin :from :to)
                                   (goblins--safe (goblins--messages-goblin-path value)))
                                  (:raid (goblins--safe (goblins--messages-raid-name value)))
                                  (:thread (goblins--messages-short value))
                                  ((or :since :until) (goblins--messages-time (* 1000 value) t))
                                  (:last (format "%d minutes" value))
                                  (_ "on"))
                                'face 'transient-value)
                  (propertize (pcase key
                                ((or :since :awaiting :failed :last) "off")
                                (:until "now")
                                (_ "any"))
                              'face 'goblins-dim))))))

(defun goblins--messages-set (key value)
  "Set filter KEY to VALUE, or clear it when VALUE is nil, and redraw."
  (with-current-buffer (goblins--messages-list)
    (setq goblins--messages-filters (plist-put goblins--messages-filters key value))
    (goblins--messages-render)))

(defun goblins--messages-toggle (key read)
  "Clear filter KEY when set, else set it to what READ returns."
  (with-current-buffer (goblins--messages-list)
    (goblins--messages-set key (if (plist-get goblins--messages-filters key) nil (funcall read)))))

(defun goblins--messages-read-participant (prompt)
  (let ((choices (with-current-buffer (goblins--messages-list) (goblins--messages-participants))))
    (unless choices (user-error "No messages yet"))
    (cdr (assoc (completing-read prompt choices nil t) choices))))

(defun goblins--messages-read-raid (prompt)
  (let ((choices (with-current-buffer (goblins--messages-list)
                   (let (ids)
                     (dolist (record (goblins--messages-records))
                       (when-let* ((raid (plist-get record :raid)))
                         (cl-pushnew raid ids :test #'equal)))
                     (mapcar (lambda (id) (cons (format "%s (%s)"
                                                        (goblins--safe (goblins--messages-raid-name id))
                                                        (goblins--messages-short id))
                                                id))
                             (nreverse ids))))))
    (unless choices (user-error "No message was sent within a raid"))
    (cdr (assoc (completing-read prompt choices nil t) choices))))

(defun goblins--messages-read-time (prompt)
  "Read a time with PROMPT, like 15:30 today or 2026-10-15 15:30.
Return it in Unix seconds."
  (let ((text (string-trim (read-string prompt))))
    (if (string-match "\\`\\([0-9]\\{1,2\\}\\):\\([0-9]\\{2\\}\\)\\(?::\\([0-9]\\{2\\}\\)\\)?\\'" text)
        (let ((now (decode-time)))
          (float-time (encode-time (list (string-to-number (or (match-string 3 text) "0"))
                                         (string-to-number (match-string 2 text))
                                         (string-to-number (match-string 1 text))
                                         (decoded-time-day now) (decoded-time-month now)
                                         (decoded-time-year now) nil -1 nil))))
      (condition-case nil
          (float-time (date-to-time text))
        (error (user-error "Write a time like 15:30 or 2026-10-15 15:30"))))))

(defun goblins-messages-filter-goblin ()
  "Filter by a goblin that sent or received the messages, or clear that filter."
  (interactive)
  (goblins--messages-toggle :goblin (lambda () (goblins--messages-read-participant "Goblin: "))))

(defun goblins-messages-filter-from ()
  "Filter by the goblin that sent the messages, or clear that filter."
  (interactive)
  (goblins--messages-toggle :from (lambda () (goblins--messages-read-participant "From: "))))

(defun goblins-messages-filter-to ()
  "Filter by the goblin that received the messages, or clear that filter."
  (interactive)
  (goblins--messages-toggle :to (lambda () (goblins--messages-read-participant "To: "))))

(defun goblins-messages-filter-raid ()
  "Filter by the raid the messages were sent in, or clear that filter."
  (interactive)
  (goblins--messages-toggle :raid (lambda () (goblins--messages-read-raid "Raid: "))))

(defun goblins-messages-filter-thread ()
  "Show one thread, unfolded, or clear that filter."
  (interactive)
  (goblins--messages-toggle :thread (lambda () (goblins--read-thread "Thread: "))))

(defun goblins-messages-filter-awaiting ()
  "Show only the messages still awaiting a reply, or all again."
  (interactive)
  (goblins--messages-toggle :awaiting #'always))

(defun goblins-messages-filter-failed ()
  "Show only the messages that failed, or all again."
  (interactive)
  (goblins--messages-toggle :failed #'always))

(defun goblins-messages-filter-since ()
  "Show only the messages sent since a time, or clear that filter."
  (interactive)
  (goblins--messages-toggle :since (lambda () (goblins--messages-read-time "Since: "))))

(defun goblins-messages-filter-until ()
  "Show only the messages sent until a time, or clear that filter."
  (interactive)
  (goblins--messages-toggle :until (lambda () (goblins--messages-read-time "Until: "))))

(defun goblins-messages-filter-last ()
  "Show only the messages sent in the last minutes, or clear that filter."
  (interactive)
  (goblins--messages-toggle :last (lambda () (max 1 (read-number "Last minutes: " 10)))))

(transient-define-prefix goblins-messages-filter ()
  "Filter the message list.  A key sets its filter, or clears it when set."
  [["Who"
    ("g" goblins-messages-filter-goblin :transient t
     :description (lambda () (goblins--messages-filter-description "goblin" :goblin)))
    ("F" goblins-messages-filter-from :transient t
     :description (lambda () (goblins--messages-filter-description "from" :from)))
    ("T" goblins-messages-filter-to :transient t
     :description (lambda () (goblins--messages-filter-description "to" :to)))
    ("r" goblins-messages-filter-raid :transient t
     :description (lambda () (goblins--messages-filter-description "raid" :raid)))]
   ["Conversation"
    ("t" goblins-messages-filter-thread :transient t
     :description (lambda () (goblins--messages-filter-description "thread" :thread)))
    ("a" goblins-messages-filter-awaiting :transient t
     :description (lambda () (goblins--messages-filter-description "awaiting reply" :awaiting)))
    ("x" goblins-messages-filter-failed :transient t
     :description (lambda () (goblins--messages-filter-description "failed only" :failed)))]
   ["Time"
    ("<" goblins-messages-filter-since :transient t
     :description (lambda () (goblins--messages-filter-description "since" :since)))
    (">" goblins-messages-filter-until :transient t
     :description (lambda () (goblins--messages-filter-description "until" :until)))
    ("m" goblins-messages-filter-last :transient t
     :description (lambda () (goblins--messages-filter-description "last N minutes" :last)))]]
  [("w" "widen (clear all)" goblins-messages-widen :transient t)
   ("RET" "done" transient-quit-one)])

;;;; Modes

(defvar goblins-messages-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (dolist (binding '(("j" . goblins-messages-next) ("k" . goblins-messages-previous)
                       ("<down>" . goblins-messages-next) ("<up>" . goblins-messages-previous)
                       ("TAB" . goblins-messages-fold) ("<tab>" . goblins-messages-fold)
                       ("<backtab>" . goblins-messages-fold-all)
                       ("RET" . goblins-messages-read) ("f" . goblins-messages-filter)
                       ("w" . goblins-messages-widen) ("g" . goblins-messages-reload)
                       ("q" . goblins-messages-quit) ("?" . goblins-messages-help)))
      (define-key map (kbd (car binding)) (cdr binding)))
    map))

(defvar goblins-message-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (define-key map (kbd "RET") #'goblins-message-visit)
    (define-key map (kbd "?") #'goblins-messages-help)
    map))

(with-eval-after-load 'evil
  (evil-set-initial-state 'goblins-messages-mode 'motion)
  (evil-set-initial-state 'goblins-message-mode 'motion)
  (evil-define-key* 'motion goblins-messages-mode-map
    (kbd "j") #'goblins-messages-next
    (kbd "k") #'goblins-messages-previous
    (kbd "TAB") #'goblins-messages-fold
    (kbd "<tab>") #'goblins-messages-fold
    (kbd "<backtab>") #'goblins-messages-fold-all
    (kbd "RET") #'goblins-messages-read
    (kbd "f") #'goblins-messages-filter
    (kbd "w") #'goblins-messages-widen
    (kbd "gr") #'goblins-messages-reload
    (kbd "q") #'goblins-messages-quit
    (kbd "?") #'goblins-messages-help)
  (evil-define-key* 'motion goblins-message-mode-map
    (kbd "RET") #'goblins-message-visit
    (kbd "q") #'quit-window
    (kbd "?") #'goblins-messages-help))

(define-derived-mode goblins-messages-mode special-mode "Goblins-Messages"
  "The messages goblins sent each other, a thread per line.
\\<goblins-messages-mode-map>
\\[goblins-messages-fold] unfolds a thread, \\[goblins-messages-filter] filters, \\[goblins-messages-read] reads the message at point."
  (setq-local truncate-lines t
              revert-buffer-function (lambda (&rest _) (goblins-messages-reload)))
  (hl-line-mode 1)
  (add-hook 'post-command-hook #'goblins--messages-follow nil t)
  (add-hook 'window-size-change-functions
            (lambda (_window) (goblins--messages-render)) nil t))

(define-derived-mode goblins-message-mode special-mode "Goblins-Message"
  "One message of the message list, which it follows.
\\<goblins-message-mode-map>\\[goblins-message-visit] on a message ID shows that message."
  (visual-line-mode 1))

;;;; Embark

(defun goblins--messages-embark-message (id)
  "Message ID as an Embark target type and string."
  (let ((record (with-current-buffer (goblins--messages-list) (goblins--messages-get id))))
    (list (if (plist-get record :raid) 'goblins-raid-message 'goblins-message)
          (goblins--messages-short id))))

(defun goblins--messages-embark-target ()
  "Embark's target in the message list or reader.
In the list, the message at point; in the reader, the goblin, thread or
message ID at point, else the message shown."
  (cond
   ((derived-mode-p 'goblins-messages-mode)
    (when-let* ((id (goblins--messages-id-at)))
      `(,@(goblins--messages-embark-message id) ,(line-beginning-position) . ,(line-end-position))))
   ((derived-mode-p 'goblins-message-mode)
    (if-let* ((found (goblins--message-target-at)))
        (let ((target (car found)) (bounds (cdr found)))
          (pcase (car target)
            ('goblin
             (let* ((list (goblins--messages-list))
                    (path (with-current-buffer list (goblins--messages-goblin-path (cdr target)))))
               `(goblin ,path ,(car bounds) . ,(cdr bounds))))
            ('thread `(goblins-thread ,(goblins--messages-short (cdr target))
                                      ,(car bounds) . ,(cdr bounds)))
            ('message `(,@(goblins--messages-embark-message (cdr target))
                        ,(car bounds) . ,(cdr bounds)))))
      (when goblins--message-id
        (let ((target (goblins--messages-embark-message goblins--message-id)))
          (cons (car target) (cadr target))))))))

(defvar goblins-embark-message-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'goblins-message-show)
    (define-key map "F" #'goblins-message-from)
    (define-key map "T" #'goblins-message-to)
    (define-key map "t" #'goblins-message-thread)
    map)
  "Embark actions for `goblins-message' targets: a message of the list.")

(defvar goblins-embark-raid-message-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map goblins-embark-message-map)
    (define-key map "r" #'goblins-message-raid)
    map)
  "Embark actions for `goblins-raid-message' targets: a message sent within a raid.")

(defvar goblins-embark-thread-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'goblins-thread-messages)
    (define-key map "t" #'goblins-thread-messages)
    map)
  "Embark actions for `goblins-thread' targets: a thread of the list.")

(defvar goblins-embark-goblin-messages-map
  (let ((map (make-sparse-keymap)))
    (define-key map "m" #'goblins-goblin-messages)
    (define-key map "f" #'goblins-goblin-messages-from)
    (define-key map "t" #'goblins-goblin-messages-to)
    map)
  "The prefix m among Embark's actions on a goblin: its messages.")

(with-eval-after-load 'embark
  (set-keymap-parent goblins-embark-message-map embark-general-map)
  (set-keymap-parent goblins-embark-thread-map embark-general-map)
  (add-to-list 'embark-keymap-alist '(goblins-message goblins-embark-message-map))
  (add-to-list 'embark-keymap-alist '(goblins-raid-message goblins-embark-raid-message-map))
  (add-to-list 'embark-keymap-alist '(goblins-thread goblins-embark-thread-map))
  (add-hook 'embark-target-finders #'goblins--messages-embark-target))

(provide 'goblins-messages)
;;; goblins-messages.el ends here
