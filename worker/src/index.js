// Telegram -> GitHub relay (Cloudflare Worker).
//
// Telegram calls this Worker whenever someone messages the bot. The Worker checks who wrote,
// then starts the `telegram-reply` GitHub workflow, which looks up the meters and answers on
// Telegram. Only chats in ALLOWED_CHATS reach GitHub; /help is answered right here.
//
// Secrets (Cloudflare dashboard -> Worker -> Settings -> Variables and Secrets, type "Secret"):
//   TELEGRAM_BOT_TOKEN       bot token from BotFather
//   TELEGRAM_WEBHOOK_SECRET  random string, also given to Telegram when registering the webhook
//   ALLOWED_CHATS            comma separated Telegram chat ids (the same as TELEGRAM_CHAT_ID)
//   GITHUB_TOKEN             fine-grained token for this repository, "Actions: read and write"
// Plain variables (wrangler.jsonc): GITHUB_REPO ("owner/name"), GITHUB_REF (branch, default main)

const MAX_TEXT_CHARS = 200;
const MAX_MESSAGE_AGE_SECS = 300;
const WORKFLOW_FILE = "telegram-reply.yml";

export const HELP_TEXT = [
  "🤖 *DESCO Sentinel — commands*",
  "",
  "📊 `status` — balance of every meter (🔴 = below its alert level)",
  "🔴 `low` — only the meters that are low (below their alert level, 150 BDT unless set otherwise)",
  "🔎 `825` — details of one meter: type the last 3 digits of its account or meter number",
  "🔎 `status 825` — same as above",
  "🏷 `lift` — details by unit name (part of the name is enough)",
  "📋 `info` — directory of all meters (account, meter, load)",
  "❓ `help` — this message",
  "",
  "Low-balance alerts arrive here automatically. You do not need to ask for them.",
  "Answers take about half a minute.",
].join("\n");

/** Extracts a text message from a Telegram update, or null. */
export function parseUpdate(update) {
  const message = update && update.message;
  if (!message || typeof message.text !== "string" || !message.chat) return null;
  return {
    chatId: String(message.chat.id),
    text: message.text.trim().slice(0, MAX_TEXT_CHARS),
    date: Number(message.date) || 0,
  };
}

export function isAllowed(chatId, allowedCsv) {
  return String(allowedCsv || "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean)
    .includes(chatId);
}

/** `/help`, `help`, `/start`, `/help@botname`, or an empty message. */
export function isHelp(text) {
  return /^[/!.]?(start|help|menu|\?)?(@\w+)?$/i.test(text.trim());
}

/** Constant-time string comparison. */
export function safeEqual(a, b) {
  if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/** Names of settings that are missing or obviously wrong (never their values). */
export function configProblems(env) {
  const problems = [];
  if (!/^\d{5,}:[A-Za-z0-9_-]{20,}$/.test(String(env.TELEGRAM_BOT_TOKEN || ""))) {
    problems.push("TELEGRAM_BOT_TOKEN (must look like 123456789:AAxxxxxxxx...)");
  }
  if (!String(env.ALLOWED_CHATS || "").trim()) problems.push("ALLOWED_CHATS");
  if (!String(env.GITHUB_TOKEN || "").trim()) problems.push("GITHUB_TOKEN");
  if (!String(env.GITHUB_REPO || "").trim()) problems.push("GITHUB_REPO");
  return problems;
}

async function telegram(env, method, body) {
  const response = await fetch(`https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/${method}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return response;
}

async function say(env, chatId, text) {
  const response = await telegram(env, "sendMessage", {
    chat_id: chatId,
    text,
    parse_mode: "Markdown",
  });
  if (response.status === 400) {
    // Unbalanced markup: send it as plain text instead of losing the message.
    await telegram(env, "sendMessage", { chat_id: chatId, text });
  }
}

async function startReplyWorkflow(env, chatId, text) {
  const url = `https://api.github.com/repos/${env.GITHUB_REPO}/actions/workflows/${WORKFLOW_FILE}/dispatches`;
  return fetch(url, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${env.GITHUB_TOKEN}`,
      Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28",
      "User-Agent": "desco-sentinel-relay",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      ref: env.GITHUB_REF || "main",
      inputs: { chat_id: chatId, text },
    }),
  });
}

export default {
  async fetch(request, env, ctx) {
    if (request.method !== "POST") {
      return new Response("desco-sentinel relay: ok\n");
    }
    // Telegram echoes the secret we gave it in setWebhook. Anything else is not Telegram.
    if (!env.TELEGRAM_WEBHOOK_SECRET) {
      return new Response("relay not configured: TELEGRAM_WEBHOOK_SECRET is missing", { status: 500 });
    }
    const supplied = request.headers.get("X-Telegram-Bot-Api-Secret-Token");
    if (!safeEqual(supplied, env.TELEGRAM_WEBHOOK_SECRET)) {
      return new Response("unauthorized", { status: 401 });
    }
    const problems = configProblems(env);
    if (problems.length > 0) {
      return new Response(`relay not configured: ${problems.join(", ")}`, { status: 500 });
    }

    let update;
    try {
      update = await request.json();
    } catch {
      return new Response("bad request", { status: 400 });
    }

    const message = parseUpdate(update);
    if (!message) return new Response("ignored");
    // Telegram re-delivers if we are slow; very old messages are not worth answering.
    if (message.date && Date.now() / 1000 - message.date > MAX_MESSAGE_AGE_SECS) {
      return new Response("stale");
    }

    if (!isAllowed(message.chatId, env.ALLOWED_CHATS)) {
      await say(
        env,
        message.chatId,
        `⛔ This bot is private.\nYour chat id is \`${message.chatId}\`. Send it to the owner to get access.`,
      );
      return new Response("not allowed");
    }

    if (isHelp(message.text)) {
      await say(env, message.chatId, HELP_TEXT);
      return new Response("help");
    }

    // Show "typing..." while GitHub starts up; failure here does not matter.
    ctx.waitUntil(
      telegram(env, "sendChatAction", { chat_id: message.chatId, action: "typing" }).catch(() => {}),
    );

    let started;
    try {
      started = await startReplyWorkflow(env, message.chatId, message.text);
    } catch {
      started = null;
    }
    if (!started || started.status !== 204) {
      await say(
        env,
        message.chatId,
        "⚠️ Could not start the check right now. Please try again in a minute.",
      );
      return new Response("dispatch failed");
    }
    return new Response("started");
  },
};
