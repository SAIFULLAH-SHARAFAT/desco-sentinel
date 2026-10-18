import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import worker, { isAllowed, isHelp, parseUpdate, safeEqual } from "../src/index.js";

const env = {
  TELEGRAM_BOT_TOKEN: "123:token",
  TELEGRAM_WEBHOOK_SECRET: "hook-secret",
  ALLOWED_CHATS: "111, -222",
  GITHUB_TOKEN: "ghp_test",
  GITHUB_REPO: "owner/repo",
  GITHUB_REF: "main",
};

let calls;
let githubStatus;
const realFetch = globalThis.fetch;

beforeEach(() => {
  calls = [];
  githubStatus = 204;
  globalThis.fetch = async (url, init) => {
    calls.push({ url, init, body: init && init.body ? JSON.parse(init.body) : null });
    if (String(url).includes("api.github.com")) return new Response(null, { status: githubStatus });
    return new Response("{}", { status: 200 });
  };
});

afterEach(() => {
  globalThis.fetch = realFetch;
});

const ctx = { waitUntil: (p) => p };

function telegramUpdate(chatId, text, ageSeconds = 0) {
  return new Request("https://relay.example/", {
    method: "POST",
    headers: { "X-Telegram-Bot-Api-Secret-Token": "hook-secret" },
    body: JSON.stringify({
      update_id: 1,
      message: { date: Math.floor(Date.now() / 1000) - ageSeconds, text, chat: { id: chatId } },
    }),
  });
}

const githubCalls = () => calls.filter((c) => String(c.url).includes("api.github.com"));
const sent = () => calls.filter((c) => String(c.url).endsWith("/sendMessage"));

test("GET is a harmless health page", async () => {
  const response = await worker.fetch(new Request("https://relay.example/"), env, ctx);
  assert.equal(response.status, 200);
  assert.equal(calls.length, 0);
});

test("a request without the right secret is rejected and does nothing", async () => {
  const request = new Request("https://relay.example/", {
    method: "POST",
    headers: { "X-Telegram-Bot-Api-Secret-Token": "wrong" },
    body: JSON.stringify({ message: { text: "status", chat: { id: 111 }, date: 1 } }),
  });
  const response = await worker.fetch(request, env, ctx);
  assert.equal(response.status, 401);
  assert.equal(calls.length, 0);

  const missing = new Request("https://relay.example/", { method: "POST", body: "{}" });
  assert.equal((await worker.fetch(missing, env, ctx)).status, 401);
});

test("a missing server-side secret never lets anything through", async () => {
  const response = await worker.fetch(telegramUpdate(111, "status"), { ...env, TELEGRAM_WEBHOOK_SECRET: "" }, ctx);
  assert.equal(response.status, 401);
});

test("an allowed chat starts the workflow with chat id and text", async () => {
  const response = await worker.fetch(telegramUpdate(111, "status 825"), env, ctx);
  assert.equal(response.status, 200);
  const [call] = githubCalls();
  assert.equal(
    call.url,
    "https://api.github.com/repos/owner/repo/actions/workflows/telegram-reply.yml/dispatches",
  );
  assert.equal(call.init.headers.Authorization, "Bearer ghp_test");
  assert.deepEqual(call.body, { ref: "main", inputs: { chat_id: "111", text: "status 825" } });
  assert.ok(calls.some((c) => String(c.url).endsWith("/sendChatAction")));
});

test("group chats (negative ids) work", async () => {
  await worker.fetch(telegramUpdate(-222, "low"), env, ctx);
  assert.equal(githubCalls()[0].body.inputs.chat_id, "-222");
});

test("a stranger gets their chat id and never reaches GitHub", async () => {
  await worker.fetch(telegramUpdate(999, "status"), env, ctx);
  assert.equal(githubCalls().length, 0);
  assert.equal(sent().length, 1);
  assert.match(sent()[0].body.text, /private/);
  assert.match(sent()[0].body.text, /`999`/);
  assert.equal(sent()[0].body.chat_id, "999");
});

test("help is answered by the worker without using GitHub", async () => {
  for (const text of ["/help", "help", "/start", "/help@desco_sentinel_bot", "?"]) {
    calls.length = 0;
    await worker.fetch(telegramUpdate(111, text), env, ctx);
    assert.equal(githubCalls().length, 0, text);
    assert.match(sent()[0].body.text, /commands/, text);
  }
});

test("old messages are ignored", async () => {
  const response = await worker.fetch(telegramUpdate(111, "status", 3600), env, ctx);
  assert.equal(await response.text(), "stale");
  assert.equal(calls.length, 0);
});

test("non-text updates and broken bodies are ignored safely", async () => {
  const photo = new Request("https://relay.example/", {
    method: "POST",
    headers: { "X-Telegram-Bot-Api-Secret-Token": "hook-secret" },
    body: JSON.stringify({ message: { chat: { id: 111 }, photo: [] } }),
  });
  assert.equal((await worker.fetch(photo, env, ctx)).status, 200);
  const broken = new Request("https://relay.example/", {
    method: "POST",
    headers: { "X-Telegram-Bot-Api-Secret-Token": "hook-secret" },
    body: "not json",
  });
  assert.equal((await worker.fetch(broken, env, ctx)).status, 400);
  assert.equal(calls.length, 0);
});

test("a failed dispatch tells the user instead of staying silent", async () => {
  githubStatus = 401;
  const response = await worker.fetch(telegramUpdate(111, "status"), env, ctx);
  assert.equal(response.status, 200, "must answer 200 or Telegram re-sends the message");
  assert.match(sent()[0].body.text, /Could not start/);
});

test("long text is cut before it reaches the workflow", async () => {
  await worker.fetch(telegramUpdate(111, "x".repeat(5000)), env, ctx);
  assert.equal(githubCalls()[0].body.inputs.text.length, 200);
});

test("helpers", () => {
  assert.equal(parseUpdate({}), null);
  assert.equal(parseUpdate({ message: { text: "hi", chat: { id: 5 }, date: 3 } }).chatId, "5");
  assert.equal(isAllowed("111", "111,222"), true);
  assert.equal(isAllowed("11", "111,222"), false);
  assert.equal(isAllowed("111", ""), false);
  assert.equal(isHelp("status"), false);
  assert.equal(isHelp("825"), false);
  assert.equal(safeEqual("abc", "abc"), true);
  assert.equal(safeEqual("abc", "abd"), false);
  assert.equal(safeEqual("abc", "ab"), false);
  assert.equal(safeEqual(undefined, "ab"), false);
});
