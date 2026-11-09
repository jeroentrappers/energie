// Makes the dashboard installable. It caches nothing on purpose: live readings and signed-in
// pages must never come from a copy on the phone. It answers only when the device reports being
// offline; every other request, sign-in included, goes to the network as if it weren't here.
"use strict";

const TEXT = {
  en: ["No connection", "energie needs a network connection to show live values.", "Try again"],
  nl: ["Geen verbinding", "energie heeft een netwerkverbinding nodig om live waarden te tonen.", "Opnieuw proberen"],
};

function offlinePage() {
  const lang = /^nl\b/i.test(self.navigator.language || "") ? "nl" : "en";
  const [title, body, retry] = TEXT[lang];
  return `<!doctype html><html lang="${lang}"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><title>${title}</title><body style="font:16px system-ui,sans-serif;text-align:center;padding:20vh 24px"><h1 style="font-size:22px">${title}</h1><p>${body}</p><p><a href="/">${retry}</a></p></body></html>`;
}

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  if (event.request.mode === "navigate" && self.navigator.onLine === false) {
    event.respondWith(new Response(offlinePage(), { headers: { "content-type": "text/html; charset=utf-8" } }));
  }
});
