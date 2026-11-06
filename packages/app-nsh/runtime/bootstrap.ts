// TweetNaCl detects Web Crypto through `self` before trying Node's require.
const runtime = globalThis as typeof globalThis & { self?: typeof globalThis };
if (typeof runtime.self === "undefined") runtime.self = globalThis;
