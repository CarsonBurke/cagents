# Notifications

The daemon owns desktop delivery and the attention journal. Windows share that
attention and report which conversation is being viewed; opening a notice uses
its exact session, review or automation identity. Opening an approval request
never approves it.

By default, desktop notifications show the provider and folder with a concise
outcome. In Settings → Notifications, **Show message previews** opts in to the
conversation name and a short response, question, approval request or error
excerpt. These details may be visible on a shared or locked screen. Disabling
previews removes retained excerpts from the attention journal's current records and
withdraws rich desktop notices. The existing event-category, enabled and
while-focused switches still control delivery. Disabling delivery or a category
withdraws its current desktop notices without marking the journal attention read.

This preference controls notification presentation and the attention journal. It
does not delete ordinary conversation history or private, bounded semantic replay
history retained for a native session while its daemon is unavailable. Replay
records retire after the daemon durably acknowledges them.

Native conversation previews use semantic provider events, independently of the
transcript page a window is viewing. They prefer prose over fenced code, omit
reasoning and raw tool output, remove control sequences and bound the excerpt to
240 characters. A code-only response keeps a short code excerpt. Request previews
never include secret field values. Observed CLI sessions can supply a conversation
name; cagents does not scrape their terminal screens for notification text.

Updates from one conversation coalesce briefly before desktop delivery; continuous
updates wait at most two seconds. Unrelated conversations have independent
delivery schedules. Desktop replacement uses a stable conversation identity;
acknowledgement still uses the exact current journal notice. A stale acknowledgement
cannot clear a later request. Replaced or acknowledged notices are withdrawn, and existing
unread attention remains available in the application if desktop delivery fails.
Channel and pane attention tooltips reuse the same bounded context when previews
are enabled.

Linux delivery uses the XDG notification portal. It supplies a title, plain body,
Open action and an input-attention priority; the desktop notification server
controls toast layout, truncation, sound and lock-screen presentation. Raising a
Wayland window depends on the portal forwarding an activation token and the
compositor accepting it. A backend that omits the token may route the conversation
without raising its window. These platform behaviors require native validation
with the actual backend; a successful portal call alone does not prove that a
toast was visible or a window was focused.

Portal reference: [XDG Desktop Portal notifications](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Notification.html).
