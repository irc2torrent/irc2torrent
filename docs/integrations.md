# Setting up the integrations

Step-by-step account setup for each way irc2torrent can talk to you. The
[README](../README.md) covers what each option *does*; this covers getting the
credentials in the first place.

Any number of these can be active at once. Three of them — Telegram, Slack and
IRC — can also carry **commands**; email and ntfy are one-way.

| Integration | Notifications | Commands | Account needs a phone number? |
|---|---|---|---|
| [Telegram](#telegram) | yes | yes | the bot, no — you, yes |
| [Slack](#slack) | yes | yes | no |
| [IRC](#irc) | yes | yes | no |
| [ntfy](#ntfy) | yes | no | no account at all |
| [Email](#email) | yes | no | no |

---

## Telegram

The easiest of the four, and the best for commands: 4096 characters per message,
so a whole torrent listing arrives in one piece, and the sender is identified by
a numeric ID that cannot be spoofed.

The **bot** has no phone number and no account of its own. You need an existing
Telegram account to create it, and yours is phone-verified — if you want no phone
anywhere in the chain, use IRC.

### 1. Create the bot

In Telegram, message [@BotFather](https://t.me/BotFather):

```
/newbot
```

It asks for a display name, then a username ending in `bot`. It replies with a
token:

```
123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw
```

**That token is a full credential** — anyone holding it controls the bot
completely. Treat it like a password: keep `options.toml` readable only by the
user the bot runs as, and never paste it into an issue or a log.

### 2. Find your user ID

The bot obeys exactly one person, identified by a number rather than a username
(usernames can be changed; the ID cannot). Message
[@userinfobot](https://t.me/userinfobot) and it replies with yours:

```
Id: 987654321
```

### 3. Say hello to your bot first

Open a chat with the bot you just created and send it anything — `/start` will
do.

**Do not skip this.** Telegram does not let a bot open a conversation with you;
you have to speak first. Until you do, every notification fails with
`sendMessage rejected: chat not found`, which is exactly what you will see in the
log if you forget.

### 4. Configure

```toml
[telegram]
token    = "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw"
owner_id = 987654321
```

Both roles are on by default. Turn either off on its own:

```toml
commands      = false   # notifications only
notifications = false   # commands only
```

And filter which events reach Telegram, exactly as for any other target:

```toml
[telegram.events]
on_torrent_added = false
```

### 5. Check it

Restart the bot. The log should say:

```
Telegram commands enabled as @your_bot.
Telegram notifications enabled.
```

Then message the bot `h!`. You should get the whole command list in one message.
`tn!` sends a test notification.

If sending fails with `chat not found`, go back to step 3 — that is the only
error whose cause is not in its own message, so the bot appends the fix to it.

`Telegram poll failed: … error sending request` at startup is the **network**,
not your token: a container often starts its bot before Docker has finished
wiring it up. It retries with a widening backoff and logs
`Telegram is reachable again` once it gets through; the token is confirmed then,
not before.

If you ever see `terminated by other getUpdates request`, two things are polling
this bot — usually an old container still running. Only one may, and they steal
each other's updates, so commands appear to work intermittently.

---

## Slack

Ten minutes of clicking — or about two, with the manifest in step 1 — and the
only one of the five that needs no phone number at any point, not for the bot
and not for you. It is a good choice if you already keep a Slack workspace open
all day.

**Two ways to run it**, and the default is the private one:

| | Where it talks | Config |
|---|---|---|
| **Direct message** (default) | a DM between you and the bot | leave `channel_id` out |
| **Channel** | one channel of your workspace | set `channel_id` |

A channel is the obvious setup and usually the wrong one: everyone in it sees
the replies, and `lt!` lists your whole library to the room. Only `owner_id` can
*issue* commands either way — the difference is who can read the answers. Use a
channel when you want a shared record of what the bot is doing.

The steps below cover both; the places they differ say so.

> **The short version:** step 1 has a manifest you can paste that does steps 2,
> 4, 5 and 6 for you. Then create the app-level token (step 3), install (step
> 7), and copy two tokens and your member ID into `options.toml`.

### 1. Create the app

Go to [api.slack.com/apps](https://api.slack.com/apps) → **Create New App**.
Slack keeps app settings under `app.slack.com/app-settings/<workspace-id>/<app-id>/…`
these days; that link redirects there, so do not be surprised when the address
bar changes.

(If you have no workspace, [create one](https://slack.com/get-started#/createnew)
first — a free one is fine, and an email address is all it asks for.)

**Option A — from an app manifest.** The faster path by a wide margin. Choose
**From an app manifest**, pick your workspace, select **YAML**, and paste one of
these. It sets the scopes, the event subscription, Socket Mode and the App Home
tab in one go, so you can skip to step 3.

Direct message — the default, private setup:

```yaml
display_information:
  name: irc2torrent
features:
  bot_user:
    display_name: irc2torrent
    always_online: false
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
oauth_config:
  scopes:
    bot:
      - chat:write
      - im:write
      - im:history
settings:
  event_subscriptions:
    bot_events:
      - message.im
  socket_mode_enabled: true
  token_rotation_enabled: false
```

Channel — for a *private* channel, swap `channels:history` for `groups:history`
and `message.channels` for `message.groups`:

```yaml
display_information:
  name: irc2torrent
features:
  bot_user:
    display_name: irc2torrent
    always_online: false
oauth_config:
  scopes:
    bot:
      - chat:write
      - channels:history
settings:
  event_subscriptions:
    bot_events:
      - message.channels
  socket_mode_enabled: true
  token_rotation_enabled: false
```

A manifest cannot create tokens, so step 3 is still yours to do by hand. You can
also paste either one over an app that already exists, at **Features → App
Manifest**.

**Option B — from scratch.** Name it, pick your workspace, create, then work
through the steps below in order.

### 2. Turn on Socket Mode

*Already on if you used the manifest.*

**Settings → Socket Mode** → toggle **Enable Socket Mode** on.

Socket Mode is why this needs no public URL: the bot dials out, and Slack pushes
events down the connection it opened. Without it you would need an HTTPS endpoint
Slack could reach.

Toggling it here offers you an app-level token immediately, which is the next
step.

### 3. Create the app-level token

Everybody does this one — a manifest cannot mint tokens.

**Settings → Basic Information → App-Level Tokens → Generate Token and Scopes**
(or take the prompt step 2 offered; same token either way). Name it anything, add
the scope **`connections:write`**, and generate. You get:

```
xapp-1-A01234567-1234567890123-abcdef…
```

That is `app_token`, and it is **shown once** — copy it now. Lose it and nothing
is broken: delete that token and generate another.

`connections:write` is an *app-level* scope. Do not go hunting for it among the
bot scopes in the next step — it is not there, and that is not a mistake.

### 4. Grant the bot permission to post

*Done for you if you used the manifest.*

**Features → OAuth & Permissions → Scopes → Bot Token Scopes** → **Add an OAuth
Scope**. Which ones depends on where it talks:

| Scope | Needed for |
|---|---|
| `chat:write` | always |
| `im:write` | direct messages — opening the conversation with you |
| `im:history` | direct messages — reading your commands |
| `channels:history` | a public channel |
| `groups:history` | a private channel |

**Put them under Bot Token Scopes, not User Token Scopes.** The two lists sit one
above the other on that page, look identical, and both offer an **Add an OAuth
Scope** button — and this is the easiest mistake here. Scopes added to the second
one produce a `xoxp-` user token that irc2torrent has no use for, while the bot
token goes on failing with `missing_scope`. **No user scopes are needed at all.**

The rest of that page — token rotation, PKCE, redirect URLs, IP ranges — is for
other kinds of app. [What the rest of that page is for](#what-the-rest-of-the-oauth--permissions-page-is-for)
says what each card does and why you are right to leave it alone.

### 5. Subscribe to messages

*Done for you if you used the manifest.*

**Features → Event Subscriptions** → toggle **Enable Events** on. There is no URL
to enter — Socket Mode covers that. Under **Subscribe to bot events**, add the
one that matches:

| Event | For |
|---|---|
| `message.im` | direct messages (the default setup) |
| `message.channels` | a public channel |
| `message.groups` | a private channel |

Save.

This is one of two steps that fail silently: with the wrong event subscribed,
Slack simply never tells the bot anything, and you get no error anywhere.

### 6. Direct message only: open the bot's message tab

*Skip for a channel. Done for you if you used the manifest.*

**Features → App Home → Show Tabs** → turn on the **Messages Tab**, then tick
**Allow users to send Slash commands and messages from the messages tab**.

This is the other silent one, and it only bites the default setup. Left off, the
DM with the bot opens read-only — Slack greys the message box out and says
sending messages to this app has been turned off — so notifications arrive fine
and commands are impossible to send. No log line anywhere mentions it, because
Slack never delivers anything for the bot to fail on.

### 7. Install it

**Settings → Install App → Install to Workspace** → Allow. The same button sits
at the top of **OAuth & Permissions**, where it is labelled **Install to** and
then your workspace's name rather than the word "Workspace".

Greyed out, under *"Please add at least one feature or permission scope below to
install your app"*? That is step 4 — Slack will not install an app that asks for
no permissions. Add the bot scopes and the button comes alive.

Installing produces the **Bot User OAuth Token**:

```
xoxb-1234567890-1234567890123-abcdef…
```

That is `bot_token`. **The two tokens are not interchangeable**: `xapp-` opens
the connection and nothing else, `xoxb-` posts and nothing else. Swapping them
gets you `not_allowed_token_type`, which names neither.

Change scopes or events later and Slack puts a **reinstall** banner at the top of
the page — do it, or the change is not actually granted.

### 8. Channel only: invite the bot

Skip this for a DM. In the channel you want to use:

```
/invite @your-app-name
```

**Do not skip it if you are using a channel.** A bot that is not a member gets
`not_in_channel` on every post, and it is the most common reason nothing
arrives.

### 9. Find your user ID (and the channel ID, if any)

Neither is the name you see in the UI, and both are case-sensitive.

- **Your user ID** — click your avatar → **Profile** → the **⋮** menu → **Copy
  member ID**. It looks like `U01234567`.
- **Channel ID** — only for the channel setup: click the channel name → scroll to
  the bottom of the **About** tab. It looks like `C01234567`.

### 10. Configure

The private setup — the bot messages you directly:

```toml
[slack]
app_token = "xapp-1-A01234567-1234567890123-abcdef"
bot_token = "xoxb-1234567890-1234567890123-abcdef"
owner_id  = "U01234567"
```

Or add one line to use a channel instead:

```toml
channel_id = "C01234567"
```

Both roles are on by default, and turn off the same way as Telegram's:

```toml
commands      = false   # notifications only
notifications = false   # commands only

[slack.events]
on_torrent_added = false
```

### 11. Check it

Restart the bot. The log should say:

```
Slack will message U01234567 directly.      (or: posting to channel C01234567)
Slack commands connected.
Slack notifications enabled.
```

Send the bot `h!` — in its DM, or in the channel — and the whole command list
should come back in one message. `tn!` sends a test notification.

Common errors, all reported with the fix attached:

| Message | What to do |
|---|---|
| `not_in_channel` | step 8 — `/invite` the bot |
| `channel_not_found` | wrong `channel_id`, or the bot cannot see a private channel |
| `user_not_found` | `owner_id` is not a member ID — step 9 |
| `cannot_dm_bot` | `owner_id` is another app; it has to be a person |
| `missing_scope` | step 4 — and check they went under *Bot* Token Scopes — then **reinstall** |
| `not_allowed_token_type` | the two tokens are swapped |
| `invalid_auth` | token mistyped, or revoked |

Nothing happens when you post, and no error anywhere at all? Two steps fail in
exactly that way, and neither says a word:

- **Step 5**, the wrong event subscribed — Slack never delivers your message.
  `message.im` for a DM, `message.channels` or `message.groups` for a channel.
- **Step 6**, the message tab still closed — a DM-only trap. If Slack will not
  let you type into the conversation at all, it is this one.

### What the rest of the OAuth & Permissions page is for

That page has grown a good deal, and almost none of it concerns a bot like this.
Everything below is safe — and correct — to leave exactly as it comes:

| Card | Leave it |
|---|---|
| **Advanced token security via token rotation** | off. It makes the bot token expire on a schedule; irc2torrent holds one static token and has no refresh flow. Its "at least one redirect URL needs to be set" warning is Slack saying the same thing another way. |
| **Proof Key for Code Exchange (PKCE)** | off. For apps that hand a browser back to themselves after a login. This one never sees a browser. |
| **Redirect URLs** | empty. Only needed to distribute the app to other workspaces or to render an *Add to Slack* button. |
| **Restrict API Token Usage** | empty — unless the machine has a fixed public address and you want the extra lock. A dynamic IP here breaks the bot silently the next time it changes. |
| **User Token Scopes** | empty. See step 4: bot scopes only. |
| **Revoke All OAuth Tokens** | alone — but this is the button if `bot_token` ever leaks. Revoke, reinstall, put the new `xoxb-` in `options.toml`. |

The sidebar is long now too — Agents, MCP Servers, Work Object Previews, Workflow
Steps, Org Level Apps, Incoming Webhooks, Interactivity & Shortcuts, Slash
Commands, User ID Translation, Manage Distribution, Submit to Slack Marketplace.
None of them are used. The six pages that matter are **Basic Information**,
**Socket Mode**, **OAuth & Permissions**, **Event Subscriptions**, **App Home**
and **Install App** — plus **App Manifest**, if you took the shortcut in step 1.

### Why not Discord

Discord's Gateway would have served exactly the same purpose. It is blocked in
the country this is deployed from, so it could be neither used nor tested, and an
integration nobody involved can exercise is worse than none.

---

## IRC

Already configured if the bot is announcing — commands and notifications reuse
the same connection. See **Commands over IRC** and **Notifications → IRC private
message** in the [README](../README.md).

Worth knowing about the differences, because IRC is the constrained one:

- A message is capped at 512 bytes *including* protocol overhead, so listings are
  sent one line per message and bounded by `max_reply_lines`.
- Servers kill clients that send too fast, so replies are paced
  (`max_messages_in_burst` / `burst_window_length` in `irc.toml`).
- A nickname is not a credential. Leave `require_identified = true` so the bot
  checks with network services before obeying anyone.

None of those apply to Telegram or Slack, which is why either is a better place
to run commands from if you have the choice.

---

## ntfy

No account, no API key. Pick a topic name, subscribe to it, and you are done.

1. Install the [ntfy app](https://ntfy.sh/#subscribe-phone), or just open
   `https://ntfy.sh/<your-topic>` in a browser.
2. Subscribe to a topic. **Choose something unguessable** — on the public server
   the topic name *is* the access control, so `irc2torrent` is a bad topic and
   `irc2torrent-8f3a9c1d4b7e` is a reasonable one.
3. Configure:

```toml
[notifications.ntfy]
topic = "irc2torrent-8f3a9c1d4b7e"
```

A full URL points at a [self-hosted server](https://docs.ntfy.sh/install/)
instead, with an optional bearer `token`.

**Notifications only.** ntfy can technically be subscribed to as well as posted
to, but a topic says nothing about *who* sent a message — anyone who learned it
could command the bot. It is a good sink and a bad control channel.

---

## Email

Usually two lines, because the SMTP host is looked up from your address:

```toml
[notifications.email]
address  = "you@gmail.com"
password = "abcd efgh ijkl mnop"
```

**Most providers reject your normal account password** and need an app-specific
one:

| Provider | Where |
|---|---|
| Gmail | [App passwords](https://myaccount.google.com/apppasswords) — requires 2-Step Verification first |
| Yahoo | [Generate an app password](https://help.yahoo.com/kb/SLN15241.html) |
| iCloud | [App-specific passwords](https://support.apple.com/en-us/102654) |
| Outlook | [App passwords](https://support.microsoft.com/en-us/account-billing/5896ed9b-4263-e681-128a-a6f2979a7944) — only with 2FA on |
| Fastmail | [App passwords](https://www.fastmail.help/hc/en-us/articles/360058752854) |
| Proton | Needs [Proton Bridge](https://proton.me/mail/bridge) running locally; point `host` at it |

Known hosts are inferred for Gmail, Outlook/Hotmail/Live, Yahoo, Fastmail,
iCloud, GMX, Yandex, Zoho and Proton Bridge. Anything else needs an explicit
`host`.

To keep the password out of the config file, use
`password_file = "/run/secrets/smtp_password"` or the
`IRC2TORRENT_SMTP_PASSWORD` environment variable. First match wins: `password`,
then `password_file`, then the variable.

---

## Checking any of them

`tn!` (or `cmd:testnotify`) sends a test notification to **every** configured
target, ignoring all the event switches. Notification setup fails silently
otherwise — a wrong SMTP password produces no error anywhere you would look — so
run it once after changing anything.
