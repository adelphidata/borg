/** Placeholder data until borgd (gRPC daemon) integration lands. */

export interface ConnectionActivity {
  when: string;
  what: string;
}

export interface Connection {
  id: string;
  badge: string;
  name: string;
  sub: string;
  /** Red dot + banner treatment instead of the neon dot. */
  needsAttention?: boolean;
  /** One-line pitch shown before connecting. */
  blurb?: string;
  /** What Borg can do once this is connected. */
  can?: string[];
  activity?: ConnectionActivity[];
}

export interface HealthCheck {
  id: string;
  name: string;
  value: string;
  ok: boolean;
  /** Plain-language explanation revealed when the row is expanded. */
  detail: string;
  /** Optional fix-it button label for failing checks. */
  action?: string;
}

export interface SettingOption {
  value: string;
  desc?: string;
}

export interface Setting {
  id: string;
  name: string;
  desc: string;
  value: string;
  options?: SettingOption[];
}

export interface AdvancedGroup {
  name: string;
  rows: { key: string; desc: string; value: string }[];
}

export interface OnboardingStep {
  title: string;
  body: string;
  cta: string;
  alt: string;
}

export const user = { name: "Rob Keane", initials: "RK" };

export const recents = ["Release check", "Rewrite the onboarding email", "Weekly board summary"];

export const connected: Connection[] = [
  {
    id: "imessage",
    badge: "iM",
    name: "iMessage",
    sub: "automatic",
    can: [
      "Send and reply when you ask",
      "Watch for messages that need you",
      "Draft responses for your approval",
    ],
    activity: [
      { when: "8:41 this morning", what: "Flagged a message from Dana about the offsite" },
      { when: "Yesterday", what: "Sent your reply to the group thread" },
      { when: "Tuesday", what: "Nothing needed you" },
    ],
  },
  {
    id: "telegram",
    badge: "TG",
    name: "Telegram",
    sub: "@borg_rk",
    can: [
      "Relay conversations when you're away from this Mac",
      "Take requests and report back here",
      "Forward anything urgent",
    ],
    activity: [
      { when: "9:02 this morning", what: "You asked about the release from your phone" },
      { when: "Yesterday", what: "Relayed the board summary" },
    ],
  },
  {
    id: "linear",
    badge: "LN",
    name: "Linear",
    sub: "3 teams",
    can: [
      "Read issues and milestones across your teams",
      "File and update issues when you ask",
      "Watch the milestones you care about",
    ],
    activity: [
      { when: "9:14 this morning", what: "Checked the 0.4 milestone for blockers" },
      { when: "Yesterday", what: "Filed the certificate issue for Rob" },
    ],
  },
  {
    id: "calendar",
    badge: "CA",
    name: "Calendar",
    sub: "rk@adelphi.dev",
    can: [
      "See what's ahead and warn you about conflicts",
      "Add and move events when you ask",
      "Keep quiet hours around your meetings",
    ],
    activity: [
      { when: "7:00 this morning", what: "Looked over today — nothing conflicts" },
      { when: "Monday", what: "Moved the design review for you" },
    ],
  },
  {
    id: "gmail",
    badge: "GM",
    name: "Gmail",
    sub: "needs sign-in",
    needsAttention: true,
    can: [
      "Read and triage new mail",
      "Draft and send replies for your approval",
      "Queue anything that arrives while it's disconnected",
    ],
    activity: [
      { when: "Yesterday, 4:12 pm", what: "The saved sign-in expired" },
      { when: "Since then", what: "Three messages queued, nothing lost" },
    ],
  },
];

export const available: Connection[] = [
  {
    id: "slack",
    badge: "SL",
    name: "Slack",
    sub: "",
    blurb: "Borg joins the channels you choose and speaks only when spoken to.",
  },
  {
    id: "discord",
    badge: "DC",
    name: "Discord",
    sub: "",
    blurb: "A presence in your server that can fetch, summarise and post.",
  },
  {
    id: "teams",
    badge: "MT",
    name: "Teams",
    sub: "",
    blurb: "Work chat without leaving the conversation here.",
  },
  {
    id: "google-chat",
    badge: "GC",
    name: "Google Chat",
    sub: "",
    blurb: "Spaces and DMs, watched quietly.",
  },
  {
    id: "whatsapp",
    badge: "TW",
    name: "WhatsApp",
    sub: "",
    blurb: "Reach Borg from the app you already use.",
  },
  {
    id: "outlook",
    badge: "OL",
    name: "Outlook",
    sub: "",
    blurb: "Mail and calendar in one connection.",
  },
  {
    id: "notion",
    badge: "NO",
    name: "Notion",
    sub: "",
    blurb: "Read and write the pages you point it at.",
  },
];

export function findConnection(id: string): Connection | undefined {
  return connected.find((c) => c.id === id) ?? available.find((c) => c.id === id);
}

export const checks: HealthCheck[] = [
  {
    id: "gmail",
    name: "Gmail sign-in",
    value: "Expired yesterday",
    ok: false,
    detail:
      "The saved sign-in expired yesterday at 4:12pm. Nothing was lost — three messages are queued and will send once you sign in again.",
    action: "Sign in again",
  },
  {
    id: "privacy",
    name: "Your data is private",
    value: "Locked to this Mac",
    ok: true,
    detail:
      "Everything Borg knows lives in a folder on this Mac. Nothing leaves unless you send it.",
  },
  {
    id: "secrets",
    name: "Secrets are protected",
    value: "Keychain",
    ok: true,
    detail: "API keys and tokens are stored in the macOS Keychain, never in plain files.",
  },
  {
    id: "daemon",
    name: "Borg is running",
    value: "11 days without a restart",
    ok: true,
    detail: "The daemon has been up for 11 days. Memory is steady at 84 MB.",
  },
  {
    id: "storage",
    name: "Storage",
    value: "1.2 GB · tidied at 2am",
    ok: true,
    detail:
      "Conversations and logs take 1.2 GB. The nightly tidy-up ran at 2am and reclaimed 240 MB.",
  },
  {
    id: "version",
    name: "Version",
    value: "Up to date",
    ok: true,
    detail: "Borg 0.4.0. You're on the latest build — updates install themselves overnight.",
  },
];

export const basicSettings: Setting[] = [
  {
    id: "model",
    name: "Model",
    desc: "Which brain Borg thinks with",
    value: "Sonnet 4.6",
    options: [
      { value: "Sonnet 4.6", desc: "Quick and sharp. The default." },
      { value: "Opus 4.5", desc: "Slower, deeper. For the hard stuff." },
      { value: "Haiku 4.5", desc: "Instant answers, lighter thinking." },
      { value: "Local", desc: "A model running on this Mac. Nothing leaves." },
    ],
  },
  {
    id: "notifications",
    name: "Notifications",
    desc: "How Borg reaches you when it finds something",
    value: "On",
    options: [
      { value: "On", desc: "Anything worth knowing, as it happens." },
      { value: "Important only", desc: "Only things that need a decision from you." },
      { value: "Off", desc: "Borg stays quiet. Check in when you like." },
    ],
  },
  {
    id: "check-in",
    name: "Check in on me",
    desc: "Borg starts the conversation when it has news",
    value: "Mornings",
    options: [
      { value: "Mornings", desc: "One quiet summary with your coffee." },
      { value: "Morning and evening", desc: "Bookends on the day." },
      { value: "Only when urgent", desc: "Silence unless something can't wait." },
      { value: "Never", desc: "You'll always start the conversation." },
    ],
  },
  { id: "appearance", name: "Appearance", desc: "", value: "" }, // value derived from theme at render time
  {
    id: "start-login",
    name: "Start when I log in",
    desc: "",
    value: "On",
    options: [
      { value: "On", desc: "Borg is already running when you sit down." },
      { value: "Off", desc: "Start it yourself when you want it." },
    ],
  },
];

export function findSetting(id: string): Setting | undefined {
  return basicSettings.find((s) => s.id === id);
}

export const advancedGroups: AdvancedGroup[] = [
  {
    name: "Safety",
    rows: [
      { key: "Sandbox", desc: "What Borg is allowed to touch", value: "Strict" },
      { key: "Protected files", desc: "Keys, credentials and env files", value: "12 rules" },
      { key: "Where secrets live", desc: "", value: "Keychain" },
    ],
  },
  {
    name: "Access",
    rows: [
      { key: "Other devices", desc: "Pair a phone or a second Mac", value: "Off" },
      { key: "Encrypted connections only", desc: "", value: "Required" },
    ],
  },
  {
    name: "Behaviour",
    rows: [
      { key: "Retry limit", desc: "Stop Borg repeating itself", value: "3" },
      { key: "Quiet hours", desc: "", value: "10pm – 7am" },
      { key: "Nightly tidy-up", desc: "", value: "2:00 am" },
    ],
  },
];

export const onboardingSteps: OnboardingStep[] = [
  {
    title: "Hello. I’m Borg.",
    body: "I run on this machine, not in someone else’s cloud. Everything I learn stays in a folder you own.",
    cta: "Get started",
    alt: "Takes about a minute",
  },
  {
    title: "Pick a brain.",
    body: "Paste a key from any provider you already use, or point me at one running locally. You can change it whenever.",
    cta: "Connect",
    alt: "I have one running locally",
  },
  {
    title: "Ready when you are.",
    body: "Ask me anything. I’ll add tools and connections as we need them — no setup up front.",
    cta: "Start talking",
    alt: "Import from another agent",
  },
];
