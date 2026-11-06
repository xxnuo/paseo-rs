import { expect, open, test } from "nsh/test";
import { encodeTerminalPaste } from "../../app/src/terminal/runtime/terminal-paste";

test("upstream terminal paste preserves text and neutralizes a premature closing marker", () => {
  expect(encodeTerminalPaste({ text: "中文\n\x1b[201~", bracketedPaste: true })).toBe(
    "\x1b[200~中文\n[201~\x1b[201~",
  );
  expect(encodeTerminalPaste({ text: "plain", bracketedPaste: false })).toBe("plain");
});

for (const platform of [undefined, "web", "linux", "android", "ios"] as const) {
  test(`signals, input and upstream logic under ${platform ?? "runtime-default"} environment`, async () => {
    const app = await open("./render.tsx", platform ? { platform } : {});
    expect(app.enabled("label:Encode paste")).toBe(false);
    await app.type("label:Paste text", "hello");
    await app.press("label:Encode paste");
    await app.waitFor("text:\x1b[200~hello\x1b[201~");
    expect(app.shows("text:\x1b[200~hello\x1b[201~")).toBe(true);
  });
}
