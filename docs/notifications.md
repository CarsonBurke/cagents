# Notifications

The daemon owns desktop delivery and the attention journal. Windows share that
attention and report which conversation is being viewed; opening a notice uses
its exact session, review or automation identity. Opening an approval request
never approves it.

By default, desktop notifications show the provider and folder with a concise
outcome. In Settings → Notifications, **Show message previews** opts in to the
conversation name and a short response, question, approval request or error
excerpt. These details may be visible on a shared or locked screen. Disabling
previews hides excerpts in the app, withdraws rich desktop notices and attempts to
remove retained excerpts from the attention journal's current records. If storage
cleanup fails, previously stored excerpts may remain on disk until a later
successful cleanup during a settings reload or daemon startup. They remain hidden
from notification presentation while previews are disabled. The existing
event-category, enabled and while-focused switches still control delivery.
Disabling delivery or a category withdraws its current desktop notices without
marking the journal attention read.

This preference controls notification presentation and the attention journal. It
does not delete ordinary conversation history, transcript files or saved run
results.

Native conversation previews use semantic provider events, independently of the
transcript page a window is viewing. When the provider supplies no conversation
name, a bounded label from the current live message identifies the work; it is
also hidden while previews are disabled. They prefer prose over fenced code, omit
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

## Phone alerts through ntfy

Pair your phone with the computer first. In a system terminal on that computer,
choose the paired phone by its name or SSH fingerprint. Alerts start off.

```sh
cagents mobile notify set --phone "Pixel 9"
cagents mobile notify subscribe
```

Subscribe with the server and private topic shown by `subscribe` in the ntfy
app. Treat the topic like a password. Anyone who learns it can read or send
alerts on a relay with open topics. Each pairing has a separate topic on each
relay. Removing the SSH pairing stops sends to that phone. If you change
servers, run `subscribe` again to get the topic for the new server.

```sh
cagents mobile notify set --enabled true
cagents mobile notify test
cagents mobile notify status
```

The daemon sends fresh, unacknowledged agent requests and failures. Use
`cagents mobile notify set --finished true` to receive successful completion
alerts too. A burst produces one alert. If the relay is unavailable, the daemon
retries briefly and then drops the alert. The original notice stays in the
daemon's attention list until it is acknowledged or resolved. Turning alerts
off with `cagents mobile notify set --enabled false` discards pending relay
work once the daemon observes the setting. A request already sent can still
arrive.

The default relay is `https://ntfy.sh`. To choose a self-hosted ntfy server, pass
its public HTTPS origin to `cagents mobile notify set --server URL`. The daemon
uses that origin directly, with redirects and proxies disabled.

The relay receives your topic, the text “An agent needs you”, and a
`cagents://conversation/<id>` link when the notice has a conversation. The relay
can see your IP address and when you send an alert. Hosted ntfy describes
logging topic names and IP addresses, caching alerts for 12 hours by default,
and forwarding them through its mobile push infrastructure. A self-hosted
server uses its own logging, retention and forwarding configuration.
[ntfy FAQ](https://docs.ntfy.sh/faq/#will-you-know-what-topics-exist-can-you-spy-on-me)
[ntfy privacy policy](https://docs.ntfy.sh/privacy/)

Use `cagents mobile notify set --detail titles` to send the available
conversation title above the generic text. This uses titles from the
attention ledger when **Show message previews** is enabled for desktop
notifications. A title can come from a bounded label based on the first
message. Alerts use generic text when a title is unavailable. The relay receives
no separate message excerpts, request contents or workspace paths.
