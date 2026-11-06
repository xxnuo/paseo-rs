import { expect, open, test } from "nsh/test";

test("Rust root validates a direct host using native input", async () => {
  const app = await open("..", { width: 800, height: 600 });
  await app.waitFor("连接开发主机");
  expect(app.enabled("role:button[检查地址]")).toBe(false);
  await app.type("label:主机地址", "ws://localhost:6767/ws?token=private");
  await app.press("role:button[检查地址]");
  await app.waitFor("地址有效：ws://localhost:6767");
  expect(app.shows("text:地址有效：ws://localhost:6767/ws?token=private")).toBe(false);
});
