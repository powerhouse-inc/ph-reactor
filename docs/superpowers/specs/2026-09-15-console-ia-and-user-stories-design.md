# Console information architecture, and the user stories rewritten — Design

The sidebar grew one item at a time, and it shows. This replaces it with a
structure derived from what a person is actually doing, and rewrites the user
stories from scratch to match.

## Problem

The sidebar puts four different *kinds* of thing at one level:

| item | what it actually is |
|---|---|
| space switcher | a **scope selector** |
| Home | node status — peers, activity |
| Spaces | managing scopes |
| Groups | the same idea as Spaces, one generation older |
| Plugins | a node-level software catalogue |
| Settings, Profile | node administration |
| *(plugin entries)* | **places inside the current space** |
| Documents, Types, Folders | orphaned — reachable by URL, in no menu |

Four defects follow:

1. **The switcher is at the top; what it scopes is at the bottom**, with three
   node-level items in between. The visual grouping contradicts the semantic
   one — what happens when a scope is added to a menu that predates scopes.
2. **"Plugins" names two things**: what you install into the node, and what
   you use in a space. So "where is Achra?" has two plausible answers.
3. **Groups and Spaces are one idea at two ages** — a migration artifact
   showing through the UI.
4. **Home has no scope.** It shows node-level facts underneath a switcher
   implying everything below is scoped.

## Who this is for

**One person wearing two hats.** Today the operator of the node and the person
doing the work are the same human. The design must not hide node
administration, but must make it unmistakable which hat is on — and must not
need rewriting the day those are different people (a hosted node, or a
teammate on someone else's reactor).

That yields one hard rule: **node administration is always exactly one click
away, never nested inside a space, and never a sibling of a space's apps.**

## Principles

1. **Scope is visible before content.** You should never have to guess which
   space you are reading or writing in.
2. **Exposure is stated where the risk is taken.** The tier appears next to the
   composer, not only in a settings page. A person about to write needs to know
   who will be able to read it.
3. **One idea, one name, one place.** Nothing appears twice under two words.
4. **Attention is a first-class screen.** "What needs me" is not something to
   assemble by visiting five spaces.
5. **Quiet where it is not the point.** The node region is lower-contrast and
   smaller, so the eye goes to the work.

## The three regions

```
◈  Inbox  (3)                  ← cross-space: what needs you
─────────────────────────────
⬢  Powerhouse          [▾]     ← the lens: switch, or clear it
     Achra
     Contributor billing
     Chat
     Drive
     Members & apps            ← this space's own settings
─────────────────────────────
⚙  This node              [›]  ← collapsed: identity, peers, software, updates
```

The switcher sits directly above the things it scopes. The Inbox sits above
the lens because it is deliberately *not* scoped by it.

## The Inbox, and where its rows come from

An app declares **attention rules** in its manifest, exactly as it declares
capabilities and projections:

```json
"attention": [
  { "model": "proposal",
    "when":  { "status": "submitted" },
    "needs": "approvers",
    "label": "Proposal awaiting your review" }
]
```

The daemon evaluates these across every space the node is a member of and
serves `/api/inbox`. Each row carries its space, so cross-space context is
never ambiguous.

Declared rather than computed, for three reasons:

- **The app owns the meaning.** Only contributor billing knows that
  `status: submitted` puts the approvers on the hook. A generic rule over
  document state would be inventing meaning the app owns.
- **It is consentable.** "This app can put things in your inbox" belongs in the
  install prompt beside "this app will publish payment records publicly".
  Something that can interrupt you is a permission.
- **It cannot lie or stall.** Asking each plugin's iframe for its own inbox
  would mean booting every installed app to draw one screen, and an app that
  hangs would hang the landing page.

Rules are signed with the rest of the manifest, so `attention` is skipped when
empty — the same load-bearing `skip_serializing_if` as `ui` and `projections`.

## What is removed, split, or moved

- **"Plugins" splits and the word disappears.** Node-level becomes **Software**
  (install, trust a publisher, update) under This node. Space-level becomes the
  apps listed under **Members & apps** in the space.
- **Groups leaves the sidebar.** A remaining group appears as a migration
  prompt inside the space list, not as a permanent menu item.
- **Documents / Types / Folders** stop being orphaned URLs and become
  **Store** under This node. Inspecting the raw store is an operator activity.
- **Home is dissolved.** It was two things under one name: "what needs me"
  (→ Inbox) and "is this node healthy" (→ This node). That conflation is why it
  never felt like a home page.

Old hashes redirect rather than 404: `#overview` → `#inbox`, `#groups` →
`#spaces`, `#plugins` → `#node/software`, `#documents|#types|#folders` →
`#node/store`. `#plugin/<name>/<view>` is unchanged.

## User stories

Rewritten from scratch, organised by hat. **Shipped** means the capability
exists after the spaces work; **new** means this design adds it.

### Doing the work

**W1 — See what needs me, without visiting every space.** *(new)*
As a contributor, I open the console and immediately see the things waiting on
me, gathered from every space I am in.
*Accepts:* the landing screen lists items from at least two different spaces,
each labelled with its space; with nothing waiting it says so in words rather
than showing an empty list or a spinner.

**W2 — Act without first working out where something lives.** *(new)*
As a reviewer, I act on an inbox item without navigating to its space first.
*Accepts:* selecting an inbox row opens the owning app already scoped to the
right space, and the lens updates to match so I know where I now am.

**W3 — Know who can read what I am about to write.** *(partly shipped)*
As someone writing in a space, I can see the audience at the moment I write.
*Accepts:* the tier and its plain-language meaning are visible next to the
composer, not only on a settings page; a protected space says that every member
holds a full unencrypted copy and keeps it after removal.

**W4 — Move between spaces without losing my place.** *(new)*
As someone working across a client space and an internal one, switching is fast
and reversible.
*Accepts:* the switcher is reachable by keyboard, filters as I type, shows each
space's tier, and returns me to the same app in the new space when that app is
enabled there.

**W5 — Find a page by name rather than hunting the sidebar.** *(new)*
*Accepts:* one keyboard shortcut opens a command palette listing spaces, apps
in the current space, and node sections; typing narrows it; Enter navigates.

**W6 — Understand what happened to a document, and who did it.** *(shipped)*
*Accepts:* every document view can show a trail of actions with author and
time; the time is a real, readable date.

**W7 — Publish something deliberately from a private space to a public one.** *(shipped)*
*Accepts:* the projection is declared by the app and disclosed at install; the
published record carries only the declared fields.

**W8 — Join a space and know what I am joining.** *(new)*
*Accepts:* before accepting, I see the space's tier, its meaning, its member
count, and which apps run in it.

### Running the node

**O1 — Know at a glance whether this node is healthy and syncing.** *(shipped, moved)*
*Accepts:* one place shows peer count, drive status per peer, document count
and version; a drive that is not synced says why.

**O2 — Decide what software runs here, and know what it may do.** *(shipped, renamed)*
*Accepts:* installing shows, in plain language, what the app may read, what it
may write, what it will publish publicly, and what it may put in my inbox;
publisher trust is a separate, revocable decision.

**O3 — Upgrade deliberately.** *(shipped)*
*Accepts:* an available release is shown with its version and publisher; the
node applies it only on an explicit action unless auto-update is on; in a
container the status reports the running binary, not the image tag.

**O4 — See who my node talks to, and cut one off.** *(shipped)*
*Accepts:* peers are listed with their id and state; banning one takes effect
without a restart.

**O5 — Inspect the store when something looks wrong.** *(shipped, moved)*
*Accepts:* raw documents, registered model types and their definitions are
reachable in one place under This node, and a document's log can be verified.

**O6 — Know what is exposed, and to whom.** *(new)*
*Accepts:* one view lists each space with its tier, its member count and how
many documents it holds, so "what is public from this node" is answerable
without reading code.

## Craft that is part of the design, not decoration

These exist because they change whether the thing is usable, not because they
look nice:

- **Every inbox row names its space.** Cross-space lists are ambiguous without
  it, and ambiguity about scope is the failure mode this whole design is for.
- **Tier colour follows the space everywhere** — switcher, chips, composer — so
  exposure is recognisable rather than re-read.
- **Empty states say what is true and what to do next**, never a bare void.
- **Counts are real or absent.** A badge that is sometimes stale is worse than
  no badge.
- **Focus is always visible**, and the palette, switcher and inbox are
  keyboard-operable. A coordination tool people use daily earns that.
- **The node region is lower contrast and smaller.** Hierarchy by weight, not
  by hiding.

## Non-goals

- Theming or a light mode.
- Mobile layout. The console is loopback on a workstation.
- Notifications outside the console (email, desktop, push).
- Per-user accounts. The console is still one node, one operator; the two-hat
  split is about clarity now and about not needing a rewrite later.

## Open questions

- Should the Inbox include items needing *any* member of a space, or only
  those naming me? (Starting with: naming me, because a shared inbox that never
  empties stops being read.)
- Does clearing the lens ("All spaces") make sense for app pages, or only for
  the Inbox?
